# whatsapp — WhatsApp untrusted inbox

Connects one WhatsApp Business number to IronClaw as a linked device. Customer
messages land in an inbox and are triaged; they never start an agent turn, and
replies go out only after the owner approves them.

- **Sidecar:** [`sidecar/`](sidecar/README.md) — a Docker service running
  Baileys and whatsapp-rust-bridge behind an authenticated HTTP API, with an
  encrypted credential store that IronClaw unlocks. HTTP contract:
  [`sidecar/API.md`](sidecar/API.md).
- **Extension crate:** not yet present. It will hold the manifest, the
  inbox-mode channel, the `whatsapp.reply` / `whatsapp.block_contact` tools and
  the installation-level link, and it talks to the sidecar only through
  `sidecar/API.md`.
