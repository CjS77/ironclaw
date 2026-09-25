# Naomi Changelog

All notable changes specific to the naomi fork are documented in this file.
Upstream IronClaw changes are recorded in `CHANGELOG.md`.

Versions follow `vX.Y.Z-naomi.J.K`: `X.Y.Z` is the upstream release this fork
is based on (the latest `ironclaw-v*` tag), and `J.K` is the naomi version held
in `.naomi-version`. Entries are added by the `/naomi-bump` skill.

## [Unreleased]

### Added
- Naomi catalog signing key `93f44233e7e0e589` trusted alongside the upstream IronHub key
- Hardcoded IronHub URL whitelist: the catalog URL, private manifest URLs and artifact URLs must start with `https://hub.ironclaw.com/` or `https://github.com/CjS77/naomi-addons/releases/download/`
- Operator guide for custom IronHub catalogs: catalog URL whitelist, adding signing keys, redirect behavior

### Changed
- IronHub downloads and their redirect hops may reach only `hub.ironclaw.com`, `github.com`, `release-assets.githubusercontent.com` and `objects.githubusercontent.com`; the `*.githubusercontent.com` wildcard, `raw.githubusercontent.com` and `github-releases.githubusercontent.com` are no longer allowed

### Fixed
- Outbound HTTP requests (web search, the `http` tool, downloads) now send a full browser header set — by default Chrome 151's User-Agent, `Sec-CH-UA` client hints, `Accept`, `Accept-Language` and `Accept-Encoding` — where before they sent no User-Agent at all and many sites refused them; compressed responses are decoded. A descriptive `Naomi/<version> (IronClaw; +https://github.com/CjS77/ironclaw)` identity is available as `OutboundIdentity::Naomi`
- The `http` tool description now says redirects are followed (each hop re-checked against the network policy) instead of claiming they are returned unfollowed
- `ironclaw ironhub` CLI commands and hub-delivered installs now reach the network; previously no network policy was granted to them and every download was refused before it was sent

## [v1.4.0-naomi.0.3] - 2026-09-24

### Changed
- Naomi Linux musl release builds automated from naomi tags

### Fixed
- Typo fix

## [v1.4.0-naomi.0.2] - 2026-09-24

### Added
- Naomi version skill for automated version bumping
- Front-end rewired to naomi
- Telegram command menu registration at activation
- Subagent approval/auth gate routes to owner's inbox (R3 slice 3a)
- Durable progressive replies and native Slack Agent UI

### Changed
- Loop host text updates coalesced to reduce re-sanitization overhead
- Frontend API boundaries typed and validated
- Frontend test infrastructure typed
- Production components and hooks typed
- TypeScript suppressions ratcheted

### Fixed
- Assistant pairing check performed before command admission for first-contact experience
- Device link error message clarified ("not configured by administrator")
- OpenAI conversation cache keys sent on request paths
- WebUI asset test trace-api.ts authorization follow-up
- Reply model call text correctly identified (earlier calls are narration)
- CLI serve keepalive when stderr closes, early binding, and mutant judgement
