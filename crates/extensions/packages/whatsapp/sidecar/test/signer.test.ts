import assert from 'node:assert/strict'
import { createHmac } from 'node:crypto'
import { test } from 'node:test'
import { sign, signatureHeaders, signedPayload } from '../src/webhook/signer.js'

// The fixed test vector published in API.md ("Signing").
const VECTOR = {
	key: 'whatsapp-sidecar-test-vector-key-0123456789',
	timestamp: '1760000000',
	body: '{"event_id":"evt_0199c8a1b2c3d4e5f60718293a4b","type":"sidecar.locked","occurred_at":"2025-10-09T08:53:20.000Z"}',
	signature: 'v1=f9766a96ae85c8653d570df10c9d12d0529df31c148168168025c3f40c4fc384'
}

test('the signature matches the documented test vector', () => {
	assert.equal(sign(VECTOR.key, VECTOR.timestamp, VECTOR.body), VECTOR.signature)
})

test('the signed payload is "v1:" + timestamp + ":" + body, as IronClaw recipe segments rebuild it', () => {
	// literal "v1:", header X-Sidecar-Timestamp, literal ":", body
	const rebuilt = Buffer.concat([
		Buffer.from('v1:'),
		Buffer.from(VECTOR.timestamp),
		Buffer.from(':'),
		Buffer.from(VECTOR.body)
	])
	assert.deepEqual(signedPayload(VECTOR.timestamp, VECTOR.body), rebuilt)
	const hex = createHmac('sha256', Buffer.from(VECTOR.key, 'utf8')).update(rebuilt).digest('hex')
	assert.equal(VECTOR.signature, `v1=${hex}`)
})

test('signatureHeaders uses whole unix seconds', () => {
	const headers = signatureHeaders(VECTOR.key, VECTOR.body, 1_760_000_000_999)
	assert.equal(headers['X-Sidecar-Timestamp'], '1760000000')
	assert.equal(headers['X-Sidecar-Signature'], VECTOR.signature)
})
