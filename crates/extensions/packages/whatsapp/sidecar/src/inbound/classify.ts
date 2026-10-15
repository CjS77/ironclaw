// Decides what an incoming Baileys message is: a customer message to forward,
// owner activity (the owner replied from the phone), or something to drop.
// Pure: no I/O, so the filtering rules are unit-tested directly.
import {
	getContentType,
	isHostedLidUser,
	isHostedPnUser,
	isLidUser,
	isPnUser,
	jidDecode,
	jidNormalizedUser,
	normalizeMessageContent,
	proto,
	toNumber
} from 'baileys'
import type { MessageUpsertType, WAMessage } from 'baileys'
import { clip, LIMITS, type ContentKind } from '../webhook/events.js'

export type DropReason =
	| 'not_notify'
	| 'no_key'
	| 'group'
	| 'status_broadcast'
	| 'broadcast_list'
	| 'newsletter'
	| 'unsupported_chat'
	| 'self_chat'
	| 'stub'
	| 'protocol'
	| 'reaction'
	| 'empty'

export interface InboundMedia {
	kind: 'image' | 'video' | 'audio' | 'document' | 'sticker'
	mime_type: string
	size_bytes: number | null
	file_name?: string
}

export interface CustomerMessage {
	kind: 'message'
	/** JIDs for this user, the address WhatsApp used first. */
	jids: string[]
	/** Preferred stable contact id: the LID when known, else the phone-number JID. */
	contactId: string
	phoneNumber?: string
	messageId: string
	pushName?: string
	contentKind: ContentKind
	text?: string
	textTruncated: boolean
	media: InboundMedia[]
	quotedMessageId?: string
	timestamp: Date
	/** The normalised message content (for quoting and media download). */
	content: proto.IMessage
	key: { remoteJid: string; id: string; fromMe: boolean }
}

export interface OwnerMessage {
	kind: 'owner_activity'
	jids: string[]
	messageId: string
	timestamp: Date
}

export type Classified = CustomerMessage | OwnerMessage | { kind: 'drop'; reason: DropReason }

function isOneToOne(jid: string): boolean {
	return Boolean(isPnUser(jid) || isLidUser(jid) || isHostedPnUser(jid) || isHostedLidUser(jid))
}

function dropReasonForChat(jid: string): DropReason | undefined {
	if (jid.endsWith('@g.us')) return 'group'
	if (jid === 'status@broadcast') return 'status_broadcast'
	if (jid.endsWith('@broadcast')) return 'broadcast_list'
	if (jid.endsWith('@newsletter')) return 'newsletter'
	if (!isOneToOne(jid)) return 'unsupported_chat'
	return undefined
}

function phoneNumberOf(jid: string | undefined): string | undefined {
	if (!jid || !(isPnUser(jid) || isHostedPnUser(jid))) return undefined
	const user = jidDecode(jid)?.user
	return user && /^\d{5,20}$/.test(user) ? user : undefined
}

function textOf(content: proto.IMessage): string | undefined {
	return (
		content.conversation ||
		content.extendedTextMessage?.text ||
		content.imageMessage?.caption ||
		content.videoMessage?.caption ||
		content.documentMessage?.caption ||
		undefined
	)
}

function contextInfoOf(content: proto.IMessage): proto.IContextInfo | undefined {
	const type = getContentType(content)
	if (!type || type === 'conversation') return undefined
	const inner = content[type] as { contextInfo?: proto.IContextInfo | null } | null | undefined
	return inner?.contextInfo ?? undefined
}

