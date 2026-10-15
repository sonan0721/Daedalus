"""Synthetic tests for the standard-library Cargo notice collector."""

from __future__ import annotations

import json
import hashlib
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from collect_cli_notices import (
    INVENTORY_FILENAME,
    NOTICES_FILENAME,
    SPDX_LICENSE_COMMIT,
    SPDX_LICENSE_REPOSITORY,
    NoticeCollectionError,
    _validate_package_path_in_vcs,
    collect_cli_notices,
)


REGISTRY_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
TARGET = "aarch64-apple-darwin"


class NoticeCollectorTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name) / "repository"
        self.output = Path(self.temporary.name) / "staging"
        self.cargo_home = Path(self.temporary.name) / "cargo-home"
        self.root.mkdir()
        self.output.mkdir()
        self.cargo_home.mkdir()
        (self.root / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
        (self.root / "Cargo.lock").write_text(
            'version = 4\n\n[[package]]\nname = "foo_bar"\nversion = "1.0.0"\n'
            f'source = "{REGISTRY_SOURCE}"\nchecksum = "{"a" * 64}"\n\n'
            '[[package]]\nname = "foo-bar"\nversion = "1.0.0"\n'
            f'source = "{REGISTRY_SOURCE}"\nchecksum = "{"b" * 64}"\n\n'
            '[[package]]\nname = "unreachable"\nversion = "9.9.9"\n'
            f'source = "{REGISTRY_SOURCE}"\nchecksum = "{"c" * 64}"\n',
            encoding="utf-8",
        )
        self.cli_manifest = self.root / "crates" / "daedalus-cli" / "Cargo.toml"
        self.cli_manifest.parent.mkdir(parents=True)
        self.cli_manifest.write_text('[package]\nname = "daedalus-cli"\n', encoding="utf-8")
        self.package_roots: dict[str, Path] = {}
        for name in ("foo_bar", "foo-bar", "unreachable"):
            package_root = (
                self.cargo_home
                / "registry"
                / "src"
                / "index.crates.io-test"
                / f"{name}-1.0.0"
            )
            package_root.mkdir(parents=True)
            (package_root / "Cargo.toml").write_text(
                f'[package]\nname = "{name}"\nversion = "1.0.0"\nlicense = "MIT OR Apache-2.0"\n', encoding="utf-8"
            )
            (package_root / "LICENSE-MIT").write_text(
                f"License text for {name}.\n", encoding="utf-8"
            )
            self.package_roots[name] = package_root
        self.addCleanup(self.temporary.cleanup)

    def metadata(self, reachable_names: tuple[str, ...] = ("foo_bar", "foo-bar")) -> dict:
        root_id = f"path+file://{self.root}#daedalus-cli@0.1.0"
        root_package = {
            "id": root_id,
            "name": "daedalus-cli",
            "version": "0.1.0",
            "source": None,
            "manifest_path": str(self.cli_manifest),
        }
        packages = [root_package]
        nodes = [{"id": root_id, "features": ["capture", "default", "window"], "deps": []}]
        for name in ("foo_bar", "foo-bar", "unreachable"):
            package_id = f"registry+https://github.com/rust-lang/crates.io-index#{name}@1.0.0"
            package = {
                "id": package_id,
                "name": name,
                "version": "1.0.0",
                "source": REGISTRY_SOURCE,
                "manifest_path": str(self.package_roots[name] / "Cargo.toml"),
                "license": "MIT OR Apache-2.0",
                "license_file": None,
            }
            packages.append(package)
            nodes.append({"id": package_id, "features": ["default"], "deps": []})
            if name in reachable_names:
                nodes[0]["deps"].append(
                    {
                        "pkg": package_id,
                        "dep_kinds": [{"kind": None, "target": None}],
                    }
                )
        return {
            "version": 1,
            "packages": packages,
            "workspace_members": [root_id],
            "resolve": {"nodes": nodes},
        }

    def collect(self, metadata: dict | None = None, *, strict: bool = True):
        with patch.dict(os.environ, {"CARGO_HOME": str(self.cargo_home)}):
            return collect_cli_notices(
                repository=self.root,
                output_directory=self.output,
                target=TARGET,
                metadata=metadata if metadata is not None else self.metadata(),
                strict=strict,
            )

    def test_collects_only_reachable_text_and_sanitizes_collision_names(self) -> None:
        artifacts = self.collect()
        inventory = json.loads(artifacts.inventory_path.read_text(encoding="utf-8"))
        self.assertEqual(inventory["dependency_count"], 2)
        self.assertEqual(inventory["target_triple"], TARGET)
        self.assertEqual(inventory["requested_features"], ["daedalus-cli/window"])
        self.assertIn("conservative-workspace-feature-unified-superset", inventory["feature_resolution"])
        dependencies = inventory["dependencies"]
        self.assertEqual({item["name"] for item in dependencies}, {"foo_bar", "foo-bar"})
        self.assertEqual(len({item["package_id"] for item in dependencies}), 2)
        for item in dependencies:
            self.assertRegex(item["package_id"], r"^[a-z0-9-]+-[a-f0-9]{16}$")
            self.assertEqual(item["source_checksum_sha256"], "a" * 64 if item["name"] == "foo_bar" else "b" * 64)
            expected_hash = hashlib.sha256(
                f"License text for {item['name']}.\n".encode()
            ).hexdigest()
            self.assertEqual(item["license_texts"][0]["sha256"], expected_hash)
        notices = artifacts.notices_path.read_text(encoding="utf-8")
        self.assertIn("License text for foo_bar.", notices)
        self.assertIn("License text for foo-bar.", notices)
        self.assertNotIn("unreachable", notices)
        self.assertNotIn(str(self.root), notices)
        self.assertNotIn(str(self.cargo_home), notices)
        self.assertNotIn(str(self.root), artifacts.inventory_path.read_text(encoding="utf-8"))
        self.assertNotIn(str(self.cargo_home), artifacts.inventory_path.read_text(encoding="utf-8"))
        self.assertEqual(artifacts.notices_path.name, NOTICES_FILENAME)
        self.assertEqual(artifacts.inventory_path.name, INVENTORY_FILENAME)

    def test_declared_license_escape_is_rejected(self) -> None:
        metadata = self.metadata(("foo_bar",))
        package = next(item for item in metadata["packages"] if item["name"] == "foo_bar")
        external_text = self.package_roots["foo_bar"].parent / "OUTSIDE-LICENSE"
        external_text.write_text("escaped text", encoding="utf-8")
        package["license_file"] = "../OUTSIDE-LICENSE"
        with self.assertRaisesRegex(NoticeCollectionError, "escapes its resolved crate source"):
            self.collect(metadata)

    def test_missing_license_text_is_a_strict_error(self) -> None:
        (self.package_roots["foo_bar"] / "LICENSE-MIT").unlink()
        (self.package_roots["foo-bar"] / "LICENSE-MIT").unlink()
        with self.assertRaisesRegex(
            NoticeCollectionError,
            r"2 resolved dependencies have no license text: foo-bar@1\.0\.0.*foo_bar@1\.0\.0",
        ):
            self.collect()
        self.assertFalse((self.output / NOTICES_FILENAME).exists())

    def test_non_strict_mode_marks_missing_text_without_inventing_it(self) -> None:
        (self.package_roots["foo_bar"] / "LICENSE-MIT").unlink()
        artifacts = self.collect(self.metadata(("foo_bar",)), strict=False)
        inventory = artifacts.inventory
        self.assertEqual(inventory["dependency_count"], 1)
        self.assertEqual(inventory["missing_license_text_count"], 1)
        self.assertEqual(inventory["missing_license_texts"][0]["name"], "foo_bar")
        self.assertIn("no collected license text", artifacts.notices_path.read_text(encoding="utf-8"))
        self.assertEqual(inventory["dependencies"][0]["license_texts"], [])

    def _write_override(
        self,
        *,
        source_url: str | None = None,
        outside_symlink: Path | None = None,
        vcs_path: str = "",
        vcs_commit: str | None = None,
        include_vcs: bool = True,
    ) -> bytes:
        (self.package_roots["foo_bar"] / "LICENSE-MIT").unlink(missing_ok=True)
        package = self.package_roots["foo_bar"]
        metadata = self.metadata(("foo_bar",))
        dependency = next(item for item in metadata["packages"] if item["name"] == "foo_bar")
        dependency["repository"] = "https://github.com/example/foo_bar"
        commit = "1" * 40
        if include_vcs:
            (package / ".cargo_vcs_info.json").write_text(
                json.dumps(
                    {
                        "git": {"sha1": vcs_commit or commit},
                        "path_in_vcs": vcs_path,
                    }
                ),
                encoding="utf-8",
            )
        override_root = self.root / "third_party" / "license-overrides"
        copied_text = override_root / "texts" / "foo_bar" / "LICENSE-MIT"
        copied_text.parent.mkdir(parents=True, exist_ok=True)
        content = b"Pinned upstream MIT text.\n"
        if outside_symlink is None:
            copied_text.write_bytes(content)
        else:
            copied_text.symlink_to(outside_symlink)
        expected_source_url = (
            "https://raw.githubusercontent.com/example/foo_bar/" + commit + "/LICENSE-MIT"
        )
        manifest = {
            "schema_version": 1,
            "overrides": [
                {
                    "name": "foo_bar",
                    "version": "1.0.0",
                    "license_expression": "MIT OR Apache-2.0",
                    "source_checksum_sha256": "a" * 64,
                    "source_repository": "https://github.com/example/foo_bar",
                    "source_commit": commit,
                    "package_path_in_repository": ".",
                    "evidence_kind": "upstream_text",
                    "files": [
                        {
                            "path": "texts/foo_bar/LICENSE-MIT",
                            "upstream_path": "LICENSE-MIT",
                            "source_url": source_url or expected_source_url,
                            "sha256": hashlib.sha256(content).hexdigest(),
                        }
                    ],
                }
            ],
        }
        (override_root / "manifest.json").write_text(
            json.dumps(manifest), encoding="utf-8"
        )
        return metadata

    def test_curated_pinned_override_is_verified_and_inventoried(self) -> None:
        metadata = self._write_override()
        artifacts = self.collect(metadata)
        dependency = artifacts.inventory["dependencies"][0]
        license_text = dependency["license_texts"][0]
        self.assertEqual(license_text["filename"], "LICENSE-MIT")
        self.assertEqual(license_text["provenance"]["source_commit"], "1" * 40)
        self.assertEqual(license_text["provenance"]["kind"], "package_upstream_text")
        self.assertEqual(
            license_text["provenance"]["source_url"],
            "https://raw.githubusercontent.com/example/foo_bar/" + "1" * 40 + "/LICENSE-MIT",
        )
        self.assertIn("Pinned upstream MIT text.", artifacts.notices_path.read_text(encoding="utf-8"))

    def test_cargo_vcs_metadata_uses_real_top_level_path_and_git_shape(self) -> None:
        # Cargo writes path_in_vcs beside git, while sha1 is nested under git.
        metadata = self._write_override(vcs_path="")
        artifacts = self.collect(metadata)
        self.assertEqual(artifacts.inventory["dependency_count"], 1)

    def test_curated_override_rejects_cargo_vcs_path_mismatch(self) -> None:
        metadata = self._write_override(vcs_path="crates/foo_bar")
        with self.assertRaisesRegex(NoticeCollectionError, "does not match Cargo VCS path_in_vcs"):
            self.collect(metadata)

    def test_curated_override_rejects_cargo_vcs_commit_mismatch(self) -> None:
        metadata = self._write_override(vcs_commit="2" * 40)
        with self.assertRaisesRegex(NoticeCollectionError, "does not match Cargo VCS git.sha1"):
            self.collect(metadata)

    def test_missing_vcs_metadata_uses_only_explicit_tag_fallbacks(self) -> None:
        metadata = self._write_override(include_vcs=False)
        package = next(item for item in metadata["packages"] if item["name"] == "foo_bar")
        with self.assertRaisesRegex(NoticeCollectionError, "no pinned tag fallback"):
            _validate_package_path_in_vcs(package, ".", "1" * 40)

        for name, version, commit in (
            ("block", "0.1.6", "47178790cfc9d4a8b092051d8b413b78bd31254a"),
            ("malloc_buf", "0.0.6", "a7811e5f4c6f9685dd57ad9bfe34e9bf6b0ba4f9"),
        ):
            _validate_package_path_in_vcs(
                {**package, "name": name, "version": version}, ".", commit
            )

    def test_curated_override_rejects_url_or_hash_mismatch(self) -> None:
        metadata = self._write_override(source_url="https://example.invalid/LICENSE-MIT")
        with self.assertRaisesRegex(NoticeCollectionError, "source URL does not match"):
            self.collect(metadata)

    def test_curated_override_rejects_symlink_escape(self) -> None:
        external = Path(self.temporary.name) / "outside-license"
        external.write_bytes(b"Pinned upstream MIT text.\n")
        metadata = self._write_override(outside_symlink=external)
        with self.assertRaisesRegex(NoticeCollectionError, "escapes its directory"):
            self.collect(metadata)

    def test_declared_spdx_reference_includes_labeled_canonical_text(self) -> None:
        metadata = self.metadata(("foo_bar",))
        package = next(item for item in metadata["packages"] if item["name"] == "foo_bar")
        package["repository"] = "https://github.com/example/foo_bar"
        crate_root = self.package_roots["foo_bar"]
        (crate_root / "LICENSE-MIT").unlink()
        commit = "2" * 40
        declaration = (
            '[package]\nname = "foo_bar"\nversion = "1.0.0"\nlicense = "MIT OR Apache-2.0"\n'
        ).encode()
        (crate_root / ".cargo_vcs_info.json").write_text(
            json.dumps({"git": {"sha1": commit}, "path_in_vcs": ""}),
            encoding="utf-8",
        )
        spdx_text = b"Canonical SPDX MIT reference text.\n"
        apache_text = b"Canonical SPDX Apache reference text.\n"
        override_root = self.root / "third_party" / "license-overrides"
        (override_root / "evidence").mkdir(parents=True)
        (override_root / "references").mkdir()
        (override_root / "evidence" / "Cargo.toml").write_bytes(declaration)
        (override_root / "references" / "MIT.txt").write_bytes(spdx_text)
        (override_root / "references" / "Apache-2.0.txt").write_bytes(apache_text)
        package_source = "https://github.com/example/foo_bar"
        spdx_source = SPDX_LICENSE_REPOSITORY
        manifest = {
            "schema_version": 1,
            "overrides": [
                {
                    "name": "foo_bar",
                    "version": "1.0.0",
                    "license_expression": "MIT OR Apache-2.0",
                    "source_checksum_sha256": "a" * 64,
                    "source_repository": package_source,
                    "source_commit": commit,
                    "package_path_in_repository": ".",
                    "evidence_kind": "declared_spdx_reference",
                    "files": [],
                    "declaration_evidence": {
                        "path": "evidence/Cargo.toml",
                        "upstream_path": "Cargo.toml",
                        "source_url": f"https://raw.githubusercontent.com/example/foo_bar/{commit}/Cargo.toml",
                        "sha256": hashlib.sha256(declaration).hexdigest(),
                    },
                    "reference": {
                        "source_repository": spdx_source,
                        "source_commit": SPDX_LICENSE_COMMIT,
                        "files": [
                            {
                                "path": "references/MIT.txt",
                                "upstream_path": "text/MIT.txt",
                                "source_url": f"https://raw.githubusercontent.com/spdx/license-list-data/{SPDX_LICENSE_COMMIT}/text/MIT.txt",
                                "sha256": hashlib.sha256(spdx_text).hexdigest(),
                            },
                            {
                                "path": "references/Apache-2.0.txt",
                                "upstream_path": "text/Apache-2.0.txt",
                                "source_url": f"https://raw.githubusercontent.com/spdx/license-list-data/{SPDX_LICENSE_COMMIT}/text/Apache-2.0.txt",
                                "sha256": hashlib.sha256(apache_text).hexdigest(),
                            },
                        ],
                    },
                }
            ],
        }
        (override_root / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
        artifacts = self.collect(metadata)
        inventory = artifacts.inventory
        dependency = inventory["dependencies"][0]
        self.assertEqual(inventory["missing_license_text_count"], 0)
        self.assertEqual(inventory["reference_only_license_count"], 1)
        self.assertEqual(dependency["license_evidence"]["kind"], "declared_spdx_reference")
        self.assertEqual({item["provenance"]["spdx_id"] for item in dependency["reference_texts"]}, {"MIT", "Apache-2.0"})
        notices = artifacts.notices_path.read_text(encoding="utf-8")
        self.assertIn("Canonical SPDX reference text", notices)
        self.assertIn("not the crate's original license file", notices)

    def test_license_file_size_limit_is_enforced(self) -> None:
        (self.package_roots["foo_bar"] / "LICENSE-MIT").write_bytes(
            b"x" * (1024 * 1024 + 1)
        )
        with self.assertRaisesRegex(NoticeCollectionError, "1 MiB file limit"):
            self.collect(self.metadata(("foo_bar",)))

    def test_preexisting_output_is_not_overwritten(self) -> None:
        notices = self.output / NOTICES_FILENAME
        notices.write_text("keep this", encoding="utf-8")
        with self.assertRaisesRegex(NoticeCollectionError, "already exists"):
            self.collect()
        self.assertEqual(notices.read_text(encoding="utf-8"), "keep this")
        self.assertFalse((self.output / INVENTORY_FILENAME).exists())


if __name__ == "__main__":
    unittest.main()
