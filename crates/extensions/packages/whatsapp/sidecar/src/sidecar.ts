// The sidecar's core: lock state, inbound processing, sends, block, media,
// and link management. The HTTP layer (http/server.ts) is a thin shell over
// this class.
import { createHash } from 'node:crypto'
import type { Readable } from 'node:stream'
import { proto } from 'baileys'
import type { MessageUpsertType, WAMessage } from 'baileys'
import type { Config } from './config.js'
import { ApiError } from './errors.js'
import { sortableId } from './crypto.js'
import { classify } from './inbound/classify.js'
import { errorKind, type Logger } from './log.js'
import { SendRateLimiter } from './outbound/rate-limiter.js'
import { CREDS_TYPE, SIGNAL_CATEGORIES, signalType } from './store/auth-state.js'
import {
	DomainStore,
	KEEP_ON_UNLINK,
	T,
	type IdempotencyRecord,
	type MediaRecord,
	type MessageRecord
} from './store/domain.js'
import {
	CorruptRecordError,
	EncryptedStore,
	KeyCheckMissingError,
	WrongKeyError,
	type UnlockResult
} from './store/encrypted-store.js'
import { envelope, type Envelope, type EventBody, type LinkState, type MediaDescriptor } from './webhook/events.js'
import { Deliverer, LockedAnnouncer, Spool, type Transport } from './webhook/spool.js'
import {
	NotConnectedError,
	type ConnectionEvents,
	type LinkDisplay,
	type LinkMode,
	type StoredMessageRef,
	type WhatsAppPort
} from './whatsapp/port.js'

export type WhatsAppFactory = (store: EncryptedStore, events: ConnectionEvents, wipeAuth: () => Promise<void>) => WhatsAppPort

export interface SendRequest {
	chat_id: string
	text: string
	quoted_message_id?: string
	idempotency_key: string
}

export interface SendResult {
	message_id: string
	server_ack_at: string
}

export interface LinkStatusResponse {
	state: LinkState
	connected: boolean
	account_label?: string
	linked_at?: string
	display?: LinkDisplay
}

interface AckWaiter {
	resolve: (ack: { at: Date; errorCode?: string }) => void
}

const PRUNE_INTERVAL_MS = 24 * 60 * 60 * 1000

/** Runs tasks one at a time per key; tasks for different keys run concurrently. */
class KeyedQueue {
	private readonly tails = new Map<string, Promise<unknown>>()

	async run<T>(key: string, task: () => Promise<T>): Promise<T> {
		const result = (this.tails.get(key) ?? Promise.resolve()).then(task)
		const tail = result.catch(() => undefined)
		this.tails.set(key, tail)
		try {
			return await result
		} finally {
			if (this.tails.get(key) === tail) this.tails.delete(key)
		}
	}
}

function requestHash(req: SendRequest): string {
	return createHash('sha256')
		.update(JSON.stringify([req.chat_id, req.text, req.quoted_message_id ?? null]))
		.digest('hex')
}

function encodeMessage(content: proto.IMessage): string {
	return Buffer.from(proto.Message.encode(content).finish()).toString('base64')
}

function decodeMessage(b64: string): proto.IMessage {
	return proto.Message.decode(Buffer.from(b64, 'base64'))
}

export class Sidecar {
	readonly store: EncryptedStore
	private domain: DomainStore
	private readonly spool: Spool
	private readonly deliverer: Deliverer
	private readonly announcer: LockedAnnouncer
	private readonly limiter: SendRateLimiter
	private whatsapp: WhatsAppPort | undefined
	private readonly ackWaiters = new Map<string, AckWaiter>()
	private inbound: Promise<void> = Promise.resolve()
	private pruneTimer: NodeJS.Timeout | undefined
	/** unlock, lock and shutdown run one at a time, in the order they were called. */
	private lifecycle: Promise<unknown> = Promise.resolve()
	/** Sends sharing an idempotency key run one at a time. */
	private readonly sendQueue = new KeyedQueue()
	private readonly log: Logger

