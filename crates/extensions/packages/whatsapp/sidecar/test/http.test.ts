import assert from 'node:assert/strict'
import { test } from 'node:test'
import { Readable } from 'node:stream'
import type { IncomingMessage } from 'node:http'
import { readJson } from '../src/http/validate.js'
import { Sidecar } from '../src/sidecar.js'
import { dataKey, eventually, memoryLogger, tempDir, testConfig } from './support/helpers.js'
import { startHarness, type Harness } from './support/harness.js'
import { FakeWhatsApp, textMessage } from './support/fake-whatsapp.js'

const CUSTOMER = '447700900123@s.whatsapp.net'

async function unlocked(t: Parameters<typeof tempDir>[0], env: Record<string, string> = {}) {
	const dir = await tempDir(t)
	const h = await startHarness(dir, env)
	t.after(() => h.close())
	const key = dataKey()
	const res = await h.call('POST', '/v1/unlock', { body: { data_key: key.toString('base64') } })
	assert.equal(res.status, 200)
	return { h, key, dir }
}

async function customerWrites(h: Harness, text = 'Hello, can I book Tuesday?') {
	const before = h.delivered.filter(d => d.body.type === 'message.received').length
	h.fake().deliver([textMessage(CUSTOMER, text)])
	await eventually(() => h.delivered.filter(d => d.body.type === 'message.received').length > before)
	return h.delivered.filter(d => d.body.type === 'message.received').at(-1)!
}

test('healthz needs no token and reveals only up/locked', async t => {
	const h = await startHarness(await tempDir(t))
	t.after(() => h.close())
	const res = await h.call('GET', '/healthz', { token: null })
	assert.equal(res.status, 200)
	assert.deepEqual(res.json, { status: 'ok', locked: true })
})

test('every /v1 route requires the bearer token', async t => {
	const h = await startHarness(await tempDir(t))
	t.after(() => h.close())
	const routes: Array<[string, string]> = [
		['POST', '/v1/unlock'],
		['POST', '/v1/send'],
		['POST', '/v1/contacts/x/block'],
		['POST', '/v1/contacts/x/unblock'],
		['GET', '/v1/media/med_1'],
		['POST', '/v1/link/begin'],
		['GET', '/v1/link/status'],
		['POST', '/v1/link/cancel'],
		['POST', '/v1/unlink']
	]
	for (const [method, path] of routes) {
		for (const token of [null, 'wrong-token', '']) {
			const res = await h.call(method, path, { token })
			assert.equal(res.status, 401, `${method} ${path} token=${token}`)
			assert.deepEqual(res.json, { error: 'unauthorized' })
		}
	}
})

test('while locked every route except unlock returns 423, and sidecar.locked is announced', async t => {
	const h = await startHarness(await tempDir(t))
	t.after(() => h.close())
	for (const [method, path] of [
		['POST', '/v1/send'],
		['GET', '/v1/link/status'],
		['POST', '/v1/link/begin'],
		['GET', '/v1/media/med_1'],
		['POST', '/v1/contacts/x/block'],
		['POST', '/v1/unlink']
	] as const) {
		const res = await h.call(method, path, method === 'POST' ? { body: {} } : {})
		assert.equal(res.status, 423, `${method} ${path}`)
		assert.deepEqual(res.json, { error: 'locked' })
	}
	await eventually(() => h.delivered.some(d => d.body.type === 'sidecar.locked'))
	const notice = h.delivered.find(d => d.body.type === 'sidecar.locked')!
	assert.match(String(notice.body.event_id), /^evt_/)
	assert.match(notice.headers['X-Sidecar-Signature']!, /^v1=[0-9a-f]{64}$/)
})

