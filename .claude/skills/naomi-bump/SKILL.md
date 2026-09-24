---
name: naomi-bump
description: Use for cutting or bumping this fork's naomi version (the J.K in vX.Y.Z-naomi.J.K), or when the user runs /naomi-bump.
argument-hint: [level] [suffix]
disable-model-invocation: true
user-invocable: true
model: haiku
allowed-tools: Read, Write, Edit, Glob, Grep, Bash(git add *), Bash(git log *), Bash(git diff *), Bash(git commit *), Bash(git tag *), Bash(git describe *)
context: fork
---
`$1` is the naomi bump level: `major` or `minor`. If no level was supplied
above, use `minor`. Naomi versions have two parts (`J.K`), so there is no
`patch`; treat any other value as `minor`.

`$2` is the optional suffix. If it is not supplied, there is no suffix and the Naomi version is simply `J.K`. Strip any leading dashes from the suffix.

This repo is a fork versioned **`vX.Y.Z-naomi.J.K{-suffix}`**. `X.Y.Z` is the upstream
IronClaw version and **always comes from upstream**. `J.K{-suffix}` is this fork's naomi
version, and its only source of truth is **`.naomi-version`** at the repo root.
Bumping the naomi version means changing the value in that one file; nothing
else defines `J.K{-suffix}`.

Versions order by `X.Y.Z` first, then `J.K{-suffix}`: `v1.5.0-naomi.0.1` is newer than
`v1.4.0-naomi.1.0-rc1`.

Do these steps in order. On any STOP condition, report it and change NOTHING.
Never guess.

## 1. Read the current naomi version

Read `.naomi-version`. It must be a single `J.K{-suffix}` line (e.g. `0.1` or `1.3-rc45`), where `J`
and `K` are non-negative integers. The suffix is optional, but must join the minor version with a dash, `-` if present. 
If the file is missing or malformed, STOP and report.

## 2. Read the upstream X.Y.Z from the latest upstream tag

```
git tag --list 'ironclaw-v*' --sort=-v:refname | grep -E '^ironclaw-v[0-9]+\.[0-9]+\.[0-9]+$' | head -n1
```

Strip the `ironclaw-v` prefix to get `X.Y.Z` (e.g. `ironclaw-v1.4.0` gives
`1.4.0`). The pattern excludes `-rc.*` and `-naomi.*` tags, so this is the
latest stable upstream release. If it prints nothing, STOP and report that the
upstream version cannot be determined. Do not read `X.Y.Z` from any
`Cargo.toml`; crate versions in this repo are not kept in step with upstream
releases.

## 3. Guardrail: does .naomi-version agree with the last naomi tag?

```
git tag --list 'ironclaw-v*-naomi.*' --sort=-v:refname | head -n1
```

- No naomi tag yet: proceed (this is the first naomi release).
- A naomi tag `ironclaw-vA.B.C-naomi.P.Q{-suffix}` exists:
  - If `A.B.C` equals the upstream `X.Y.Z` from step 2 and `P.Q{-suffix}` differs from
    `.naomi-version`, STOP. The file has drifted from the last tag (it was
    probably bumped by hand or a previous bump was not tagged). Report the file
    value and the tag.
  - Otherwise proceed. Either `P.Q` matches the file, or upstream has moved on
    since the last naomi tag.

## 4. Compute the new naomi version

From the current `J.K`:

- `major` gives `(J+1).0{-suffix}`
- `minor` gives `J.(K+1){-suffix}`

If suffix is not supplied as an argument, do not add the suffix or the dash. Suffixes never carry over from old versions. They are either explicitly provided or omitted.

State the change explicitly, e.g. `naomi minor: 0.1 → 0.2`, and the full new
version `vX.Y.Z-naomi.<new J.K>{-suffix}`.

Then check the new tag name does not already exist:

```
git tag --list 'ironclaw-vX.Y.Z-naomi.<new J.K>{-suffix}'
```

If it prints anything, STOP and report.

## 5. Update .naomi-version

Write the new `J.K{-suffix}` as a single line followed by a newline. This file is the
only place the naomi version lives; do not write it anywhere else.

## 6. Update CHANGELOG-NAOMI.md

Update `CHANGELOG-NAOMI.md` at the repo root (create it if it is missing, with a
`# Naomi Changelog` heading and an empty `## [Unreleased]` section). This is the
fork's own changelog. NEVER edit the upstream `CHANGELOG.md`.

Add a section for the new version, dated today. `## [Unreleased]` must stay the
first version heading in the file: put the new section AFTER it and BEFORE
any older version sections, so the newest release comes first below
`Unreleased`:

```
# Naomi Changelog
...intro text...

## [Unreleased]

## [vX.Y.Z-naomi.<new J.K>{-suffix}] - YYYY-MM-DD      <- new section goes here
...

## [vX.Y.Z-naomi.<previous J.K>{-suffix}] - YYYY-MM-DD <- older sections stay below
...
```

Fill it from the commits since the last naomi tag found in step 3. If there
is no naomi tag yet, use the upstream tag from step 2 as the starting point
instead (never the whole history):

```
git log <last-naomi-tag>..HEAD --no-merges --format='%h %s'
git log ironclaw-vX.Y.Z..HEAD --no-merges --format='%h %s'   # no naomi tag yet
```

Summarise them under `### Added`, `### Changed`, and `### Fixed` headings, the
Keep a Changelog style `CHANGELOG.md` uses. Omit empty headings. One bullet per
user-visible change; fold routine chores together or leave them out. If the
range has no commits, write the single line `- Version bump only.` under the
new heading.

## 7. Commit

```
git add .naomi-version CHANGELOG-NAOMI.md
git commit -m "chore(naomi): bump naomi version to vX.Y.Z-naomi.<new J.K>{-suffix}"
```

Stage only those two files.

## 8. Tag

```
git tag ironclaw-vX.Y.Z-naomi.<new J.K>{-suffix}
```

This follows the repo's `ironclaw-v*` tag namespace. Upstream's cargo-dist
publisher (`.github/workflows/ironclaw-release.yml`) excludes `ironclaw-v*-naomi.*`
tags in its trigger; pushing this tag instead starts
`.github/workflows/naomi-release.yml`, which builds the Linux musl binaries and
publishes a GitHub Release. Verify both with
`grep -n "naomi" .github/workflows/ironclaw-release.yml .github/workflows/naomi-release.yml`.

## 9. Report

Report the old and new naomi versions, the commit, and the tag. State that
nothing was pushed: the result is a local commit and tag only. To release,
push the commit and then the tag:
`git push origin HEAD && git push origin ironclaw-vX.Y.Z-naomi.<new J.K>{-suffix}`.
