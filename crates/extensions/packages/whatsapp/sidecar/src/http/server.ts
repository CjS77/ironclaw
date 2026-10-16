// HTTP API (see API.md). node:http only.
//
// Order of checks for every /v1 route: route match -> bearer token (401) ->
// lock state (423, except /v1/unlock) -> body (415/413/400) -> operation.
// Error bodies are {"error": "<code>", ...}; they never carry stack traces,
// file paths, message text or phone numbers.
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http'
import { Transform, type Readable } from 'node:stream'
import { pipeline } from 'node:stream/promises'
import { constantTimeEqual } from '../crypto.js'
import { errorKind, type Logger } from '../log.js'
import { ApiError } from '../errors.js'
import type { Sidecar } from '../sidecar.js'
import { LIMITS } from '../webhook/events.js'
import {
	readJson,
	requireEnum,
	optionalString,
	requireString,
	rejectUnknownFields
} from './validate.js'

type Handler = (ctx: RequestContext) => Promise<void>

interface Route {
	method: 'GET' | 'POST'
	pattern: RegExp
	name: string
	auth: boolean
	/** Allowed while the store is locked. */
	whileLocked: boolean
	handler: Handler
}

interface RequestContext {
	req: IncomingMessage
	res: ServerResponse
	params: string[]
	sidecar: Sidecar
	maxBodyBytes: number
}

