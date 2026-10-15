// Webhook request signing (see API.md, "Signing").
//
//   X-Sidecar-Timestamp: <unix seconds>
//   X-Sidecar-Signature: v1=<lowercase hex HMAC-SHA256(key, "v1:" + timestamp + ":" + body)>
//
// The HMAC key is the UTF-8 bytes of SIDECAR_WEBHOOK_HMAC_KEY, used as-is.
import { hmacSha256 } from '../crypto.js'

export const TIMESTAMP_HEADER = 'X-Sidecar-Timestamp'
export const SIGNATURE_HEADER = 'X-Sidecar-Signature'
export const SIGNATURE_PREFIX = 'v1='

export function signedPayload(timestamp: string, body: Uint8Array | string): Buffer {
	return Buffer.concat([Buffer.from(`v1:${timestamp}:`, 'utf8'), Buffer.from(body)])
}

export function sign(key: string, timestamp: string, body: Uint8Array | string): string {
	return SIGNATURE_PREFIX + hmacSha256(Buffer.from(key, 'utf8'), signedPayload(timestamp, body)).toString('hex')
}

export function signatureHeaders(key: string, body: Uint8Array | string, nowMs = Date.now()): Record<string, string> {
	const timestamp = Math.floor(nowMs / 1000).toString()
	return {
		[TIMESTAMP_HEADER]: timestamp,
		[SIGNATURE_HEADER]: sign(key, timestamp, body)
	}
}
