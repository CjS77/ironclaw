// Request body parsing and field validation.
import type { IncomingMessage } from 'node:http'
import { ApiError } from '../errors.js'

export type JsonObject = Record<string, unknown>

/**
 * Reads a JSON object body of at most `maxBytes`. With `allowEmpty`, an
 * empty body is accepted as `{}` (for POST routes that take no input). With
 * `wipe`, the request bytes are zeroed once parsed (for bodies that carry a
 * key; the parsed strings stay in memory until garbage collection).
 */
export async function readJson(
	req: IncomingMessage,
	maxBytes: number,
	options: { allowEmpty?: boolean; wipe?: boolean } = {}
): Promise<JsonObject> {
	const declared = Number(req.headers['content-length'])
	if (Number.isFinite(declared) && declared > maxBytes) {
		req.resume()
		throw new ApiError(413, 'body_too_large', undefined, { max_bytes: maxBytes })
	}
	const chunks: Buffer[] = []
	let size = 0
	for await (const chunk of req) {
		size += (chunk as Buffer).length
		if (size > maxBytes) {
			req.resume()
			throw new ApiError(413, 'body_too_large', undefined, { max_bytes: maxBytes })
		}
		chunks.push(chunk as Buffer)
	}
	if (size === 0) {
		if (options.allowEmpty) return {}
		throw new ApiError(400, 'invalid_request', 'a JSON body is required')
	}
	const type = req.headers['content-type'] ?? ''
	if (!/^application\/json\s*(;|$)/i.test(type)) {
		throw new ApiError(415, 'unsupported_media_type', 'content-type must be application/json')
	}
	const bytes = Buffer.concat(chunks)
	let value: unknown
	try {
		value = JSON.parse(bytes.toString('utf8'))
	} catch {
		throw new ApiError(400, 'invalid_json')
	} finally {
		if (options.wipe) {
			bytes.fill(0)
			for (const chunk of chunks) chunk.fill(0)
		}
	}
	if (typeof value !== 'object' || value === null || Array.isArray(value)) {
		throw new ApiError(400, 'invalid_request', 'the body must be a JSON object')
	}
	return value as JsonObject
}

export function rejectUnknownFields(body: JsonObject, allowed: readonly string[]): void {
	for (const field of Object.keys(body)) {
		if (!allowed.includes(field)) throw new ApiError(400, 'invalid_request', 'unknown field', { field })
	}
}

export function optionalString(body: JsonObject, field: string, maxChars: number): string | undefined {
	const value = body[field]
	if (value === undefined || value === null) return undefined
	if (typeof value !== 'string' || value.length === 0) {
		throw new ApiError(400, 'invalid_request', 'expected a non-empty string', { field })
	}
	if (value.length > maxChars) throw new ApiError(400, 'invalid_request', `longer than ${maxChars} characters`, { field })
	return value
}

export function requireString(body: JsonObject, field: string, maxChars: number): string {
	const value = optionalString(body, field, maxChars)
	if (value === undefined) throw new ApiError(400, 'invalid_request', 'required', { field })
	return value
}

export function requireEnum<T extends string>(body: JsonObject, field: string, values: readonly T[]): T {
	const value = body[field]
	if (typeof value !== 'string' || !values.includes(value as T)) {
		throw new ApiError(400, 'invalid_request', `must be one of ${values.join(', ')}`, { field })
	}
	return value as T
}
