// Runs a Sidecar with a FakeWhatsApp behind the real HTTP server.
import type { AddressInfo } from 'node:net'
import { createApiServer } from '../../src/http/server.js'
import { Sidecar } from '../../src/sidecar.js'
import type { Transport } from '../../src/webhook/spool.js'
import { FakeWhatsApp } from './fake-whatsapp.js'
import { BEARER, memoryLogger, testConfig } from './helpers.js'

export interface Delivered {
	body: Record<string, unknown>
	raw: string
	headers: Record<string, string>
}

export interface Harness {
	sidecar: Sidecar
	fake: () => FakeWhatsApp
	delivered: Delivered[]
	logLines: string[]
	webhookStatus: { value: number }
	call: (method: string, path: string, options?: { body?: unknown; token?: string | null; rawBody?: string; headers?: Record<string, string> }) => Promise<{ status: number; json: any; headers: Headers; text: string }>
	close: () => Promise<void>
}

export async function startHarness(dataDir: string, env: Record<string, string> = {}): Promise<Harness> {
	const config = testConfig(dataDir, env)
	const { logger, lines } = memoryLogger('debug')
	const delivered: Delivered[] = []
	const webhookStatus = { value: 200 }
	const transport: Transport = async ({ body, headers }) => {
		delivered.push({ body: JSON.parse(body), raw: body, headers })
		return webhookStatus.value
	}
	let fake: FakeWhatsApp | undefined
	const sidecar = new Sidecar(
		config,
		logger,
		(_store, events) => {
			fake = new FakeWhatsApp(events)
			return fake
		},
		transport
	)
	await sidecar.boot()
	const server = createApiServer(sidecar, logger)
	await new Promise<void>(r => server.listen(0, '127.0.0.1', r))
	const base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`

	return {
		sidecar,
		fake: () => {
			if (!fake) throw new Error('not unlocked yet')
			return fake
		},
		delivered,
		logLines: lines,
		webhookStatus,
		call: async (method, path, options = {}) => {
			const headers: Record<string, string> = { ...(options.headers ?? {}) }
			const token = options.token === undefined ? BEARER : options.token
			if (token !== null) headers.authorization = `Bearer ${token}`
			let body: string | undefined
			if (options.rawBody !== undefined) body = options.rawBody
			else if (options.body !== undefined) {
				body = JSON.stringify(options.body)
				headers['content-type'] ??= 'application/json'
			}
			const response = await fetch(base + path, { method, headers, ...(body !== undefined ? { body } : {}) })
			const text = await response.text()
			let json: unknown
			try {
				json = JSON.parse(text)
			} catch {
				json = undefined
			}
			return { status: response.status, json, headers: response.headers, text }
		},
		close: async () => {
			await new Promise<void>(r => server.close(() => r()))
			server.closeAllConnections()
			await sidecar.shutdown()
		}
	}
}
