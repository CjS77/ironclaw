import assert from 'node:assert/strict'
import { readdir, readFile, rename, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { test } from 'node:test'
import { DecryptError } from '../src/crypto.js'
import {
	CorruptRecordError,
	EncryptedStore,
	KeyCheckMissingError,
	StoreLockedError,
	WrongKeyError
} from '../src/store/encrypted-store.js'
import { dataKey, tempDir } from './support/helpers.js'

async function recordFiles(root: string): Promise<string[]> {
	const out: string[] = []
	for (const entry of await readdir(root, { withFileTypes: true })) {
		if (entry.isDirectory()) {
			for (const f of await readdir(join(root, entry.name))) out.push(join(root, entry.name, f))
		}
	}
	return out
}

test('records round-trip and survive a restart', async t => {
	const root = await tempDir(t)
	const key = dataKey()
	const store = new EncryptedStore(root)
	await store.init()
	assert.deepEqual(await store.unlock(key), { initialized: true })
	await store.putBytes('chat', '447700900123@s.whatsapp.net', Buffer.from('hello'))
	assert.equal((await store.getBytes('chat', '447700900123@s.whatsapp.net'))?.toString(), 'hello')
	assert.equal(await store.getBytes('chat', 'missing'), undefined)

	const reopened = new EncryptedStore(root)
	assert.deepEqual(await reopened.unlock(Buffer.from(key)), { initialized: false })
	assert.equal((await reopened.getBytes('chat', '447700900123@s.whatsapp.net'))?.toString(), 'hello')
	const listed = await reopened.list('chat')
	assert.deepEqual(listed.map(r => [r.id, r.value.toString()]), [['447700900123@s.whatsapp.net', 'hello']])

	await reopened.delete('chat', '447700900123@s.whatsapp.net')
	assert.equal(await reopened.getBytes('chat', '447700900123@s.whatsapp.net'), undefined)
})

test('file names and contents do not reveal record ids or values', async t => {
	const root = await tempDir(t)
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(dataKey())
	const id = '447700900123@s.whatsapp.net'
	await store.putBytes('chat', id, Buffer.from('secret message text'))
	const files = await recordFiles(root)
	assert.equal(files.length, 1)
	assert.ok(!files[0]!.includes('447700900123'))
	assert.ok(!files[0]!.includes('chat'))
	const bytes = await readFile(files[0]!)
	assert.ok(!bytes.includes(Buffer.from('447700900123')))
	assert.ok(!bytes.includes(Buffer.from('secret message')))
	assert.ok(!(await readFile(join(root, 'keycheck.json'), 'utf8')).includes('secret'))
})

test('a wrong key is rejected and leaves the store locked', async t => {
	const root = await tempDir(t)
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(dataKey())
	await store.putBytes('chat', 'a', Buffer.from('x'))

	const other = new EncryptedStore(root)
	await assert.rejects(other.unlock(dataKey()), WrongKeyError)
	assert.equal(other.isUnlocked, false)
	await assert.rejects(other.getBytes('chat', 'a'), StoreLockedError)
	await assert.rejects(other.unlock(Buffer.alloc(16)), WrongKeyError)
})

test('matchesKey compares the unlocked key', async t => {
	const root = await tempDir(t)
	const key = dataKey()
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(key)
	assert.equal(store.matchesKey(Buffer.from(key)), true)
	assert.equal(store.matchesKey(dataKey()), false)
	store.lock()
	assert.equal(store.isUnlocked, false)
	assert.equal(store.matchesKey(key), false)
})

test('tampered ciphertext is rejected', async t => {
	const root = await tempDir(t)
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(dataKey())
	await store.putBytes('creds', 'creds', Buffer.from('{"k":1}'))
	const [file] = await recordFiles(root)
	const bytes = await readFile(file!)
	bytes[bytes.length - 20]! ^= 0x01
	await writeFile(file!, bytes)
	await assert.rejects(store.getBytes('creds', 'creds'), DecryptError)
	const corrupt: Error[] = []
	assert.deepEqual(await store.list('creds', e => corrupt.push(e)), [])
	assert.equal(corrupt.length, 1)
})

test('a record moved to another id is rejected (AAD binds type and id)', async t => {
	const root = await tempDir(t)
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(dataKey())
	await store.putBytes('chat', 'alice', Buffer.from('A'))
	await store.putBytes('chat', 'bob', Buffer.from('B'))
	const files = await recordFiles(root)
	assert.equal(files.length, 2)
	// Swap the two files.
	const [f1, f2] = files as [string, string]
	await rename(f1, f1 + '.x')
	await rename(f2, f1)
	await rename(f1 + '.x', f2)
	await assert.rejects(store.getBytes('chat', 'alice'), CorruptRecordError)
	await assert.rejects(store.getBytes('chat', 'bob'), CorruptRecordError)
	const corrupt: Error[] = []
	assert.deepEqual(await store.list('chat', e => corrupt.push(e)), [])
	assert.equal(corrupt.length, 2)
})

test('a record copied under another type does not decrypt', async t => {
	const root = await tempDir(t)
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(dataKey())
	await store.putBytes('blocked', 'y', Buffer.from('1'))
	const [blockedFile] = await recordFiles(root)
	await store.putBytes('chat', 'y', Buffer.from('2'))
	const chatFile = (await recordFiles(root)).find(f => f !== blockedFile)!
	await rename(blockedFile!, chatFile)
	await assert.rejects(store.getBytes('chat', 'y'), DecryptError)
})

test('unlocking refuses to adopt a key over records without a key-check record', async t => {
	const root = await tempDir(t)
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(dataKey())
	await store.putBytes('chat', 'a', Buffer.from('x'))
	await rename(join(root, 'keycheck.json'), join(root, '.keycheck.bak'))
	await assert.rejects(new EncryptedStore(root).unlock(dataKey()), KeyCheckMissingError)
})

test('wipeAllExcept keeps the key-check record and the kept types', async t => {
	const root = await tempDir(t)
	const key = dataKey()
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(key)
	await store.putBytes('spool', 'e1', Buffer.from('event'))
	await store.putBytes('creds', 'creds', Buffer.from('c'))
	await store.putBytes('signal:session', 's', Buffer.from('s'))
	await store.wipeAllExcept(['spool'])
	assert.equal(await store.getBytes('creds', 'creds'), undefined)
	assert.equal(await store.getBytes('signal:session', 's'), undefined)
	assert.equal((await store.getBytes('spool', 'e1'))?.toString(), 'event')
	store.lock()
	assert.deepEqual(await new EncryptedStore(root).unlock(key), { initialized: false })
})

test('concurrent writes to one record leave a complete value', async t => {
	const root = await tempDir(t)
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(dataKey())
	await Promise.all(Array.from({ length: 20 }, (_, i) => store.putBytes('x', 'same', Buffer.from(`value-${i}`))))
	const value = (await store.getBytes('x', 'same'))?.toString()
	assert.match(value ?? '', /^value-\d+$/)
	const dir = (await readdir(root, { withFileTypes: true })).find(e => e.isDirectory())!
	assert.deepEqual((await readdir(join(root, dir.name))).filter(f => f.endsWith('.tmp')), [])
})
