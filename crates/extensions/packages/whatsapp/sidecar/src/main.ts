// Entry point: load configuration, start locked, serve the API.
import { BaileysConnection } from './connection/baileys-connection.js'
import { ConfigError, loadConfig } from './config.js'
import { guardConsole } from './console-guard.js'
import { createApiServer } from './http/server.js'
import { errorKind, Logger } from './log.js'
import { Sidecar } from './sidecar.js'

async function main(): Promise<void> {
	let config
	try {
		config = loadConfig()
	} catch (error) {
		if (error instanceof ConfigError) {
			process.stderr.write(`whatsapp-sidecar: ${error.message}\n`)
			process.exit(2)
		}
		throw error
	}
	const logger = new Logger(config.logLevel)
	// Before anything reaches Baileys: libsignal prints session keys to the console.
	guardConsole(logger)
	const sidecar = new Sidecar(config, logger, (store, events, wipeAuth) =>
		new BaileysConnection({
			store,
			events,
			logger,
			wipeAuth,
			reconnectBaseMs: config.reconnectBaseMs,
			reconnectMaxMs: config.reconnectMaxMs,
			linkFlowTtlMs: config.linkFlowTtlMs
		})
	)
	await sidecar.boot()
	const server = createApiServer(sidecar, logger)
	server.listen(config.listenPort, config.listenHost, () => {
		logger.info('listening; store is locked until IronClaw unlocks it', { port: config.listenPort })
	})

	let stopping = false
	const stop = (signal: string) => {
		if (stopping) return
		stopping = true
		logger.info('shutting down', { signal })
		server.close()
		sidecar
			.shutdown()
			.catch(error => logger.error('shutdown failed', errorKind(error)))
			.finally(() => process.exit(0))
		setTimeout(() => process.exit(1), 10_000).unref()
	}
	process.on('SIGTERM', () => stop('SIGTERM'))
	process.on('SIGINT', () => stop('SIGINT'))
	process.on('unhandledRejection', error => logger.error('unhandled rejection', errorKind(error)))
}

main().catch(error => {
	process.stderr.write(`whatsapp-sidecar: fatal: ${(error as Error)?.name ?? 'error'}\n`)
	process.exit(1)
})
