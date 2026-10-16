import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import type { AddressInfo } from 'node:net'
import { test } from 'node:test'
import { DisconnectReason, initAuthCreds } from 'baileys'
import { accountLabel, BaileysConnection, closeAction, disconnectCode, formatPairingCode } from '../src/connection/baileys-connection.js'
import { checkMediaLocation } from '../src/connection/media-location.js'
import { EncryptedStore } from '../src/store/encrypted-store.js'
import { dataKey, memoryLogger, tempDir } from './support/helpers.js'

test('close codes: logged out and forbidden end the session; restart reconnects at once', () => {
	assert.equal(closeAction(DisconnectReason.loggedOut), 'logged_out')
	assert.equal(closeAction(DisconnectReason.forbidden), 'logged_out')
	assert.equal(closeAction(DisconnectReason.restartRequired), 'restart_now')
	assert.equal(closeAction(DisconnectReason.connectionLost), 'reconnect')
	assert.equal(closeAction(DisconnectReason.connectionReplaced), 'reconnect')
	assert.equal(closeAction(undefined), 'reconnect')
	assert.equal(disconnectCode({ lastDisconnect: { error: Object.assign(new Error('x'), { output: { statusCode: 401 } }), date: new Date() } }), 401)
	assert.equal(disconnectCode({}), undefined)
})

test('account label: push name, else the last four digits only', () => {
	const creds = initAuthCreds()
	assert.equal(accountLabel(creds), undefined)
	creds.me = { id: '447700900000:5@s.whatsapp.net', name: '~' }
	assert.equal(accountLabel(creds), '…0000')
	creds.me = { id: '447700900000:5@s.whatsapp.net', name: 'Acme Plumbing' }
	assert.equal(accountLabel(creds), 'Acme Plumbing')
})

test('pairing codes are shown as two groups of four', () => {
	assert.equal(formatPairingCode('ABCD1234'), 'ABCD-1234')
})

test('with no linked account, start() stays unlinked and opens no socket', async t => {
	const root = await tempDir(t)
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(dataKey())
	const states: string[] = []
	const conn = new BaileysConnection({
		store,
		logger: memoryLogger().logger,
		reconnectBaseMs: 10,
		reconnectMaxMs: 100,
		linkFlowTtlMs: 60_000,
		wipeAuth: async () => undefined,
		events: {
			onUpsert: () => undefined,
			onLinkState: s => states.push(s),
			onAck: () => undefined,
			getSentMessage: async () => undefined
		}
	})
	await conn.start()
	assert.deepEqual(conn.status(), { state: 'unlinked', connected: false })
	assert.equal(conn.ownJids().size, 0)
	await assert.rejects(conn.sendText('x@s.whatsapp.net', 'hi', { messageId: 'id' }), /not connected/)
	await conn.stop()
	assert.deepEqual(states, [])
})

function unlinkedConnection(store: EncryptedStore): BaileysConnection {
	return new BaileysConnection({
		store,
		logger: memoryLogger().logger,
		reconnectBaseMs: 10,
		reconnectMaxMs: 100,
		linkFlowTtlMs: 60_000,
		wipeAuth: async () => undefined,
		events: {
			onUpsert: () => undefined,
			onLinkState: () => undefined,
			onAck: () => undefined,
			getSentMessage: async () => undefined
		}
	})
}

test('media is never fetched from a location the customer chose', async t => {
	let connections = 0
	const server = createServer((_req, res) => res.end(Buffer.alloc(64)))
	server.on('connection', () => connections++)
	await new Promise<void>(r => server.listen(0, '127.0.0.1', r))
	t.after(() => new Promise<void>(r => server.close(() => r())))
	const local = `127.0.0.1:${(server.address() as AddressInfo).port}`

	const root = await tempDir(t)
	const store = new EncryptedStore(root)
	await store.init()
	await store.unlock(dataKey())
	const conn = unlinkedConnection(store)
	const mediaKey = Buffer.alloc(32, 1)
	for (const media of [
		{ url: `http://${local}/no-direct-path` },
		{ url: `https://${local}/x`, directPath: '/v/t62.7118-24/x.enc' },
		{ url: 'https://mmg.whatsapp.net/x', directPath: `@${local}/x` },
		{ directPath: `.${local}/x` }
	]) {
		const ref = {
			key: { remoteJid: '447700900123@s.whatsapp.net', id: 'X', fromMe: false },
			content: { imageMessage: { mimetype: 'image/jpeg', mediaKey, ...media } }
		}
		await assert.rejects(
			async () => {
				for await (const _ of await conn.downloadMedia(ref)) {
					// drain
				}
			},
			/media location/,
			JSON.stringify(media)
		)
	}
	assert.equal(connections, 0, 'no request left the sidecar')
})

test('media locations: WhatsApp media hosts and clean direct paths only', () => {
	const ok = (media: Record<string, unknown>, wrap = (m: Record<string, unknown>) => m) =>
		checkMediaLocation(wrap({ imageMessage: { mediaKey: Buffer.alloc(32), ...media } }))
	// Allowed: what WhatsApp actually sends.
	for (const media of [
		{ url: 'https://mmg.whatsapp.net/v/t62.7118-24/abc.enc?ccb=11-4&oh=01', directPath: '/v/t62.7118-24/abc.enc?ccb=11-4&oh=01' },
		{ directPath: '/v/t62.7118-24/abc.enc' },
		{ url: 'https://media-lhr8-1.cdn.whatsapp.net/v/t62/abc.enc' }
	]) {
		assert.doesNotThrow(() => ok(media), JSON.stringify(media))
	}
	assert.doesNotThrow(() => ok({ directPath: '/v/x.enc' }, m => ({ ephemeralMessage: { message: m } })))
	assert.doesNotThrow(() =>
		checkMediaLocation({ stickerMessage: { thumbnailDirectPath: '/v/t62/thumb.enc', mediaKey: Buffer.alloc(32) } } as never)
	)
	// Refused.
	for (const media of [
		{ url: 'http://mmg.whatsapp.net/v/x.enc' },
		{ url: 'https://evil.example/v/x.enc' },
		{ url: 'https://whatsapp.net.evil.example/v/x.enc' },
		{ url: 'https://user@mmg.whatsapp.net/v/x.enc' },
		{ url: 'https://mmg.whatsapp.net:8443/v/x.enc' },
		{ url: 'not a url' },
		{ directPath: 'v/x.enc' },
		{ directPath: '//evil.example/x' },
		{ directPath: '/v/x\\@evil' },
		{ directPath: '/v/x.enc@evil.example' },
		{ directPath: '/v/x enc' },
		{}
	]) {
		assert.throws(() => ok(media), /media location/, JSON.stringify(media))
	}
	assert.throws(() => ok({ directPath: 'evil' }, m => ({ viewOnceMessageV2: { message: m } })), /media location/)
	assert.throws(() => checkMediaLocation({ conversation: 'text only' }), /media location/)
})