test('unlock: idempotent, wrong key 403, malformed key 400, survives restart', async t => {
	const dir = await tempDir(t)
	const key = dataKey()
	{
		const h = await startHarness(dir)
		const first = await h.call('POST', '/v1/unlock', { body: { data_key: key.toString('base64') } })
		assert.deepEqual(first.json, { state: 'unlocked', initialized: true })
		const again = await h.call('POST', '/v1/unlock', { body: { data_key: key.toString('base64') } })
		assert.deepEqual(again.json, { state: 'unlocked', initialized: false })
		const wrong = await h.call('POST', '/v1/unlock', { body: { data_key: dataKey().toString('base64') } })
		assert.equal(wrong.status, 403)
		assert.equal(wrong.json.error, 'wrong_key')
		for (const bad of ['', 'abc', Buffer.alloc(16).toString('base64'), 42]) {
			const res = await h.call('POST', '/v1/unlock', { body: { data_key: bad } })
			assert.equal(res.status, 400, String(bad))
		}
		await h.close()
	}
	const h = await startHarness(dir)
	t.after(() => h.close())
	assert.equal((await h.call('POST', '/v1/unlock', { body: { data_key: dataKey().toString('base64') } })).status, 403)
	assert.equal((await h.call('GET', '/v1/link/status')).status, 423)
	const ok = await h.call('POST', '/v1/unlock', { body: { data_key: key.toString('base64') } })
	assert.deepEqual(ok.json, { state: 'unlocked', initialized: false })
	assert.equal((await h.call('GET', '/v1/link/status')).status, 200)
})

test('inbound message is forwarded, and the chat becomes sendable', async t => {
	const { h } = await unlocked(t)
	const unknown = await h.call('POST', '/v1/send', {
		body: { chat_id: CUSTOMER, text: 'Hi!', idempotency_key: 'k1' }
	})
	assert.equal(unknown.status, 403)
	assert.equal(unknown.json.error, 'not_a_known_chat')
	assert.equal(h.fake().sent.length, 0)

	const event = await customerWrites(h)
	assert.equal(event.body.chat_id, CUSTOMER)
	assert.equal(event.body.contact_id, CUSTOMER)
	assert.equal(event.body.contact_phone_number, '447700900123')
	assert.equal(event.body.text, 'Hello, can I book Tuesday?')
	assert.equal(event.body.blocked, false)
	assert.deepEqual(event.body.media, [])

	const sent = await h.call('POST', '/v1/send', {
		body: { chat_id: CUSTOMER, text: 'Yes, 4pm works.', quoted_message_id: event.body.message_id, idempotency_key: 'k2' }
	})
	assert.equal(sent.status, 200, sent.text)
	assert.match(sent.json.message_id, /^3EB0FAKE/)
	assert.equal(sent.json.server_ack_at, '2025-10-09T08:53:20.000Z')
	assert.equal(h.fake().sent.length, 1)
	assert.equal(h.fake().sent[0]?.text, 'Yes, 4pm works.')
	assert.equal(h.fake().sent[0]?.quoted?.key.id, event.body.message_id)

	// Same idempotency key, same request: the stored result, no second send.
	const replay = await h.call('POST', '/v1/send', {
		body: { chat_id: CUSTOMER, text: 'Yes, 4pm works.', quoted_message_id: event.body.message_id, idempotency_key: 'k2' }
	})
	assert.deepEqual(replay.json, sent.json)
	assert.equal(h.fake().sent.length, 1)
	// Same key, different request: conflict.
	const conflict = await h.call('POST', '/v1/send', { body: { chat_id: CUSTOMER, text: 'other', idempotency_key: 'k2' } })
	assert.equal(conflict.status, 409)
	assert.equal(conflict.json.error, 'idempotency_conflict')
})

test('message text never reaches the log above debug', async t => {
	const { h } = await unlocked(t)
	await customerWrites(h, 'my secret appointment details')
	await h.call('POST', '/v1/send', { body: { chat_id: CUSTOMER, text: 'secret reply text', idempotency_key: 'k' } })
	const visible = h.logLines.filter(l => !/"level":"(debug|trace)"/.test(l))
	assert.ok(visible.length > 0)
	for (const line of visible) {
		assert.ok(!line.includes('secret'), line)
		assert.ok(!line.includes('447700900123'), line)
	}
})

test('groups, status and fromMe are not forwarded as customer messages', async t => {
	const { h } = await unlocked(t)
	await customerWrites(h)
	const count = () => h.delivered.filter(d => d.body.type === 'message.received').length
	const before = count()
	h.fake().deliver([
		textMessage('120363000000000000@g.us', 'group chatter'),
		textMessage('status@broadcast', 'status'),
		textMessage('120363000000000000@newsletter', 'news')
	])
	h.fake().deliver([textMessage(CUSTOMER, 'append, not notify')], 'append')
	// The owner replies from the phone in the customer's chat.
	h.fake().deliver([textMessage(CUSTOMER, 'owner reply', { key: { remoteJid: CUSTOMER, fromMe: true, id: 'OWNER1' } })])
	// The owner messages someone who never wrote to the business: not forwarded.
	h.fake().deliver([
		textMessage('447700900999@s.whatsapp.net', 'private', { key: { remoteJid: '447700900999@s.whatsapp.net', fromMe: true, id: 'OWNER2' } })
	])
	await eventually(() => h.delivered.some(d => d.body.type === 'owner.activity'))
	await new Promise(r => setTimeout(r, 50))
	assert.equal(count(), before)
	const owner = h.delivered.filter(d => d.body.type === 'owner.activity')
	assert.equal(owner.length, 1)
	assert.equal(owner[0]?.body.chat_id, CUSTOMER)
	assert.equal(owner[0]?.body.message_id, 'OWNER1')
	assert.equal(owner[0]?.body.text, undefined)
})

