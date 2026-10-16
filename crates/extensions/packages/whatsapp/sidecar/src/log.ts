// Structured JSON-lines logging.
//
// Rule: message text, phone numbers, JIDs and keys are never logged above
// `debug`. Callers pass only non-identifying fields (counts, codes, kinds,
// opaque internal ids) at `info` and above. Everything Baileys logs is routed
// to `debug` or below, because its log objects routinely contain JIDs.
// Dependencies that print straight to the console bypass this module;
// console-guard.ts silences them.
import type { SocketConfig } from 'baileys'

type ILogger = SocketConfig['logger']

export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error' | 'silent'

const ORDER: Record<LogLevel, number> = { trace: 10, debug: 20, info: 30, warn: 40, error: 50, silent: 100 }

export function isLogLevel(value: string): value is LogLevel {
	return value in ORDER
}

export type LogFields = Record<string, unknown>

export type LogSink = (line: string) => void

export class Logger {
	constructor(
		readonly level: LogLevel,
		private readonly sink: LogSink = line => process.stdout.write(line + '\n'),
		private readonly bindings: LogFields = {}
	) {}

	enabled(level: LogLevel): boolean {
		return ORDER[level] >= ORDER[this.level] && level !== 'silent'
	}

	child(bindings: LogFields): Logger {
		return new Logger(this.level, this.sink, { ...this.bindings, ...bindings })
	}

	trace(msg: string, fields?: LogFields): void {
		this.write('trace', msg, fields)
	}

	debug(msg: string, fields?: LogFields): void {
		this.write('debug', msg, fields)
	}

	info(msg: string, fields?: LogFields): void {
		this.write('info', msg, fields)
	}

	warn(msg: string, fields?: LogFields): void {
		this.write('warn', msg, fields)
	}

	error(msg: string, fields?: LogFields): void {
		this.write('error', msg, fields)
	}

	private write(level: LogLevel, msg: string, fields?: LogFields): void {
		if (!this.enabled(level)) return
		let line: string
		try {
			line = JSON.stringify({ time: new Date().toISOString(), level, msg, ...this.bindings, ...fields }, jsonSafe)
		} catch {
			line = JSON.stringify({ time: new Date().toISOString(), level, msg, log_error: 'unserialisable fields' })
		}
		this.sink(line)
	}
}

function jsonSafe(_key: string, value: unknown): unknown {
	if (typeof value === 'bigint') return value.toString()
	if (value instanceof Error) return { name: value.name, message: value.message }
	if (Buffer.isBuffer(value) || value instanceof Uint8Array) return `<${value.length} bytes>`
	return value
}

/** The short error description that is safe at info level: a class name and a code, never a message. */
export function errorKind(error: unknown): LogFields {
	if (error instanceof Error) {
		const code = (error as { code?: unknown }).code
		const status = (error as { output?: { statusCode?: unknown } }).output?.statusCode
		const cause = (error as { cause?: { code?: unknown } }).cause?.code
		return {
			error: error.name,
			...(typeof code === 'string' || typeof code === 'number' ? { code } : {}),
			...(typeof status === 'number' ? { status } : {}),
			...(typeof cause === 'string' ? { cause } : {})
		}
	}
	return { error: typeof error }
}

/**
 * Adapts {@link Logger} to Baileys' ILogger. Baileys' `info`/`warn`/`error`
 * output goes to our `debug`, its `debug`/`trace` to our `trace`.
 */
export function baileysLogger(logger: Logger): ILogger {
	const target = logger.child({ component: 'baileys' })
	const emit = (level: 'debug' | 'trace', obj: unknown, msg?: string) => {
		if (!target.enabled(level)) return
		if (typeof obj === 'string') {
			target[level](obj)
		} else {
			target[level](msg ?? 'baileys', { detail: obj })
		}
	}
	const level = logger.enabled('trace') ? 'trace' : logger.enabled('debug') ? 'debug' : 'silent'
	const adapter: ILogger = {
		level,
		child: bindings => baileysLogger(logger.child({ baileys: bindings })),
		trace: (obj, msg) => emit('trace', obj, msg),
		debug: (obj, msg) => emit('trace', obj, msg),
		info: (obj, msg) => emit('debug', obj, msg),
		warn: (obj, msg) => emit('debug', obj, msg),
		error: (obj, msg) => emit('debug', obj, msg)
	}
	return adapter
}
