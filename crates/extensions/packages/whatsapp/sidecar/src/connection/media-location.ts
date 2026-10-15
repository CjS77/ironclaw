// Where a media download may go. A media message's `url` and `directPath`
// come from the customer's client, and Baileys builds the download address
// from them (`downloadMediaMessage` -> `downloadContentFromMessage`): with no
// `directPath` it fetches `url` as given, and otherwise
// `https://<host><directPath>`. A modified client could point either at any
// server, including ones on the operator's private network, so only
// WhatsApp's media hosts are allowed.
import { extractMessageContent, getContentType, type proto } from 'baileys'

/** The host every `directPath` download goes to (Baileys' default media host). */
export const MEDIA_HOST = 'mmg.whatsapp.net'

export class UnsafeMediaLocationError extends Error {
	constructor(readonly reason: string) {
		super(`refused media location: ${reason}`)
		this.name = 'UnsafeMediaLocationError'
	}
}

function isWhatsAppMediaHost(hostname: string): boolean {
	return hostname === 'whatsapp.net' || hostname.endsWith('.whatsapp.net')
}

function checkDirectPath(directPath: unknown): void {
	if (typeof directPath !== 'string' || !directPath.startsWith('/') || directPath.startsWith('//')) {
		throw new UnsafeMediaLocationError('directPath must be an absolute path')
	}
	if (/[\s\\@\u0000-\u001f\u007f]/.test(directPath)) {
		throw new UnsafeMediaLocationError('directPath has characters a media path never has')
	}
	if (new URL(`https://${MEDIA_HOST}${directPath}`).host !== MEDIA_HOST) {
		throw new UnsafeMediaLocationError('directPath changes the host')
	}
}

function checkUrl(url: unknown): void {
	let parsed: URL
	try {
		parsed = new URL(String(url))
	} catch {
		throw new UnsafeMediaLocationError('url is not a URL')
	}
	if (parsed.protocol !== 'https:') throw new UnsafeMediaLocationError('url must be https')
	if (parsed.username || parsed.password || parsed.port) {
		throw new UnsafeMediaLocationError('url must not carry credentials or a port')
	}
	if (!isWhatsAppMediaHost(parsed.hostname)) throw new UnsafeMediaLocationError('url is not a WhatsApp media host')
}

/**
 * Throws `UnsafeMediaLocationError` unless downloading `content` would only
 * contact a WhatsApp media host. It picks the media object and the address
 * field exactly as Baileys does. The caller must also pass `host: MEDIA_HOST`
 * to the download, so a `directPath` download never takes its host from `url`.
 */
export function checkMediaLocation(content: proto.IMessage | null | undefined): void {
	const inner = extractMessageContent(content)
	const type = inner ? getContentType(inner) : undefined
	const media: unknown = type ? (inner as Record<string, unknown>)[type] : undefined
	if (!media || typeof media !== 'object') throw new UnsafeMediaLocationError('not a media message')
	const fields = media as Record<string, unknown>
	// Baileys' own test: a thumbnail-only link preview has no `url` key at all.
	// (On a decoded protobuf message `url` is always present on the prototype.)
	if (!('url' in fields) && 'thumbnailDirectPath' in fields) {
		checkDirectPath(fields.thumbnailDirectPath)
		return
	}
	if (!fields.directPath && !fields.url) throw new UnsafeMediaLocationError('no media location')
	// Baileys fetches `url` only when there is no `directPath`, but WhatsApp
	// always sends a WhatsApp `url` alongside one, so a foreign `url` is
	// refused either way.
	if (fields.url) checkUrl(fields.url)
	if (fields.directPath) checkDirectPath(fields.directPath)
}
