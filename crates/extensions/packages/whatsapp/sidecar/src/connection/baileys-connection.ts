// WhatsAppPort over a Baileys socket: socket lifecycle, reconnect with capped
// exponential backoff, logout detection, and the two link flows (QR code and
// pairing code).
import type { Readable } from 'node:stream'
import {
	Browsers,
	DisconnectReason,
	downloadMediaMessage,
	generateMessageIDV2,
	isJidBroadcast,
	isJidGroup,
	isJidNewsletter,
	jidDecode,
	jidNormalizedUser,
	makeCacheableSignalKeyStore,
	makeWASocket
} from 'baileys'
import type { AuthenticationCreds, BinaryNode, ConnectionState, WAMessage } from 'baileys'
import { baileysLogger, errorKind, type Logger } from '../log.js'
import { checkMediaLocation, MEDIA_HOST } from './media-location.js'
import { loadEncryptedAuthState, type EncryptedAuthState } from '../store/auth-state.js'
import type { EncryptedStore } from '../store/encrypted-store.js'
import { backoffMs } from '../webhook/spool.js'
import { clip, LIMITS, type LinkState } from '../webhook/events.js'
import {
	LinkConflictError,
	NotConnectedError,
	type ConnectionEvents,
	type LinkDisplay,
	type LinkMode,
	type LinkStatus,
	type StoredMessageRef,
	type WhatsAppPort
} from '../whatsapp/port.js'

type Socket = ReturnType<typeof makeWASocket>

export class LinkFailedError extends Error {
	constructor(readonly reason: 'cancelled' | 'expired' | 'rejected' | 'pairing_code_failed' | 'display_timeout') {
		super(`link flow ended: ${reason}`)
		this.name = 'LinkFailedError'
	}
}

/** WhatsApp's first QR code lasts 60 s and later ones 20 s (Baileys' defaults). */
const FIRST_QR_SECS = 60
const NEXT_QR_SECS = 20
/** Pairing codes are accepted for a few minutes after they are issued. */
const PAIRING_CODE_SECS = 180
/** How long POST /v1/link/begin waits for the first QR code or pairing code. */
const FIRST_DISPLAY_TIMEOUT_MS = 30_000

export function disconnectCode(update: Partial<ConnectionState>): number | undefined {
	const error = update.lastDisconnect?.error as { output?: { statusCode?: unknown } } | undefined
	const code = error?.output?.statusCode
	return typeof code === 'number' ? code : undefined
}

export type CloseAction = 'logged_out' | 'restart_now' | 'reconnect'

/** What to do after the socket closed with `code`. */
export function closeAction(code: number | undefined): CloseAction {
	if (code === DisconnectReason.loggedOut || code === DisconnectReason.forbidden) return 'logged_out'
	if (code === DisconnectReason.restartRequired) return 'restart_now'
	return 'reconnect'
}

export function accountLabel(creds: AuthenticationCreds): string | undefined {
	const name = creds.me?.name
	if (name && name !== '~' && name.trim()) return clip(name.trim(), LIMITS.labelChars)
	const user = jidDecode(creds.me?.id)?.user
	return user && user.length >= 4 ? `…${user.slice(-4)}` : undefined
}

export function formatPairingCode(code: string): string {
	return code.length === 8 ? `${code.slice(0, 4)}-${code.slice(4)}` : code
}

interface Flow {
	mode: LinkMode
	phoneNumber?: string
	display?: LinkDisplay
	qrCount: number
	pairingRequested: boolean
	waiters: Array<{ resolve: (d: LinkDisplay) => void; reject: (e: Error) => void }>
	timer: NodeJS.Timeout
}

export interface BaileysConnectionOptions {
	store: EncryptedStore
	events: ConnectionEvents
	logger: Logger
	reconnectBaseMs: number
	reconnectMaxMs: number
	linkFlowTtlMs: number
	/** Removes the Baileys credentials and signal keys from the store. */
	wipeAuth: () => Promise<void>
}

export class BaileysConnection implements WhatsAppPort {
	private sock: Socket | undefined
	private auth: EncryptedAuthState | undefined
	private generation = 0
	private state: LinkState = 'unlinked'
	private connected = false
	private label: string | undefined
	private flow: Flow | undefined
	private reconnectTimer: NodeJS.Timeout | undefined
	private attempt = 0
	private stopped = false
	private readonly log: Logger

	constructor(private readonly options: BaileysConnectionOptions) {
		this.log = options.logger.child({ component: 'connection' })
	}

	status(): LinkStatus {
		return {
			state: this.state,
			connected: this.connected,
			...(this.label ? { account_label: this.label } : {}),
			...(this.flow?.display ? { display: this.flow.display } : {})
		}
	}

	ownJids(): ReadonlySet<string> {
		const me = this.auth?.state.creds.me
		const out = new Set<string>()
		if (me?.id) out.add(jidNormalizedUser(me.id))
		if (me?.lid) out.add(jidNormalizedUser(me.lid))
		return out
	}

