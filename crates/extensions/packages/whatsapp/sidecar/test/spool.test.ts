import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import type { AddressInfo } from 'node:net'
import { test } from 'node:test'
import { hmacSha256 } from '../src/crypto.js'
import { EncryptedStore } from '../src/store/encrypted-store.js'
import { envelope } from '../src/webhook/events.js'
import { backoffMs, Deliverer, fetchTransport, LockedAnnouncer, Spool, type Transport } from '../src/webhook/spool.js'
import { dataKey, eventually, HMAC_KEY, memoryLogger, tempDir } from './support/helpers.js'

const URL_ = new URL('http://ironclaw.test/hook')

function options(transport: Transport) {
	return {
		url: URL_,
		hmacKey: HMAC_KEY,
		timeoutMs: 1000,
		retryBaseMs: 5,
		retryMaxMs: 20,
		transport,
		logger: memoryLogger().logger
	}
}

test('backoff grows exponentially up to the cap', () => {
	const fixed = () => 1 // no jitter reduction
	assert.equal(backoffMs(1, 100, 1000, fixed), 100)
	assert.equal(backoffMs(2, 100, 1000, fixed), 200)
	assert.equal(backoffMs(4, 100, 1000, fixed), 800)
	assert.equal(backoffMs(9, 100, 1000, fixed), 1000)
	assert.equal(backoffMs(9, 100, 1000, () => 0), 500)
})

test('failed deliveries are retried with the same event id until a 2xx', async t => {
	const root = await tempDir(t)
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(dataKey())
	const spool = new Spool(store, memoryLogger().logger)
	const seen: Array<{ body: string; headers: Record<string, string> }> = []
	const statuses = [500, 503, 0, 204]
	const transport: Transport = async ({ body, headers }) => {
		seen.push({ body, headers })
		const status = statuses.shift() ?? 200
		if (status === 0) throw new Error('ECONNREFUSED')
		return status
	}
	const deliverer = new Deliverer(spool, options(transport))
	deliverer.start()
	t.after(() => deliverer.stop())
	await spool.enqueue(envelope({ type: 'owner.activity', data: { chat_id: 'c', message_id: 'm', sent_at: 'x' } }))
	await eventually(() => spool.size === 0)
	assert.equal(seen.length, 4)
	assert.equal(new Set(seen.map(s => s.body)).size, 1)
	for (const s of seen) {
		const ts = s.headers['X-Sidecar-Timestamp']!
		const expected = 'v1=' + hmacSha256(HMAC_KEY, `v1:${ts}:${s.body}`).toString('hex')
		assert.equal(s.headers['X-Sidecar-Signature'], expected)
	}
	assert.deepEqual(await store.list('spool'), [])
})

test('the spool survives a restart and delivers in order', async t => {
	const root = await tempDir(t)
	const key = dataKey()
	{
		const store = new EncryptedStore(root)
		await store.init()
		await store.unlock(key)
		const spool = new Spool(store, memoryLogger().logger)
		for (let i = 0; i < 3; i++) {
			await spool.enqueue(envelope({ type: 'owner.activity', data: { chat_id: 'c', message_id: `m${i}`, sent_at: 'x' } }))
		}
		// IronClaw is down: one failed attempt, then the process stops.
		const deliverer = new Deliverer(spool, options(async () => 503))
		deliverer.start()
		await eventually(() => (spool.head()?.attempts ?? 0) >= 1)
		await deliverer.stop()
		store.lock()
	}
	const store = new EncryptedStore(root)
	await store.unlock(Buffer.from(key))
	const spool = new Spool(store, memoryLogger().logger)
	await spool.load()
	assert.equal(spool.size, 3)
	const delivered: string[] = []
	const deliverer = new Deliverer(
		spool,
		options(async ({ body }) => {
			delivered.push((JSON.parse(body) as { message_id: string }).message_id)
			return 200
		})
	)
	deliverer.start()
	t.after(() => deliverer.stop())
	await eventually(() => spool.size === 0)
	assert.deepEqual(delivered, ['m0', 'm1', 'm2'])
})

test('fetchTransport posts the signed body to a real HTTP endpoint', async t => {
	let received: { headers: Record<string, unknown>; body: string } | undefined
	const server = createServer((req, res) => {
		const chunks: Buffer[] = []
		req.on('data', c => chunks.push(c))
		req.on('end', () => {
			received = { headers: req.headers, body: Buffer.concat(chunks).toString() }
			res.writeHead(202).end()
		})
	})
	await new Promise<void>(r => server.listen(0, '127.0.0.1', r))
	t.after(() => new Promise<void>(r => server.close(() => r())))
	const port = (server.address() as AddressInfo).port
	const status = await fetchTransport({
		url: new URL(`http://127.0.0.1:${port}/hook`),
		body: '{"a":1}',
		headers: { 'X-Sidecar-Timestamp': '1', 'X-Sidecar-Signature': 'v1=00' },
		timeoutMs: 2000
	})
	assert.equal(status, 202)
	assert.equal(received?.body, '{"a":1}')
	assert.equal(received?.headers['content-type'], 'application/json')
	assert.equal(received?.headers['x-sidecar-signature'], 'v1=00')
})

test('the locked notice is retried until acknowledged', async () => {
	let calls = 0
	const announcer = new LockedAnnouncer(options(async ({ body }) => {
		calls++
		assert.equal((JSON.parse(body) as { type: string }).type, 'sidecar.locked')
		return calls < 3 ? 502 : 200
	}))
	announcer.start(envelope({ type: 'sidecar.locked', data: {} }))
	await eventually(() => calls === 3)
	await new Promise(r => setTimeout(r, 50))
	assert.equal(calls, 3)
	announcer.stop()
})
