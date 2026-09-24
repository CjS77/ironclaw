"""Plan, stamp, and describe a naomi fork release from its tag.

Naomi releases are tagged `ironclaw-vX.Y.Z-naomi.J.K{-suffix}` by the
`/naomi-bump` skill. `J.K{-suffix}` must equal `.naomi-version`, the fork's only
stored copy of it (AGENTS.md, "Versioning (naomi fork)"). No Cargo manifest
carries the full version, so the release build writes it into the shipping
`ironclaw` package here, in CI only, so the binary reports the version it was
released as. Used by `.github/workflows/naomi-release.yml`.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))
from crate_tree import CrateTreeError, crate_directory  # noqa: E402

NUMERIC_IDENTIFIER = r"(?:0|[1-9][0-9]*)"
PRERELEASE_IDENTIFIER = r"(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
# Build metadata ('+') is excluded, as in cut_ironclaw_release.py.
SEMVER_PATTERN = re.compile(
    rf"{NUMERIC_IDENTIFIER}\.{NUMERIC_IDENTIFIER}\.{NUMERIC_IDENTIFIER}"
    rf"(?:-{PRERELEASE_IDENTIFIER}(?:\.{PRERELEASE_IDENTIFIER})*)?"
)
TAG_PATTERN = re.compile(
    rf"ironclaw-v(?P<version>{NUMERIC_IDENTIFIER}\.{NUMERIC_IDENTIFIER}\.{NUMERIC_IDENTIFIER}"
    rf"-naomi\.(?P<naomi>{NUMERIC_IDENTIFIER}\.{NUMERIC_IDENTIFIER}(?P<suffix>-.+)?))"
)
SHIPPING_PACKAGE_NAME = "ironclaw"
SHIPPING_CRATE_DIRECTORY = "ironclaw_cli"
NAOMI_VERSION_FILE = ".naomi-version"
CHANGELOG_FILE = "CHANGELOG-NAOMI.md"


class NaomiReleaseError(RuntimeError):
    """The naomi release tag or checkout cannot produce a release."""


@dataclass(frozen=True)
class ReleasePlan:
    version: str
    prerelease: bool


def plan_release(tag: str, naomi_version_file_text: str) -> ReleasePlan:
    """Validate `tag` against `.naomi-version` and return what to release."""
    match = TAG_PATTERN.fullmatch(tag)
    if match is None:
        raise NaomiReleaseError(
            f"{tag!r} is not a naomi release tag (ironclaw-vX.Y.Z-naomi.J.K{{-suffix}})"
        )
    version = match["version"]
    if SEMVER_PATTERN.fullmatch(version) is None:
        raise NaomiReleaseError(f"{version!r} is not a valid SemVer version")
    recorded = naomi_version_file_text.strip()
    if match["naomi"] != recorded:
        raise NaomiReleaseError(
            f"tag {tag} carries naomi version {match['naomi']}, but "
            f"{NAOMI_VERSION_FILE} at the tagged commit says {recorded!r}"
        )
    return ReleasePlan(version=version, prerelease=match["suffix"] is not None)


def stamp_manifest(text: str, version: str) -> str:
    """Replace the `[package]` version of the shipping manifest."""
    table = re.search(r"^\[package\]\n(?P<body>.*?)(?=^\[|\Z)", text, re.M | re.S)
    if table is None:
        raise NaomiReleaseError("shipping manifest has no [package] table")
    body = table["body"]
    if re.search(rf'^name = "{SHIPPING_PACKAGE_NAME}"$', body, re.M) is None:
        raise NaomiReleaseError(
            f"shipping manifest [package] is not named {SHIPPING_PACKAGE_NAME!r}"
        )
    stamped, count = re.subn(
        r'^version = "[^"]*"$', f'version = "{version}"', body, flags=re.M
    )
    if count != 1:
        raise NaomiReleaseError(
            "shipping manifest [package] must declare exactly one literal "
            f'`version = "..."` line, found {count}'
        )
    return text[: table.start("body")] + stamped + text[table.end("body") :]


def stamp_lock(text: str, version: str) -> str:
    """Replace the lockfile version of the shipping workspace package."""
    stamped, count = re.subn(
        rf'^(\[\[package\]\]\nname = "{SHIPPING_PACKAGE_NAME}"\nversion = ")[^"]*(")$',
        rf"\g<1>{version}\g<2>",
        text,
        flags=re.M,
    )
    if count != 1:
        raise NaomiReleaseError(
            f"Cargo.lock must hold exactly one {SHIPPING_PACKAGE_NAME!r} package, "
            f"found {count}"
        )
    return stamped


def shipping_manifest(root: Path) -> Path:
    try:
        return root / crate_directory(SHIPPING_CRATE_DIRECTORY, root) / "Cargo.toml"
    except CrateTreeError as error:
        raise NaomiReleaseError(str(error)) from error


def stamp_checkout(root: Path, manifest: Path, version: str) -> None:
    lock = root / "Cargo.lock"
    manifest_text = stamp_manifest(manifest.read_text(encoding="utf-8"), version)
    lock_text = stamp_lock(lock.read_text(encoding="utf-8"), version)
    manifest.write_text(manifest_text, encoding="utf-8")
    lock.write_text(lock_text, encoding="utf-8")


def release_notes(changelog: str, version: str) -> str:
    """Return the body of the `## [v<version>]` section of the naomi changelog."""
    heading = re.search(rf"^## \[v{re.escape(version)}\][^\n]*\n", changelog, re.M)
    if heading is None:
        raise NaomiReleaseError(
            f"{CHANGELOG_FILE} has no `## [v{version}]` section; run /naomi-bump"
        )
    following = re.search(r"^## ", changelog[heading.end() :], re.M)
    end = heading.end() + following.start() if following else len(changelog)
    notes = changelog[heading.end() : end].strip()
    if not notes:
        raise NaomiReleaseError(f"{CHANGELOG_FILE} section for v{version} is empty")
    return notes


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path("."))
    commands = parser.add_subparsers(dest="command", required=True)
    plan = commands.add_parser("plan", help="print GITHUB_OUTPUT lines for a tag")
    plan.add_argument("--tag", required=True)
    stamp = commands.add_parser("stamp", help="write the version into the checkout")
    stamp.add_argument("--version", required=True)
    notes = commands.add_parser("notes", help="print the changelog section")
    notes.add_argument("--version", required=True)
    args = parser.parse_args()

    root = args.root.resolve()
    if args.command == "plan":
        result = plan_release(
            args.tag, (root / NAOMI_VERSION_FILE).read_text(encoding="utf-8")
        )
        print(f"version={result.version}")
        print(f"prerelease={'true' if result.prerelease else 'false'}")
    elif args.command == "stamp":
        if SEMVER_PATTERN.fullmatch(args.version) is None:
            raise NaomiReleaseError(f"{args.version!r} is not a valid SemVer version")
        stamp_checkout(root, shipping_manifest(root), args.version)
        print(f"stamped {SHIPPING_PACKAGE_NAME} {args.version}")
    else:
        changelog = (root / CHANGELOG_FILE).read_text(encoding="utf-8")
        print(release_notes(changelog, args.version))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (NaomiReleaseError, OSError) as error:
        raise SystemExit(f"error: {error}") from error
