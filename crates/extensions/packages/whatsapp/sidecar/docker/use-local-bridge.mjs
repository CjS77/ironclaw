// Build helper (runs inside the Docker `baileys` stage).
//
// 1. Points Baileys' whatsapp-rust-bridge dependency at the bridge tarball
//    built from the local checkout, through a `resolutions` entry.
// 2. Checks that every name Baileys imports from whatsapp-rust-bridge is
//    exported by the local bridge's type declarations. (`yarn build` then
//    type-checks every call site against those declarations.)
//
// Usage: node use-local-bridge.mjs <baileys-dir> <bridge-pkg-dir>
import { execFileSync } from 'node:child_process'
import { mkdtempSync, readdirSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const [baileysDir, bridgePkgDir] = process.argv.slice(2)
if (!baileysDir || !bridgePkgDir) {
	console.error('usage: use-local-bridge.mjs <baileys-dir> <bridge-pkg-dir>')
	process.exit(2)
}

const tarballs = readdirSync(bridgePkgDir).filter(f => /^whatsapp-rust-bridge-.*\.tgz$/.test(f))
if (tarballs.length !== 1) {
	console.error(`expected exactly one bridge tarball in ${bridgePkgDir}, found ${tarballs.length}`)
	process.exit(1)
}
const tarball = join(bridgePkgDir, tarballs[0])

// --- API surface check -----------------------------------------------------
const unpacked = mkdtempSync(join(tmpdir(), 'bridge-'))
execFileSync('tar', ['-xzf', tarball, '-C', unpacked])
const pkgRoot = join(unpacked, 'package')
const bridgePkg = JSON.parse(readFileSync(join(pkgRoot, 'package.json'), 'utf8'))
const declarations =
	readFileSync(join(pkgRoot, 'dist/index.d.ts'), 'utf8') +
	'\n' +
	readFileSync(join(pkgRoot, 'pkg/whatsapp_rust_bridge.d.ts'), 'utf8')
const exported = new Set()
for (const m of declarations.matchAll(/export\s+(?:declare\s+)?(?:function|class|const|let|type|interface|enum)\s+([A-Za-z_$][\w$]*)/g)) {
	exported.add(m[1])
}

const imported = new Set()
const walk = dir => {
	for (const entry of readdirSync(dir, { withFileTypes: true })) {
		const path = join(dir, entry.name)
		if (entry.isDirectory()) {
			if (entry.name !== '__tests__') walk(path)
		} else if (entry.name.endsWith('.ts')) {
			const src = readFileSync(path, 'utf8')
			for (const m of src.matchAll(/(?:import|export)\s*(?:type\s*)?\{([^}]*)\}\s*from\s*['"]whatsapp-rust-bridge['"]/g)) {
				for (const part of m[1].split(',')) {
					const name = part.trim().replace(/^type\s+/, '').split(/\s+as\s+/)[0].trim()
					if (name) imported.add(name)
				}
			}
			if (/import\s*\*\s*as\s+\w+\s+from\s*['"]whatsapp-rust-bridge['"]/.test(src) || /require\(\s*['"]whatsapp-rust-bridge['"]\s*\)/.test(src)) {
				console.error(`${path}: namespace/require import of whatsapp-rust-bridge; extend this check`)
				process.exit(1)
			}
		}
	}
}
walk(join(baileysDir, 'src'))

const missing = [...imported].filter(name => !exported.has(name))
console.log(`whatsapp-rust-bridge ${bridgePkg.version}: Baileys imports [${[...imported].sort().join(', ')}]`)
if (missing.length) {
	console.error(`missing from the local bridge: ${missing.join(', ')}`)
	process.exit(1)
}
console.log('all imported names are exported by the local bridge')

// --- resolutions -----------------------------------------------------------
const manifestPath = join(baileysDir, 'package.json')
const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'))
manifest.resolutions = { ...(manifest.resolutions ?? {}), 'whatsapp-rust-bridge': `file:${tarball}` }
writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + '\n')
console.log(`resolutions: whatsapp-rust-bridge -> file:${tarball}`)