test('a duplicate delivery of the same message is forwarded once', async t => {
	const { h } = await unlocked(t)
	const m = textMessage(CUSTOMER, 'once')
	h.fake().deliver([m])
	h.fake().deliver([m])
	await eventually(() => h.delivered.some(d => d.body.type === 'message.received'))
	await new Promise(r => setTimeout(r, 50))
	assert.equal(h.delivered.filter(d => d.body.type === 'message.received').length, 1)
})

test('sends are rate limited per chat with 429 and retry_after', async t => {
	const { h } = await unlocked(t, { SIDECAR_SEND_PER_CHAT_BURST: '2', SIDECAR_SEND_PER_CHAT_PER_MINUTE: '1' })
	await customerWrites(h)
	for (const k of ['a', 'b']) {
		assert.equal((await h.call('POST', '/v1/send', { body: { chat_id: CUSTOMER, text: 't', idempotency_key: k } })).status, 200)
	}
	const limited = await h.call('POST', '/v1/send', { body: { chat_id: CUSTOMER, text: 't', idempotency_key: 'c' } })
	assert.equal(limited.status, 429)
	assert.equal(limited.json.error, 'rate_limited')
	assert.ok(limited.json.retry_after_secs >= 1)
	assert.equal(limited.headers.get('retry-after'), String(limited.json.retry_after_secs))
	assert.equal(h.fake().sent.length, 2)
})

test('negative ack and ack timeout are reported, not claimed as delivered', async t => {
	const { h } = await unlocked(t, { SIDECAR_SEND_ACK_TIMEOUT_MS: '1000' })
	await customerWrites(h)
	h.fake().ackMode = 'error'
	const rejected = await h.call('POST', '/v1/send', { body: { chat_id: CUSTOMER, text: 't', idempotency_key: 'r' } })
	assert.equal(rejected.status, 502)
	assert.equal(rejected.json.error, 'send_rejected')
	assert.equal(rejected.json.code, '479')

	h.fake().ackMode = 'none'
	const timeout = await h.call('POST', '/v1/send', { body: { chat_id: CUSTOMER, text: 't2', idempotency_key: 'n' } })
	assert.equal(timeout.status, 504)
	assert.equal(timeout.json.error, 'ack_timeout')
	assert.ok(timeout.json.message_id)
	// The ack arrives late; a retry with the same key now reports it.
	h.fake().events.onAck(timeout.json.message_id, 1_760_000_100)
	await eventually(async () => {
		const retry = await h.call('POST', '/v1/send', { body: { chat_id: CUSTOMER, text: 't2', idempotency_key: 'n' } })
		return retry.status === 200 && retry.json.message_id === timeout.json.message_id
	})
	assert.equal(h.fake().sent.length, 2)
})

test('send needs a connection', async t => {
	const { h } = await unlocked(t)
	await customerWrites(h)
	h.fake().connected = false
	const res = await h.call('POST', '/v1/send', { body: { chat_id: CUSTOMER, text: 't', idempotency_key: 'x' } })
	assert.equal(res.status, 503)
	assert.equal(res.json.error, 'not_connected')
})

