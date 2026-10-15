// Keeps dependencies' console output out of the process's stdout and stderr.
//
// libsignal, which Baileys uses for sessions, prints straight to the console:
// `console.info("Closing session:", session)` dumps a session's root key and
// private keys, and its error paths print stacks. That would bypass log.ts and
// put key material in container logs whatever SIDECAR_LOG_LEVEL is. Every
// console output method is replaced with one that notes the call at `trace`,
// naming only the method: the arguments are dropped, never formatted.
import type { Logger } from './log.js'

const METHODS = ['log', 'info', 'warn', 'error', 'debug', 'trace', 'dir', 'dirxml', 'table'] as const

/** Installs the guard; returns a function that puts the original console back. */
export function guardConsole(logger: Logger): () => void {
	const target = console as unknown as Record<string, unknown>
	const saved = METHODS.map(method => [method, target[method]] as const)
	for (const method of METHODS) {
		target[method] = () => logger.trace('dependency console output suppressed', { method })
	}
	return () => {
		for (const [method, original] of saved) target[method] = original
	}
}