	constructor(
		readonly config: Config,
		logger: Logger,
		private readonly createWhatsApp: WhatsAppFactory,
		transport?: Transport
	) {
		this.log = logger
		this.store = new EncryptedStore(config.dataDir.replace(/\/+$/, '') + '/store')
		this.domain = new DomainStore(this.store)
		this.spool = new Spool(this.store, logger.child({ component: 'spool' }))
		const delivery = {
			url: config.webhookUrl,
			hmacKey: config.webhookHmacKey,
			timeoutMs: config.webhookTimeoutMs,
			retryBaseMs: config.webhookRetryBaseMs,
			retryMaxMs: config.webhookRetryMaxMs,
			logger: logger.child({ component: 'webhook' }),
			...(transport ? { transport } : {})
		}
		this.deliverer = new Deliverer(this.spool, delivery)
		this.announcer = new LockedAnnouncer(delivery)
		this.limiter = new SendRateLimiter(
			{ perMinute: config.limits.sendPerChatPerMinute, burst: config.limits.sendPerChatBurst },
			{ perMinute: config.limits.sendGlobalPerMinute, burst: config.limits.sendGlobalBurst }
		)
	}

	get locked(): boolean {
		return !this.store.isUnlocked
	}

	/** Prepares the data directory and announces that the sidecar is locked. */
	async boot(): Promise<void> {
		await this.store.init()
		this.announceLocked()
	}

	private announceLocked(): void {
		// One locked notice per boot (or per re-lock); the announcer retries this
		// same envelope, so its event_id is stable across retries.
		this.announcer.start(envelope({ type: 'sidecar.locked', data: {} }))
	}

	// --- lock state -------------------------------------------------------

	private serially<T>(task: () => Promise<T>): Promise<T> {
		const run = this.lifecycle.then(task)
		this.lifecycle = run.catch(() => undefined)
		return run
	}

	/**
	 * Unlocks the store and starts the connection. Unlocks, locks and
	 * shutdown are queued, so concurrent callers never build two connections:
	 * each one sees the state the previous one left.
	 */
	unlock(dataKey: Buffer): Promise<UnlockResult> {
		return this.serially(async () => {
			if (this.store.isUnlocked) {
				if (!this.store.matchesKey(dataKey)) throw new ApiError(403, 'wrong_key', 'the data key does not match this store')
				return { initialized: false }
			}
			return this.doUnlock(dataKey)
		})
	}

	private async doUnlock(dataKey: Buffer): Promise<UnlockResult> {
		let result: UnlockResult
		try {
			result = await this.store.unlock(dataKey)
		} catch (error) {
			if (error instanceof WrongKeyError) throw new ApiError(403, 'wrong_key', 'the data key does not match this store')
			if (error instanceof KeyCheckMissingError) {
				throw new ApiError(409, 'key_check_missing', 'the store holds records but no key-check record')
			}
			if (error instanceof CorruptRecordError) throw new ApiError(500, 'store_corrupt')
			throw error
		}
		this.announcer.stop()
		try {
			await this.spool.load()
			this.deliverer.start()
			this.whatsapp = this.createWhatsApp(this.store, this.connectionEvents(), () => this.wipeAuth())
		} catch (error) {
			// The store is open but the sidecar cannot run: lock it again rather
			// than answer 200 to /v1/unlock and 423 to everything else.
			this.log.error('unlock failed after opening the store; locking it again', errorKind(error))
			await this.doLock()
			throw error
		}
		this.log.info('store unlocked', { initialized: result.initialized })
		try {
			await this.whatsapp.start()
		} catch (error) {
			this.log.error('starting the WhatsApp connection failed', errorKind(error))
		}
		void this.prune()
		this.pruneTimer = setInterval(() => void this.prune(), PRUNE_INTERVAL_MS)
		this.pruneTimer.unref()
		return result
	}

	/** Stops everything that needs the key and forgets it. */
	lock(): Promise<void> {
		return this.serially(() => this.doLock())
	}

	private async doLock(): Promise<void> {
		clearInterval(this.pruneTimer)
		this.pruneTimer = undefined
		await this.whatsapp?.stop()
		this.whatsapp = undefined
		await this.deliverer.stop()
		await this.inbound.catch(() => undefined)
		this.spool.unload()
		this.store.lock()
		this.log.info('store locked')
		this.announceLocked()
	}

	shutdown(): Promise<void> {
		return this.serially(async () => {
			this.announcer.stop()
			clearInterval(this.pruneTimer)
			await this.whatsapp?.stop()
			await this.deliverer.stop()
			await this.inbound.catch(() => undefined)
			this.store.lock()
		})
	}