test('request validation: unknown fields, blank text, content type, body size, bad JSON', async t => {
	const { h } = await unlocked(t, { SIDECAR_MAX_BODY_BYTES: '2048' })
	const cases: Array<[unknown, number, string]> = [
		[{ chat_id: CUSTOMER, text: 'x', idempotency_key: 'k', extra: 1 }, 400, 'invalid_request'],
		[{ chat_id: CUSTOMER, text: '   ', idempotency_key: 'k' }, 400, 'invalid_request'],
		[{ chat_id: CUSTOMER, text: 'x' }, 400, 'invalid_request'],
		[{ chat_id: CUSTOMER, text: 'x', idempotency_key: 'bad key!' }, 400, 'invalid_request'],
		[{ chat_id: CUSTOMER, text: 'x'.repeat(3000), idempotency_key: 'k' }, 413, 'body_too_large'],
		[[1, 2], 400, 'invalid_request']
	]
	for (const [body, status, error] of cases) {
		const res = await h.call('POST', '/v1/send', { body })
		assert.equal(res.status, status, JSON.stringify(body).slice(0, 80))
		assert.equal(res.json.error, error)
	}
	const badJson = await h.call('POST', '/v1/send', { rawBody: '{nope', headers: { 'content-type': 'application/json' } })
	assert.equal(badJson.status, 400)
	assert.equal(badJson.json.error, 'invalid_json')
	const wrongType = await h.call('POST', '/v1/send', { rawBody: 'a=b', headers: { 'content-type': 'application/x-www-form-urlencoded' } })
	assert.equal(wrongType.status, 415)
	assert.equal((await h.call('GET', '/v1/nope')).status, 404)
	const wrongMethod = await h.call('GET', '/v1/send')
	assert.equal(wrongMethod.status, 405)
	assert.equal(wrongMethod.headers.get('allow'), 'POST')
	for (const res of [badJson, wrongType]) {
		assert.ok(!res.text.includes('/build/') && !res.text.includes('at '), 'no stack traces or paths')
	}
})

test('block and unblock a known contact; blocked chats cannot be sent to and inbound is flagged', async t => {
	const { h } = await unlocked(t)
	const event = await customerWrites(h)
	const contact = encodeURIComponent(String(event.body.contact_id))
	assert.equal((await h.call('POST', '/v1/contacts/999%40s.whatsapp.net/block')).status, 404)
	const blocked = await h.call('POST', `/v1/contacts/${contact}/block`)
	assert.equal(blocked.status, 200)
	assert.deepEqual(blocked.json, { contact_id: CUSTOMER, blocked: true })
	assert.deepEqual(h.fake().blocks, [{ jid: CUSTOMER, blocked: true }])
	const send = await h.call('POST', '/v1/send', { body: { chat_id: CUSTOMER, text: 't', idempotency_key: 'b' } })
	assert.equal(send.status, 403)
	assert.equal(send.json.error, 'contact_blocked')
	const again = await customerWrites(h, 'let me in')
	assert.equal(again.body.blocked, true)
	const unblocked = await h.call('POST', `/v1/contacts/${contact}/unblock`)
	assert.deepEqual(unblocked.json, { contact_id: CUSTOMER, blocked: false })
	assert.equal((await h.call('POST', '/v1/send', { body: { chat_id: CUSTOMER, text: 't', idempotency_key: 'u' } })).status, 200)
})

test('media is described inbound and streamed on request, with a size cap', async t => {
	const { h } = await unlocked(t, { SIDECAR_MEDIA_MAX_BYTES: '4096' })
	h.fake().deliver([
		textMessage(CUSTOMER, '', {
			message: { imageMessage: { mimetype: 'image/png', fileLength: 16, caption: 'photo of the leak' } }
		}),
		textMessage(CUSTOMER, '', {
			message: { documentMessage: { mimetype: 'application/pdf', fileLength: 999_999, fileName: 'big.pdf' } }
		})
	])
	await eventually(() => h.delivered.filter(d => d.body.type === 'message.received').length === 2)
	const [image, doc] = h.delivered.filter(d => d.body.type === 'message.received')
	assert.equal(image?.body.text, 'photo of the leak')
	const media = (image?.body.media as Array<{ media_id: string; mime_type: string; size_bytes: number }>)[0]!
	assert.equal(media.mime_type, 'image/png')
	assert.equal(media.size_bytes, 16)
	const res = await h.call('GET', `/v1/media/${media.media_id}`)
	assert.equal(res.status, 200)
	assert.equal(res.headers.get('content-type'), 'image/png')
	assert.equal(res.text, 'fake media bytes')

	const big = (doc?.body.media as Array<{ media_id: string; file_name: string }>)[0]!
	assert.equal(big.file_name, 'big.pdf')
	const tooBig = await h.call('GET', `/v1/media/${big.media_id}`)
	assert.equal(tooBig.status, 413)
	assert.equal(tooBig.json.error, 'media_too_large')
	assert.equal((await h.call('GET', '/v1/media/med_unknown')).status, 404)
})