const ID_PATTERN = /^[A-Za-z0-9._:@+-]{1,128}$/
const IDEMPOTENCY_PATTERN = /^[A-Za-z0-9._:-]{1,128}$/
const MIME_PATTERN = /^[A-Za-z0-9!#$&^_.+-]{1,63}\/[A-Za-z0-9!#$&^_.+-]{1,63}(?:\s*;\s*[A-Za-z0-9!#$&^_.+-]+=[A-Za-z0-9!#$&^_.+-]+)*$/

function send(res: ServerResponse, status: number, body: unknown, headers: Record<string, string> = {}): void {
	const data = Buffer.from(JSON.stringify(body))
	res.writeHead(status, {
		'content-type': 'application/json; charset=utf-8',
		'content-length': String(data.length),
		'cache-control': 'no-store',
		'x-content-type-options': 'nosniff',
		...headers
	})
	res.end(data)
}

function decodeParam(raw: string): string {
	let value: string
	try {
		value = decodeURIComponent(raw)
	} catch {
		throw new ApiError(400, 'invalid_request', 'malformed path parameter')
	}
	if (!ID_PATTERN.test(value)) throw new ApiError(400, 'invalid_request', 'malformed path parameter')
	return value
}

function validPhoneNumber(raw: string): string {
	const digits = raw.replace(/^\+/, '')
	if (!/^\d{7,15}$/.test(digits)) {
		throw new ApiError(400, 'invalid_request', 'phone_number must be 7 to 15 digits, optionally after a +', {
			field: 'phone_number'
		})
	}
	return digits
}

const routes: Route[] = [
	{
		method: 'POST',
		pattern: /^\/v1\/unlock$/,
		name: 'unlock',
		auth: true,
		whileLocked: true,
		handler: async ({ req, res, sidecar, maxBodyBytes }) => {
			const body = await readJson(req, maxBodyBytes, { wipe: true })
			rejectUnknownFields(body, ['data_key'])
			const encoded = requireString(body, 'data_key', 64)
			const key = /^[A-Za-z0-9+/]{43}=$/.test(encoded) ? Buffer.from(encoded, 'base64') : undefined
			if (!key || key.length !== 32) {
				throw new ApiError(400, 'invalid_request', 'data_key must be 32 bytes, base64 encoded', { field: 'data_key' })
			}
			try {
				const result = await sidecar.unlock(key)
				send(res, 200, { state: 'unlocked', initialized: result.initialized })
			} finally {
				key.fill(0)
			}
		}
	},
	{
		method: 'POST',
		pattern: /^\/v1\/send$/,
		name: 'send',
		auth: true,
		whileLocked: false,
		handler: async ({ req, res, sidecar, maxBodyBytes }) => {
			const body = await readJson(req, maxBodyBytes)
			rejectUnknownFields(body, ['chat_id', 'text', 'quoted_message_id', 'idempotency_key'])
			const chatId = requireString(body, 'chat_id', LIMITS.idChars)
			const text = requireString(body, 'text', LIMITS.textChars)
			if (!text.trim()) throw new ApiError(400, 'invalid_request', 'text must not be blank', { field: 'text' })
			const quoted = optionalString(body, 'quoted_message_id', LIMITS.idChars)
			const idempotencyKey = requireString(body, 'idempotency_key', 128)
			if (!IDEMPOTENCY_PATTERN.test(idempotencyKey)) {
				throw new ApiError(400, 'invalid_request', 'idempotency_key has invalid characters', { field: 'idempotency_key' })
			}
			const result = await sidecar.send({
				chat_id: chatId,
				text,
				idempotency_key: idempotencyKey,
				...(quoted ? { quoted_message_id: quoted } : {})
			})
			send(res, 200, result)
		}
	},
	{
		method: 'POST',
		pattern: /^\/v1\/contacts\/([^/]+)\/(block|unblock)$/,
		name: 'contact-block',
		auth: true,
		whileLocked: false,
		handler: async ({ req, res, params, sidecar, maxBodyBytes }) => {
			rejectUnknownFields(await readJson(req, maxBodyBytes, { allowEmpty: true }), [])
			const contactId = decodeParam(params[0] ?? '')
			send(res, 200, await sidecar.setBlocked(contactId, params[1] === 'block'))
		}
	},
	{
		method: 'GET',
		pattern: /^\/v1\/media\/([^/]+)$/,
		name: 'media',
		auth: true,
		whileLocked: false,
		handler: async ({ res, params, sidecar }) => {
			const mediaId = decodeParam(params[0] ?? '')
			const { record, stream } = await sidecar.media(mediaId)
			await streamMedia(res, stream, record.mime_type, sidecar.config.limits.mediaMaxBytes)
		}
	},
	{
		method: 'POST',
		pattern: /^\/v1\/link\/begin$/,
		name: 'link-begin',
		auth: true,
		whileLocked: false,
		handler: async ({ req, res, sidecar, maxBodyBytes }) => {
			const body = await readJson(req, maxBodyBytes)
			rejectUnknownFields(body, ['mode', 'phone_number'])
			const mode = requireEnum(body, 'mode', ['qr', 'pairing_code'] as const)
			const phoneRaw = optionalString(body, 'phone_number', 20)
			if (mode === 'pairing_code' && !phoneRaw) {
				throw new ApiError(400, 'invalid_request', 'phone_number is required for pairing_code', { field: 'phone_number' })
			}
			const phone = phoneRaw ? validPhoneNumber(phoneRaw) : undefined
			send(res, 200, await sidecar.linkBegin(mode, mode === 'pairing_code' ? phone : undefined))
		}
	},
	{
		method: 'GET',
		pattern: /^\/v1\/link\/status$/,
		name: 'link-status',
		auth: true,
		whileLocked: false,
		handler: async ({ res, sidecar }) => send(res, 200, await sidecar.linkStatus())
	},
	{
		method: 'POST',
		pattern: /^\/v1\/link\/cancel$/,
		name: 'link-cancel',
		auth: true,
		whileLocked: false,
		handler: async ({ req, res, sidecar, maxBodyBytes }) => {
			rejectUnknownFields(await readJson(req, maxBodyBytes, { allowEmpty: true }), [])
			send(res, 200, await sidecar.linkCancel())
		}
	},
	{
		method: 'POST',
		pattern: /^\/v1\/unlink$/,
		name: 'unlink',
		auth: true,
		whileLocked: false,
		handler: async ({ req, res, sidecar, maxBodyBytes }) => {
			rejectUnknownFields(await readJson(req, maxBodyBytes, { allowEmpty: true }), [])
			send(res, 200, await sidecar.unlink())
		}
	},
	{
		method: 'GET',
		pattern: /^\/healthz$/,
		name: 'healthz',
		auth: false,
		whileLocked: true,
		handler: async ({ res, sidecar }) => send(res, 200, { status: 'ok', locked: sidecar.locked })
	}
]

async function streamMedia(res: ServerResponse, stream: Readable, mime: string, maxBytes: number): Promise<void> {
	let seen = 0
	const cap = new Transform({
		transform(chunk: Buffer, _enc, done) {
			seen += chunk.length
			if (seen > maxBytes) done(new ApiError(413, 'media_too_large'))
			else done(null, chunk)
		}
	})
	res.writeHead(200, {
		'content-type': MIME_PATTERN.test(mime) ? mime : 'application/octet-stream',
		'content-disposition': 'attachment',
		'cache-control': 'no-store',
		'x-content-type-options': 'nosniff'
	})
	// Headers are sent; a failure from here on can only abort the response,
	// which the client sees as a truncated (unterminated) chunked body.
	await pipeline(stream, cap, res)
}

function bearerOk(req: IncomingMessage, token: string): boolean {
	const header = req.headers.authorization
	if (typeof header !== 'string') return false
	const m = /^Bearer ([^\s]+)$/.exec(header)
	return m !== null && constantTimeEqual(m[1] ?? '', token)
}

export function createApiServer(sidecar: Sidecar, logger: Logger): Server {
	const log = logger.child({ component: 'http' })
	const { bearerToken, limits } = sidecar.config
	const server = createServer(async (req, res) => {
		const started = Date.now()
		let routeName = 'unmatched'
		res.once('close', () => {
			const fields = { route: routeName, method: req.method, status: res.statusCode, ms: Date.now() - started }
			if (routeName === 'healthz') log.debug('request', fields)
			else log.info('request', fields)
		})
		try {
			const path = (req.url ?? '/').split('?')[0] ?? '/'
			const candidates = routes.filter(r => r.pattern.test(path))
			if (candidates.length === 0) throw new ApiError(404, 'not_found')
			const route = candidates.find(r => r.method === req.method)
			if (!route) {
				throw new ApiError(405, 'method_not_allowed', undefined, {}, { allow: [...new Set(candidates.map(r => r.method))].join(', ') })
			}
			routeName = route.name
			if (route.auth && !bearerOk(req, bearerToken)) {
				throw new ApiError(401, 'unauthorized', undefined, {}, { 'www-authenticate': 'Bearer' })
			}
			if (!route.whileLocked && sidecar.locked) throw new ApiError(423, 'locked')
			const params = route.pattern.exec(path)?.slice(1) ?? []
			await route.handler({ req, res, params, sidecar, maxBodyBytes: limits.maxBodyBytes })
		} catch (error) {
			if (res.headersSent) {
				log.warn('response aborted after headers were sent', { route: routeName, ...errorKind(error) })
				res.destroy()
			} else if (error instanceof ApiError) {
				send(res, error.status, { error: error.code, ...(error.message !== error.code ? { message: error.message } : {}), ...error.extra }, error.headers)
			} else {
				log.error('unhandled error', { route: routeName, ...errorKind(error) })
				send(res, 500, { error: 'internal_error' })
			}
		}
	})
	server.headersTimeout = 10_000
	server.requestTimeout = 60_000
	server.keepAliveTimeout = 5_000
	server.maxHeadersCount = 50
	return server
}

