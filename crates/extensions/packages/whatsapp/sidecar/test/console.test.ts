import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
import { test } from 'node:test'
import { guardConsole } from '../src/console-guard.js'
import { memoryLogger } from './support/helpers.js'

/** libsignal, loaded the way Baileys loads it. */
function libsignal(): { SessionRecord: new () => { closeSession(session: unknown): void } } {
	const fromSidecar = createRequire(import.meta.url)
	return createRequire(fromSidecar.resolve('baileys/package.json'))('libsignal')
}

test('dependency console output never reaches stdout or stderr', t => {
	const { logger, lines } = memoryLogger('trace')
	const restore = guardConsole(logger)
	t.after(restore)
	const written: string[] = []
	const out = process.stdout.write.bind(process.stdout)
	const err = process.stderr.write.bind(process.stderr)
	process.stdout.write = ((chunk: unknown) => (written.push(String(chunk)), true)) as typeof process.stdout.write
	process.stderr.write = ((chunk: unknown) => (written.push(String(chunk)), true)) as typeof process.stderr.write
	try {
		// libsignal prints a whole session, private keys included, when it closes one.
		const session = {
			registrationId: 1,
			currentRatchet: {
				rootKey: Buffer.alloc(32, 0xab),
				ephemeralKeyPair: { privKey: Buffer.alloc(32, 0xcd), pubKey: Buffer.alloc(33, 1) }
			},
			indexInfo: { baseKey: Buffer.alloc(33, 2), closed: -1, remoteIdentityKey: Buffer.alloc(33, 3) },
			_chains: {}
		}
		new (libsignal().SessionRecord)().closeSession(session)
		console.error(new Error('decrypt failed for 447700900123@s.whatsapp.net'))
	} finally {
		process.stdout.write = out
		process.stderr.write = err
	}
	assert.deepEqual(written, [])
	assert.ok(lines.length > 0, 'the suppressed calls are noted at trace')
	for (const line of lines) {
		assert.doesNotMatch(line, /cdcd|abab|privKey|447700900123/)
	}
})

test('restoring puts the original console back', () => {
	const original = console.info
	const restore = guardConsole(memoryLogger().logger)
	assert.notEqual(console.info, original)
	restore()
	assert.equal(console.info, original)
})
