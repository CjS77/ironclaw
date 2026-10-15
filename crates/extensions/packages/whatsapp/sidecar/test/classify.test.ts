import assert from 'node:assert/strict'
import { test } from 'node:test'
import type { WAMessage } from 'baileys'
import { classify } from '../src/inbound/classify.js'

const OWN = new Set(['447700900000@s.whatsapp.net', '111111111111111@lid'])

function msg(remoteJid: string, message: WAMessage['message'], extra: Partial<WAMessage['key']> = {}, rest: Partial<WAMessage> = {}): WAMessage {
	return {
		key: { remoteJid, id: 'ABC123', fromMe: false, ...extra },
		message,
		messageTimestamp: 1_760_000_000,
		pushName: 'Customer',
		...rest
	} as WAMessage
}

test('only notify upserts are processed', () => {
	assert.deepEqual(classify('append', msg('447700900123@s.whatsapp.net', { conversation: 'hi' }), OWN), {
		kind: 'drop',
		reason: 'not_notify'
	})
})

test('groups, status, broadcast lists and newsletters are dropped', () => {
	const cases: Array<[string, string]> = [
		['120363000000000000@g.us', 'group'],
		['status@broadcast', 'status_broadcast'],
		['1234567890@broadcast', 'broadcast_list'],
		['120363000000000000@newsletter', 'newsletter'],
		['13135550000@bot', 'unsupported_chat']
	]
	for (const [jid, reason] of cases) {
		assert.deepEqual(classify('notify', msg(jid, { conversation: 'hi' }), OWN), { kind: 'drop', reason }, jid)
	}
})

test('fromMe in a 1:1 chat is owner activity', () => {
	const c = classify('notify', msg('447700900123@s.whatsapp.net', { conversation: 'on my way' }, { fromMe: true }), OWN)
	assert.equal(c.kind, 'owner_activity')
	if (c.kind !== 'owner_activity') return
	assert.deepEqual(c.jids, ['447700900123@s.whatsapp.net'])
	assert.equal(c.messageId, 'ABC123')
	assert.equal(c.timestamp.toISOString(), '2025-10-09T08:53:20.000Z')
})

test('fromMe in a group is still dropped', () => {
	assert.equal(classify('notify', msg('120363000000000000@g.us', { conversation: 'x' }, { fromMe: true }), OWN).kind, 'drop')
})

test('the self chat is dropped', () => {
	assert.deepEqual(classify('notify', msg('447700900000@s.whatsapp.net', { conversation: 'note to self' }, { fromMe: true }), OWN), {
		kind: 'drop',
		reason: 'self_chat'
	})
})

test('protocol messages, reactions and empty stubs are dropped', () => {
	assert.deepEqual(classify('notify', msg('447700900123@s.whatsapp.net', { protocolMessage: { type: 0 } }), OWN), {
		kind: 'drop',
		reason: 'protocol'
	})
	assert.deepEqual(
		classify('notify', msg('447700900123@s.whatsapp.net', { reactionMessage: { text: '👍', key: { id: 'X' } } }), OWN),
		{ kind: 'drop', reason: 'reaction' }
	)
	assert.deepEqual(classify('notify', msg('447700900123@s.whatsapp.net', undefined, {}, { messageStubType: 1 }), OWN), {
		kind: 'drop',
		reason: 'stub'
	})
})

test('a plain text message from a phone-number JID', () => {
	const c = classify('notify', msg('447700900123:12@s.whatsapp.net', { conversation: 'Can I get a quote?' }), OWN)
	assert.equal(c.kind, 'message')
	if (c.kind !== 'message') return
	assert.deepEqual(c.jids, ['447700900123@s.whatsapp.net'])
	assert.equal(c.contactId, '447700900123@s.whatsapp.net')
	assert.equal(c.phoneNumber, '447700900123')
	assert.equal(c.text, 'Can I get a quote?')
	assert.equal(c.contentKind, 'text')
	assert.equal(c.pushName, 'Customer')
	assert.deepEqual(c.media, [])
})

test('a LID chat with a phone-number alternate prefers the LID as contact id', () => {
	const c = classify(
		'notify',
		msg('123456789012345@lid', { extendedTextMessage: { text: 'hello', contextInfo: { stanzaId: 'QUOTED1' } } }, {
			remoteJidAlt: '447700900123@s.whatsapp.net'
		} as Partial<WAMessage['key']>),
		OWN
	)
	assert.equal(c.kind, 'message')
	if (c.kind !== 'message') return
	assert.deepEqual(c.jids, ['123456789012345@lid', '447700900123@s.whatsapp.net'])
	assert.equal(c.contactId, '123456789012345@lid')
	assert.equal(c.phoneNumber, '447700900123')
	assert.equal(c.quotedMessageId, 'QUOTED1')
})

test('captions and media descriptors are extracted; wrappers are unwrapped', () => {
	const c = classify(
		'notify',
		msg('447700900123@s.whatsapp.net', {
			ephemeralMessage: {
				message: {
					documentWithCaptionMessage: {
						message: {
							documentMessage: { caption: 'my invoice', mimetype: 'application/pdf', fileLength: 2048, fileName: 'invoice.pdf' }
						}
					}
				}
			}
		}),
		OWN
	)
	assert.equal(c.kind, 'message')
	if (c.kind !== 'message') return
	assert.equal(c.text, 'my invoice')
	assert.equal(c.contentKind, 'document')
	assert.deepEqual(c.media, [{ kind: 'document', mime_type: 'application/pdf', size_bytes: 2048, file_name: 'invoice.pdf' }])

	const image = classify('notify', msg('447700900123@s.whatsapp.net', { imageMessage: { mimetype: 'image/jpeg', fileLength: 10 } }), OWN)
	assert.equal(image.kind === 'message' && image.contentKind, 'image')
	assert.equal(image.kind === 'message' && image.text, undefined)
})

test('over-long text is truncated and flagged', () => {
	const long = 'a'.repeat(70_000)
	const c = classify('notify', msg('447700900123@s.whatsapp.net', { conversation: long }), OWN)
	assert.equal(c.kind, 'message')
	if (c.kind !== 'message') return
	assert.equal(c.text?.length, 65_536)
	assert.equal(c.textTruncated, true)
})
