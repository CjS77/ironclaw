import assert from 'node:assert/strict'
import { test } from 'node:test'
import { ConfigError, loadConfig, parseListen } from '../src/config.js'
import { BEARER, HMAC_KEY } from './support/helpers.js'

const base = {
	SIDECAR_BEARER_TOKEN: BEARER,
	SIDECAR_WEBHOOK_HMAC_KEY: HMAC_KEY,
	IRONCLAW_WEBHOOK_URL: 'https://ironclaw.example/webhooks/extensions/whatsapp/events'
}

test('defaults', () => {
	const c = loadConfig(base)
	assert.equal(c.dataDir, '/data')
	assert.equal(c.listenHost, '0.0.0.0')
	assert.equal(c.listenPort, 8080)
	assert.equal(c.logLevel, 'info')
	assert.equal(c.limits.sendPerChatBurst, 3)
})

test('startup fails when secrets are missing, short, equal, or the URL is bad', () => {
	const cases: Array<[Record<string, string | undefined>, RegExp]> = [
		[{ ...base, SIDECAR_BEARER_TOKEN: undefined }, /SIDECAR_BEARER_TOKEN is required/],
		[{ ...base, SIDECAR_WEBHOOK_HMAC_KEY: 'short' }, /SIDECAR_WEBHOOK_HMAC_KEY must be at least 32/],
		[{ ...base, SIDECAR_WEBHOOK_HMAC_KEY: BEARER }, /must differ/],
		[{ ...base, IRONCLAW_WEBHOOK_URL: undefined }, /IRONCLAW_WEBHOOK_URL is required/],
		[{ ...base, IRONCLAW_WEBHOOK_URL: 'ftp://x/y' }, /http or https/],
		[{ ...base, IRONCLAW_WEBHOOK_URL: 'https://u:p@x/y' }, /credentials/],
		[{ ...base, SIDECAR_SEND_GLOBAL_BURST: '0' }, /SIDECAR_SEND_GLOBAL_BURST/],
		[{ ...base, SIDECAR_DATA_DIR: 'relative' }, /absolute/],
		[{ ...base, SIDECAR_LISTEN: 'nonsense' }, /SIDECAR_LISTEN/]
	]
	for (const [env, pattern] of cases) {
		assert.throws(() => loadConfig(env), (e: unknown) => e instanceof ConfigError && pattern.test(e.message))
	}
})

test('error messages never echo secret values', () => {
	try {
		loadConfig({ ...base, SIDECAR_BEARER_TOKEN: 'tooshort-but-secret' })
		assert.fail('expected a ConfigError')
	} catch (e) {
		assert.ok(!(e as Error).message.includes('tooshort-but-secret'))
	}
})

test('parseListen', () => {
	assert.deepEqual(parseListen('0.0.0.0:8080'), { host: '0.0.0.0', port: 8080 })
	assert.deepEqual(parseListen(':9000'), { host: '0.0.0.0', port: 9000 })
	assert.deepEqual(parseListen('[::1]:80'), { host: '::1', port: 80 })
	assert.equal(parseListen('host:99999'), undefined)
})