test('link routes and unlink (which wipes and locks again)', async t => {
	const { h, key } = await unlocked(t)
	const status = await h.call('GET', '/v1/link/status')
	assert.equal(status.json.state, 'linked')
	assert.equal(status.json.connected, true)
	const conflict = await h.call('POST', '/v1/link/begin', { body: { mode: 'qr' } })
	assert.equal(conflict.status, 409)
	assert.equal(conflict.json.error, 'already_linked')
	assert.equal((await h.call('POST', '/v1/link/begin', { body: { mode: 'pairing_code' } })).status, 400)
	assert.equal((await h.call('POST', '/v1/link/begin', { body: { mode: 'pairing_code', phone_number: '12ab' } })).status, 400)

	await customerWrites(h)
	const unlink = await h.call('POST', '/v1/unlink')
	assert.equal(unlink.status, 200)
	assert.deepEqual(unlink.json, { state: 'unlinked', locked: true })
	assert.equal((await h.call('GET', '/v1/link/status')).status, 423)

	const again = await h.call('POST', '/v1/unlock', { body: { data_key: key.toString('base64') } })
	assert.deepEqual(again.json, { state: 'unlocked', initialized: false })
	h.fake().state = 'unlinked'
	h.fake().connected = false
	const begin = await h.call('POST', '/v1/link/begin', { body: { mode: 'pairing_code', phone_number: '+447700900000' } })
	assert.deepEqual(begin.json, { display_kind: 'pairing_code', payload: 'CODE-0000', expires_in_secs: 180 })
	const linking = await h.call('GET', '/v1/link/status')
	assert.equal(linking.json.state, 'linking')
	// The known-chat allowlist was wiped with the account.
	h.fake().state = 'linked'
	h.fake().connected = true
	const send = await h.call('POST', '/v1/send', { body: { chat_id: CUSTOMER, text: 't', idempotency_key: 'z' } })
	assert.equal(send.status, 403)
	assert.equal(send.json.error, 'not_a_known_chat')
})

test('undelivered events stay spooled across restarts and lock cycles', async t => {
	const dir = await tempDir(t)
	const key = dataKey()
	{
		const h = await startHarness(dir)
		h.webhookStatus.value = 503
		await h.call('POST', '/v1/unlock', { body: { data_key: key.toString('base64') } })
		h.fake().deliver([textMessage(CUSTOMER, 'while IronClaw is down')])
		await eventually(() => h.delivered.some(d => d.body.type === 'message.received'))
		await h.close()
	}
	const h = await startHarness(dir)
	t.after(() => h.close())
	// A lock cycle (an unlink here) while IronClaw is still down keeps the event too.
	h.webhookStatus.value = 503
	await h.call('POST', '/v1/unlock', { body: { data_key: key.toString('base64') } })
	const unlinked = await h.call('POST', '/v1/unlink')
	assert.equal(unlinked.status, 200)
	const attemptsWhileDown = h.delivered.filter(d => d.body.type === 'message.received').length
	h.webhookStatus.value = 200
	await h.call('POST', '/v1/unlock', { body: { data_key: key.toString('base64') } })
	await eventually(() => h.delivered.filter(d => d.body.type === 'message.received').length > attemptsWhileDown)
	const ids = new Set(h.delivered.filter(d => d.body.type === 'message.received').map(d => d.body.event_id))
	assert.equal(ids.size, 1)
})

test('concurrent sends with one idempotency key send once and report the same message', async t => {
	const { h } = await unlocked(t)
	await customerWrites(h)
	const body = { chat_id: CUSTOMER, text: 'one reply', idempotency_key: 'dup-key' }
	const [a, b] = await Promise.all([h.call('POST', '/v1/send', { body }), h.call('POST', '/v1/send', { body })])
	assert.equal(h.fake().sent.length, 1, 'the message went to WhatsApp once')
	assert.equal(a.status, 200)
	assert.equal(b.status, 200)
	assert.equal(a.json.message_id, b.json.message_id)
})

