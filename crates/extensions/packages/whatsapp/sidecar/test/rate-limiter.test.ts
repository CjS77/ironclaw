import assert from 'node:assert/strict'
import { test } from 'node:test'
import { SendRateLimiter } from '../src/outbound/rate-limiter.js'

test('per-chat burst, then refill at the per-minute rate', () => {
	let now = 0
	const limiter = new SendRateLimiter({ perMinute: 6, burst: 2 }, { perMinute: 100, burst: 100 }, () => now)
	assert.deepEqual(limiter.tryAcquire('a'), { ok: true })
	assert.deepEqual(limiter.tryAcquire('a'), { ok: true })
	const denied = limiter.tryAcquire('a')
	assert.equal(denied.ok, false)
	assert.equal(!denied.ok && denied.retryAfterMs, 10_000)
	// another chat is independent
	assert.deepEqual(limiter.tryAcquire('b'), { ok: true })
	now += 10_000
	assert.deepEqual(limiter.tryAcquire('a'), { ok: true })
	assert.equal(limiter.tryAcquire('a').ok, false)
})

test('the global bucket limits all chats together', () => {
	let now = 0
	const limiter = new SendRateLimiter({ perMinute: 60, burst: 10 }, { perMinute: 2, burst: 2 }, () => now)
	assert.equal(limiter.tryAcquire('a').ok, true)
	assert.equal(limiter.tryAcquire('b').ok, true)
	const denied = limiter.tryAcquire('c')
	assert.equal(denied.ok, false)
	assert.equal(!denied.ok && denied.retryAfterMs, 30_000)
	now += 30_000
	assert.equal(limiter.tryAcquire('c').ok, true)
})

test('a denied send consumes no tokens', () => {
	let now = 0
	const limiter = new SendRateLimiter({ perMinute: 60, burst: 1 }, { perMinute: 60, burst: 1 }, () => now)
	assert.equal(limiter.tryAcquire('a').ok, true)
	assert.equal(limiter.tryAcquire('b').ok, false) // global empty; b's own bucket untouched
	now += 1000
	assert.equal(limiter.tryAcquire('b').ok, true)
})