	private requireUnlocked(): WhatsAppPort {
		if (!this.store.isUnlocked || !this.whatsapp) throw new ApiError(423, 'locked')
		return this.whatsapp
	}

	private async wipeAuth(): Promise<void> {
		await this.store.wipeTypes([CREDS_TYPE, ...SIGNAL_CATEGORIES.map(signalType)])
	}

	private async prune(): Promise<void> {
		try {
			const cutoff = new Date(Date.now() - this.config.messageRetentionDays * 24 * 60 * 60 * 1000)
			const removed = await this.domain.prune(cutoff)
			if (removed) this.log.info('pruned old message references', { removed })
		} catch (error) {
			this.log.warn('pruning failed', errorKind(error))
		}
	}

	// --- events from the connection ----------------------------------------

	private connectionEvents(): ConnectionEvents {
		return {
			onUpsert: (type, messages) => {
				this.inbound = this.inbound
					.then(() => this.handleUpsert(type, messages))
					.catch(error => this.log.error('inbound processing failed', errorKind(error)))
			},
			onLinkState: (state, accountLabel) => {
				this.inbound = this.inbound
					.then(() => this.handleLinkState(state, accountLabel))
					.catch(error => this.log.error('link state handling failed', errorKind(error)))
			},
			onAck: (messageId, ackAtSecs, errorCode) => this.handleAck(messageId, ackAtSecs, errorCode),
			getSentMessage: async messageId => {
				if (!this.store.isUnlocked) return undefined
				const sent = await this.domain.getSent(messageId)
				return sent ? decodeMessage(sent.message_b64) : undefined
			}
		}
	}

	private async emit(body: EventBody, eventId?: string): Promise<Envelope> {
		const event = envelope(body)
		if (eventId) event.event_id = eventId
		await this.spool.enqueue(event)
		return event
	}

	async handleUpsert(type: MessageUpsertType, messages: WAMessage[]): Promise<void> {
		const whatsapp = this.whatsapp
		if (!whatsapp) return
		for (const msg of messages) {
			// One message's failure (say, a full disk) must not drop the rest of
			// the batch. A failed customer message stays pending (see below), so
			// WhatsApp's redelivery of it is forwarded, not ignored.
			try {
				await this.handleInbound(classify(type, msg, whatsapp.ownJids()))
			} catch (error) {
				this.log.error('inbound message failed', errorKind(error))
			}
		}
	}

	private async handleInbound(c: ReturnType<typeof classify>): Promise<void> {
		if (c.kind === 'drop') {
			this.log.debug('inbound dropped', { reason: c.reason })
			return
		}
		if (c.kind === 'owner_activity') {
			if (await this.domain.getSent(c.messageId)) return // sent through this sidecar
			const chat = await this.findChatForJids(c.jids)
			if (!chat) {
				this.log.debug('owner activity in a chat no customer started; not forwarded')
				return
			}
			await this.emit({
				type: 'owner.activity',
				data: { chat_id: chat.chat_id, message_id: c.messageId, sent_at: c.timestamp.toISOString() }
			})
			this.log.info('owner activity forwarded')
			return
		}

		const now = new Date().toISOString()
		const existingChat = await this.findChatForJids(c.jids)
		const existing = existingChat ? await this.domain.getMessage(existingChat.chat_id, c.messageId) : undefined
		if (existing && !existing.pending_event_id) {
			this.log.debug('duplicate inbound message ignored')
			return
		}
		// The message record is written before its event is spooled (the event
		// names its media, and media downloads need the record) and marked
		// pending until the spool holds the event. A redelivery of a pending
		// message is forwarded again under the same event_id, which IronClaw
		// de-duplicates on.
		const eventId = existing?.pending_event_id ?? sortableId('evt_')
		const chat = await this.domain.touchChat({
			jids: c.jids,
			contactId: c.contactId,
			...(c.phoneNumber ? { phoneNumber: c.phoneNumber } : {}),
			at: now
		})
		const blocked = await this.domain.isBlocked(chat.contact_id)
		const message: MessageRecord = {
			chat_id: chat.chat_id,
			message_id: c.messageId,
			key: c.key,
			message_b64: encodeMessage(c.content),
			received_at: now
		}
		await this.domain.putMessage({ ...message, pending_event_id: eventId })
		const media: MediaDescriptor[] = []
		for (const m of c.media) {
			const record: MediaRecord = {
				media_id: sortableId('med_'),
				chat_id: chat.chat_id,
				message_id: c.messageId,
				mime_type: m.mime_type,
				size_bytes: m.size_bytes,
				...(m.file_name ? { file_name: m.file_name } : {}),
				created_at: now
			}
			await this.domain.putMedia(record)
			media.push({
				media_id: record.media_id,
				mime_type: record.mime_type,
				size_bytes: record.size_bytes,
				...(record.file_name ? { file_name: record.file_name } : {})
			})
		}
		await this.emit(
			{
				type: 'message.received',
				data: {
					message_id: c.messageId,
					chat_id: chat.chat_id,
					contact_id: chat.contact_id,
					...(chat.contact_phone_number ? { contact_phone_number: chat.contact_phone_number } : {}),
					...(c.pushName ? { contact_display_name: c.pushName } : {}),
					content_kind: c.contentKind,
					...(c.text !== undefined ? { text: c.text } : {}),
					...(c.textTruncated ? { text_truncated: true } : {}),
					media,
					...(c.quotedMessageId ? { quoted_message_id: c.quotedMessageId } : {}),
					blocked,
					received_at: c.timestamp.toISOString()
				}
			},
			eventId
		)
		await this.domain.putMessage(message)
		this.log.info('message received', { content_kind: c.contentKind, media: media.length, blocked })
	}

