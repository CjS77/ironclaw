// Encrypted record store on the local filesystem.
//
// Layout under <root>:
//   keycheck.json                     key-check record (no secrets)
//   <type-dir>/<record-name>.rec      one encrypted record
//
// <type-dir> and <record-name> are HMAC-SHA256 values under a key derived from
// the data key, so neither record types nor record ids (which include WhatsApp
// JIDs, i.e. phone numbers) appear in clear on the volume.
//
// A record file is:
//   "WSR1" | u32be header length | seal(id, AAD = "<type>|")
//          | seal(value, AAD = "<type>|<id>")
// `seal` is AES-256-GCM with a fresh random 96-bit nonce (nonce || ct || tag).
// The header lets `list()` learn a record's id; after decrypting it, the file
// name is recomputed from (type, id) and must match, so a record cannot be
// moved to another id's slot. The value is bound to "<type>|<id>" by its AAD.
//
// Writes go to a temporary file in the same directory, are fsynced, renamed
// over the target and the directory is fsynced, so a crash leaves either the
// old or the new record, never a torn one.
import { randomBytes } from 'node:crypto'
import { mkdir, open as openFile, readdir, readFile, rename, rm, unlink } from 'node:fs/promises'
import { join } from 'node:path'
import {
	AES_KEY_BYTES,
	constantTimeEqual,
	DecryptError,
	hkdfSha256,
	hmacSha256,
	open,
	seal,
	zeroize
} from '../crypto.js'

const MAGIC = Buffer.from('WSR1', 'ascii')
const KEYCHECK_FILE = 'keycheck.json'
const KDF_SALT = 'ironclaw-whatsapp-sidecar/v1'
const KEYCHECK_MESSAGE = 'ironclaw-whatsapp-sidecar key check v1'
const MAX_RECORD_BYTES = 8 * 1024 * 1024

export class StoreLockedError extends Error {
	constructor() {
		super('store is locked')
		this.name = 'StoreLockedError'
	}
}

export class WrongKeyError extends Error {
	constructor() {
		super('data key does not match the key-check record')
		this.name = 'WrongKeyError'
	}
}

export class CorruptRecordError extends Error {
	constructor(message: string) {
		super(message)
		this.name = 'CorruptRecordError'
	}
}

export class KeyCheckMissingError extends Error {
	constructor() {
		super('store holds records but no key-check record')
		this.name = 'KeyCheckMissingError'
	}
}

interface Keys {
	enc: Buffer
	name: Buffer
	check: Buffer
}

interface KeyCheckRecord {
	v: 1
	verifier: string
	created_at: string
}

export interface UnlockResult {
	/** True when this unlock created the key-check record (empty store). */
	initialized: boolean
}

/** Serialises async operations per key. */
class KeyedMutex {
	private readonly tails = new Map<string, Promise<unknown>>()

	async run<T>(key: string, fn: () => Promise<T>): Promise<T> {
		const prev = this.tails.get(key) ?? Promise.resolve()
		let release!: () => void
		const gate = new Promise<void>(resolve => (release = resolve))
		const tail = prev.then(() => gate)
		this.tails.set(key, tail)
		try {
			await prev
			return await fn()
		} finally {
			release()
			if (this.tails.get(key) === tail) this.tails.delete(key)
		}
	}
}

function deriveKeys(dataKey: Uint8Array): Keys {
	return {
		enc: hkdfSha256(dataKey, KDF_SALT, 'record-encryption', AES_KEY_BYTES),
		name: hkdfSha256(dataKey, KDF_SALT, 'record-names', 32),
		check: hkdfSha256(dataKey, KDF_SALT, 'key-check', 32)
	}
}

async function fsyncDir(dir: string): Promise<void> {
	const handle = await openFile(dir, 'r')
	try {
		await handle.sync()
	} finally {
		await handle.close()
	}
}

async function atomicWrite(dir: string, file: string, data: Uint8Array): Promise<void> {
	const tmp = join(dir, `.${randomBytes(8).toString('hex')}.tmp`)
	const handle = await openFile(tmp, 'wx', 0o600)
	try {
		await handle.writeFile(data)
		await handle.sync()
	} finally {
		await handle.close()
	}
	try {
		await rename(tmp, join(dir, file))
	} catch (error) {
		await unlink(tmp).catch(() => undefined)
		throw error
	}
	await fsyncDir(dir)
}

