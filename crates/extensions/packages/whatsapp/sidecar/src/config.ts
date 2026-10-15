// Configuration from environment variables. Startup fails on any problem.
import { isLogLevel, type LogLevel } from './log.js'

export const MIN_SECRET_LENGTH = 32

export interface Limits {
	/** Largest accepted JSON request body. */
	maxBodyBytes: number
	/** Largest media object GET /v1/media will stream. */
	mediaMaxBytes: number
	sendPerChatPerMinute: number
	sendPerChatBurst: number
	sendGlobalPerMinute: number
	sendGlobalBurst: number
	/** How long POST /v1/send waits for WhatsApp's server ack. */
	sendAckTimeoutMs: number
}

export interface Config {
	bearerToken: string
	webhookHmacKey: string
	webhookUrl: URL
	dataDir: string
	listenHost: string
	listenPort: number
	logLevel: LogLevel
	limits: Limits
	webhookTimeoutMs: number
	webhookRetryBaseMs: number
	webhookRetryMaxMs: number
	reconnectBaseMs: number
	reconnectMaxMs: number
	/** How long a link flow (QR or pairing code) may stay open. */
	linkFlowTtlMs: number
	/** Days to keep message/media references used for quoting and media download. */
	messageRetentionDays: number
}

export class ConfigError extends Error {
	constructor(readonly problems: string[]) {
		super(`invalid configuration:\n  - ${problems.join('\n  - ')}`)
		this.name = 'ConfigError'
	}
}

type Env = Record<string, string | undefined>

function intVar(env: Env, name: string, fallback: number, min: number, max: number, problems: string[]): number {
	const raw = env[name]
	if (raw === undefined || raw === '') return fallback
	if (!/^\d+$/.test(raw)) {
		problems.push(`${name} must be a whole number`)
		return fallback
	}
	const value = Number(raw)
	if (value < min || value > max) {
		problems.push(`${name} must be between ${min} and ${max}`)
		return fallback
	}
	return value
}

function secretVar(env: Env, name: string, problems: string[]): string {
	const value = env[name]
	if (value === undefined || value === '') {
		problems.push(`${name} is required`)
		return ''
	}
	if (value.length < MIN_SECRET_LENGTH) {
		problems.push(`${name} must be at least ${MIN_SECRET_LENGTH} characters`)
	}
	if (/\s/.test(value)) {
		problems.push(`${name} must not contain whitespace`)
	}
	return value
}

/** Parses `host:port`, `[v6]:port` or `:port`. */
export function parseListen(raw: string): { host: string; port: number } | undefined {
	const m = /^(?:\[([0-9a-fA-F:.]+)\]|([^:[\]]*)):(\d{1,5})$/.exec(raw)
	if (!m) return undefined
	const port = Number(m[3])
	if (port < 1 || port > 65535) return undefined
	return { host: m[1] ?? (m[2] || '0.0.0.0'), port }
}