test('a send that never reached WhatsApp can be retried with the same key; other failures replay as send_failed', async t => {
	const { h } = await unlocked(t)
	await customerWrites(h)
	const dropped = { chat_id: CUSTOMER, text: 'after a drop', idempotency_key: 'dropped' }
	h.fake().failNextSend = 'not_connected'
	const first = await h.call('POST', '/v1/send', { body: dropped })
	assert.equal(first.status, 503)
	assert.equal(first.json.error, 'not_connected')
	const retry = await h.call('POST', '/v1/send', { body: dropped })
	assert.equal(retry.status, 200, 'nothing was sent, so the key is free to succeed')
	assert.equal(h.fake().sent.length, 1)

	const broken = { chat_id: CUSTOMER, text: 'socket error', idempotency_key: 'broken' }
	h.fake().failNextSend = 'error'
	const failed = await h.call('POST', '/v1/send', { body: broken })
	assert.equal(failed.status, 502)
	assert.equal(failed.json.error, 'send_failed')
	const replay = await h.call('POST', '/v1/send', { body: broken })
	assert.equal(replay.status, 502)
	assert.equal(replay.json.error, 'send_failed', 'a replay must not claim WhatsApp rejected the message')
	assert.equal(h.fake().sent.length, 1)
})

test('an ack that arrives after a restart still settles the send', async t => {
	const dir = await tempDir(t)
	const key = dataKey()
	const body = { chat_id: CUSTOMER, text: 'slow ack', idempotency_key: 'slow' }
	let messageId: string
	{
		const h = await startHarness(dir, { SIDECAR_SEND_ACK_TIMEOUT_MS: '1000' })
		await h.call('POST', '/v1/unlock', { body: { data_key: key.toString('base64') } })
		await customerWrites(h)
		h.fake().ackMode = 'none'
		const timeout = await h.call('POST', '/v1/send', { body })
		assert.equal(timeout.status, 504)
		messageId = timeout.json.message_id
		await h.close()
	}
	const h = await startHarness(dir)
	t.after(() => h.close())
	await h.call('POST', '/v1/unlock', { body: { data_key: key.toString('base64') } })
	h.fake().events.onAck(messageId, 1_760_000_200)
	await eventually(async () => {
		const retry = await h.call('POST', '/v1/send', { body })
		return retry.status === 200 && retry.json.message_id === messageId
	})
	assert.equal(h.fake().sent.length, 0, 'the retry replays; it does not send again')
})

test('an inbound message whose event could not be spooled is forwarded when WhatsApp delivers it again', async t => {
	const { h } = await unlocked(t)
	const spool = (h.sidecar as unknown as { spool: { enqueue: (e: { message_id?: unknown }) => Promise<void> } }).spool
	const enqueue = spool.enqueue.bind(spool)
	const failing = textMessage(CUSTOMER, 'first, hits a full disk')
	const following = textMessage(CUSTOMER, 'second, in the same batch')
	let failures = 0
	spool.enqueue = async event => {
		if (event.message_id === failing.key.id && failures === 0) {
			failures++
			throw new Error('ENOSPC')
		}
		return enqueue(event)
	}
	h.fake().deliver([failing, following])
	await eventually(() => h.delivered.some(d => d.body.message_id === following.key.id))
	assert.ok(!h.delivered.some(d => d.body.message_id === failing.key.id))
	assert.equal(failures, 1)

	h.fake().deliver([failing]) // WhatsApp's redelivery
	await eventually(() => h.delivered.some(d => d.body.message_id === failing.key.id))
	h.fake().deliver([failing]) // and a genuine duplicate after that
	h.fake().deliver([textMessage(CUSTOMER, 'a later message')])
	await eventually(() => h.delivered.filter(d => d.body.type === 'message.received').length === 3)
	assert.equal(h.delivered.filter(d => d.body.message_id === failing.key.id).length, 1)
})

test('routes without input still reject unknown fields', async t => {
	const { h } = await unlocked(t)
	await customerWrites(h)
	const contact = h.delivered.find(d => d.body.type === 'message.received')!.body.contact_id as string
	for (const path of [`/v1/contacts/${encodeURIComponent(contact)}/block`, '/v1/link/cancel', '/v1/unlink']) {
		const res = await h.call('POST', path, { body: { surprise: true } })
		assert.equal(res.status, 400, path)
		assert.equal(res.json.field, 'surprise', path)
	}
	assert.equal(h.fake().blocks.length, 0)
	assert.equal(h.fake().loggedOut, false)
})

test('the locked notice uses the documented event id format', async t => {
	const h = await startHarness(await tempDir(t))
	t.after(() => h.close())
	await eventually(() => h.delivered.some(d => d.body.type === 'sidecar.locked'))
	const notice = h.delivered.find(d => d.body.type === 'sidecar.locked')!
	assert.match(String(notice.body.event_id), /^evt_[0-9a-f]{32}$/)
})

