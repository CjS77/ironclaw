// Sidecar records kept next to the Baileys state in the encrypted store:
// known chats (the send allowlist) and their JID aliases, blocked contacts,
// inbound message references (quoting, media download, de-duplication),
// sent messages (Baileys' getMessage for retries), idempotency results and
// link metadata. The webhook spool lives in the same store (webhook/spool.ts).
import type { EncryptedStore } from './encrypted-store.js'
import { decodeJson, getJson, putJson } from './auth-state.js'

export const T = {
	chat: 'chat',
	alias: 'alias',
	blocked: 'blocked',
	msg: 'msg',
	media: 'media',
	sent: 'sent',
	idem: 'idem',
	link: 'link',
	linkState: 'link-state',
	spool: 'spool'
} as const

/** Record types kept when the account is unlinked. */
export const KEEP_ON_UNLINK: readonly string[] = [T.spool]

export interface ChatRecord {
	chat_id: string
	contact_id: string
	contact_phone_number?: string
	aliases: string[]
	first_seen_at: string
	last_inbound_at: string
}

export interface BlockedRecord {
	contact_id: string
	blocked_at: string
}

export interface MessageRecord {
	chat_id: string
	message_id: string
	/** Baileys message key, as received. */
	key: { remoteJid: string; id: string; fromMe: boolean; participant?: string }
	/** proto.Message bytes, base64. */
	message_b64: string
	received_at: string
	/**
	 * Set until the message's event is in the spool: the event_id it will be
	 * (or was) sent under. A redelivery of a pending message is forwarded
	 * again under this id; once it is cleared, a redelivery is a duplicate.
	 */
	pending_event_id?: string
}

export interface MediaRecord {
	media_id: string
	chat_id: string
	message_id: string
	mime_type: string
	size_bytes: number | null
	file_name?: string
	created_at: string
}

export interface SentRecord {
	chat_id: string
	message_id: string
	message_b64: string
	sent_at: string
}

/**
 * `rejected`: WhatsApp answered with a negative ack. `failed`: handing the
 * message to the socket failed, so WhatsApp never saw it.
 */
export type IdempotencyState = 'pending' | 'sent' | 'acked' | 'rejected' | 'failed'

export interface IdempotencyRecord {
	idempotency_key: string
	request_hash: string
	state: IdempotencyState
	chat_id: string
	message_id: string
	server_ack_at?: string
	error_code?: string
	created_at: string
}

export interface LinkRecord {
	linked_at: string
	account_label?: string
}

const LINK_ID = 'link'

export class DomainStore {
	constructor(readonly store: EncryptedStore) {}

	// --- chats -------------------------------------------------------------

	/** Resolves any JID (canonical or alias) to its known chat. */
	async findChat(jid: string): Promise<ChatRecord | undefined> {
		const direct = await getJson<ChatRecord>(this.store, T.chat, jid)
		if (direct) return direct
		const alias = await getJson<{ chat_id: string }>(this.store, T.alias, jid)
		return alias ? getJson<ChatRecord>(this.store, T.chat, alias.chat_id) : undefined
	}

	/**
	 * Records an inbound message's chat. The first JID seen becomes the stable
	 * chat_id; any other JIDs for the same user become aliases.
	 */
	async touchChat(input: {
		jids: string[]
		contactId: string
		phoneNumber?: string
		at: string
	}): Promise<ChatRecord> {
		let chat: ChatRecord | undefined
		for (const jid of input.jids) {
			chat = await this.findChat(jid)
			if (chat) break
		}
		if (!chat) {
			const first = input.jids[0]
			if (!first) throw new Error('touchChat needs at least one JID')
			chat = {
				chat_id: first,
				contact_id: input.contactId,
				aliases: [],
				first_seen_at: input.at,
				last_inbound_at: input.at
			}
		}
		chat.last_inbound_at = input.at
		if (input.phoneNumber && !chat.contact_phone_number) chat.contact_phone_number = input.phoneNumber
		for (const jid of input.jids) {
			if (jid !== chat.chat_id && !chat.aliases.includes(jid)) {
				chat.aliases.push(jid)
				await putJson(this.store, T.alias, jid, { chat_id: chat.chat_id })
			}
		}
		await putJson(this.store, T.chat, chat.chat_id, chat)
		return chat
	}