export function loadConfig(env: Env = process.env): Config {
	const problems: string[] = []
	const bearerToken = secretVar(env, 'SIDECAR_BEARER_TOKEN', problems)
	const webhookHmacKey = secretVar(env, 'SIDECAR_WEBHOOK_HMAC_KEY', problems)
	if (bearerToken && bearerToken === webhookHmacKey) {
		problems.push('SIDECAR_BEARER_TOKEN and SIDECAR_WEBHOOK_HMAC_KEY must differ')
	}

	let webhookUrl = new URL('http://invalid.invalid/')
	const rawUrl = env.IRONCLAW_WEBHOOK_URL
	if (!rawUrl) {
		problems.push('IRONCLAW_WEBHOOK_URL is required')
	} else {
		try {
			webhookUrl = new URL(rawUrl)
			if (webhookUrl.protocol !== 'https:' && webhookUrl.protocol !== 'http:') {
				problems.push('IRONCLAW_WEBHOOK_URL must be an http or https URL')
			}
			if (webhookUrl.username || webhookUrl.password) {
				problems.push('IRONCLAW_WEBHOOK_URL must not contain credentials')
			}
			if (webhookUrl.hash) {
				problems.push('IRONCLAW_WEBHOOK_URL must not contain a fragment')
			}
		} catch {
			problems.push('IRONCLAW_WEBHOOK_URL is not a valid URL')
		}
	}

	const listenRaw = env.SIDECAR_LISTEN || '0.0.0.0:8080'
	const listen = parseListen(listenRaw)
	if (!listen) problems.push('SIDECAR_LISTEN must look like host:port')

	const logLevelRaw = env.SIDECAR_LOG_LEVEL || 'info'
	if (!isLogLevel(logLevelRaw)) problems.push('SIDECAR_LOG_LEVEL must be trace, debug, info, warn, error or silent')

	const dataDir = env.SIDECAR_DATA_DIR || '/data'
	if (!dataDir.startsWith('/')) problems.push('SIDECAR_DATA_DIR must be an absolute path')

	const limits: Limits = {
		maxBodyBytes: intVar(env, 'SIDECAR_MAX_BODY_BYTES', 512 * 1024, 1024, 4 * 1024 * 1024, problems),
		mediaMaxBytes: intVar(env, 'SIDECAR_MEDIA_MAX_BYTES', 64 * 1024 * 1024, 1024, 2 * 1024 * 1024 * 1024, problems),
		sendPerChatPerMinute: intVar(env, 'SIDECAR_SEND_PER_CHAT_PER_MINUTE', 6, 1, 600, problems),
		sendPerChatBurst: intVar(env, 'SIDECAR_SEND_PER_CHAT_BURST', 3, 1, 100, problems),
		sendGlobalPerMinute: intVar(env, 'SIDECAR_SEND_GLOBAL_PER_MINUTE', 20, 1, 6000, problems),
		sendGlobalBurst: intVar(env, 'SIDECAR_SEND_GLOBAL_BURST', 5, 1, 1000, problems),
		sendAckTimeoutMs: intVar(env, 'SIDECAR_SEND_ACK_TIMEOUT_MS', 30_000, 1000, 300_000, problems)
	}

	const config: Config = {
		bearerToken,
		webhookHmacKey,
		webhookUrl,
		dataDir,
		listenHost: listen?.host ?? '0.0.0.0',
		listenPort: listen?.port ?? 8080,
		logLevel: isLogLevel(logLevelRaw) ? logLevelRaw : 'info',
		limits,
		webhookTimeoutMs: intVar(env, 'SIDECAR_WEBHOOK_TIMEOUT_MS', 10_000, 1000, 120_000, problems),
		webhookRetryBaseMs: intVar(env, 'SIDECAR_WEBHOOK_RETRY_BASE_MS', 1000, 10, 600_000, problems),
		webhookRetryMaxMs: intVar(env, 'SIDECAR_WEBHOOK_RETRY_MAX_MS', 300_000, 10, 3_600_000, problems),
		reconnectBaseMs: intVar(env, 'SIDECAR_RECONNECT_BASE_MS', 1000, 10, 600_000, problems),
		reconnectMaxMs: intVar(env, 'SIDECAR_RECONNECT_MAX_MS', 120_000, 10, 3_600_000, problems),
		linkFlowTtlMs: intVar(env, 'SIDECAR_LINK_FLOW_TTL_SECS', 300, 30, 3600, problems) * 1000,
		messageRetentionDays: intVar(env, 'SIDECAR_MESSAGE_RETENTION_DAYS', 30, 1, 3650, problems)
	}
	if (config.webhookRetryBaseMs > config.webhookRetryMaxMs) {
		problems.push('SIDECAR_WEBHOOK_RETRY_BASE_MS must not exceed SIDECAR_WEBHOOK_RETRY_MAX_MS')
	}
	if (config.reconnectBaseMs > config.reconnectMaxMs) {
		problems.push('SIDECAR_RECONNECT_BASE_MS must not exceed SIDECAR_RECONNECT_MAX_MS')
	}
	if (problems.length) throw new ConfigError(problems)
	return config
}
