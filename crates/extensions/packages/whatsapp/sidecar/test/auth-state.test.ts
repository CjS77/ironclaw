import assert from 'node:assert/strict'
import { test } from 'node:test'
import { makeCacheableSignalKeyStore, proto } from 'baileys'
import { loadEncryptedAuthState } from '../src/store/auth-state.js'
import { EncryptedStore } from '../src/store/encrypted-store.js'
import { baileysLogger } from '../src/log.js'
import { dataKey, memoryLogger, tempDir } from './support/helpers.js'

test('Baileys credentials and signal keys round-trip through the encrypted store', async t => {
	const root = await tempDir(t)
	const key = dataKey()
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(key)

	const first = await loadEncryptedAuthState(store)
	assert.equal(first.existing, false)
	first.state.creds.registered = true
	first.state.creds.me = { id: '447700900000:3@s.whatsapp.net', name: 'Shop' }
	await first.saveCreds()

	const session = new Uint8Array([1, 2, 3, 250])
	await first.state.keys.set({
		session: { '447700900123.0': session },
		'pre-key': { '7': { public: Buffer.alloc(32, 1), private: Buffer.alloc(32, 2) } },
		'app-state-sync-key': { AAAA: { keyData: Buffer.alloc(32, 9), timestamp: 5 } },
		'lid-mapping': { '447700900123': '123456789@lid' }
	})

	const reopened = new EncryptedStore(root)
	await reopened.unlock(Buffer.from(key))
	const second = await loadEncryptedAuthState(reopened)
	assert.equal(second.existing, true)
	assert.equal(second.state.creds.registered, true)
	assert.equal(second.state.creds.me?.name, 'Shop')
	assert.ok(Buffer.isBuffer(second.state.creds.noiseKey.private))
	assert.deepEqual(second.state.creds.noiseKey.private, first.state.creds.noiseKey.private)
	assert.equal(second.state.creds.advSecretKey, first.state.creds.advSecretKey)

	const sessions = await second.state.keys.get('session', ['447700900123.0', 'unknown'])
	assert.deepEqual([...(sessions['447700900123.0'] as Uint8Array)], [...session])
	assert.equal(sessions.unknown, undefined)
	const preKeys = await second.state.keys.get('pre-key', ['7'])
	assert.deepEqual(preKeys['7']?.private, Buffer.alloc(32, 2))
	const appKeys = await second.state.keys.get('app-state-sync-key', ['AAAA'])
	assert.ok(appKeys.AAAA instanceof proto.Message.AppStateSyncKeyData)
	assert.deepEqual(Buffer.from(appKeys.AAAA.keyData!), Buffer.alloc(32, 9))
	assert.equal((await second.state.keys.get('lid-mapping', ['447700900123']))['447700900123'], '123456789@lid')

	// null deletes (Baileys removes used pre-keys this way)
	await second.state.keys.set({ 'pre-key': { '7': null } })
	assert.deepEqual(await second.state.keys.get('pre-key', ['7']), {})
})

test('the key store works behind makeCacheableSignalKeyStore', async t => {
	const root = await tempDir(t)
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(dataKey())
	const auth = await loadEncryptedAuthState(store)
	const cached = makeCacheableSignalKeyStore(auth.state.keys, baileysLogger(memoryLogger().logger))
	await cached.set({ 'sender-key': { 'g@g.us::a::0': Buffer.from('sk') } })
	const fresh = await loadEncryptedAuthState(store)
	assert.deepEqual((await fresh.state.keys.get('sender-key', ['g@g.us::a::0']))['g@g.us::a::0'], Buffer.from('sk'))
})
