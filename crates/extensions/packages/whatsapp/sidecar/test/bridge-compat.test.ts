// The locally built whatsapp-rust-bridge (0.5.5) must behave as Baileys
// expects for the four functions Baileys imports from it. References are
// computed with node:crypto, following the pure-JS implementations Baileys
// used before it adopted the bridge.
import assert from 'node:assert/strict'
import { createHash, hkdfSync, randomBytes } from 'node:crypto'
import { test } from 'node:test'

interface Bridge {
	md5(data: Uint8Array): Uint8Array
	hkdf(key: Uint8Array, length: number, info: { salt?: Uint8Array; info?: string }): Uint8Array
	expandAppStateKeys(keyData: Uint8Array): {
		indexKey: Uint8Array
		valueEncryptionKey: Uint8Array
		valueMacKey: Uint8Array
		snapshotMacKey: Uint8Array
		patchMacKey: Uint8Array
	}
	LTHashAntiTampering: new () => { subtractThenAdd(base: Uint8Array, subtract: Uint8Array[], add: Uint8Array[]): Uint8Array }
	__wasmSimdActive: boolean
}

// Resolve the bridge the way Baileys does: from Baileys' own dependency tree.
const bridgeUrl = new URL('../node_modules/whatsapp-rust-bridge/dist/index.js', import.meta.resolve('baileys'))
const bridge = (await import(bridgeUrl.href)) as Bridge

const hex = (b: Uint8Array) => Buffer.from(b).toString('hex')
const refHkdf = (key: Uint8Array, length: number, info: string, salt?: Uint8Array) =>
	Buffer.from(hkdfSync('sha256', key, salt ?? Buffer.alloc(0), Buffer.from(info), length))

test('md5 matches node:crypto', () => {
	for (const data of [Buffer.alloc(0), Buffer.from('MD5 test data'), randomBytes(4097)]) {
		assert.equal(hex(bridge.md5(data)), createHash('md5').update(data).digest('hex'))
	}
})

test('hkdf matches RFC 5869 HKDF-SHA256 (with and without salt)', () => {
	const key = randomBytes(32)
	assert.equal(hex(bridge.hkdf(key, 112, { info: 'WhatsApp Image Keys' })), hex(refHkdf(key, 112, 'WhatsApp Image Keys')))
	const salt = randomBytes(32)
	assert.equal(hex(bridge.hkdf(key, 64, { salt, info: '' })), hex(refHkdf(key, 64, '', salt)))
	assert.equal(hex(bridge.hkdf(key, 32, { info: 'adv_secret' })), hex(refHkdf(key, 32, 'adv_secret')))
})

test('expandAppStateKeys splits HKDF("WhatsApp Mutation Keys", 160) into five keys', () => {
	const keyData = randomBytes(32)
	const expanded = refHkdf(keyData, 160, 'WhatsApp Mutation Keys')
	const keys = bridge.expandAppStateKeys(keyData)
	assert.equal(hex(keys.indexKey), hex(expanded.subarray(0, 32)))
	assert.equal(hex(keys.valueEncryptionKey), hex(expanded.subarray(32, 64)))
	assert.equal(hex(keys.valueMacKey), hex(expanded.subarray(64, 96)))
	assert.equal(hex(keys.snapshotMacKey), hex(expanded.subarray(96, 128)))
	assert.equal(hex(keys.patchMacKey), hex(expanded.subarray(128, 160)))
})

test('LTHashAntiTampering.subtractThenAdd matches the reference LT hash', () => {
	const pointwise = (a: Buffer, b: Buffer, op: (x: number, y: number) => number) => {
		const out = Buffer.alloc(a.length)
		for (let i = 0; i < a.length; i += 2) out.writeUInt16LE(op(a.readUInt16LE(i), b.readUInt16LE(i)) & 0xffff, i)
		return out
	}
	const ref = (base: Buffer, subtract: Buffer[], add: Buffer[]) => {
		let h = base
		for (const s of subtract) h = pointwise(h, refHkdf(s, 128, 'WhatsApp Patch Integrity'), (x, y) => x - y)
		for (const a of add) h = pointwise(h, refHkdf(a, 128, 'WhatsApp Patch Integrity'), (x, y) => x + y)
		return h
	}
	const lt = new bridge.LTHashAntiTampering()
	const base = randomBytes(128)
	const subtract = [randomBytes(32), randomBytes(32)]
	const add = [randomBytes(32), randomBytes(32), randomBytes(32)]
	assert.equal(hex(lt.subtractThenAdd(base, subtract, add)), hex(ref(base, subtract, add)))
	assert.equal(hex(lt.subtractThenAdd(Buffer.alloc(128), [], [])), hex(Buffer.alloc(128)))
})
