// Durable outbound event spool and its delivery loop.
//
// Every event is written to the encrypted store before delivery is attempted,
// and removed only after IronClaw answers 2xx. Delivery is strictly in order
// (one event in flight); a failure backs off exponentially, with jitter, up
// to a cap, and retries the same event with the same body and event_id. The
// signature (and its timestamp) is recomputed on every attempt.
import type { EncryptedStore } from '../store/encrypted-store.js'
import { decodeJson, encodeJson } from '../store/auth-state.js'
import { T } from '../store/domain.js'
import { errorKind, type Logger } from '../log.js'
import type { Envelope } from './events.js'
import { signatureHeaders } from './signer.js'

export interface SpoolEntry {
	event_id: string
	type: string
	seq: number
	body: string
	created_at: string
	attempts: number
	next_attempt_at: number
}

/** Sends one request; resolves to the HTTP status, rejects on a network failure. */
export type Transport = (request: {
	url: URL
	body: string
	headers: Record<string, string>
	timeoutMs: number
}) => Promise<number>

export const fetchTransport: Transport = async ({ url, body, headers, timeoutMs }) => {
	const response = await fetch(url, {
		method: 'POST',
		body,
		headers: { 'content-type': 'application/json', 'user-agent': 'ironclaw-whatsapp-sidecar/0.1', ...headers },
		redirect: 'error',
		signal: AbortSignal.timeout(timeoutMs)
	})
	// Drain the body so the connection can be reused; its content is ignored.
	await response.arrayBuffer().catch(() => undefined)
	return response.status
}

export function backoffMs(attempt: number, baseMs: number, maxMs: number, random = Math.random): number {
	const exp = Math.min(maxMs, baseMs * 2 ** Math.max(0, Math.min(30, attempt - 1)))
	return Math.max(1, Math.round(exp * (0.5 + random() * 0.5)))
}

export class Spool {
	private entries: SpoolEntry[] = []
	private lastSeq = 0
	private listeners: Array<() => void> = []

	constructor(
		private readonly store: EncryptedStore,
		private readonly logger: Logger
	) {}

	get size(): number {
		return this.entries.length
	}

	/** Loads persisted entries (after unlock). */
	async load(): Promise<void> {
		const rows = await this.store.list(T.spool, error =>
			this.logger.error('spool record failed to decrypt; left in place', errorKind(error))
		)
		this.entries = rows.map(r => decodeJson<SpoolEntry>(r.value)).sort((a, b) => a.seq - b.seq)
		this.lastSeq = this.entries.at(-1)?.seq ?? 0
		this.notify()
	}

	/** Forgets in-memory entries (on lock). Persisted entries stay. */
	unload(): void {
		this.entries = []
	}

	async enqueue(event: Envelope): Promise<SpoolEntry> {
		const seq = Math.max(Date.now() * 1000, this.lastSeq + 1)
		this.lastSeq = seq
		const entry: SpoolEntry = {
			event_id: event.event_id,
			type: event.type,
			seq,
			body: JSON.stringify(event),
			created_at: event.occurred_at,
			attempts: 0,
			next_attempt_at: 0
		}
		await this.store.putBytes(T.spool, entry.event_id, encodeJson(entry))
		this.entries.push(entry)
		this.notify()
		return entry
	}

	head(): SpoolEntry | undefined {
		return this.entries[0]
	}

	async markFailed(entry: SpoolEntry, nextAttemptAt: number): Promise<void> {
		entry.attempts += 1
		entry.next_attempt_at = nextAttemptAt
		await this.store.putBytes(T.spool, entry.event_id, encodeJson(entry))
	}

	async acknowledge(entry: SpoolEntry): Promise<void> {
		await this.store.delete(T.spool, entry.event_id)
		this.entries = this.entries.filter(e => e.event_id !== entry.event_id)
	}

	/** Resolves when an entry is added or `signal` aborts. */
	waitForEntry(signal: AbortSignal): Promise<void> {
		return new Promise(resolve => {
			const done = () => {
				this.listeners = this.listeners.filter(l => l !== done)
				signal.removeEventListener('abort', done)
				resolve()
			}
			this.listeners.push(done)
			signal.addEventListener('abort', done, { once: true })
		})
	}