	private async findChatForJids(jids: string[]) {
		for (const jid of jids) {
			const chat = await this.domain.findChat(jid)
			if (chat) return chat
		}
		return undefined
	}

	private async handleLinkState(state: LinkState, accountLabel?: string): Promise<void> {
		if (!this.store.isUnlocked) return
		if (state === 'linked') {
			const previous = await this.domain.getLink()
			const linkedAt = previous?.linked_at ?? new Date().toISOString()
			await this.domain.putLink({ linked_at: linkedAt, ...(accountLabel ? { account_label: accountLabel } : {}) })
			if (previous) return // a reconnect, not a new link
		}
		if (state === 'logged_out') {
			// WhatsApp ended the session: the credentials are dead, and the
			// known-chat allowlist belonged to that session.
			await this.store.wipeAllExcept(KEEP_ON_UNLINK)
			await this.store.putBytes(T.linkState, 'state', Buffer.from('logged_out'))
		}
		await this.emit({
			type: 'link.state_changed',
			data: { state, ...(accountLabel ? { account_label: accountLabel } : {}) }
		})
		this.log.info('link state changed', { state })
	}

	private handleAck(messageId: string, ackAtSecs: number | undefined, errorCode?: string): void {
		const at = ackAtSecs ? new Date(ackAtSecs * 1000) : new Date()
		const waiter = this.ackWaiters.get(messageId)
		if (waiter) {
			this.ackWaiters.delete(messageId)
			waiter.resolve({ at, ...(errorCode ? { errorCode } : {}) })
			return
		}
		// A late ack: the send already answered 504 (possibly before a restart).
		// Settle its stored result so a retry with the same key reports it.
		if (!this.store.isUnlocked) return
		this.inbound = this.inbound
			.then(async () => {
				const record = await this.domain.findIdempotencyByMessage(messageId)
				if (!record || record.state !== 'sent') return
				await this.domain.putIdempotency(
					errorCode
						? { ...record, state: 'rejected', error_code: errorCode }
						: { ...record, state: 'acked', server_ack_at: at.toISOString() }
				)
			})
			.catch(error => this.log.error('recording a late ack failed', errorKind(error)))
	}

	// --- API operations ----------------------------------------------------

	/**
	 * Sends a text message. Requests sharing an idempotency key are queued:
	 * the second waits for the first and then replays its stored result, so
	 * the message is never sent twice.
	 */
	send(req: SendRequest): Promise<SendResult> {
		return this.sendQueue.run(req.idempotency_key, () => this.sendOnce(req))
	}

