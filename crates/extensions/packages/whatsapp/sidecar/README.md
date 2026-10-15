# WhatsApp sidecar

A small service that connects one WhatsApp account to IronClaw as a **linked
device**, the same way WhatsApp Web does. It runs
[Baileys](https://github.com/WhiskeySockets/Baileys) (an unofficial WhatsApp
Web client) with
[whatsapp-rust-bridge](https://github.com/jlucaso1/whatsapp-rust-bridge)
(Baileys' Rust/WASM crypto and app-state helpers), and puts a narrow,
authenticated HTTP API in front of it.

- Customer messages are pushed to IronClaw as signed webhook events.
- IronClaw sends replies, blocks contacts, downloads media and manages the
  link through the HTTP API.
- The sidecar only replies to chats that have written to the number first.

The full HTTP contract, including the webhook signing algorithm and a test
vector, is in [`API.md`](API.md). One sidecar serves one WhatsApp account.

## Trust model

The sidecar holds the WhatsApp session keys. Anyone with those keys can read
and send messages as the business, so the design keeps them encrypted at rest
and keeps the decryption key out of the sidecar's volume.

- **Encrypted store.** Every record (Baileys credentials, signal keys, the
  event spool, the known-chat list, blocked contacts, message references) is a
  separate file encrypted with AES-256-GCM, a fresh random 96-bit nonce per
  write, and the record's type and id as additional authenticated data. File
  names are HMAC values, so neither record types nor ids (which include phone
  numbers) appear on disk. Writes are atomic (temporary file, `fsync`,
  `rename`, directory `fsync`).
- **Starts locked.** On boot the sidecar has no key. It answers `423` on every
  route except `/v1/unlock` and `/healthz`, and tells IronClaw it is locked
  (`sidecar.locked`). IronClaw generates the 32-byte data key, keeps it in its
  own secret store, and unlocks the sidecar with it. A key-check record on the
  volume lets the sidecar reject a wrong key; the key itself is never written
  to disk.
- **What this protects.** A copy of the volume (a backup, a stolen disk, a
  misplaced snapshot) is useless without IronClaw's data key.
- **What it does not protect.** While the sidecar runs unlocked, the key and
  the decrypted session live in its memory. Anyone who can read the process
  memory or attach a debugger to it (root on the host, or the container
  runtime) can take the session. Protect the host accordingly.
- **Losing IronClaw's secret store** makes the sidecar store unreadable. The
  fix is to clear the sidecar volume and link the number again.
- **Secrets.** `SIDECAR_BEARER_TOKEN` authenticates IronClaw to the sidecar.
  `SIDECAR_WEBHOOK_HMAC_KEY` signs events to IronClaw. Both come from the
  sidecar's environment, and the operator copies the same values into
  IronClaw's admin configuration.
- **Logs** never contain message text, phone numbers, JIDs or keys at `info`
  level or above. Baileys' own log output is only emitted at `debug`/`trace`,
  because it routinely contains JIDs. Do not run production at `debug`.
  Dependencies that print straight to the console (libsignal prints whole
  sessions, private keys included) are silenced: each such call is noted at
  `trace` by method name only, never with its arguments.
- **Media downloads** go only to WhatsApp's media hosts. A media message's
  location comes from the sender's client, so the sidecar refuses any `url`
  that is not `https` on `*.whatsapp.net` and any `directPath` that could
  change the host, and pins Baileys to `mmg.whatsapp.net`.

### Risks of an unofficial client

- **Ban risk.** WhatsApp does not support third-party clients. Accounts that
  use them can be restricted or banned, especially for bulk or unsolicited
  messages. The sidecar reduces the risk: it only replies to chats that wrote
  first, never starts conversations, and rate-limits sends per chat and
  overall. The risk is not zero; use a number you can afford to lose.
- **Linked-device expiry.** WhatsApp logs out linked devices when the main
  phone has not been online for about 14 days. Keep the phone connected. The
  sidecar reports `logged_out` and must then be linked again.
- **Protocol changes.** WhatsApp changes its protocol without notice. A
  Baileys update may be needed to keep working.

## Linking

1. IronClaw calls `POST /v1/link/begin` with `{"mode":"qr"}`, or with
   `{"mode":"pairing_code","phone_number":"+…"}` as the fallback.
2. The admin page shows the QR code (refreshed from `/v1/link/status`) or the
   8-character code.
3. On the phone: *Settings → Linked devices → Link a device*, then scan the QR
   code, or choose *Link with phone number instead* and type the code.
4. `/v1/link/status` turns `linked`, and a `link.state_changed` event follows.

The sidecar shows up on the phone as an Ubuntu/Chrome linked device. It does
not mark itself online, so the phone keeps getting notifications.

## Configuration

All configuration comes from environment variables. The sidecar refuses to
start if a required value is missing or invalid.

| Variable | Default | Meaning |
|---|---|---|
| `SIDECAR_BEARER_TOKEN` | required | Token IronClaw sends as `Authorization: Bearer …`. At least 32 characters, no whitespace. |
| `SIDECAR_WEBHOOK_HMAC_KEY` | required | HMAC-SHA256 key for webhook signatures (used as its UTF-8 bytes). At least 32 characters, no whitespace, different from the bearer token. |
| `IRONCLAW_WEBHOOK_URL` | required | Where events are POSTed, e.g. `http://ironclaw:3000/webhooks/extensions/whatsapp/events`. `http` or `https`; no credentials or fragment. |
| `SIDECAR_DATA_DIR` | `/data` | The encrypted store lives in `<dir>/store`. |
| `SIDECAR_LISTEN` | `0.0.0.0:8080` | `host:port` to listen on. |
| `SIDECAR_LOG_LEVEL` | `info` | `trace`, `debug`, `info`, `warn`, `error`, `silent`. |
| `SIDECAR_MAX_BODY_BYTES` | `524288` | Largest accepted request body. |
| `SIDECAR_MEDIA_MAX_BYTES` | `67108864` | Largest media object `GET /v1/media` will stream. |
| `SIDECAR_SEND_PER_CHAT_PER_MINUTE` | `6` | Per-chat send rate. |
| `SIDECAR_SEND_PER_CHAT_BURST` | `3` | Per-chat burst. |
| `SIDECAR_SEND_GLOBAL_PER_MINUTE` | `20` | Send rate across all chats. |
| `SIDECAR_SEND_GLOBAL_BURST` | `5` | Burst across all chats. |
| `SIDECAR_SEND_ACK_TIMEOUT_MS` | `30000` | How long `/v1/send` waits for WhatsApp's server ack. |
| `SIDECAR_WEBHOOK_TIMEOUT_MS` | `10000` | Timeout per webhook attempt. |
| `SIDECAR_WEBHOOK_RETRY_BASE_MS` | `1000` | First retry delay (doubles per attempt, with jitter). |
| `SIDECAR_WEBHOOK_RETRY_MAX_MS` | `300000` | Retry delay cap. |
| `SIDECAR_RECONNECT_BASE_MS` | `1000` | First WhatsApp reconnect delay. |
| `SIDECAR_RECONNECT_MAX_MS` | `120000` | Reconnect delay cap. |
| `SIDECAR_LINK_FLOW_TTL_SECS` | `300` | How long a QR or pairing-code flow stays open. |
| `SIDECAR_MESSAGE_RETENTION_DAYS` | `30` | How long message and media references (for quoting and media download), sent-message copies and idempotency results are kept. |

Generate the two secrets with, for example, `openssl rand -hex 32`.

## Build

The image is built from local checkouts of Baileys and whatsapp-rust-bridge at
pinned commits, inside Docker. The host needs only Docker with BuildKit and
git; Rust, wasm-pack, wasm-bindgen, binaryen, bun and yarn are all installed
inside builder stages.

```sh
BAILEYS_DIR=~/oss/Baileys \
BRIDGE_DIR=~/oss/whatsapp-rust-bridge \
./build.sh
```

`build.sh` checks that each checkout is at its pinned commit with a clean
working tree, then runs:

```sh
docker build \
  --build-context bridge="$BRIDGE_DIR" \
  --build-context baileys="$BAILEYS_DIR" \
  -t ironclaw-whatsapp-sidecar:local .
```

Pinned sources:

| Source | Commit |
|---|---|
| Baileys | `0af2386292907f7d9742d8d41f830d8c48208fa1` |
| whatsapp-rust-bridge | `4b185ecf26b2f0077606ba8b181cc6ae0d562ef9` |
| whatsapp-rust (from the bridge's `Cargo.lock`) | `88a0fe074fdc09d73be95cdd23743fba22ef172e` |

Set `BASE_REGISTRY=public.ecr.aws/docker/library` (or another mirror of the
Docker official images) if Docker Hub rate-limits you; base images are pinned
by digest, so a mirror cannot substitute other content. `DOCKER_TARGET`
builds only up to one stage.

What the build does:

1. **`bridge-build`**: Rust `nightly-2026-01-30` with `wasm32-unknown-unknown`
   (from the bridge's `rust-toolchain.toml`); `wasm-pack` and
   `wasm-bindgen-cli` (the version in the bridge's `Cargo.lock`) compiled from
   crates.io with `cargo install --locked`; `wasm-opt` from Debian's binaryen
   package; bun from npm. It extracts the bridge at the pinned commit with
   `git archive`, checks that every whatsapp-rust crate in `Cargo.lock` comes
   from the pinned commit, runs `cargo fetch --locked`, builds offline (SIMD
   and non-SIMD WASM, the JS bundle, the type declarations) and packs the npm
   tarball.
2. **`baileys-build`**: Node with Yarn 4.9.2 through corepack. It extracts
   Baileys at the pinned commit, checks that every name Baileys imports from
   whatsapp-rust-bridge is exported by the local build, points
   `whatsapp-rust-bridge` at the local tarball with a `resolutions` entry,
   runs `yarn install`, fails if the lockfile changed anywhere except that
   entry, runs `yarn build`, runs Baileys' own unit tests against the local
   bridge, and makes a production-only dependency tree (which must contain no
   native addons).
3. **`app`**: compiles the sidecar with Baileys' TypeScript and runs the
   sidecar's tests (`node --test`) twice: against the full Baileys tree and
   against the production tree. **The image build fails if any test fails.**
4. **`runtime`**: `node:24-slim` with npm, npx, corepack and yarn removed. It
   contains only the compiled sidecar and the production dependency tree, runs
   as the non-root user `sidecar` (uid 10001), and has a `HEALTHCHECK` on
   `/healthz`. Only `/data` needs to be writable.

No prebuilt whatsapp-rust-bridge WASM and no prebuilt Baileys `lib/` is ever
downloaded. Baileys' other dependencies come from the npm registry as pinned
by its `yarn.lock` (checksums verified by yarn). Two of Baileys' development
dependencies ship prebuilt native binaries and are present only in the build
stage, never in the image: `@esbuild/linux-x64` (via `tsx` and
`esbuild-register`) and `@unrs/resolver-binding-linux-x64-gnu` (via jest).

## Development

The sidecar has no registry dependencies of its own: it compiles against the
Baileys tree the Docker build produces. To work on it outside Docker, build
the `baileys-build` stage, copy `/build/baileys` out of it, and link it:

```sh
DOCKER_TARGET=baileys-build IMAGE_TAG=wa-sidecar-baileys ./build.sh
id=$(docker create wa-sidecar-baileys) && docker cp "$id":/build/baileys /tmp/baileys && docker rm "$id"
mkdir -p node_modules
ln -s /tmp/baileys node_modules/baileys
ln -s /tmp/baileys/node_modules/@types node_modules/@types
/tmp/baileys/node_modules/.bin/tsc -p tsconfig.json
node --test 'dist/test/**/*.test.js'
```

Layout:

| Path | What |
|---|---|
| `src/store/` | encrypted record store, Baileys auth state, sidecar records |
| `src/connection/` | Baileys socket lifecycle, reconnect, QR and pairing-code linking |
| `src/inbound/` | which incoming messages are forwarded, and how |
| `src/webhook/` | event envelopes, signing, the durable spool and its delivery loop |
| `src/outbound/` | send rate limiter |
| `src/http/` | the HTTP API |
| `src/sidecar.ts` | ties the parts together |
| `test/` | `node --test` suites |
| `docker/` | helper scripts the Docker build runs |

## Running

Example `docker compose` service (adjust names, networks and the IronClaw URL
to your deployment):

```yaml
services:
  whatsapp-sidecar:
    image: ironclaw-whatsapp-sidecar:local
    restart: unless-stopped
    read_only: true
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]
    environment:
      SIDECAR_BEARER_TOKEN: ${WHATSAPP_SIDECAR_BEARER_TOKEN:?}
      SIDECAR_WEBHOOK_HMAC_KEY: ${WHATSAPP_SIDECAR_HMAC_KEY:?}
      IRONCLAW_WEBHOOK_URL: http://ironclaw:3000/webhooks/extensions/whatsapp/events
    volumes:
      - whatsapp-sidecar-data:/data
    networks: [internal]
    # No published ports: only IronClaw, on the internal network, talks to it.

volumes:
  whatsapp-sidecar-data:

networks:
  internal:
```

The sidecar needs outbound internet access to WhatsApp
(`web.whatsapp.com`, `*.whatsapp.net`) and network access to IronClaw. Do not
publish its port to the internet; the bearer token is its only gate.
