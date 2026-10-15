// Small cryptographic helpers built on node:crypto.
import { createCipheriv, createDecipheriv, createHash, createHmac, hkdfSync, randomBytes, timingSafeEqual } from 'node:crypto'

export const AES_KEY_BYTES = 32
export const GCM_NONCE_BYTES = 12
export const GCM_TAG_BYTES = 16

/** Thrown when a ciphertext does not authenticate (wrong key, tampering, wrong AAD). */
export class DecryptError extends Error {
	constructor(message = 'ciphertext failed authentication') {
		super(message)
		this.name = 'DecryptError'
	}
}

/** Compares two secrets in constant time, whatever their lengths. */
export function constantTimeEqual(a: Uint8Array | string, b: Uint8Array | string): boolean {
	// Hashing first gives equal-length inputs to timingSafeEqual, so the
	// comparison does not leak the length of either value through an early exit.
	const ha = createHash('sha256').update(a).digest()
	const hb = createHash('sha256').update(b).digest()
	return timingSafeEqual(ha, hb)
}

export function hmacSha256(key: Uint8Array | string, data: Uint8Array | string): Buffer {
	return createHmac('sha256', key).update(data).digest()
}

export function hkdfSha256(ikm: Uint8Array, salt: string, info: string, length: number): Buffer {
	return Buffer.from(hkdfSync('sha256', ikm, Buffer.from(salt, 'utf8'), Buffer.from(info, 'utf8'), length))
}

/** AES-256-GCM with a fresh random 96-bit nonce. Output: nonce || ciphertext || tag. */
export function seal(key: Uint8Array, plaintext: Uint8Array, aad: string): Buffer {
	const nonce = randomBytes(GCM_NONCE_BYTES)
	const cipher = createCipheriv('aes-256-gcm', key, nonce, { authTagLength: GCM_TAG_BYTES })
	cipher.setAAD(Buffer.from(aad, 'utf8'))
	const ct = Buffer.concat([cipher.update(plaintext), cipher.final()])
	return Buffer.concat([nonce, ct, cipher.getAuthTag()])
}

/** Inverse of {@link seal}. Throws {@link DecryptError} if authentication fails. */
export function open(key: Uint8Array, sealed: Uint8Array, aad: string): Buffer {
	if (sealed.length < GCM_NONCE_BYTES + GCM_TAG_BYTES) {
		throw new DecryptError('ciphertext too short')
	}
	const buf = Buffer.from(sealed.buffer, sealed.byteOffset, sealed.byteLength)
	const nonce = buf.subarray(0, GCM_NONCE_BYTES)
	const tag = buf.subarray(buf.length - GCM_TAG_BYTES)
	const ct = buf.subarray(GCM_NONCE_BYTES, buf.length - GCM_TAG_BYTES)
	try {
		const decipher = createDecipheriv('aes-256-gcm', key, nonce, { authTagLength: GCM_TAG_BYTES })
		decipher.setAAD(Buffer.from(aad, 'utf8'))
		decipher.setAuthTag(tag)
		return Buffer.concat([decipher.update(ct), decipher.final()])
	} catch {
		throw new DecryptError()
	}
}

/** Overwrites a key buffer with zeros. */
export function zeroize(buf: Uint8Array | undefined): void {
	buf?.fill(0)
}

/** Sortable, unique id: 12 hex digits of milliseconds + 20 random hex digits. */
export function sortableId(prefix: string, now = Date.now()): string {
	return `${prefix}${now.toString(16).padStart(12, '0')}${randomBytes(10).toString('hex')}`
}