	async start(): Promise<void> {
		this.stopped = false
		this.auth = await loadEncryptedAuthState(this.options.store)
		const creds = this.auth.state.creds
		if (creds.account && creds.me) {
			this.state = 'linked'
			this.label = accountLabel(creds)
			this.connect()
		} else {
			this.state = 'unlinked'
		}
	}

	async stop(): Promise<void> {
		this.stopped = true
		this.teardown(new LinkFailedError('cancelled'))
	}

	async beginLink(mode: LinkMode, phoneNumber?: string): Promise<LinkDisplay> {
		if (this.state === 'linked') throw new LinkConflictError('an account is already linked; unlink it first')
		this.teardown(new LinkFailedError('cancelled'))
		await this.options.wipeAuth()
		this.auth = await loadEncryptedAuthState(this.options.store)
		this.stopped = false
		this.state = 'linking'
		this.label = undefined
		const flow: Flow = {
			mode,
			...(phoneNumber ? { phoneNumber } : {}),
			qrCount: 0,
			pairingRequested: false,
			waiters: [],
			timer: setTimeout(() => void this.endFlow('expired'), this.options.linkFlowTtlMs)
		}
		flow.timer.unref()
		this.flow = flow
		const first = new Promise<LinkDisplay>((resolve, reject) => flow.waiters.push({ resolve, reject }))
		this.log.info('link flow started', { mode })
		this.connect()
		let timer: NodeJS.Timeout | undefined
		const timeout = new Promise<never>((_, reject) => {
			timer = setTimeout(() => reject(new LinkFailedError('display_timeout')), FIRST_DISPLAY_TIMEOUT_MS)
		})
		try {
			return await Promise.race([first, timeout])
		} finally {
			clearTimeout(timer)
		}
	}

	async cancelLink(): Promise<void> {
		if (this.state !== 'linking') return
		await this.endFlow('cancelled')
	}

	async logout(): Promise<void> {
		const sock = this.sock
		// The close that follows a deliberate logout is not a "logged out by
		// WhatsApp" event and must not trigger a reconnect.
		this.stopped = true
		if (sock && this.connected) {
			try {
				await sock.logout()
			} catch (error) {
				this.log.warn('logout request failed; closing anyway', errorKind(error))
			}
		}
		this.teardown(new LinkFailedError('cancelled'))
		this.state = 'unlinked'
		this.label = undefined
	}

	newMessageId(): string {
		return generateMessageIDV2(this.sock?.user?.id)
	}

	async sendText(
		jid: string,
		text: string,
		options: { messageId: string; quoted?: StoredMessageRef }
	): Promise<import('baileys').proto.IMessage> {
		const sock = this.sock
		if (!sock || !this.connected) throw new NotConnectedError()
		const sent = await sock.sendMessage(
			jid,
			{ text },
			{
				messageId: options.messageId,
				...(options.quoted ? { quoted: { key: options.quoted.key, message: options.quoted.content } as WAMessage } : {})
			}
		)
		return sent?.message ?? { conversation: text }
	}

	async setBlocked(jid: string, blocked: boolean): Promise<void> {
		const sock = this.sock
		if (!sock || !this.connected) throw new NotConnectedError()
		await sock.updateBlockStatus(jid, blocked ? 'block' : 'unblock')
	}

	async downloadMedia(ref: StoredMessageRef): Promise<Readable> {
		const sock = this.sock
		// The customer's client wrote these fields; refuse any address that is
		// not a WhatsApp media host before Baileys fetches it. A re-upload
		// (below) returns a fresh directPath from WhatsApp's server, which
		// still goes to MEDIA_HOST.
		checkMediaLocation(ref.content)
		const message = { key: ref.key, message: ref.content } as WAMessage
		return downloadMediaMessage(
			message,
			'stream',
			{ host: MEDIA_HOST },
			{
				logger: baileysLogger(this.log),
				reuploadRequest: msg => (sock ? sock.updateMediaMessage(msg) : Promise.reject(new NotConnectedError()))
			}
		)
	}

	// --- internals ------------------------------------------------------

	private connect(): void {
		const auth = this.auth
		if (!auth) return
		const generation = ++this.generation
		const blog = baileysLogger(this.log)
		const sock = makeWASocket({
			auth: { creds: auth.state.creds, keys: makeCacheableSignalKeyStore(auth.state.keys, blog) },
			logger: blog,
			browser: Browsers.ubuntu('Chrome'),
			markOnlineOnConnect: false,
			syncFullHistory: false,
			shouldIgnoreJid: jid => Boolean(isJidGroup(jid) || isJidBroadcast(jid) || isJidNewsletter(jid)),
			getMessage: async key => (key.id ? this.options.events.getSentMessage(key.id) : undefined)
		})
		this.sock = sock

		sock.ev.process(async events => {
			if (events['creds.update']) {
				try {
					await auth.saveCreds()
				} catch (error) {
					this.log.error('saving credentials failed', errorKind(error))
				}
			}
			if (generation !== this.generation) return
			if (events['connection.update']) {
				await this.onConnectionUpdate(generation, sock, events['connection.update'])
			}
			if (events['messages.upsert']) {
				const { type, messages } = events['messages.upsert']
				this.options.events.onUpsert(type, messages)
			}
		})

		sock.ws.on('CB:ack,class:message', (node: BinaryNode) => {
			if (generation !== this.generation) return
			const id = node.attrs.id
			if (!id) return
			const t = Number(node.attrs.t)
			this.options.events.onAck(id, Number.isFinite(t) && t > 0 ? t : undefined, node.attrs.error || undefined)
		})
	}

