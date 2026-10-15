// Shared test helpers.
import { randomBytes } from 'node:crypto'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { loadConfig, type Config } from '../../src/config.js'
import { Logger } from '../../src/log.js'

export const BEARER = 'test-bearer-token-0123456789abcdefghijklmnop'
export const HMAC_KEY = 'test-hmac-key-0123456789abcdefghijklmnopqrs'

export async function tempDir(t: { after: (fn: () => Promise<void>) => void }): Promise<string> {
	const dir = await mkdtemp(join(tmpdir(), 'wa-sidecar-test-'))
	t.after(() => rm(dir, { recursive: true, force: true }))
	return dir
}

export function dataKey(): Buffer {
	return randomBytes(32)
}

export function testConfig(dataDir: string, overrides: Record<string, string> = {}): Config {
	return loadConfig({
		SIDECAR_BEARER_TOKEN: BEARER,
		SIDECAR_WEBHOOK_HMAC_KEY: HMAC_KEY,
		IRONCLAW_WEBHOOK_URL: 'http://ironclaw.test/webhooks/extensions/whatsapp/events',
		SIDECAR_DATA_DIR: dataDir,
		SIDECAR_LISTEN: '127.0.0.1:18080',
		SIDECAR_WEBHOOK_RETRY_BASE_MS: '10',
		SIDECAR_WEBHOOK_RETRY_MAX_MS: '40',
		...overrides
	})
}

/** A logger that records lines (for asserting what reaches the log). */
export function memoryLogger(level: 'trace' | 'debug' | 'info' = 'info'): { logger: Logger; lines: string[] } {
	const lines: string[] = []
	return { logger: new Logger(level, line => lines.push(line)), lines }
}

export async function eventually(check: () => boolean | Promise<boolean>, timeoutMs = 3000): Promise<void> {
	const deadline = Date.now() + timeoutMs
	while (Date.now() < deadline) {
		if (await check()) return
		await new Promise(r => setTimeout(r, 5))
	}
	throw new Error('condition not met in time')
}