	private async sendOnce(req: SendRequest): Promise<SendResult> {
		const whatsapp = this.requireUnlocked()
		const hash = requestHash(req)
		const previous = await this.domain.getIdempotency(req.idempotency_key)
		if (previous) return this.replay(previous, hash)

		const chat = await this.domain.findChat(req.chat_id)
		if (!chat) throw new ApiError(403, 'not_a_known_chat', 'this chat has not sent a message to this number')
		if (await this.domain.isBlocked(chat.contact_id)) throw new ApiError(403, 'contact_blocked')
		if (!whatsapp.status().connected) throw new ApiError(503, 'not_connected')

		let quoted: StoredMessageRef | undefined
		if (req.quoted_message_id) {
			const record = await this.domain.getMessage(chat.chat_id, req.quoted_message_id)
			if (!record) throw new ApiError(422, 'unknown_quoted_message')
			quoted = { key: record.key, content: decodeMessage(record.message_b64) }
		}

		const decision = this.limiter.tryAcquire(chat.chat_id)
		if (!decision.ok) {
			const secs = Math.max(1, Math.ceil(decision.retryAfterMs / 1000))
			throw new ApiError(429, 'rate_limited', undefined, { retry_after_secs: secs }, { 'retry-after': String(secs) })
		}

		const messageId = whatsapp.newMessageId()
		const record: IdempotencyRecord = {
			idempotency_key: req.idempotency_key,
			request_hash: hash,
			state: 'pending',
			chat_id: chat.chat_id,
			message_id: messageId,
			created_at: new Date().toISOString()
		}
		await this.domain.putIdempotency(record)

		let ackTimer: NodeJS.Timeout | undefined
		const ack = new Promise<{ at: Date; errorCode?: string } | undefined>(resolve => {
			this.ackWaiters.set(messageId, { resolve })
			ackTimer = setTimeout(() => {
				this.ackWaiters.delete(messageId)
				resolve(undefined)
			}, this.config.limits.sendAckTimeoutMs)
		})

		try {
			let content: proto.IMessage
			try {
				content = await whatsapp.sendText(chat.chat_id, req.text, {
					messageId,
					...(quoted ? { quoted } : {})
				})
			} catch (error) {
				this.ackWaiters.delete(messageId)
				if (error instanceof NotConnectedError) {
					// Nothing reached WhatsApp: free the key so a retry can send.
					await this.domain.deleteIdempotency(req.idempotency_key)
					throw new ApiError(503, 'not_connected')
				}
				await this.domain.putIdempotency({ ...record, state: 'failed', error_code: 'send_failed' })
				this.log.warn('send failed', errorKind(error))
				throw new ApiError(502, 'send_failed')
			}
			await this.domain.putSent({
				chat_id: chat.chat_id,
				message_id: messageId,
				message_b64: encodeMessage(content),
				sent_at: new Date().toISOString()
			})
			await this.domain.putIdempotency({ ...record, state: 'sent' })

			const result = await ack
			if (!result) {
				this.log.warn('send not acknowledged in time')
				throw new ApiError(504, 'ack_timeout', 'WhatsApp did not acknowledge the message in time', {
					message_id: messageId
				})
			}
			if (result.errorCode) {
				await this.domain.putIdempotency({ ...record, state: 'rejected', error_code: result.errorCode })
				this.log.warn('send rejected by WhatsApp', { code: result.errorCode })
				throw new ApiError(502, 'send_rejected', undefined, { code: result.errorCode, message_id: messageId })
			}
			const serverAckAt = result.at.toISOString()
			await this.domain.putIdempotency({ ...record, state: 'acked', server_ack_at: serverAckAt })
			this.log.info('message sent')
			return { message_id: messageId, server_ack_at: serverAckAt }
		} finally {
			clearTimeout(ackTimer)
		}
	}

	private replay(previous: IdempotencyRecord, hash: string): SendResult {
		if (previous.request_hash !== hash) {
			throw new ApiError(409, 'idempotency_conflict', 'this idempotency key was used for a different request')
		}
		switch (previous.state) {
			case 'acked':
				return { message_id: previous.message_id, server_ack_at: previous.server_ack_at ?? previous.created_at }
			case 'sent':
				throw new ApiError(504, 'ack_timeout', 'WhatsApp has not acknowledged the message yet', {
					message_id: previous.message_id
				})
			case 'rejected':
				throw new ApiError(502, 'send_rejected', undefined, {
					message_id: previous.message_id,
					...(previous.error_code ? { code: previous.error_code } : {})
				})
			case 'failed':
				throw new ApiError(502, 'send_failed')
			case 'pending':
				throw new ApiError(409, 'send_outcome_unknown', 'an earlier attempt with this key did not finish', {
					message_id: previous.message_id
				})
		}
	}

