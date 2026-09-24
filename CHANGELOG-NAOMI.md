# Naomi Changelog

All notable changes specific to the naomi fork are documented in this file.
Upstream IronClaw changes are recorded in `CHANGELOG.md`.

Versions follow `vX.Y.Z-naomi.J.K`: `X.Y.Z` is the upstream release this fork
is based on (the latest `ironclaw-v*` tag), and `J.K` is the naomi version held
in `.naomi-version`. Entries are added by the `/naomi-bump` skill.

## [Unreleased]

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