	private async onConnectionUpdate(generation: number, sock: Socket, update: Partial<ConnectionState>): Promise<void> {
		const flow = this.flow
		if (update.qr && flow) {
			if (flow.mode === 'qr') {
				flow.qrCount += 1
				this.showDisplay(flow, {
					display_kind: 'qr_code',
					payload: update.qr,
					expires_in_secs: flow.qrCount === 1 ? FIRST_QR_SECS : NEXT_QR_SECS
				})
			} else if (!flow.pairingRequested && flow.phoneNumber) {
				flow.pairingRequested = true
				try {
					const code = await sock.requestPairingCode(flow.phoneNumber)
					if (generation !== this.generation || this.flow !== flow) return
					this.showDisplay(flow, {
						display_kind: 'pairing_code',
						payload: formatPairingCode(code),
						expires_in_secs: PAIRING_CODE_SECS
					})
				} catch (error) {
					this.log.warn('pairing code request failed', errorKind(error))
					await this.endFlow('pairing_code_failed')
					return
				}
			}
		}

		if (update.connection === 'open') {
			this.connected = true
			this.attempt = 0
			const creds = this.auth?.state.creds
			if (creds?.account) {
				const wasLinked = this.state === 'linked'
				this.state = 'linked'
				this.label = accountLabel(creds)
				if (this.flow) {
					clearTimeout(this.flow.timer)
					this.flow = undefined
					this.log.info('link flow completed')
				}
				this.log.info('connected to WhatsApp', { first_link: !wasLinked })
				this.options.events.onLinkState('linked', this.label)
			}
			return
		}

		if (update.connection === 'close') {
			if (generation !== this.generation) return
			this.connected = false
			this.sock = undefined
			const code = disconnectCode(update)
			const action = closeAction(code)
			const linked = Boolean(this.auth?.state.creds.account)
			this.log.info('connection closed', { ...(code !== undefined ? { status: code } : {}), action })
			if (this.stopped) return

			if (action === 'logged_out') {
				if (this.flow && !linked) {
					await this.endFlow('rejected')
				} else {
					this.teardown(new LinkFailedError('cancelled'))
					this.state = 'logged_out'
					this.label = undefined
					this.options.events.onLinkState('logged_out')
				}
				return
			}
			if (this.flow && !linked && code === DisconnectReason.timedOut) {
				// Baileys gave up issuing QR codes.
				await this.endFlow('expired')
				return
			}
			if (action === 'restart_now') {
				this.scheduleReconnect(0)
				return
			}
			this.scheduleReconnect(backoffMs(++this.attempt, this.options.reconnectBaseMs, this.options.reconnectMaxMs))
		}
	}

	private showDisplay(flow: Flow, display: LinkDisplay): void {
		flow.display = display
		const waiters = flow.waiters
		flow.waiters = []
		for (const w of waiters) w.resolve(display)
	}

	private scheduleReconnect(delayMs: number): void {
		clearTimeout(this.reconnectTimer)
		this.log.debug('reconnecting', { delay_ms: delayMs, attempt: this.attempt })
		this.reconnectTimer = setTimeout(() => {
			this.reconnectTimer = undefined
			if (!this.stopped) this.connect()
		}, delayMs)
	}

	private async endFlow(reason: LinkFailedError['reason']): Promise<void> {
		if (!this.flow) return
		this.log.info('link flow ended', { reason })
		this.teardown(new LinkFailedError(reason))
		this.state = 'unlinked'
		try {
			await this.options.wipeAuth()
		} catch (error) {
			this.log.error('clearing partial link state failed', errorKind(error))
		}
		this.auth = undefined
	}

	/** Closes the socket and ends any flow. Handlers of the old socket are ignored from here on. */
	private teardown(flowError: Error): void {
		this.generation++
		clearTimeout(this.reconnectTimer)
		this.reconnectTimer = undefined
		const sock = this.sock
		this.sock = undefined
		this.connected = false
		if (sock) {
			try {
				sock.end(undefined)
			} catch (error) {
				this.log.debug('closing socket failed', errorKind(error))
			}
		}
		const flow = this.flow
		this.flow = undefined
		if (flow) {
			clearTimeout(flow.timer)
			for (const w of flow.waiters) w.reject(flowError)
		}
	}
}
