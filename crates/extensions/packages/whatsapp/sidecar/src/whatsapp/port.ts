// The sidecar's view of a WhatsApp connection. The production implementation
// wraps a Baileys socket (connection/baileys-connection.ts); tests use a fake.
import type { Readable } from 'node:stream'
import type { MessageUpsertType, proto, WAMessage } from 'baileys'
import type { LinkState } from '../webhook/events.js'

export type LinkMode = 'qr' | 'pairing_code'

export interface LinkDisplay {
	display_kind: 'qr_code' | 'pairing_code'
	payload: string
	expires_in_secs: number
}

export interface LinkStatus {
	state: LinkState
	/** True while the socket to WhatsApp is open. */
	connected: boolean
	account_label?: string
	display?: LinkDisplay
}

export interface StoredMessageRef {
	key: { remoteJid: string; id: string; fromMe: boolean }
	content: proto.IMessage
}

export interface ConnectionEvents {
	/** A `messages.upsert` batch. */
	onUpsert(type: MessageUpsertType, messages: WAMessage[]): void
	/** The link state changed (linked on first open, logged_out when WhatsApp ends the session). */
	onLinkState(state: LinkState, accountLabel?: string): void
	/** WhatsApp's server ack for a message this device sent; `errorCode` set on a negative ack. */
	onAck(messageId: string, ackAtSecs: number | undefined, errorCode?: string): void
	/** Baileys asks for a message this device sent (to re-encrypt it for a retry). */
	getSentMessage(messageId: string): Promise<proto.IMessage | undefined>
}

export class LinkConflictError extends Error {
	constructor(message: string) {
		super(message)
		this.name = 'LinkConflictError'
	}
}

export class NotConnectedError extends Error {
	constructor() {
		super('not connected to WhatsApp')
		this.name = 'NotConnectedError'
	}
}

export interface WhatsAppPort {
	status(): LinkStatus
	/** The account's own JIDs, normalised (empty until linked). */
	ownJids(): ReadonlySet<string>
	/** Connects when the store holds a linked account; otherwise stays unlinked. */
	start(): Promise<void>
	/** Closes the socket without logging out. */
	stop(): Promise<void>
	beginLink(mode: LinkMode, phoneNumber?: string): Promise<LinkDisplay>
	cancelLink(): Promise<void>
	/** Logs the device out (best effort) and closes the socket. */
	logout(): Promise<void>
	newMessageId(): string
	/** Relays a text message; resolves once it is handed to the socket. Returns the content sent. */
	sendText(jid: string, text: string, options: { messageId: string; quoted?: StoredMessageRef }): Promise<proto.IMessage>
	setBlocked(jid: string, blocked: boolean): Promise<void>
	downloadMedia(ref: StoredMessageRef): Promise<Readable>
}