	async setBlocked(contactId: string, blocked: boolean): Promise<{ contact_id: string; blocked: boolean }> {
		const whatsapp = this.requireUnlocked()
		const chat = await this.domain.findChatByContact(contactId)
		if (!chat) throw new ApiError(404, 'unknown_contact')
		if (!whatsapp.status().connected) throw new ApiError(503, 'not_connected')
		try {
			await whatsapp.setBlocked(chat.contact_id, blocked)
		} catch (error) {
			if (error instanceof NotConnectedError) throw new ApiError(503, 'not_connected')
			this.log.warn('block status update failed', errorKind(error))
			throw new ApiError(502, 'block_failed')
		}
		await this.domain.setBlocked(chat.contact_id, blocked, new Date().toISOString())
		this.log.info(blocked ? 'contact blocked' : 'contact unblocked')
		return { contact_id: chat.contact_id, blocked }
	}

	async media(mediaId: string): Promise<{ record: MediaRecord; stream: Readable }> {
		const whatsapp = this.requireUnlocked()
		const record = await this.domain.getMedia(mediaId)
		if (!record) throw new ApiError(404, 'unknown_media')
		if (record.size_bytes !== null && record.size_bytes > this.config.limits.mediaMaxBytes) {
			throw new ApiError(413, 'media_too_large', undefined, { max_bytes: this.config.limits.mediaMaxBytes })
		}
		const message = await this.domain.getMessage(record.chat_id, record.message_id)
		if (!message) throw new ApiError(410, 'media_expired')
		try {
			const stream = await whatsapp.downloadMedia({ key: message.key, content: decodeMessage(message.message_b64) })
			return { record, stream }
		} catch (error) {
			this.log.warn('media download failed', errorKind(error))
			throw new ApiError(502, 'media_download_failed')
		}
	}

	async linkBegin(mode: LinkMode, phoneNumber?: string): Promise<LinkDisplay> {
		const whatsapp = this.requireUnlocked()
		try {
			await this.store.delete(T.linkState, 'state')
			return await whatsapp.beginLink(mode, phoneNumber)
		} catch (error) {
			if (error instanceof ApiError) throw error
			const name = (error as Error)?.name
			if (name === 'LinkConflictError') throw new ApiError(409, 'already_linked')
			const reason = (error as { reason?: string }).reason
			if (reason === 'display_timeout') throw new ApiError(504, 'link_timeout')
			if (reason === 'pairing_code_failed') throw new ApiError(502, 'pairing_code_failed')
			if (reason) throw new ApiError(409, 'link_ended', undefined, { reason })
			this.log.warn('link begin failed', errorKind(error))
			throw new ApiError(502, 'link_failed')
		}
	}

	async linkStatus(): Promise<LinkStatusResponse> {
		const whatsapp = this.requireUnlocked()
		const status = whatsapp.status()
		const link = await this.domain.getLink()
		let state = status.state
		if (state === 'unlinked' && (await this.store.getBytes(T.linkState, 'state'))?.toString() === 'logged_out') {
			state = 'logged_out'
		}
		return {
			state,
			connected: status.connected,
			...(status.account_label ?? link?.account_label ? { account_label: status.account_label ?? link?.account_label } : {}),
			...(state === 'linked' && link?.linked_at ? { linked_at: link.linked_at } : {}),
			...(status.display && state === 'linking' ? { display: status.display } : {})
		}
	}

	async linkCancel(): Promise<LinkStatusResponse> {
		const whatsapp = this.requireUnlocked()
		await whatsapp.cancelLink()
		return this.linkStatus()
	}

	/** Logs the device out, wipes the store (keeping undelivered events) and locks. */
	async unlink(): Promise<{ state: 'unlinked'; locked: true }> {
		const whatsapp = this.requireUnlocked()
		await whatsapp.logout()
		await this.inbound.catch(() => undefined)
		await this.store.wipeAllExcept(KEEP_ON_UNLINK)
		this.log.info('account unlinked; store wiped')
		await this.lock()
		return { state: 'unlinked', locked: true }
	}
}
