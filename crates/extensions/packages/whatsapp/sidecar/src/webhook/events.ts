// Sidecar -> IronClaw event envelopes (see API.md, "Webhook events").
import { sortableId } from '../crypto.js'

export const LIMITS = {
	idChars: 128,
	textChars: 65_536,
	displayNameChars: 256,
	mimeTypeChars: 127,
	fileNameChars: 255,
	mediaItems: 10,
	labelChars: 256
} as const

export type LinkState = 'unlinked' | 'linking' | 'linked' | 'logged_out'

export interface MediaDescriptor {
	media_id: string
	mime_type: string
	size_bytes: number | null
	file_name?: string
}

export type ContentKind =
	| 'text'
	| 'image'
	| 'video'
	| 'audio'
	| 'document'
	| 'sticker'
	| 'location'
	| 'contact'
	| 'other'

export interface MessageReceived {
	message_id: string
	chat_id: string
	contact_id: string
	contact_phone_number?: string
	contact_display_name?: string
	content_kind: ContentKind
	text?: string
	text_truncated?: boolean
	media: MediaDescriptor[]
	quoted_message_id?: string
	blocked: boolean
	received_at: string
}

export interface OwnerActivity {
	chat_id: string
	message_id: string
	sent_at: string
}

export interface LinkStateChanged {
	state: LinkState
	account_label?: string
}

export type EventBody =
	| { type: 'message.received'; data: MessageReceived }
	| { type: 'owner.activity'; data: OwnerActivity }
	| { type: 'link.state_changed'; data: LinkStateChanged }
	| { type: 'sidecar.locked'; data: Record<string, never> }

export interface Envelope {
	event_id: string
	type: EventBody['type']
	occurred_at: string
	[field: string]: unknown
}

/** Builds an envelope: {event_id, type, occurred_at, ...data}. */
export function envelope(body: EventBody, now = new Date()): Envelope {
	return {
		event_id: sortableId('evt_', now.getTime()),
		type: body.type,
		occurred_at: now.toISOString(),
		...body.data
	}
}

/** Truncates to at most `max` UTF-16 code units without splitting a surrogate pair. */
export function clip(value: string, max: number): string {
	if (value.length <= max) return value
	let end = max
	const code = value.charCodeAt(end - 1)
	if (code >= 0xd800 && code <= 0xdbff) end--
	return value.slice(0, end)
}