test('a logout started by WhatsApp wipes the session, says so, and reports logged_out', async t => {
	const { h } = await unlocked(t)
	await customerWrites(h)
	h.fake().state = 'unlinked'
	h.fake().connected = false
	h.fake().events.onLinkState('logged_out')
	await eventually(() => h.delivered.some(d => d.body.type === 'link.state_changed' && d.body.state === 'logged_out'))
	const status = await h.call('GET', '/v1/link/status')
	assert.equal(status.json.state, 'logged_out')
	h.fake().connected = true
	const send = await h.call('POST', '/v1/send', { body: { chat_id: CUSTOMER, text: 'x', idempotency_key: 'after-logout' } })
	assert.equal(send.status, 403, 'the known-chat allowlist went with the session')
	assert.equal(send.json.error, 'not_a_known_chat')
})

test('media past the retention period is unknown', async t => {
	const { h } = await unlocked(t)
	h.fake().deliver([
		textMessage(CUSTOMER, '', { message: { imageMessage: { mimetype: 'image/jpeg', fileLength: 16, caption: 'pic' } } } as never)
	])
	await eventually(() => h.delivered.some(d => d.body.type === 'message.received'))
	const mediaId = (h.delivered.at(-1)!.body.media as Array<{ media_id: string }>)[0]!.media_id
	const domain = (h.sidecar as unknown as { domain: { prune: (cutoff: Date) => Promise<number> } }).domain
	await domain.prune(new Date(Date.now() + 60_000))
	const res = await h.call('GET', `/v1/media/${mediaId}`)
	assert.equal(res.status, 404)
	assert.equal(res.json.error, 'unknown_media')
})

/** A sidecar over `dir` whose WhatsApp factory counts the connections it builds. */
async function countingSidecar(dir: string, factory?: () => void) {
	const made: FakeWhatsApp[] = []
	const sidecar = new Sidecar(
		testConfig(dir),
		memoryLogger().logger,
		(_store, events) => {
			factory?.()
			const fake = new FakeWhatsApp(events)
			made.push(fake)
			return fake
		},
		async () => 200
	)
	await sidecar.boot()
	return { sidecar, made }
}

test('concurrent unlocks build one WhatsApp connection, even when the first one fails', async t => {
	const dir = await tempDir(t)
	const key = dataKey()
	{
		const { sidecar } = await countingSidecar(dir)
		await sidecar.unlock(Buffer.from(key))
		await sidecar.shutdown()
	}
	const { sidecar, made } = await countingSidecar(dir)
	t.after(() => sidecar.shutdown())
	const results = await Promise.allSettled([sidecar.unlock(dataKey()), sidecar.unlock(Buffer.from(key)), sidecar.unlock(Buffer.from(key))])
	assert.deepEqual(
		results.map(r => r.status),
		['rejected', 'fulfilled', 'fulfilled']
	)
	assert.equal(made.length, 1, 'one connection for one unlocked store')
	await sidecar.lock()
	assert.equal(made[0]!.connected, false, 'lock() stops the only connection')
})

test('an unlock that fails after opening the store locks it again', async t => {
	const dir = await tempDir(t)
	const key = dataKey()
	let fail = true
	const { sidecar, made } = await countingSidecar(dir, () => {
		if (fail) throw new Error('connection setup failed')
	})
	t.after(() => sidecar.shutdown())
	await assert.rejects(sidecar.unlock(Buffer.from(key)))
	assert.equal(sidecar.locked, true, 'a half-finished unlock must not leave the store open')
	fail = false
	await sidecar.unlock(Buffer.from(key))
	assert.equal(sidecar.locked, false)
	assert.equal(made.length, 1)
})

test('the unlock body is wiped once the key is read', async () => {
	const chunks = [Buffer.from('{"data_key":"'), Buffer.from('A'.repeat(43) + '="}')]
	const req = Object.assign(Readable.from(chunks), {
		headers: { 'content-type': 'application/json' }
	}) as unknown as IncomingMessage
	const body = await readJson(req, 1024, { wipe: true })
	assert.equal(body.data_key, 'A'.repeat(43) + '=')
	for (const chunk of chunks) assert.ok(chunk.every(b => b === 0), 'request chunks are zeroed')
})
