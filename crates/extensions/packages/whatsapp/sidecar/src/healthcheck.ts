// Docker HEALTHCHECK: GET /healthz on the configured listen address.
// Healthy whether or not the store is locked (locked is the normal state
// until IronClaw unlocks it).
import { parseListen } from './config.js'

const listen = parseListen(process.env.SIDECAR_LISTEN || '0.0.0.0:8080')
const host = !listen || listen.host === '0.0.0.0' || listen.host === '::' ? '127.0.0.1' : listen.host
const port = listen?.port ?? 8080
const url = `http://${host.includes(':') ? `[${host}]` : host}:${port}/healthz`

fetch(url, { signal: AbortSignal.timeout(4000) })
	.then(response => process.exit(response.ok ? 0 : 1))
	.catch(() => process.exit(1))