function mediaOf(content: proto.IMessage): InboundMedia | undefined {
	const size = (n: unknown) => {
		const v = toNumber(n as number)
		return Number.isSafeInteger(v) && v >= 0 ? v : null
	}
	const mime = (m: string | null | undefined, fallback: string) => clip(m || fallback, LIMITS.mimeTypeChars)
	if (content.imageMessage) {
		return { kind: 'image', mime_type: mime(content.imageMessage.mimetype, 'image/jpeg'), size_bytes: size(content.imageMessage.fileLength) }
	}
	if (content.videoMessage) {
		return { kind: 'video', mime_type: mime(content.videoMessage.mimetype, 'video/mp4'), size_bytes: size(content.videoMessage.fileLength) }
	}
	if (content.audioMessage) {
		return { kind: 'audio', mime_type: mime(content.audioMessage.mimetype, 'audio/ogg'), size_bytes: size(content.audioMessage.fileLength) }
	}
	if (content.documentMessage) {
		const fileName = content.documentMessage.fileName
		return {
			kind: 'document',
			mime_type: mime(content.documentMessage.mimetype, 'application/octet-stream'),
			size_bytes: size(content.documentMessage.fileLength),
			...(fileName ? { file_name: clip(fileName, LIMITS.fileNameChars) } : {})
		}
	}
	if (content.stickerMessage) {
		return { kind: 'sticker', mime_type: mime(content.stickerMessage.mimetype, 'image/webp'), size_bytes: size(content.stickerMessage.fileLength) }
	}
	return undefined
}

function contentKindOf(content: proto.IMessage, media: InboundMedia | undefined): ContentKind {
	if (media) return media.kind
	if (content.conversation || content.extendedTextMessage) return 'text'
	if (content.locationMessage || content.liveLocationMessage) return 'location'
	if (content.contactMessage || content.contactsArrayMessage) return 'contact'
	return 'other'
}

function timestampOf(msg: WAMessage, fallback: Date): Date {
	const seconds = toNumber(msg.messageTimestamp as number)
	return seconds > 0 ? new Date(seconds * 1000) : fallback
}

/**
 * @param ownJids the linked account's own JIDs (PN and LID), normalised.
 */
export function classify(
	type: MessageUpsertType,
	msg: WAMessage,
	ownJids: ReadonlySet<string>,
	now = new Date()
): Classified {
	if (type !== 'notify') return { kind: 'drop', reason: 'not_notify' }
	const key = msg.key
	if (!key?.remoteJid || !key.id || key.id.length > LIMITS.idChars || key.remoteJid.length > LIMITS.idChars) {
		return { kind: 'drop', reason: 'no_key' }
	}

	const chatReason = dropReasonForChat(key.remoteJid)
	if (chatReason) return { kind: 'drop', reason: chatReason }

	const remote = jidNormalizedUser(key.remoteJid)
	const alt = key.remoteJidAlt && isOneToOne(key.remoteJidAlt) ? jidNormalizedUser(key.remoteJidAlt) : undefined
	const jids = [remote, ...(alt && alt !== remote ? [alt] : [])]
	if (jids.some(j => ownJids.has(j))) return { kind: 'drop', reason: 'self_chat' }

	const content = normalizeMessageContent(msg.message)
	if (!content) return { kind: 'drop', reason: msg.messageStubType ? 'stub' : 'empty' }
	if (content.protocolMessage) return { kind: 'drop', reason: 'protocol' }
	if (content.reactionMessage) return { kind: 'drop', reason: 'reaction' }
	const contentType = getContentType(content)
	if (!contentType || contentType === 'senderKeyDistributionMessage') return { kind: 'drop', reason: 'empty' }

	const timestamp = timestampOf(msg, now)
	if (key.fromMe) {
		return { kind: 'owner_activity', jids, messageId: key.id, timestamp }
	}

	const media = mediaOf(content)
	const rawText = textOf(content)
	const text = rawText === undefined ? undefined : clip(rawText, LIMITS.textChars)
	const quoted = contextInfoOf(content)?.stanzaId ?? undefined
	const lid = jids.find(j => isLidUser(j) || isHostedLidUser(j))
	const pn = jids.find(j => isPnUser(j) || isHostedPnUser(j))
	const phoneNumber = phoneNumberOf(pn)
	const pushName = msg.pushName ? clip(msg.pushName, LIMITS.displayNameChars) : undefined

	return {
		kind: 'message',
		jids,
		contactId: lid ?? remote,
		...(phoneNumber ? { phoneNumber } : {}),
		messageId: key.id,
		...(pushName ? { pushName } : {}),
		contentKind: contentKindOf(content, media),
		...(text !== undefined ? { text } : {}),
		textTruncated: rawText !== undefined && text !== rawText,
		media: media ? [media] : [],
		...(quoted ? { quotedMessageId: clip(quoted, LIMITS.idChars) } : {}),
		timestamp,
		content,
		key: { remoteJid: key.remoteJid, id: key.id, fromMe: false }
	}
}