	// --- blocked contacts ---------------------------------------------------

	async isBlocked(contactId: string): Promise<boolean> {
		return (await getJson<BlockedRecord>(this.store, T.blocked, contactId)) !== undefined
	}

	async setBlocked(contactId: string, blocked: boolean, at: string): Promise<void> {
		if (blocked) await putJson(this.store, T.blocked, contactId, { contact_id: contactId, blocked_at: at })
		else await this.store.delete(T.blocked, contactId)
	}

	/** Finds the chat whose contact_id is `contactId`. */
	async findChatByContact(contactId: string): Promise<ChatRecord | undefined> {
		const direct = await this.findChat(contactId)
		if (direct) return direct
		for (const { value } of await this.store.list(T.chat)) {
			const chat = decodeJson<ChatRecord>(value)
			if (chat.contact_id === contactId) return chat
		}
		return undefined
	}

	// --- messages and media --------------------------------------------------

	private msgId(chatId: string, messageId: string): string {
		return `${chatId}|${messageId}`
	}

	async getMessage(chatId: string, messageId: string): Promise<MessageRecord | undefined> {
		return getJson<MessageRecord>(this.store, T.msg, this.msgId(chatId, messageId))
	}

	async putMessage(record: MessageRecord): Promise<void> {
		await putJson(this.store, T.msg, this.msgId(record.chat_id, record.message_id), record)
	}

	async getMedia(mediaId: string): Promise<MediaRecord | undefined> {
		return getJson<MediaRecord>(this.store, T.media, mediaId)
	}

	async putMedia(record: MediaRecord): Promise<void> {
		await putJson(this.store, T.media, record.media_id, record)
	}

	async getSent(messageId: string): Promise<SentRecord | undefined> {
		return getJson<SentRecord>(this.store, T.sent, messageId)
	}

	async putSent(record: SentRecord): Promise<void> {
		await putJson(this.store, T.sent, record.message_id, record)
	}

	// --- idempotency ------------------------------------------------------

	async getIdempotency(key: string): Promise<IdempotencyRecord | undefined> {
		return getJson<IdempotencyRecord>(this.store, T.idem, key)
	}

	async putIdempotency(record: IdempotencyRecord): Promise<void> {
		await putJson(this.store, T.idem, record.idempotency_key, record)
	}

	async deleteIdempotency(key: string): Promise<void> {
		await this.store.delete(T.idem, key)
	}

	/** Finds the idempotency record for a sent message id (for late acks). */
	async findIdempotencyByMessage(messageId: string): Promise<IdempotencyRecord | undefined> {
		for (const { value } of await this.store.list(T.idem)) {
			const record = decodeJson<IdempotencyRecord>(value)
			if (record.message_id === messageId) return record
		}
		return undefined
	}

	// --- link metadata ----------------------------------------------------

	async getLink(): Promise<LinkRecord | undefined> {
		return getJson<LinkRecord>(this.store, T.link, LINK_ID)
	}

	async putLink(record: LinkRecord): Promise<void> {
		await putJson(this.store, T.link, LINK_ID, record)
	}

	// --- retention ------------------------------------------------------

	/**
	 * Deletes message references, media references, sent-message copies and
	 * idempotency results older than `cutoff`. These are the sidecar's own
	 * working copies; IronClaw holds the durable record of every event.
	 */
	async prune(cutoff: Date): Promise<number> {
		const limit = cutoff.toISOString()
		let removed = 0
		const sweep = async (type: string, stamp: (v: Record<string, unknown>) => unknown) => {
			for (const { id, value } of await this.store.list(type)) {
				const at = stamp(decodeJson<Record<string, unknown>>(value))
				if (typeof at === 'string' && at < limit) {
					await this.store.delete(type, id)
					removed++
				}
			}
		}
		await sweep(T.msg, v => v.received_at)
		await sweep(T.media, v => v.created_at)
		await sweep(T.sent, v => v.sent_at)
		await sweep(T.idem, v => v.created_at)
		return removed
	}
}
