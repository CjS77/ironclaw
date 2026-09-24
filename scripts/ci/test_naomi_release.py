"""Behavioral tests for naomi release tag planning and version stamping."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "scripts/ci/naomi_release.py"
SPEC = importlib.util.spec_from_file_location("naomi_release", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
release = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = release
SPEC.loader.exec_module(release)

MANIFEST = """[package]
name = "ironclaw"
version = "1.2.0"
edition = "2024"

[dependencies]
serde = { version = "1", features = ["derive"] }
"""

LOCK = """[[package]]
name = "ironclaw"
version = "1.2.0"
dependencies = [
 "anyhow",
]

[[package]]
name = "ironclaw-registry"
version = "1.2.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"""

CHANGELOG = """# Naomi Changelog

## [Unreleased]

## [v1.4.0-naomi.0.3] - 2026-09-25

### Added
- Linux musl release builds

## [v1.4.0-naomi.0.2] - 2026-09-24

### Added
- Older entry
"""


class PlanReleaseTest(unittest.TestCase):
    def test_stable_tag_matching_naomi_version_is_not_a_prerelease(self) -> None:
        plan = release.plan_release("ironclaw-v1.4.0-naomi.0.3", "0.3\n")
        self.assertEqual(plan.version, "1.4.0-naomi.0.3")
        self.assertFalse(plan.prerelease)

    def test_suffixed_tag_is_a_prerelease(self) -> None:
        plan = release.plan_release("ironclaw-v1.5.0-naomi.1.0-rc3", "1.0-rc3\n")
        self.assertEqual(plan.version, "1.5.0-naomi.1.0-rc3")
        self.assertTrue(plan.prerelease)

    def test_tag_that_disagrees_with_naomi_version_is_refused(self) -> None:
        with self.assertRaisesRegex(release.NaomiReleaseError, ".naomi-version"):
            release.plan_release("ironclaw-v1.4.0-naomi.0.3", "0.2\n")

    def test_suffix_must_match_naomi_version_too(self) -> None:
        with self.assertRaisesRegex(release.NaomiReleaseError, ".naomi-version"):
            release.plan_release("ironclaw-v1.4.0-naomi.0.3-rc1", "0.3\n")

    def test_non_naomi_and_malformed_tags_are_refused(self) -> None:
        for tag in (
            "ironclaw-v1.4.0",
            "ironclaw-v1.4.0-rc.1",
            "v1.4.0-naomi.0.3",
            "ironclaw-v1.4-naomi.0.3",
            "ironclaw-v01.4.0-naomi.0.3",
            "ironclaw-v1.4.0-naomi.03",
            "ironclaw-v1.4.0-naomi.0.3+build",
            "ironclaw-v1.4.0-naomi.0.3-rc.01",
        ):
            with self.subTest(tag=tag):
                with self.assertRaises(release.NaomiReleaseError):
                    release.plan_release(tag, tag.rsplit("naomi.", 1)[-1])


class StampTest(unittest.TestCase):
    def test_manifest_package_version_is_replaced_and_nothing_else(self) -> None:
        stamped = release.stamp_manifest(MANIFEST, "1.4.0-naomi.0.3")
        self.assertIn('version = "1.4.0-naomi.0.3"\nedition', stamped)
        self.assertIn('serde = { version = "1"', stamped)
        self.assertEqual(stamped.count("1.4.0-naomi.0.3"), 1)

    def test_manifest_without_literal_package_version_is_refused(self) -> None:
        inherited = MANIFEST.replace('version = "1.2.0"', "version.workspace = true")
        with self.assertRaisesRegex(release.NaomiReleaseError, "version"):
            release.stamp_manifest(inherited, "1.4.0-naomi.0.3")

    def test_manifest_for_another_package_is_refused(self) -> None:
        other = MANIFEST.replace('name = "ironclaw"', 'name = "ironclaw_cli"')
        with self.assertRaisesRegex(release.NaomiReleaseError, "ironclaw"):
            release.stamp_manifest(other, "1.4.0-naomi.0.3")

    def test_lock_entry_for_the_workspace_package_only_is_replaced(self) -> None:
        stamped = release.stamp_lock(LOCK, "1.4.0-naomi.0.3")
        self.assertIn('name = "ironclaw"\nversion = "1.4.0-naomi.0.3"\n', stamped)
        self.assertIn('name = "ironclaw-registry"\nversion = "1.2.0"\n', stamped)

    def test_lock_without_the_package_is_refused(self) -> None:
        with self.assertRaisesRegex(release.NaomiReleaseError, "Cargo.lock"):
            release.stamp_lock(LOCK.replace('"ironclaw"', '"other"'), "1.4.0-naomi.0.3")

    def test_stamp_rewrites_the_real_checkout_layout(self) -> None:
        # The fixtures above can drift from the tree; this drives the real
        # manifest and lockfile through the same resolution the workflow uses.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest_relative = release.shipping_manifest(ROOT).relative_to(ROOT)
            (root / manifest_relative).parent.mkdir(parents=True)
            (root / manifest_relative).write_bytes((ROOT / manifest_relative).read_bytes())
            (root / "Cargo.lock").write_bytes((ROOT / "Cargo.lock").read_bytes())

            release.stamp_checkout(root, root / manifest_relative, "1.4.0-naomi.0.3")

            self.assertIn(
                'version = "1.4.0-naomi.0.3"',
                (root / manifest_relative).read_text(encoding="utf-8"),
            )
            self.assertIn(
                'name = "ironclaw"\nversion = "1.4.0-naomi.0.3"\n',
                (root / "Cargo.lock").read_text(encoding="utf-8"),
            )


class NotesTest(unittest.TestCase):
    def test_notes_are_the_matching_section_body(self) -> None:
        notes = release.release_notes(CHANGELOG, "1.4.0-naomi.0.3")
        self.assertEqual(notes, "### Added\n- Linux musl release builds")

    def test_missing_section_is_refused(self) -> None:
        with self.assertRaisesRegex(release.NaomiReleaseError, "CHANGELOG-NAOMI.md"):
            release.release_notes(CHANGELOG, "1.4.0-naomi.0.4")

    def test_empty_section_is_refused(self) -> None:
        empty = CHANGELOG.replace("### Added\n- Linux musl release builds\n", "")
        with self.assertRaisesRegex(release.NaomiReleaseError, "empty"):
            release.release_notes(empty, "1.4.0-naomi.0.3")


if __name__ == "__main__":
    unittest.main()
