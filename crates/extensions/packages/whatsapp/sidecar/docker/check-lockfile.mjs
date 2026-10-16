// Build helper (runs inside the Docker `baileys` stage).
//
// Fails unless the only differences between Baileys' committed yarn.lock and
// the lockfile after `yarn install` concern whatsapp-rust-bridge. Every other
// dependency must stay exactly as Baileys pinned it.
//
// Usage: node check-lockfile.mjs <original yarn.lock> <new yarn.lock>
import { readFileSync } from 'node:fs'

const [origPath, newPath] = process.argv.slice(2)
const MARK = 'whatsapp-rust-bridge'

const blocks = path => {
	const out = new Map()
	for (const block of readFileSync(path, 'utf8').split(/\n\s*\n/)) {
		const lines = block.split('\n').filter(l => l.trim() && !l.startsWith('#'))
		if (!lines.length) continue
		const header = lines[0]
		if (header.includes(MARK)) continue
		out.set(header, lines.filter(l => !l.includes(MARK)).join('\n'))
	}
	return out
}

const a = blocks(origPath)
const b = blocks(newPath)
const problems = []
for (const [header, body] of a) {
	if (!b.has(header)) problems.push(`removed: ${header}`)
	else if (b.get(header) !== body) problems.push(`changed: ${header}`)
}
for (const header of b.keys()) {
	if (!a.has(header)) problems.push(`added: ${header}`)
}
if (problems.length) {
	console.error('yarn.lock changed beyond the whatsapp-rust-bridge entry:')
	for (const p of problems) console.error(`  ${p}`)
	process.exit(1)
}
const show = path =>
	readFileSync(path, 'utf8')
		.split(/\n\s*\n/)
		.filter(b => b.split('\n').find(l => l.trim())?.includes(MARK))
		.join('\n\n')
console.log('yarn.lock: only the whatsapp-rust-bridge entry changed')
console.log('--- before\n' + show(origPath))
console.log('--- after\n' + show(newPath))