function isNotFound(error: unknown): boolean {
	return (error as NodeJS.ErrnoException)?.code === 'ENOENT'
}

export class EncryptedStore {
	private keys: Keys | undefined
	private readonly mutex = new KeyedMutex()

	constructor(readonly root: string) {}

	get isUnlocked(): boolean {
		return this.keys !== undefined
	}

	/** Creates the store directory if needed. Does not need the key. */
	async init(): Promise<void> {
		await mkdir(this.root, { recursive: true, mode: 0o700 })
	}

	async hasKeyCheck(): Promise<boolean> {
		try {
			await readFile(join(this.root, KEYCHECK_FILE))
			return true
		} catch (error) {
			if (isNotFound(error)) return false
			throw error
		}
	}

	/**
	 * Unlocks the store with a 32-byte data key. On an empty store the first
	 * unlock writes the key-check record. Idempotent: unlocking an unlocked
	 * store with the same key succeeds; with a different key it fails.
	 */
	async unlock(dataKey: Uint8Array): Promise<UnlockResult> {
		if (dataKey.length !== AES_KEY_BYTES) throw new WrongKeyError()
		const candidate = deriveKeys(dataKey)
		const verifier = hmacSha256(candidate.check, KEYCHECK_MESSAGE)
		try {
			return await this.mutex.run(KEYCHECK_FILE, async () => {
				let stored: KeyCheckRecord | undefined
				try {
					stored = JSON.parse(await readFile(join(this.root, KEYCHECK_FILE), 'utf8')) as KeyCheckRecord
				} catch (error) {
					if (!isNotFound(error)) throw new CorruptRecordError('key-check record is unreadable')
				}
				if (stored) {
					if (stored.v !== 1 || typeof stored.verifier !== 'string') {
						throw new CorruptRecordError('key-check record has an unknown format')
					}
					if (!constantTimeEqual(Buffer.from(stored.verifier, 'hex'), verifier)) throw new WrongKeyError()
					this.replaceKeys(candidate)
					return { initialized: false }
				}
				// No key-check record. Refuse to adopt a key over existing records:
				// they were written under some key, and silently choosing a new
				// one would make them unreadable.
				const entries = (await readdir(this.root)).filter(e => !e.startsWith('.'))
				if (entries.length > 0) throw new KeyCheckMissingError()
				const record: KeyCheckRecord = {
					v: 1,
					verifier: verifier.toString('hex'),
					created_at: new Date().toISOString()
				}
				await atomicWrite(this.root, KEYCHECK_FILE, Buffer.from(JSON.stringify(record) + '\n'))
				this.replaceKeys(candidate)
				return { initialized: true }
			})
		} catch (error) {
			if (this.keys !== candidate) {
				zeroize(candidate.enc)
				zeroize(candidate.name)
				zeroize(candidate.check)
			}
			throw error
		}
	}

	/** Returns true when `dataKey` is the key the store is unlocked with. */
	matchesKey(dataKey: Uint8Array): boolean {
		if (!this.keys || dataKey.length !== AES_KEY_BYTES) return false
		const candidate = deriveKeys(dataKey)
		const same = constantTimeEqual(candidate.check, this.keys.check)
		zeroize(candidate.enc)
		zeroize(candidate.name)
		zeroize(candidate.check)
		return same
	}

	/** Forgets the keys (overwriting them with zeros). */
	lock(): void {
		this.replaceKeys(undefined)
	}

	private replaceKeys(next: Keys | undefined): void {
		const prev = this.keys
		this.keys = next
		if (prev && prev !== next) {
			zeroize(prev.enc)
			zeroize(prev.name)
			zeroize(prev.check)
		}
	}

	private requireKeys(): Keys {
		if (!this.keys) throw new StoreLockedError()
		return this.keys
	}

	private typeDir(keys: Keys, type: string): string {
		return hmacSha256(keys.name, `type\u0000${type}`).subarray(0, 16).toString('hex')
	}

	private recordName(keys: Keys, type: string, id: string): string {
		return hmacSha256(keys.name, `record\u0000${type}\u0000${id}`).toString('hex') + '.rec'
	}