	private notify(): void {
		for (const listener of [...this.listeners]) listener()
	}
}

export interface DelivererOptions {
	url: URL
	hmacKey: string
	timeoutMs: number
	retryBaseMs: number
	retryMaxMs: number
	transport?: Transport
	logger: Logger
	random?: () => number
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
	return new Promise(resolve => {
		if (signal.aborted) return resolve()
		const timer = setTimeout(done, ms)
		function done() {
			clearTimeout(timer)
			signal.removeEventListener('abort', done)
			resolve()
		}
		signal.addEventListener('abort', done, { once: true })
	})
}

async function attempt(options: DelivererOptions, body: string): Promise<{ ok: boolean; status?: number; error?: unknown }> {
	const transport = options.transport ?? fetchTransport
	try {
		const status = await transport({
			url: options.url,
			body,
			headers: signatureHeaders(options.hmacKey, body),
			timeoutMs: options.timeoutMs
		})
		return { ok: status >= 200 && status < 300, status }
	} catch (error) {
		return { ok: false, error }
	}
}

/** Drains the spool to IronClaw, in order, until stopped. */
export class Deliverer {
	private controller: AbortController | undefined
	private loop: Promise<void> | undefined

	constructor(
		private readonly spool: Spool,
		private readonly options: DelivererOptions
	) {}

	get running(): boolean {
		return this.controller !== undefined
	}

	start(): void {
		if (this.controller) return
		const controller = new AbortController()
		this.controller = controller
		this.loop = this.run(controller.signal)
	}

	async stop(): Promise<void> {
		this.controller?.abort()
		this.controller = undefined
		await this.loop
		this.loop = undefined
	}

	private async run(signal: AbortSignal): Promise<void> {
		const log = this.options.logger
		while (!signal.aborted) {
			const entry = this.spool.head()
			if (!entry) {
				await this.spool.waitForEntry(signal)
				continue
			}
			const wait = entry.next_attempt_at - Date.now()
			if (wait > 0) {
				await sleep(wait, signal)
				continue
			}
			const result = await attempt(this.options, entry.body)
			if (signal.aborted) break
			try {
				if (result.ok) {
					await this.spool.acknowledge(entry)
					log.info('event delivered', { event_type: entry.type, attempts: entry.attempts + 1, queued: this.spool.size })
				} else {
					const delay = backoffMs(entry.attempts + 1, this.options.retryBaseMs, this.options.retryMaxMs, this.options.random)
					await this.spool.markFailed(entry, Date.now() + delay)
					log.warn('event delivery failed; will retry', {
						event_type: entry.type,
						attempts: entry.attempts,
						retry_in_ms: delay,
						...(result.status !== undefined ? { status: result.status } : errorKind(result.error))
					})
				}
			} catch (error) {
				// The store failed (disk full, locked). Pause briefly and retry.
				log.error('spool update failed', errorKind(error))
				await sleep(this.options.retryBaseMs, signal)
			}
		}
	}
}

/**
 * Announces `sidecar.locked` while the store is locked. This event cannot use
 * the spool (the spool is encrypted), so it is retried from memory until
 * IronClaw acknowledges it or the sidecar is unlocked.
 */
export class LockedAnnouncer {
	private controller: AbortController | undefined

	constructor(private readonly options: DelivererOptions) {}

	start(event: Envelope): void {
		this.stop()
		const controller = new AbortController()
		this.controller = controller
		void this.run(event, controller.signal)
	}

	stop(): void {
		this.controller?.abort()
		this.controller = undefined
	}

	private async run(event: Envelope, signal: AbortSignal): Promise<void> {
		const body = JSON.stringify(event)
		for (let attempts = 1; !signal.aborted; attempts++) {
			const result = await attempt(this.options, body)
			if (signal.aborted) return
			if (result.ok) {
				this.options.logger.info('locked notice delivered', { attempts })
				return
			}
			const delay = backoffMs(attempts, this.options.retryBaseMs, this.options.retryMaxMs, this.options.random)
			this.options.logger.warn('locked notice delivery failed; will retry', {
				attempts,
				retry_in_ms: delay,
				...(result.status !== undefined ? { status: result.status } : errorKind(result.error))
			})
			await sleep(delay, signal)
		}
	}
}
