#!/usr/bin/env python3
"""Collect the Cargo-resolved CLI dependency notice set into a package staging directory.

This records a conservative Cargo graph inventory. Cargo feature sets can be unified across
workspace members, so the inventory is not a binary-symbol SBOM or a legal-compliance report.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping
from urllib.parse import urlsplit


REQUESTED_FEATURES = ("daedalus-cli/window",)
NOTICES_FILENAME = "THIRD_PARTY_NOTICES.txt"
INVENTORY_FILENAME = "dependency-inventory.json"
MAX_LICENSE_FILE_BYTES = 1024 * 1024
MAX_TOTAL_LICENSE_BYTES = 32 * 1024 * 1024
MAX_METADATA_TIMEOUT_SECONDS = 180
LICENSE_OVERRIDE_DIRECTORY = Path("third_party") / "license-overrides"
LICENSE_OVERRIDE_MANIFEST = "manifest.json"
SPDX_LICENSE_REPOSITORY = "https://github.com/spdx/license-list-data"
SPDX_LICENSE_COMMIT = "31ba1a50e5397e00a304dbadc76531740e89ee48"
SUPPORTED_SPDX_REFERENCE_IDS = {"MIT", "Apache-2.0", "CC0-1.0"}
# These published crates omit Cargo's .cargo_vcs_info.json. Their immutable upstream
# revisions are pinned in the curated manifest and are the explicit fallback evidence.
NO_VCS_INFO_TAG_FALLBACKS = {
    ("block", "0.1.6"): "47178790cfc9d4a8b092051d8b413b78bd31254a",
    ("malloc_buf", "0.0.6"): "a7811e5f4c6f9685dd57ad9bfe34e9bf6b0ba4f9",
}


class NoticeCollectionError(Exception):
    """A bounded dependency-notice collection failure."""


@dataclass(frozen=True)
class NoticeArtifacts:
    notices_path: Path
    inventory_path: Path
    inventory: dict[str, Any]


def collect_cli_notices(
    *,
    repository: Path,
    output_directory: Path,
    target: str,
    metadata: Mapping[str, Any] | Path | None = None,
    cargo_executable: str = "cargo",
    strict: bool = True,
) -> NoticeArtifacts:
    """Create third-party notices and a sanitized inventory in an existing staging directory.

    `metadata` may be a Cargo metadata object or JSON path. If omitted, the helper runs a
    locked, platform-filtered Cargo metadata query for `daedalus-cli/window`.
    `strict=False` is for local diagnostics only and records every missing license text.
    """

    repo = _resolve_directory(repository, "repository")
    output = _resolve_directory(output_directory, "staging directory")
    if not isinstance(strict, bool):
        raise NoticeCollectionError("strict must be a boolean")
    if not _valid_target(target):
        raise NoticeCollectionError("target triple contains unsupported characters")
    notices_path = output / NOTICES_FILENAME
    inventory_path = output / INVENTORY_FILENAME
    for path in (notices_path, inventory_path):
        if path.exists() or path.is_symlink():
            raise NoticeCollectionError("dependency notice output already exists; refusing to overwrite")

    metadata_document = _load_metadata(
        metadata,
        repository=repo,
        target=target,
        cargo_executable=cargo_executable,
    )
    packages, edges, activated_features = _reachable_packages(metadata_document, repo)
    lock_entries = _load_lock_entries(repo / "Cargo.lock")
    cargo_home = _cargo_home()
    license_overrides = _load_license_overrides(repo)
    inventory_packages: list[dict[str, Any]] = []
    notice_sections: list[
        tuple[
            dict[str, Any],
            list[tuple[str, bytes, str]],
            list[tuple[str, bytes, str]],
        ]
    ] = []
    total_license_bytes = 0
    used_section_names: set[str] = set()
    missing_license_packages: list[dict[str, Any]] = []
    reference_only_packages: list[dict[str, Any]] = []

    for package in sorted(packages, key=lambda item: (str(item.get("name")), str(item.get("version")), str(item.get("id")))):
        source = package.get("source")
        if source is None:
            # Workspace source is represented by the archive's top-level Daedalus notices.
            manifest_path = _package_manifest_path(package)
            if not _is_relative_to(manifest_path, repo):
                raise NoticeCollectionError(
                    "resolved local path dependency outside the repository requires manual license review"
                )
            continue
        if not isinstance(source, str):
            raise NoticeCollectionError("resolved dependency has an invalid Cargo source")
        source_kind = _source_kind(source)
        manifest_path = _package_manifest_path(package)
        package_root = manifest_path.parent
        _validate_package_source_root(package_root, source_kind, repo, cargo_home)

        raw_id = package.get("id")
        if not isinstance(raw_id, str) or not raw_id:
            raise NoticeCollectionError("resolved dependency has an invalid Cargo package ID")
        section_name = _safe_package_filename(package, raw_id)
        if section_name in used_section_names:
            raise NoticeCollectionError("dependency package filename collision after sanitization")
        used_section_names.add(section_name)

        source_key = (str(package.get("name")), str(package.get("version")), source)
        checksum = lock_entries.get(source_key)
        license_files = _collect_license_files(package, package_root)
        override = license_overrides.get((str(package.get("name")), str(package.get("version"))))
        override_evidence = None
        reference_files: list[tuple[str, bytes, dict[str, str]]] = []
        if override is not None:
            override_files, reference_files, override_evidence = _apply_license_override(
                package, checksum, override
            )
            if not license_files:
                license_files.extend(override_files)

        node_id = raw_id
        features = sorted(set(activated_features.get(node_id, [])))
        sanitized_id = section_name
        package_edges = edges.get(node_id, [])
        edge_inventory = [
            {
                "from_package_id": edge["from_package_id"],
                "dependency_kinds": edge["dependency_kinds"],
                "target_conditions": edge["target_conditions"],
            }
            for edge in sorted(package_edges, key=lambda item: (item["from_package_id"], item["dependency_kinds"], item["target_conditions"]))
        ]
        license_metadata: list[dict[str, Any]] = []
        files_for_notices: list[tuple[str, bytes, str]] = []
        reference_metadata: list[dict[str, Any]] = []
        references_for_notices: list[tuple[str, bytes, str]] = []
        for filename, content, file_provenance in license_files:
            total_license_bytes += len(content)
            if total_license_bytes > MAX_TOTAL_LICENSE_BYTES:
                raise NoticeCollectionError("resolved dependency license texts exceed the 32 MiB total limit")
            digest = hashlib.sha256(content).hexdigest()
            file_item = {"filename": filename, "sha256": digest, "notice_section": section_name}
            if file_provenance is not None:
                file_item["provenance"] = file_provenance
            license_metadata.append(file_item)
            files_for_notices.append((filename, content, digest))

        for filename, content, file_provenance in reference_files:
            total_license_bytes += len(content)
            if total_license_bytes > MAX_TOTAL_LICENSE_BYTES:
                raise NoticeCollectionError("resolved dependency license texts exceed the 32 MiB total limit")
            digest = hashlib.sha256(content).hexdigest()
            reference_metadata.append(
                {
                    "filename": filename,
                    "sha256": digest,
                    "notice_section": section_name,
                    "provenance": file_provenance,
                }
            )
            references_for_notices.append((filename, content, digest))

        license_expression = package.get("license")
        if license_expression is not None and not isinstance(license_expression, str):
            raise NoticeCollectionError("resolved dependency has an invalid license expression")
        item = {
            "package_id": sanitized_id,
            "name": package["name"],
            "version": package["version"],
            "source_kind": source_kind,
            "license_expression": license_expression,
            "source_checksum_sha256": checksum,
            "activated_features": features,
            "reached_by": edge_inventory,
            "license_texts": license_metadata,
            "reference_texts": reference_metadata,
        }
        if not license_files and override_evidence is not None:
            item["license_evidence"] = override_evidence
        inventory_packages.append(item)
        if license_files or reference_files:
            notice_sections.append((item, files_for_notices, references_for_notices))
        if not license_files and override_evidence is not None:
            reference_only_packages.append(item)
        elif not license_files:
            missing_license_packages.append(
                {
                    "package_id": sanitized_id,
                    "name": package["name"],
                    "version": package["version"],
                    "source_kind": source_kind,
                    "license_expression": license_expression,
                    "source_checksum_sha256": checksum,
                    "activated_features": features,
                    "reached_by": edge_inventory,
                    "reason": "resolved crate source contains no declared license_file or root LICENSE*/NOTICE*/COPYING* text and no approved declaration reference matched",
                }
            )

    if missing_license_packages:
        missing = ", ".join(
            f"{item['name']}@{item['version']} ({item['license_expression'] or 'license expression not declared'})"
            for item in missing_license_packages
        )
        if strict:
            raise NoticeCollectionError(
                f"{len(missing_license_packages)} resolved dependencies have no license text: {missing}"
            )

    inventory = {
        "schema_version": 1,
        "target_triple": target,
        "requested_features": list(REQUESTED_FEATURES),
        "resolution_scope": "Cargo-resolved packages reachable from daedalus-cli for the requested target; workspace packages are handled by top-level Daedalus notices.",
        "feature_resolution": "conservative-workspace-feature-unified-superset; activated feature sets may include workspace unification and this is not a binary-symbol SBOM",
        "dependency_count": len(inventory_packages),
        "missing_license_text_count": len(missing_license_packages),
        "missing_license_texts": missing_license_packages,
        "reference_only_license_count": len(reference_only_packages),
        "reference_only_license_packages": [item["package_id"] for item in reference_only_packages],
        "dependencies": inventory_packages,
    }
    notices_bytes = _render_notices(
        notice_sections, target, missing_license_packages, reference_only_packages
    )
    inventory_bytes = (json.dumps(inventory, indent=2, sort_keys=True) + "\n").encode("utf-8")
    _write_new_outputs(
        ((notices_path, notices_bytes), (inventory_path, inventory_bytes))
    )
    return NoticeArtifacts(notices_path, inventory_path, inventory)


def _resolve_directory(path: Path, description: str) -> Path:
    try:
        resolved = path.resolve(strict=True)
    except OSError as error:
        raise NoticeCollectionError(f"{description} does not exist as a directory") from error
    if not resolved.is_dir():
        raise NoticeCollectionError(f"{description} does not exist as a directory")
    return resolved


def _valid_target(target: str) -> bool:
    return bool(target) and re.fullmatch(r"[A-Za-z0-9_+-]+", target) is not None


def _load_metadata(
    metadata: Mapping[str, Any] | Path | None,
    *,
    repository: Path,
    target: str,
    cargo_executable: str,
) -> dict[str, Any]:
    if isinstance(metadata, Mapping):
        document = dict(metadata)
    elif isinstance(metadata, Path):
        try:
            document = json.loads(metadata.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            raise NoticeCollectionError("Cargo metadata JSON input is unreadable or invalid") from error
    else:
        command = [
            cargo_executable,
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            target,
            "--manifest-path",
            os.fspath(repository / "Cargo.toml"),
            "--features",
            ",".join(REQUESTED_FEATURES),
        ]
        try:
            completed = subprocess.run(
                command,
                cwd=repository,
                check=False,
                capture_output=True,
                text=True,
                timeout=MAX_METADATA_TIMEOUT_SECONDS,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise NoticeCollectionError("could not obtain locked Cargo metadata") from error
        if completed.returncode != 0:
            raise NoticeCollectionError(
                f"cargo metadata failed with exit code {completed.returncode}"
            )
        try:
            document = json.loads(completed.stdout)
        except json.JSONDecodeError as error:
            raise NoticeCollectionError("cargo metadata returned invalid JSON") from error
    if not isinstance(document, dict) or document.get("version") != 1:
        raise NoticeCollectionError("Cargo metadata must use format version 1")
    return document


def _reachable_packages(
    metadata: Mapping[str, Any], repository: Path
) -> tuple[list[dict[str, Any]], dict[str, list[dict[str, Any]]], dict[str, list[str]]]:
    raw_packages = metadata.get("packages")
    raw_resolve = metadata.get("resolve")
    raw_nodes = raw_resolve.get("nodes") if isinstance(raw_resolve, dict) else None
    members = metadata.get("workspace_members")
    if not isinstance(raw_packages, list) or not isinstance(raw_nodes, list) or not isinstance(members, list):
        raise NoticeCollectionError("Cargo metadata is missing package or resolved-graph data")
    package_by_id = {
        package.get("id"): package
        for package in raw_packages
        if isinstance(package, dict) and isinstance(package.get("id"), str)
    }
    node_by_id = {
        node.get("id"): node
        for node in raw_nodes
        if isinstance(node, dict) and isinstance(node.get("id"), str)
    }
    cli_manifest = repository / "crates" / "daedalus-cli" / "Cargo.toml"
    root_ids = [
        package_id
        for package_id, package in package_by_id.items()
        if package.get("name") == "daedalus-cli"
        and _metadata_path_matches(package.get("manifest_path"), cli_manifest)
    ]
    if len(root_ids) != 1 or root_ids[0] not in members:
        raise NoticeCollectionError("Cargo metadata does not contain the workspace daedalus-cli root")
    root_id = root_ids[0]
    if root_id not in node_by_id:
        raise NoticeCollectionError("Cargo metadata resolution is missing the daedalus-cli root node")

    reachable: set[str] = set()
    edges: dict[str, list[dict[str, Any]]] = {}
    pending = [root_id]
    while pending:
        package_id = pending.pop()
        if package_id in reachable:
            continue
        node = node_by_id.get(package_id)
        package = package_by_id.get(package_id)
        if node is None or package is None:
            raise NoticeCollectionError("Cargo metadata contains a broken resolved dependency edge")
        reachable.add(package_id)
        dependencies = node.get("deps", [])
        if not isinstance(dependencies, list):
            raise NoticeCollectionError("Cargo metadata has malformed dependency edges")
        for dependency in dependencies:
            if not isinstance(dependency, dict) or not isinstance(dependency.get("pkg"), str):
                raise NoticeCollectionError("Cargo metadata has a malformed package edge")
            kinds: set[str] = set()
            targets: set[str] = set()
            dep_kinds = dependency.get("dep_kinds", [])
            if not isinstance(dep_kinds, list):
                raise NoticeCollectionError("Cargo metadata has malformed dependency kinds")
            for dep_kind in dep_kinds:
                if not isinstance(dep_kind, dict):
                    raise NoticeCollectionError("Cargo metadata has malformed dependency kind")
                kind = dep_kind.get("kind")
                # Development dependencies do not enter the packaged binary.
                if kind == "dev":
                    continue
                kinds.add(kind if isinstance(kind, str) else "normal")
                condition = dep_kind.get("target")
                targets.add(condition if isinstance(condition, str) else "all")
            if not kinds:
                continue
            dependency_id = dependency["pkg"]
            edges.setdefault(dependency_id, []).append(
                {
                    "from_package_id": _safe_package_filename(package, package_id),
                    "dependency_kinds": sorted(kinds),
                    "target_conditions": sorted(targets),
                }
            )
            if dependency_id not in reachable:
                pending.append(dependency_id)

    packages = [package_by_id[package_id] for package_id in reachable]
    activated_features = {
        package_id: node.get("features", [])
        for package_id, node in node_by_id.items()
        if package_id in reachable and isinstance(node.get("features", []), list)
    }
    return packages, edges, activated_features


def _metadata_path_matches(raw_path: Any, expected: Path) -> bool:
    if not isinstance(raw_path, str):
        return False
    try:
        return Path(raw_path).resolve(strict=True) == expected.resolve(strict=True)
    except OSError:
        return False


def _load_lock_entries(lock_path: Path) -> dict[tuple[str, str, str], str | None]:
    try:
        with lock_path.open("rb") as lock_file:
            lock = tomllib.load(lock_file)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise NoticeCollectionError("Cargo.lock is missing or invalid") from error
    packages = lock.get("package")
    if not isinstance(packages, list):
        raise NoticeCollectionError("Cargo.lock has no package list")
    entries: dict[tuple[str, str, str], str | None] = {}
    for package in packages:
        if not isinstance(package, dict):
            continue
        name, version, source = package.get("name"), package.get("version"), package.get("source")
        checksum = package.get("checksum")
        if not isinstance(name, str) or not isinstance(version, str) or not isinstance(source, str):
            continue
        if checksum is not None and (
            not isinstance(checksum, str) or re.fullmatch(r"[0-9a-f]{64}", checksum) is None
        ):
            raise NoticeCollectionError("Cargo.lock contains a malformed source checksum")
        entries[(name, version, source)] = checksum
    return entries


def _load_license_overrides(repository: Path) -> dict[tuple[str, str], dict[str, Any]]:
    override_root = repository / LICENSE_OVERRIDE_DIRECTORY
    manifest_path = override_root / LICENSE_OVERRIDE_MANIFEST
    if not override_root.exists() and not override_root.is_symlink():
        return {}
    if not manifest_path.exists() and not manifest_path.is_symlink():
        return {}
    try:
        resolved_root = override_root.resolve(strict=True)
        if not _is_relative_to(resolved_root, repository):
            raise NoticeCollectionError("license override directory escapes the repository")
        resolved_manifest = manifest_path.resolve(strict=True)
        if not _is_relative_to(resolved_manifest, resolved_root) or not resolved_manifest.is_file():
            raise NoticeCollectionError("license override manifest escapes its directory or is not a file")
        if resolved_manifest.stat().st_size > MAX_LICENSE_FILE_BYTES:
            raise NoticeCollectionError("license override manifest exceeds the 1 MiB limit")
        document = json.loads(resolved_manifest.read_text(encoding="utf-8"))
    except NoticeCollectionError:
        raise
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise NoticeCollectionError("license override manifest is unreadable or invalid") from error
    if not isinstance(document, dict) or document.get("schema_version") != 1:
        raise NoticeCollectionError("license override manifest must use schema version 1")
    records = document.get("overrides")
    if not isinstance(records, list):
        raise NoticeCollectionError("license override manifest has no overrides list")

    overrides: dict[tuple[str, str], dict[str, Any]] = {}
    for record in records:
        if not isinstance(record, dict):
            raise NoticeCollectionError("license override record must be an object")
        name, version = record.get("name"), record.get("version")
        license_expression = record.get("license_expression")
        checksum = record.get("source_checksum_sha256")
        repository_url = record.get("source_repository")
        commit = record.get("source_commit")
        package_path = _safe_package_repo_path(record.get("package_path_in_repository"))
        evidence_kind = record.get("evidence_kind")
        files = record.get("files", [])
        if not all(isinstance(value, str) and value for value in (name, version, license_expression)):
            raise NoticeCollectionError("license override identity fields must be non-empty strings")
        if not isinstance(checksum, str) or re.fullmatch(r"[0-9a-f]{64}", checksum) is None:
            raise NoticeCollectionError("license override has an invalid Cargo source checksum")
        if not isinstance(repository_url, str) or not _valid_repository_url(repository_url):
            raise NoticeCollectionError("license override has an invalid source repository URL")
        if not isinstance(commit, str) or re.fullmatch(r"[0-9a-f]{40}", commit) is None:
            raise NoticeCollectionError("license override source commit must be a 40-character lowercase hex SHA")
        if evidence_kind not in ("upstream_text", "declared_spdx_reference"):
            raise NoticeCollectionError("license override has an unsupported evidence kind")
        if not isinstance(files, list):
            raise NoticeCollectionError("license override files must be a list")
        if evidence_kind == "declared_spdx_reference" and files:
            raise NoticeCollectionError("declaration-only override must not claim package source text")
        identity = (name, version)
        if identity in overrides:
            raise NoticeCollectionError("license override manifest has duplicate package identities")

        loaded_files: list[tuple[str, bytes, dict[str, str]]] = []
        license_evidence: dict[str, Any] | None = None
        loaded_references: list[tuple[str, bytes, dict[str, str]]] = []
        seen_upstream_paths: set[str] = set()
        for file_record in files or []:
            if not isinstance(file_record, dict):
                raise NoticeCollectionError("license override file record must be an object")
            relative_path = _safe_relative_path(file_record.get("path"), "license override path")
            upstream_path = _safe_relative_path(file_record.get("upstream_path"), "upstream license path")
            source_url = file_record.get("source_url")
            digest = file_record.get("sha256")
            if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None:
                raise NoticeCollectionError("license override file has an invalid SHA-256")
            if not isinstance(source_url, str) or not source_url.startswith("https://"):
                raise NoticeCollectionError("license override file has an invalid pinned source URL")
            if source_url != _github_raw_url(repository_url, commit, upstream_path):
                raise NoticeCollectionError("license override source URL does not match repository, commit, and upstream path")
            if upstream_path in seen_upstream_paths:
                raise NoticeCollectionError("license override repeats an upstream license path")
            seen_upstream_paths.add(upstream_path)
            try:
                source_path = (resolved_root / relative_path).resolve(strict=True)
                if not _is_relative_to(source_path, resolved_root) or not source_path.is_file():
                    raise NoticeCollectionError("license override text escapes its directory or is not a file")
                if source_path.stat().st_size > MAX_LICENSE_FILE_BYTES:
                    raise NoticeCollectionError("license override text exceeds the 1 MiB file limit")
                content = source_path.read_bytes()
            except NoticeCollectionError:
                raise
            except OSError as error:
                raise NoticeCollectionError("license override text is unavailable") from error
            if len(content) > MAX_LICENSE_FILE_BYTES:
                raise NoticeCollectionError("license override text exceeds the 1 MiB file limit")
            try:
                decoded = content.decode("utf-8")
            except UnicodeDecodeError as error:
                raise NoticeCollectionError("license override text is not UTF-8") from error
            if "\x00" in decoded:
                raise NoticeCollectionError("license override text contains binary data")
            if hashlib.sha256(content).hexdigest() != digest:
                raise NoticeCollectionError("license override text SHA-256 does not match its manifest")
            provenance = {
                "kind": "package_upstream_text",
                "source_repository": repository_url,
                "source_commit": commit,
                "package_path_in_repository": package_path,
                "upstream_path": upstream_path,
                "source_url": source_url,
            }
            loaded_files.append((Path(upstream_path).name, content, provenance))
        reference_record = record.get("reference")
        if reference_record is not None:
            loaded_references = _load_spdx_reference_files(
                resolved_root, reference_record, license_expression
            )
        if evidence_kind == "declared_spdx_reference":
            declaration_record = record.get("declaration_evidence")
            declaration_content, declaration = _read_curated_evidence_file(
                resolved_root,
                declaration_record,
                repository_url,
                commit,
                "Cargo.toml declaration evidence",
            )
            expected_manifest_path = (
                "Cargo.toml" if package_path == "." else f"{package_path}/Cargo.toml"
            )
            if declaration["upstream_path"] != expected_manifest_path:
                raise NoticeCollectionError("SPDX declaration evidence path does not match package_path_in_repository")
            try:
                cargo_manifest = tomllib.loads(declaration_content.decode("utf-8"))
            except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
                raise NoticeCollectionError("SPDX declaration Cargo.toml evidence is invalid TOML") from error
            declared_package = cargo_manifest.get("package")
            if (
                not isinstance(declared_package, dict)
                or declared_package.get("license") != license_expression
                or declared_package.get("name") != name
                or declared_package.get("version") != version
            ):
                raise NoticeCollectionError("Cargo.toml declaration evidence does not match package identity or license")
            if not loaded_references:
                raise NoticeCollectionError("declaration-only override must include canonical SPDX reference text")
            if _spdx_license_ids(license_expression) != _spdx_reference_ids(loaded_references):
                raise NoticeCollectionError("declared SPDX references do not cover the Cargo license expression")
            license_evidence = {
                "kind": "declared_spdx_reference",
                "package_path_in_repository": package_path,
                "license_expression": license_expression,
                "declaration_evidence": declaration,
            }
        elif loaded_references:
            referenced_ids = _spdx_reference_ids(loaded_references)
            if not referenced_ids.issubset(_spdx_license_ids(license_expression)):
                raise NoticeCollectionError("supplemental SPDX reference does not match the Cargo license expression")
        overrides[identity] = {
            "license_expression": license_expression,
            "source_checksum_sha256": checksum,
            "source_repository": repository_url,
            "source_commit": commit,
            "package_path_in_repository": package_path,
            "evidence_kind": evidence_kind,
            "files": loaded_files,
            "references": loaded_references,
            "license_evidence": license_evidence,
        }
    return overrides


def _read_curated_evidence_file(
    override_root: Path,
    record: Any,
    repository_url: str,
    commit: str,
    description: str,
) -> tuple[bytes, dict[str, str]]:
    if not isinstance(record, dict):
        raise NoticeCollectionError(f"{description} record must be an object")
    relative_path = _safe_relative_path(record.get("path"), f"{description} path")
    upstream_path = _safe_relative_path(record.get("upstream_path"), f"{description} upstream path")
    source_url = record.get("source_url")
    digest = record.get("sha256")
    if not isinstance(source_url, str) or not source_url.startswith("https://"):
        raise NoticeCollectionError(f"{description} has an invalid pinned source URL")
    if source_url != _github_raw_url(repository_url, commit, upstream_path):
        raise NoticeCollectionError(f"{description} source URL does not match repository, commit, and path")
    if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None:
        raise NoticeCollectionError(f"{description} has an invalid SHA-256")
    try:
        source_path = (override_root / relative_path).resolve(strict=True)
        if not _is_relative_to(source_path, override_root) or not source_path.is_file():
            raise NoticeCollectionError(f"{description} escapes its directory or is not a file")
        if source_path.stat().st_size > MAX_LICENSE_FILE_BYTES:
            raise NoticeCollectionError(f"{description} exceeds the 1 MiB file limit")
        content = source_path.read_bytes()
    except NoticeCollectionError:
        raise
    except OSError as error:
        raise NoticeCollectionError(f"{description} is unavailable") from error
    if len(content) > MAX_LICENSE_FILE_BYTES:
        raise NoticeCollectionError(f"{description} exceeds the 1 MiB file limit")
    try:
        decoded = content.decode("utf-8")
    except UnicodeDecodeError as error:
        raise NoticeCollectionError(f"{description} is not UTF-8") from error
    if "\x00" in decoded:
        raise NoticeCollectionError(f"{description} contains binary data")
    if hashlib.sha256(content).hexdigest() != digest:
        raise NoticeCollectionError(f"{description} SHA-256 does not match its manifest")
    return content, {"upstream_path": upstream_path, "source_url": source_url, "sha256": digest}


def _load_spdx_reference_files(
    override_root: Path, record: Any, license_expression: str
) -> list[tuple[str, bytes, dict[str, str]]]:
    if not isinstance(record, dict):
        raise NoticeCollectionError("SPDX reference must be an object")
    repository_url = record.get("source_repository")
    commit = record.get("source_commit")
    files = record.get("files")
    if repository_url != SPDX_LICENSE_REPOSITORY:
        raise NoticeCollectionError("SPDX references must use the approved official license-list-data repository")
    if commit != SPDX_LICENSE_COMMIT:
        raise NoticeCollectionError("SPDX references must use the approved pinned license-list-data commit")
    if not isinstance(files, list) or not files:
        raise NoticeCollectionError("SPDX reference must include at least one canonical license text")

    output: list[tuple[str, bytes, dict[str, str]]] = []
    seen_ids: set[str] = set()
    for file_record in files:
        content, provenance = _read_curated_evidence_file(
            override_root,
            file_record,
            SPDX_LICENSE_REPOSITORY,
            SPDX_LICENSE_COMMIT,
            "canonical SPDX license text",
        )
        upstream_path = provenance["upstream_path"]
        match = re.fullmatch(r"text/([A-Za-z0-9.-]+)\.txt", upstream_path)
        if match is None:
            raise NoticeCollectionError("SPDX license text path must be text/<license-id>.txt")
        spdx_id = match.group(1)
        if spdx_id not in SUPPORTED_SPDX_REFERENCE_IDS:
            raise NoticeCollectionError("SPDX license reference ID is not in the explicitly supported set")
        if spdx_id not in _spdx_license_ids(license_expression):
            raise NoticeCollectionError("SPDX reference ID is not declared by the resolved package")
        if spdx_id in seen_ids:
            raise NoticeCollectionError("SPDX reference repeats a license ID")
        seen_ids.add(spdx_id)
        provenance.update(
            {
                "kind": "official_spdx_reference_text",
                "source_repository": SPDX_LICENSE_REPOSITORY,
                "source_commit": SPDX_LICENSE_COMMIT,
                "spdx_id": spdx_id,
            }
        )
        output.append((f"SPDX-{spdx_id}.txt", content, provenance))
    return output


def _spdx_license_ids(expression: str) -> set[str]:
    tokens = re.findall(r"[A-Za-z0-9][A-Za-z0-9.-]*", expression)
    identifiers = {token for token in tokens if token not in {"AND", "OR", "WITH"}}
    if not identifiers or not identifiers.issubset(SUPPORTED_SPDX_REFERENCE_IDS):
        raise NoticeCollectionError("SPDX reference mode supports only MIT, Apache-2.0, and CC0-1.0 expressions")
    return identifiers


def _spdx_reference_ids(files: list[tuple[str, bytes, dict[str, str]]]) -> set[str]:
    return {file_record[2]["spdx_id"] for file_record in files}


def _safe_package_repo_path(value: Any) -> str:
    if value == ".":
        return "."
    return _safe_relative_path(value, "package_path_in_repository")


def _safe_relative_path(value: Any, description: str) -> str:
    if not isinstance(value, str) or not value or "\\" in value:
        raise NoticeCollectionError(f"{description} must be a safe relative POSIX path")
    path = Path(value)
    if path.is_absolute() or any(
        part in ("", ".", "..") or re.fullmatch(r"[A-Za-z0-9_.-]+", part) is None
        for part in path.parts
    ):
        raise NoticeCollectionError(f"{description} must be a safe relative POSIX path")
    return path.as_posix()


def _github_raw_url(repository_url: str, commit: str, upstream_path: str) -> str:
    parsed = urlsplit(repository_url)
    if parsed.hostname is None or parsed.hostname.lower() != "github.com":
        raise NoticeCollectionError("curated upstream license sources must be on github.com")
    repository_path = parsed.path.strip("/")
    if repository_path.endswith(".git"):
        repository_path = repository_path[:-4]
    parts = repository_path.split("/")
    if len(parts) != 2 or any(re.fullmatch(r"[A-Za-z0-9_.-]+", part) is None for part in parts):
        raise NoticeCollectionError("curated GitHub repository must name one owner and repository")
    return f"https://raw.githubusercontent.com/{parts[0]}/{parts[1]}/{commit}/{upstream_path}"


def _valid_repository_url(value: str) -> bool:
    try:
        parsed = urlsplit(value)
    except ValueError:
        return False
    return (
        parsed.scheme in ("http", "https")
        and bool(parsed.hostname)
        and parsed.username is None
        and parsed.password is None
        and not parsed.query
        and not parsed.fragment
        and not any(character.isspace() for character in value)
    )


def _apply_license_override(
    package: Mapping[str, Any],
    checksum: str | None,
    override: Mapping[str, Any],
) -> tuple[
    list[tuple[str, bytes, dict[str, str]]],
    list[tuple[str, bytes, dict[str, str]]],
    dict[str, Any] | None,
]:
    if override.get("license_expression") != package.get("license"):
        raise NoticeCollectionError("curated license override license expression does not match Cargo metadata")
    if checksum is None or override.get("source_checksum_sha256") != checksum:
        raise NoticeCollectionError("curated license override checksum does not match Cargo.lock")
    if override.get("source_repository") != package.get("repository"):
        raise NoticeCollectionError("curated license override repository does not match Cargo metadata")
    package_path = override.get("package_path_in_repository")
    if not isinstance(package_path, str):
        raise NoticeCollectionError("curated license override has no package path in upstream repository")
    source_commit = override.get("source_commit")
    if not isinstance(source_commit, str):
        raise NoticeCollectionError("curated license override has no pinned source commit")
    _validate_package_path_in_vcs(package, package_path, source_commit)
    if override.get("evidence_kind") == "declared_spdx_reference":
        evidence = override.get("license_evidence")
        if not isinstance(evidence, dict):
            raise NoticeCollectionError("curated SPDX reference has no validated evidence")
        return [], list(override.get("references", [])), evidence
    if override.get("evidence_kind") != "upstream_text":
        raise NoticeCollectionError("curated license override has an unsupported evidence kind")
    return list(override["files"]), list(override.get("references", [])), None


def _validate_package_path_in_vcs(
    package: Mapping[str, Any], expected: str, expected_commit: str
) -> None:
    manifest_path = _package_manifest_path(package)
    vcs_path = manifest_path.parent / ".cargo_vcs_info.json"
    if not vcs_path.exists() and not vcs_path.is_symlink():
        identity = (package.get("name"), package.get("version"))
        if NO_VCS_INFO_TAG_FALLBACKS.get(identity) == expected_commit:
            return
        raise NoticeCollectionError(
            "Cargo VCS metadata is missing and no pinned tag fallback is approved for this package"
        )
    if vcs_path.is_symlink():
        raise NoticeCollectionError("Cargo VCS metadata must not be a symlink")
    try:
        resolved_vcs_path = vcs_path.resolve(strict=True)
        if resolved_vcs_path != vcs_path.parent.resolve(strict=True) / vcs_path.name:
            raise NoticeCollectionError("Cargo VCS metadata resolves outside its crate source")
    except OSError as error:
        raise NoticeCollectionError("Cargo VCS metadata is unavailable") from error
    try:
        if resolved_vcs_path.stat().st_size > MAX_LICENSE_FILE_BYTES:
            raise NoticeCollectionError("Cargo VCS metadata exceeds the 1 MiB limit")
        vcs = json.loads(resolved_vcs_path.read_text(encoding="utf-8"))
    except NoticeCollectionError:
        raise
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise NoticeCollectionError("resolved dependency Cargo VCS metadata is invalid") from error
    if not isinstance(vcs, dict):
        raise NoticeCollectionError("resolved dependency Cargo VCS metadata is invalid")
    git = vcs.get("git")
    source_commit = git.get("sha1") if isinstance(git, dict) else None
    if not isinstance(source_commit, str) or re.fullmatch(r"[0-9a-f]{40}", source_commit) is None:
        raise NoticeCollectionError("Cargo VCS metadata has no valid git.sha1 commit")
    if source_commit != expected_commit:
        raise NoticeCollectionError("curated package source commit does not match Cargo VCS git.sha1")
    if "path_in_vcs" not in vcs:
        return
    path_in_vcs = vcs.get("path_in_vcs")
    if not isinstance(path_in_vcs, str):
        raise NoticeCollectionError("Cargo VCS metadata has an invalid path_in_vcs")
    actual = _safe_package_repo_path(path_in_vcs or ".")
    if actual != expected:
        raise NoticeCollectionError("curated package path does not match Cargo VCS path_in_vcs")


def _cargo_home() -> Path:
    raw = os.environ.get("CARGO_HOME")
    candidate = Path(raw).expanduser() if raw else Path.home() / ".cargo"
    try:
        return candidate.resolve(strict=True)
    except OSError as error:
        raise NoticeCollectionError("Cargo source cache directory is unavailable") from error


def _source_kind(source: str) -> str:
    if source.startswith("registry+"):
        return "registry"
    if source.startswith("git+"):
        return "git"
    raise NoticeCollectionError("resolved dependency uses an unsupported Cargo source kind")


def _package_manifest_path(package: Mapping[str, Any]) -> Path:
    raw_path = package.get("manifest_path")
    if not isinstance(raw_path, str) or not Path(raw_path).is_absolute():
        raise NoticeCollectionError("resolved dependency manifest path is not an absolute Cargo path")
    try:
        manifest_path = Path(raw_path).resolve(strict=True)
    except OSError as error:
        raise NoticeCollectionError("resolved dependency source manifest is unavailable") from error
    if not manifest_path.is_file() or manifest_path.name != "Cargo.toml":
        raise NoticeCollectionError("resolved dependency manifest path is not a regular Cargo manifest")
    return manifest_path


def _validate_package_source_root(
    package_root: Path, source_kind: str, repository: Path, cargo_home: Path
) -> None:
    try:
        resolved_root = package_root.resolve(strict=True)
        if source_kind == "registry":
            cache_root = cargo_home / "registry" / "src"
        else:
            cache_root = cargo_home / "git" / "checkouts"
        if cache_root.exists():
            allowed = cache_root.resolve(strict=True)
            if _is_relative_to(resolved_root, allowed):
                return
        vendor_root = repository / "vendor"
        if vendor_root.is_dir() and _is_relative_to(resolved_root, vendor_root.resolve(strict=True)):
            return
    except OSError as error:
        raise NoticeCollectionError("resolved dependency source root is unavailable") from error
    raise NoticeCollectionError("resolved dependency source lies outside Cargo caches or repository vendor tree")


def _collect_license_files(
    package: Mapping[str, Any], package_root: Path
) -> list[tuple[str, bytes, dict[str, str] | None]]:
    candidates: dict[str, Path] = {}
    declared = package.get("license_file")
    if declared is not None:
        if not isinstance(declared, str) or not declared:
            raise NoticeCollectionError("resolved dependency declares an invalid license-file path")
        declared_path = Path(declared)
        if declared_path.is_absolute():
            candidate = declared_path
        else:
            candidate = package_root / declared_path
        resolved = _safe_license_path(candidate, package_root)
        candidates[resolved.relative_to(package_root).as_posix()] = resolved

    try:
        root_entries = sorted(package_root.iterdir(), key=lambda path: path.name.casefold())
    except OSError as error:
        raise NoticeCollectionError("resolved dependency source directory cannot be read") from error
    for entry in root_entries:
        upper_name = entry.name.upper()
        if not upper_name.startswith(("LICENSE", "NOTICE", "COPYING")):
            continue
        resolved = _safe_license_path(entry, package_root)
        if not resolved.is_file():
            continue
        candidates[resolved.relative_to(package_root).as_posix()] = resolved

    output: list[tuple[str, bytes, dict[str, str] | None]] = []
    for relative_name, path in sorted(candidates.items(), key=lambda item: item[0].casefold()):
        try:
            if not path.is_file():
                raise NoticeCollectionError(f"declared license text {Path(relative_name).name} is not a regular file")
            size = path.stat().st_size
            if size > MAX_LICENSE_FILE_BYTES:
                raise NoticeCollectionError(f"license text {Path(relative_name).name} exceeds the 1 MiB file limit")
            content = path.read_bytes()
        except OSError as error:
            raise NoticeCollectionError(f"could not read license text {Path(relative_name).name}") from error
        if len(content) > MAX_LICENSE_FILE_BYTES:
            raise NoticeCollectionError(f"license text {Path(relative_name).name} exceeds the 1 MiB file limit")
        try:
            decoded = content.decode("utf-8")
        except UnicodeDecodeError as error:
            raise NoticeCollectionError(f"license text {Path(relative_name).name} is not UTF-8 text") from error
        if "\x00" in decoded:
            raise NoticeCollectionError(f"license text {Path(relative_name).name} contains binary data")
        output.append((Path(relative_name).name, content, None))
    return output


def _safe_license_path(path: Path, package_root: Path) -> Path:
    try:
        resolved = path.resolve(strict=True)
    except OSError as error:
        raise NoticeCollectionError(f"license text {path.name} is missing or has a broken symlink") from error
    root = package_root.resolve(strict=True)
    if not _is_relative_to(resolved, root):
        raise NoticeCollectionError(f"license text {path.name} escapes its resolved crate source")
    return resolved


def _is_relative_to(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


def _safe_package_filename(package: Mapping[str, Any], raw_id: str) -> str:
    name = str(package.get("name", "package"))
    version = str(package.get("version", "unknown"))
    slug = re.sub(r"[^a-z0-9]+", "-", f"{name}-{version}".lower()).strip("-")
    slug = (slug or "package")[:64].rstrip("-") or "package"
    identity = hashlib.sha256(raw_id.encode("utf-8", "strict")).hexdigest()[:16]
    return f"{slug}-{identity}"


def _render_notices(
    sections: list[
        tuple[
            dict[str, Any],
            list[tuple[str, bytes, str]],
            list[tuple[str, bytes, str]],
        ]
    ],
    target: str,
    missing_license_packages: list[dict[str, Any]],
    reference_only_packages: list[dict[str, Any]],
) -> bytes:
    output = [
        "Daedalus CLI third-party notices",
        "",
        f"Target: {target}",
        "This file contains license and notice texts for Cargo-resolved dependencies reachable from the CLI window build.",
        "The included package set is conservative and feature-unified; it is not a binary-symbol SBOM or a legal-compliance certification.",
        "",
    ]
    if missing_license_packages:
        output.extend(
            [
                f"WARNING: {len(missing_license_packages)} resolved dependencies have no collected license text.",
                "No text has been inferred or substituted. See dependency-inventory.json `missing_license_texts` for package identities and reasons.",
                "",
            ]
        )
    if reference_only_packages:
        output.extend(
            [
                f"REFERENCE-ONLY LICENSE TEXT: {len(reference_only_packages)} dependencies have no original license file in the resolved crate source.",
                "The canonical SPDX reference text is included below; it is not the crate's original license file and does not supply package-specific copyright attribution.",
                "",
            ]
        )
    for package, files, reference_files in sections:
        output.extend(
            [
                f"## {package['package_id']} — {package['name']} {package['version']}",
                f"License expression: {package['license_expression'] or 'not declared'}",
                f"Source kind: {package['source_kind']}",
                f"Cargo.lock source checksum: {package['source_checksum_sha256'] or 'not recorded'}",
                "",
            ]
        )
        evidence = package.get("license_evidence")
        if evidence is not None:
            declaration = evidence["declaration_evidence"]
            output.extend(
                [
                    "License evidence kind: declared_spdx_reference",
                    f"Pinned Cargo.toml declaration: {declaration['source_url']}",
                    f"Cargo.toml evidence SHA-256: {declaration['sha256']}",
                    "No original upstream license file was found for this package; canonical SPDX text follows as reference only.",
                    "",
                ]
            )
        for filename, content, digest in files:
            output.extend(
                [
                    f"### {filename}",
                    f"SHA-256: {digest}",
                    "",
                    content.decode("utf-8").rstrip("\n"),
                    "",
                ]
            )
        for filename, content, digest in reference_files:
            reference_metadata = next(
                item
                for item in package["reference_texts"]
                if item["filename"] == filename and item["sha256"] == digest
            )
            provenance = reference_metadata["provenance"]
            output.extend(
                [
                    f"### Canonical SPDX reference text — {filename}",
                    "Evidence kind: reference-only canonical SPDX text; not a copied original package license file.",
                    f"Pinned source: {provenance['source_url']}",
                    f"SHA-256: {digest}",
                    "",
                    content.decode("utf-8").rstrip("\n"),
                    "",
                ]
            )
    for package in missing_license_packages:
        output.extend(
            [
                f"## {package['package_id']} — {package['name']} {package['version']}",
                f"License expression: {package['license_expression'] or 'not declared'}",
                f"Source kind: {package['source_kind']}",
                f"Cargo.lock source checksum: {package['source_checksum_sha256'] or 'not recorded'}",
                "License text unavailable in the resolved crate source; no text has been created for this package.",
                "",
            ]
        )
    return ("\n".join(output).rstrip() + "\n").encode("utf-8")


def _write_new_outputs(outputs: tuple[tuple[Path, bytes], ...]) -> None:
    created: list[Path] = []
    try:
        for path, content in outputs:
            with path.open("xb") as output_file:
                created.append(path)
                output_file.write(content)
    except OSError as error:
        for path in created:
            try:
                path.unlink()
            except OSError:
                pass
        raise NoticeCollectionError("could not create dependency notice outputs without overwriting files") from error


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Collect Cargo-resolved CLI third-party notices")
    parser.add_argument("--repository", type=Path, default=Path.cwd())
    parser.add_argument("--output-directory", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--metadata-json", type=Path, help="use a previously captured Cargo metadata JSON document")
    parser.add_argument("--cargo", default="cargo", help="Cargo executable used when metadata JSON is omitted")
    parser.add_argument(
        "--allow-missing-notices",
        action="store_true",
        help="local diagnostics only: write an explicit missing-license list instead of failing",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    arguments = _parse_args(argv)
    try:
        artifacts = collect_cli_notices(
            repository=arguments.repository,
            output_directory=arguments.output_directory,
            target=arguments.target,
            metadata=arguments.metadata_json,
            cargo_executable=arguments.cargo,
            strict=not arguments.allow_missing_notices,
        )
    except NoticeCollectionError as error:
        print(f"collect_cli_notices: {error}", file=sys.stderr)
        return 1
    print(f"wrote {artifacts.notices_path.name}")
    print(f"wrote {artifacts.inventory_path.name}")
    print(f"inventoried {artifacts.inventory['dependency_count']} resolved dependencies")
    if artifacts.inventory["missing_license_text_count"]:
        print(
            f"warning: {artifacts.inventory['missing_license_text_count']} dependencies have no collected license text",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
