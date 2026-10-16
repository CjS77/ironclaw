// In-memory WhatsAppPort for API tests.
import { Readable } from 'node:stream'
import type { proto, WAMessage } from 'baileys'
import type { LinkState } from '../../src/webhook/events.js'
import {
	LinkConflictError,
	NotConnectedError,
	type ConnectionEvents,
	type LinkDisplay,
	type LinkMode,
	type LinkStatus,
	type StoredMessageRef,
	type WhatsAppPort
} from '../../src/whatsapp/port.js'

export type AckMode = 'ack' | 'error' | 'none'

export class FakeWhatsApp implements WhatsAppPort {
	state: LinkState = 'linked'
	connected = true
	ackMode: AckMode = 'ack'
	sent: Array<{ jid: string; text: string; messageId: string; quoted?: StoredMessageRef }> = []
	blocks: Array<{ jid: string; blocked: boolean }> = []
	mediaBytes = Buffer.from('fake media bytes')
	loggedOut = false
	/** Makes the next sendText throw: the socket dropped, or some other failure. */
	failNextSend: 'not_connected' | 'error' | undefined
	private counter = 0

	constructor(readonly events: ConnectionEvents) {}

	status(): LinkStatus {
		return { state: this.state, connected: this.connected, ...(this.state === 'linked' ? { account_label: 'Test Shop' } : {}) }
	}

	ownJids(): ReadonlySet<string> {
		return new Set(['447700900000@s.whatsapp.net', '111111111111111@lid'])
	}

	async start(): Promise<void> {}
	async stop(): Promise<void> {
		this.connected = false
	}

	async beginLink(mode: LinkMode, phoneNumber?: string): Promise<LinkDisplay> {
		if (this.state === 'linked') throw new LinkConflictError('linked')
		this.state = 'linking'
		return mode === 'qr'
			? { display_kind: 'qr_code', payload: 'qr-payload', expires_in_secs: 60 }
			: { display_kind: 'pairing_code', payload: `CODE-${(phoneNumber ?? '').slice(-4)}`, expires_in_secs: 180 }
	}

	async cancelLink(): Promise<void> {
		if (this.state === 'linking') this.state = 'unlinked'
	}

	async logout(): Promise<void> {
		this.loggedOut = true
		this.state = 'unlinked'
		this.connected = false
	}

	newMessageId(): string {
		return `3EB0FAKE${(++this.counter).toString().padStart(8, '0')}`
	}

	async sendText(jid: string, text: string, options: { messageId: string; quoted?: StoredMessageRef }): Promise<proto.IMessage> {
		if (!this.connected) throw new NotConnectedError()
		const failure = this.failNextSend
		this.failNextSend = undefined
		if (failure === 'not_connected') throw new NotConnectedError()
		if (failure === 'error') throw new Error('socket write failed')
		this.sent.push({ jid, text, messageId: options.messageId, ...(options.quoted ? { quoted: options.quoted } : {}) })
		if (this.ackMode !== 'none') {
			setTimeout(() => this.events.onAck(options.messageId, 1_760_000_000, this.ackMode === 'error' ? '479' : undefined), 1)
		}
		return { conversation: text }
	}

	async setBlocked(jid: string, blocked: boolean): Promise<void> {
		if (!this.connected) throw new NotConnectedError()
		this.blocks.push({ jid, blocked })
	}

	async downloadMedia(_ref: StoredMessageRef): Promise<Readable> {
		return Readable.from([this.mediaBytes])
	}

	/** Simulates an inbound `messages.upsert` notify batch. */
	deliver(messages: WAMessage[], type: 'notify' | 'append' = 'notify'): void {
		this.events.onUpsert(type, messages)
	}
}

let seq = 0
export function textMessage(from: string, text: string, extra: Partial<WAMessage> = {}): WAMessage {
	seq++
	return {
		key: { remoteJid: from, fromMe: false, id: `MSG${seq.toString().padStart(6, '0')}` },
		message: { conversation: text },
		messageTimestamp: 1_760_000_000 + seq,
		pushName: 'Customer A',
		...extra
	} as WAMessage
}