	/** Stores raw bytes under (type, id), replacing any previous value. */
	async putBytes(type: string, id: string, value: Uint8Array): Promise<void> {
		const keys = this.requireKeys()
		const dir = join(this.root, this.typeDir(keys, type))
		const file = this.recordName(keys, type, id)
		const header = seal(keys.enc, Buffer.from(id, 'utf8'), `${type}|`)
		const body = seal(keys.enc, value, `${type}|${id}`)
		const len = Buffer.alloc(4)
		len.writeUInt32BE(header.length)
		const data = Buffer.concat([MAGIC, len, header, body])
		await this.mutex.run(join(dir, file), async () => {
			await mkdir(dir, { recursive: true, mode: 0o700 })
			await atomicWrite(dir, file, data)
		})
	}

	/** Reads raw bytes for (type, id); undefined when absent. Throws on tampering. */
	async getBytes(type: string, id: string): Promise<Buffer | undefined> {
		const keys = this.requireKeys()
		const dir = join(this.root, this.typeDir(keys, type))
		const file = this.recordName(keys, type, id)
		let data: Buffer
		try {
			data = await this.mutex.run(join(dir, file), () => readFile(join(dir, file)))
		} catch (error) {
			if (isNotFound(error)) return undefined
			throw error
		}
		const { id: storedId, body } = this.decodeFile(keys, type, data)
		if (storedId !== id) throw new CorruptRecordError('record header does not match its name')
		return open(keys.enc, body, `${type}|${id}`)
	}

	async delete(type: string, id: string): Promise<void> {
		const keys = this.requireKeys()
		const dir = join(this.root, this.typeDir(keys, type))
		const file = this.recordName(keys, type, id)
		await this.mutex.run(join(dir, file), async () => {
			try {
				await unlink(join(dir, file))
				await fsyncDir(dir)
			} catch (error) {
				if (!isNotFound(error)) throw error
			}
		})
	}

	/** Lists every record of a type as (id, bytes). Corrupt records are reported, not returned. */
	async list(type: string, onCorrupt?: (error: Error) => void): Promise<Array<{ id: string; value: Buffer }>> {
		const keys = this.requireKeys()
		const dir = join(this.root, this.typeDir(keys, type))
		let files: string[]
		try {
			files = await readdir(dir)
		} catch (error) {
			if (isNotFound(error)) return []
			throw error
		}
		const out: Array<{ id: string; value: Buffer }> = []
		for (const file of files) {
			if (!file.endsWith('.rec')) continue
			try {
				const data = await this.mutex.run(join(dir, file), () => readFile(join(dir, file)))
				const { id, body } = this.decodeFile(keys, type, data)
				if (this.recordName(keys, type, id) !== file) {
					throw new CorruptRecordError('record file name does not match its header')
				}
				out.push({ id, value: open(keys.enc, body, `${type}|${id}`) })
			} catch (error) {
				if (isNotFound(error)) continue
				if (error instanceof DecryptError || error instanceof CorruptRecordError) {
					onCorrupt?.(error)
					continue
				}
				throw error
			}
		}
		return out
	}

	/** Removes every record of the given types. */
	async wipeTypes(types: readonly string[]): Promise<void> {
		const keys = this.requireKeys()
		for (const type of types) {
			await rm(join(this.root, this.typeDir(keys, type)), { recursive: true, force: true })
		}
		await fsyncDir(this.root)
	}

	/** Removes every record except those of `keep` types, and keeps the key-check record. */
	async wipeAllExcept(keep: readonly string[]): Promise<void> {
		const keys = this.requireKeys()
		const kept = new Set(keep.map(t => this.typeDir(keys, t)))
		for (const entry of await readdir(this.root)) {
			if (entry === KEYCHECK_FILE || kept.has(entry)) continue
			await rm(join(this.root, entry), { recursive: true, force: true })
		}
		await fsyncDir(this.root)
	}

	private decodeFile(keys: Keys, type: string, data: Buffer): { id: string; body: Buffer } {
		if (data.length > MAX_RECORD_BYTES) throw new CorruptRecordError('record too large')
		if (data.length < 8 || !data.subarray(0, 4).equals(MAGIC)) {
			throw new CorruptRecordError('record has an unknown format')
		}
		const headerLen = data.readUInt32BE(4)
		if (8 + headerLen > data.length) throw new CorruptRecordError('record header is truncated')
		const header = data.subarray(8, 8 + headerLen)
		const body = data.subarray(8 + headerLen)
		const id = open(keys.enc, header, `${type}|`).toString('utf8')
		return { id, body }
	}
}
