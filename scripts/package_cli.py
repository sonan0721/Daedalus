#!/usr/bin/env python3
"""Build and package a native Daedalus CLI release candidate."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import uuid
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Mapping, Sequence

from collect_cli_notices import NoticeCollectionError, collect_cli_notices


SCHEMA_VERSION = 1
SUPPORTED_TARGETS = frozenset(
    {
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
    }
)
PACKAGE_FEATURES = ("capture",)
ARCHIVE_BINARY = "daedalus"
ARCHIVE_USAGE = "USAGE.txt"
ARCHIVE_MANIFEST = "build-manifest.json"
ARCHIVE_THIRD_PARTY_NOTICES = "THIRD_PARTY_NOTICES.txt"
ARCHIVE_DEPENDENCY_INVENTORY = "dependency-inventory.json"
ARCHIVE_LICENSES = ("LICENSE-MIT", "LICENSE-APACHE")
RUST_PATH_REMAP_POLICY = {
    "policy_version": 1,
    "prefixes": [
        {"scope": "build-home", "remapped_prefix": "/daedalus/build-home"},
        {"scope": "cargo-home", "remapped_prefix": "/daedalus/cargo"},
        {"scope": "rustup-home", "remapped_prefix": "/daedalus/rustup"},
        {"scope": "repository", "remapped_prefix": "/daedalus/source"},
    ],
    "rustflags_policy": (
        "CARGO_ENCODED_RUSTFLAGS takes precedence, including when empty; otherwise RUSTFLAGS "
        "is split on whitespace. Packaging remap flags are appended in a build-only environment "
        "and override Cargo configuration build.rustflags."
    ),
}
SEMVER_PATTERN = re.compile(
    r"^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    r"(?:-(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)
COMMIT_PATTERN = re.compile(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")
BUILD_ID_PATTERN = re.compile(r"^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{32}$")


class PackageError(Exception):
    """A packaging error with the exit status of a failed child process, if any."""

    def __init__(self, message: str, exit_code: int = 1) -> None:
        super().__init__(message)
        self.exit_code = exit_code


@dataclass(frozen=True)
class PackagePaths:
    output_directory: Path
    archive: Path
    checksum: Path
    manifest: dict[str, Any]


def run_command(
    command: Sequence[str | os.PathLike[str]],
    *,
    cwd: Path,
    env: Mapping[str, str] | None = None,
    capture_output: bool = False,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run an argument-vector command and preserve its failure status."""

    try:
        result = subprocess.run(
            [os.fspath(part) for part in command],
            cwd=cwd,
            env=env,
            check=False,
            capture_output=capture_output,
            text=True,
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise PackageError(f"could not complete {Path(os.fspath(command[0])).name}") from error
    if result.returncode != 0:
        raise PackageError(
            f"{Path(os.fspath(command[0])).name} command failed with exit code {result.returncode}",
            exit_code=result.returncode,
        )
    return result


def read_cli_version(repository: Path) -> str:
    manifest_path = repository / "crates" / "daedalus-cli" / "Cargo.toml"
    try:
        with manifest_path.open("rb") as manifest_file:
            manifest = tomllib.load(manifest_file)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise PackageError("could not read the daedalus-cli Cargo manifest") from error
    version = manifest.get("package", {}).get("version")
    if not isinstance(version, str) or not is_semver(version):
        raise PackageError("daedalus-cli Cargo.toml must contain a valid package SemVer")
    return version


def is_semver(version: str) -> bool:
    return SEMVER_PATTERN.fullmatch(version) is not None


def validate_tag(tag: str | None, version: str) -> None:
    if tag is None:
        return
    if not is_semver(version) or not tag.startswith("v") or tag[1:] != version:
        raise PackageError(f"tag must be v{version}; received {tag!r}")


def validate_target(target: str, host_target: str) -> None:
    if target != host_target:
        raise PackageError(
            f"cross-target packaging is unsupported: requested {target}, host is {host_target}"
        )
    if target not in SUPPORTED_TARGETS:
        supported = ", ".join(sorted(SUPPORTED_TARGETS))
        raise PackageError(f"no native packaging contract exists for {target}; supported: {supported}")


def read_rust_identity(repository: Path) -> dict[str, str]:
    rustc_result = run_command(["rustc", "-vV"], cwd=repository, capture_output=True)
    rustc_fields: dict[str, str] = {}
    for line in rustc_result.stdout.splitlines():
        if ": " in line:
            key, value = line.split(": ", 1)
            rustc_fields[key] = value
    required = ("release", "commit-hash", "host", "LLVM version")
    if any(not rustc_fields.get(field) for field in required):
        raise PackageError("rustc -vV did not report its version, commit, host, and LLVM version")
    cargo_result = run_command(["cargo", "--version"], cwd=repository, capture_output=True)
    return {
        "rustc_version": rustc_fields["release"],
        "rustc_commit_hash": rustc_fields["commit-hash"],
        "rustc_host": rustc_fields["host"],
        "llvm_version": rustc_fields["LLVM version"],
        "cargo_version": cargo_result.stdout.strip(),
    }


def read_git_identity(repository: Path) -> tuple[str, bool]:
    commit_result = run_command(
        ["git", "rev-parse", "--verify", "HEAD"], cwd=repository, capture_output=True
    )
    commit = commit_result.stdout.strip()
    if not COMMIT_PATTERN.fullmatch(commit):
        raise PackageError("Git did not return a full commit identifier")
    status_result = run_command(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"],
        cwd=repository,
        capture_output=True,
    )
    return commit, bool(status_result.stdout.strip())


def make_build_id() -> tuple[str, str]:
    now = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
    timestamp = now.isoformat().replace("+00:00", "Z")
    build_id = now.strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex
    return build_id, timestamp


def _absolute_build_path(path: str | Path, repository: Path) -> Path:
    candidate = Path(path).expanduser()
    if not candidate.is_absolute():
        candidate = repository / candidate
    return Path(os.path.abspath(candidate))


def _build_environment(repository: Path) -> dict[str, str]:
    """Add neutral Rust path mappings while retaining Cargo's environment-flag precedence."""

    environment = os.environ.copy()
    repository = repository.resolve()
    home_value = environment.get("HOME") or str(Path.home())
    home = _absolute_build_path(home_value, repository)
    cargo_home = _absolute_build_path(
        environment.get("CARGO_HOME") or home / ".cargo", repository
    )
    rustup_home = _absolute_build_path(
        environment.get("RUSTUP_HOME") or home / ".rustup", repository
    )

    # rustc uses the last matching prefix. Keep the broad home mapping first, followed by
    # cache and checkout mappings so paths nested under HOME receive their stable labels.
    roots = [
        (home, "/daedalus/build-home"),
        (cargo_home, "/daedalus/cargo"),
        (rustup_home, "/daedalus/rustup"),
        (repository, "/daedalus/source"),
    ]
    roots.sort(key=lambda entry: len(entry[0].parts))

    encoded_flags = environment.get("CARGO_ENCODED_RUSTFLAGS")
    if encoded_flags is not None:
        # Cargo treats an explicitly empty encoded value as an empty flag list and gives it
        # precedence over RUSTFLAGS. A non-empty value uses U+001F as its argument separator.
        existing_flags = encoded_flags.split("\x1f") if encoded_flags else []
    else:
        # Cargo's RUSTFLAGS contract is whitespace splitting, not shell evaluation or quoting.
        existing_flags = environment.get("RUSTFLAGS", "").split()

    remap_flags: list[str] = []
    for source, destination in roots:
        source_text = str(source)
        if "=" in source_text or "\x1f" in source_text:
            raise PackageError("build path cannot be represented safely in Rust remap flags")
        remap_flags.extend(("--remap-path-prefix", f"{source_text}={destination}"))
    environment["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(existing_flags + remap_flags)
    return environment


def build_cli(repository: Path, target: str, cargo_executable: str = "cargo") -> Path:
    command = [
        cargo_executable,
        "build",
        "--release",
        "-p",
        "daedalus-cli",
        "--features",
        "capture",
        "--locked",
        "--target-dir",
        repository / "target",
        "--target",
        target,
    ]
    run_command(command, cwd=repository, env=_build_environment(repository))
    binary_path = repository / "target" / target / "release" / ARCHIVE_BINARY
    if not binary_path.is_file() or binary_path.is_symlink():
        raise PackageError("Cargo succeeded but the expected daedalus CLI binary is missing")
    return binary_path


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as file:
            for chunk in iter(lambda: file.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        raise PackageError(f"could not read {path.name} for SHA-256") from error
    return digest.hexdigest()


def usage_text(version: str, target: str) -> str:
    return f"""# Daedalus CLI {version}

Native command-line build for `{target}`. The archive contains the `daedalus` executable, the
Daedalus MIT and Apache-2.0 license texts, this guide, a dependency inventory, third-party
license and notice texts, and `build-manifest.json`.

## Quick check

```sh
./daedalus --version
./daedalus inspect --entities 2
```

`inspect`, `run`, and `step` use the CPU control path. This package is built with the `capture`
feature; GPU capture requires a compatible graphics adapter and reports adapter failures directly.

This is a native CLI package only. It does not include a C# SDK, CoreCLR runtime, game assets,
external content, or a game export. The manifest records the exact source commit, dirty state,
target, profile, explicit Cargo features, Rust toolchain, build identifier, and executable SHA-256.
The SHA-256 checksum detects accidental changes; it is not a code signature or notarization.
Build identifiers distinguish local packages. Rebuilding the same commit is not a claim of
byte-for-byte reproducibility.
"""


def make_manifest(
    *,
    version: str,
    commit: str,
    dirty: bool,
    target: str,
    rust_toolchain: dict[str, str],
    build_id: str,
    build_timestamp_utc: str,
    binary_sha256: str,
    notices_sha256: str,
    dependency_inventory_sha256: str,
) -> dict[str, Any]:
    if not is_semver(version):
        raise PackageError("manifest CLI version is not valid SemVer")
    if not COMMIT_PATTERN.fullmatch(commit):
        raise PackageError("manifest commit must be a full Git commit identifier")
    if target not in SUPPORTED_TARGETS:
        raise PackageError(f"manifest target {target!r} is not supported")
    expected_toolchain_fields = {
        "rustc_version",
        "rustc_commit_hash",
        "rustc_host",
        "llvm_version",
        "cargo_version",
    }
    if (
        not isinstance(rust_toolchain, dict)
        or set(rust_toolchain) != expected_toolchain_fields
        or any(not isinstance(value, str) or not value for value in rust_toolchain.values())
        or rust_toolchain.get("rustc_host") != target
    ):
        raise PackageError("manifest Rust toolchain identity does not match its target")
    if not BUILD_ID_PATTERN.fullmatch(build_id):
        raise PackageError("manifest build identifier is malformed")
    if not re.fullmatch(r"[0-9a-f]{64}", binary_sha256):
        raise PackageError("manifest binary SHA-256 is malformed")
    if not re.fullmatch(r"[0-9a-f]{64}", notices_sha256):
        raise PackageError("manifest third-party notices SHA-256 is malformed")
    if not re.fullmatch(r"[0-9a-f]{64}", dependency_inventory_sha256):
        raise PackageError("manifest dependency inventory SHA-256 is malformed")
    return {
        "schema_version": SCHEMA_VERSION,
        "cli_version": version,
        "exact_commit": commit,
        "dirty": dirty,
        "target_triple": target,
        "cargo_profile": "release",
        "cargo_features": list(PACKAGE_FEATURES),
        "rust_path_remapping": RUST_PATH_REMAP_POLICY,
        "rust_toolchain": rust_toolchain,
        "binary_name": ARCHIVE_BINARY,
        "binary_sha256": binary_sha256,
        "third_party_notices_sha256": notices_sha256,
        "dependency_inventory_sha256": dependency_inventory_sha256,
        "build_id": build_id,
        "build_timestamp_utc": build_timestamp_utc,
    }


def _archive_files(
    binary_path: Path,
    license_directory: Path,
    usage_path: Path,
    manifest_path: Path,
    notices_path: Path,
    dependency_inventory_path: Path,
) -> list[tuple[Path, str, int]]:
    files: list[tuple[Path, str, int]] = [(binary_path, ARCHIVE_BINARY, 0o755)]
    for license_name in ARCHIVE_LICENSES:
        files.append((license_directory / license_name, license_name, 0o644))
    files.extend(
        [
            (usage_path, ARCHIVE_USAGE, 0o644),
            (notices_path, ARCHIVE_THIRD_PARTY_NOTICES, 0o644),
            (dependency_inventory_path, ARCHIVE_DEPENDENCY_INVENTORY, 0o644),
            (manifest_path, ARCHIVE_MANIFEST, 0o644),
        ]
    )
    for source, archive_name, _mode in files:
        if not source.is_file() or source.is_symlink():
            raise PackageError(f"required regular package file is missing: {archive_name}")
    return files


def _write_archive(archive_path: Path, files: list[tuple[Path, str, int]]) -> None:
    try:
        with tarfile.open(archive_path, mode="w:gz", format=tarfile.PAX_FORMAT) as archive:
            for source, archive_name, mode in files:
                info = archive.gettarinfo(str(source), arcname=archive_name)
                info.mode = mode
                info.uid = 0
                info.gid = 0
                info.uname = ""
                info.gname = ""
                with source.open("rb") as source_file:
                    archive.addfile(info, source_file)
    except (OSError, tarfile.TarError) as error:
        raise PackageError("could not create the CLI archive") from error


def _extract_archive_safely(archive_path: Path, destination: Path) -> dict[str, bytes]:
    extracted: dict[str, bytes] = {}
    allowed = {
        ARCHIVE_BINARY,
        ARCHIVE_USAGE,
        ARCHIVE_MANIFEST,
        ARCHIVE_THIRD_PARTY_NOTICES,
        ARCHIVE_DEPENDENCY_INVENTORY,
        *ARCHIVE_LICENSES,
    }
    try:
        with tarfile.open(archive_path, mode="r:gz") as archive:
            members = archive.getmembers()
            names = [member.name for member in members]
            if len(names) != len(set(names)) or set(names) != allowed:
                raise PackageError(
                    "archive must contain exactly the CLI, usage, manifests, notices, and licenses"
                )
            for member in members:
                member_path = PurePosixPath(member.name)
                if (
                    member_path.is_absolute()
                    or len(member_path.parts) != 1
                    or member_path.parts[0] in {".", ".."}
                    or not member.isfile()
                ):
                    raise PackageError("archive contains an unsafe or unexpected entry")
                source = archive.extractfile(member)
                if source is None:
                    raise PackageError("archive contains an unreadable file")
                contents = source.read()
                extracted[member.name] = contents
                output = destination / member.name
                output.write_bytes(contents)
                output.chmod(member.mode & 0o777)
                if member.name == ARCHIVE_BINARY and not (member.mode & 0o111):
                    raise PackageError("packaged CLI binary does not preserve executable permissions")
    except PackageError:
        raise
    except (OSError, tarfile.TarError) as error:
        raise PackageError("could not inspect or extract the CLI archive") from error
    return extracted


def _run_binary_smoke(binary: Path, version: str, cwd: Path) -> None:
    version_result = run_command(
        [binary, "--version"], cwd=cwd, capture_output=True, timeout=30
    )
    if version not in version_result.stdout.split():
        raise PackageError("extracted CLI --version output does not match the package manifest")
    inspect_result = run_command(
        [binary, "inspect", "--entities", "2"],
        cwd=cwd,
        capture_output=True,
        timeout=30,
    )
    try:
        output = json.loads(inspect_result.stdout)
    except json.JSONDecodeError as error:
        raise PackageError("extracted CLI inspect smoke did not return JSON") from error
    if not isinstance(output, dict) or output.get("command") != "inspect" or output.get("entity_count") != 2:
        raise PackageError("extracted CLI inspect smoke returned unexpected CPU state")


def verify_archive(
    archive_path: Path,
    *,
    expected_tag: str | None = None,
    expected_commit: str | None = None,
    run_smoke: bool = True,
) -> dict[str, Any]:
    if archive_path.is_symlink():
        raise PackageError("CLI archive does not exist as a regular file")
    try:
        archive_path = archive_path.resolve(strict=True)
    except OSError as error:
        raise PackageError("CLI archive does not exist as a regular file") from error
    if not archive_path.is_file():
        raise PackageError("CLI archive does not exist as a regular file")
    checksum_path = archive_path.with_name(archive_path.name + ".sha256")
    if not checksum_path.is_file() or checksum_path.is_symlink():
        raise PackageError("CLI archive checksum sidecar is missing")
    expected_checksum_line = f"{sha256_file(archive_path)}  {archive_path.name}\n"
    try:
        actual_checksum_line = checksum_path.read_text(encoding="ascii")
    except (OSError, UnicodeDecodeError) as error:
        raise PackageError("CLI archive checksum sidecar is unreadable") from error
    if actual_checksum_line != expected_checksum_line:
        raise PackageError("CLI archive checksum sidecar does not match the archive")

    with tempfile.TemporaryDirectory(prefix="daedalus-cli-verify-") as temporary_directory:
        extracted_directory = Path(temporary_directory)
        files = _extract_archive_safely(archive_path, extracted_directory)
        try:
            manifest = json.loads(files[ARCHIVE_MANIFEST].decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise PackageError("CLI package manifest is invalid JSON") from error
        if not isinstance(manifest, dict):
            raise PackageError("CLI package manifest must be a JSON object")
        _validate_manifest(manifest)
        if hashlib.sha256(files[ARCHIVE_BINARY]).hexdigest() != manifest["binary_sha256"]:
            raise PackageError("manifest executable SHA-256 does not match the archived binary")
        if hashlib.sha256(files[ARCHIVE_THIRD_PARTY_NOTICES]).hexdigest() != manifest[
            "third_party_notices_sha256"
        ]:
            raise PackageError("manifest third-party notices SHA-256 does not match the archive")
        if hashlib.sha256(files[ARCHIVE_DEPENDENCY_INVENTORY]).hexdigest() != manifest[
            "dependency_inventory_sha256"
        ]:
            raise PackageError("manifest dependency inventory SHA-256 does not match the archive")
        try:
            inventory = json.loads(files[ARCHIVE_DEPENDENCY_INVENTORY].decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise PackageError("CLI dependency inventory is invalid JSON") from error
        if (
            not isinstance(inventory, dict)
            or type(inventory.get("schema_version")) is not int
            or inventory.get("schema_version") != 1
            or inventory.get("target_triple") != manifest["target_triple"]
            or inventory.get("requested_features") != ["daedalus-cli/capture"]
            or not isinstance(inventory.get("dependencies"), list)
            or type(inventory.get("dependency_count")) is not int
            or inventory.get("dependency_count") != len(inventory["dependencies"])
            or "not a binary-symbol SBOM" not in str(inventory.get("feature_resolution", ""))
        ):
            raise PackageError("CLI dependency inventory does not match the package contract")
        if expected_tag is not None:
            validate_tag(expected_tag, manifest["cli_version"])
        if expected_commit is not None and manifest["exact_commit"] != expected_commit:
            raise PackageError("package manifest commit does not match the release commit")
        if run_smoke:
            _run_binary_smoke(
                extracted_directory / ARCHIVE_BINARY,
                manifest["cli_version"],
                extracted_directory,
            )
        return manifest


def _validate_manifest(manifest: dict[str, Any]) -> None:
    required = {
        "schema_version",
        "cli_version",
        "exact_commit",
        "dirty",
        "target_triple",
        "cargo_profile",
        "cargo_features",
        "rust_path_remapping",
        "rust_toolchain",
        "binary_name",
        "binary_sha256",
        "third_party_notices_sha256",
        "dependency_inventory_sha256",
        "build_id",
        "build_timestamp_utc",
    }
    if set(manifest) != required:
        raise PackageError("CLI package manifest fields are missing or unexpected")
    if type(manifest["schema_version"]) is not int or manifest["schema_version"] != SCHEMA_VERSION:
        raise PackageError("CLI package manifest schema version is unsupported")
    if not isinstance(manifest["cli_version"], str) or not is_semver(manifest["cli_version"]):
        raise PackageError("CLI package manifest version is invalid")
    if not isinstance(manifest["exact_commit"], str) or not COMMIT_PATTERN.fullmatch(manifest["exact_commit"]):
        raise PackageError("CLI package manifest commit is invalid")
    if not isinstance(manifest["dirty"], bool):
        raise PackageError("CLI package manifest dirty flag is invalid")
    if (
        not isinstance(manifest["target_triple"], str)
        or manifest["target_triple"] not in SUPPORTED_TARGETS
    ):
        raise PackageError("CLI package manifest target is unsupported")
    if manifest["cargo_profile"] != "release" or manifest["cargo_features"] != list(PACKAGE_FEATURES):
        raise PackageError("CLI package manifest profile or feature list is invalid")
    if manifest["rust_path_remapping"] != RUST_PATH_REMAP_POLICY:
        raise PackageError("CLI package manifest Rust path-remapping policy is invalid")
    if manifest["binary_name"] != ARCHIVE_BINARY:
        raise PackageError("CLI package manifest binary name is invalid")
    if not isinstance(manifest["binary_sha256"], str) or not re.fullmatch(
        r"[0-9a-f]{64}", manifest["binary_sha256"]
    ):
        raise PackageError("CLI package manifest executable SHA-256 is invalid")
    for field, label in (
        ("third_party_notices_sha256", "third-party notices"),
        ("dependency_inventory_sha256", "dependency inventory"),
    ):
        if not isinstance(manifest[field], str) or not re.fullmatch(
            r"[0-9a-f]{64}", manifest[field]
        ):
            raise PackageError(f"CLI package manifest {label} SHA-256 is invalid")
    if not isinstance(manifest["build_id"], str) or not BUILD_ID_PATTERN.fullmatch(manifest["build_id"]):
        raise PackageError("CLI package manifest build identifier is invalid")
    if not isinstance(manifest["build_timestamp_utc"], str) or not manifest["build_timestamp_utc"].endswith("Z"):
        raise PackageError("CLI package manifest build timestamp is invalid")
    toolchain = manifest["rust_toolchain"]
    expected_toolchain_fields = {
        "rustc_version",
        "rustc_commit_hash",
        "rustc_host",
        "llvm_version",
        "cargo_version",
    }
    if (
        not isinstance(toolchain, dict)
        or set(toolchain) != expected_toolchain_fields
        or any(not isinstance(value, str) or not value for value in toolchain.values())
    ):
        raise PackageError("CLI package manifest Rust toolchain identity is invalid")


def create_package(
    *,
    binary_path: Path,
    output_directory: Path,
    repository: Path,
    version: str,
    commit: str,
    dirty: bool,
    target: str,
    rust_toolchain: dict[str, str],
    build_id: str,
    build_timestamp_utc: str,
    metadata: dict[str, Any] | None = None,
) -> PackagePaths:
    if not binary_path.is_file() or binary_path.is_symlink():
        raise PackageError("input CLI binary is not a regular file")
    if not is_semver(version):
        raise PackageError("CLI package version is invalid")
    if not COMMIT_PATTERN.fullmatch(commit):
        raise PackageError("CLI package commit must be a full Git commit identifier")
    if target not in SUPPORTED_TARGETS:
        raise PackageError(f"CLI package target {target!r} is unsupported")
    if not BUILD_ID_PATTERN.fullmatch(build_id):
        raise PackageError("CLI package build identifier is malformed")

    output_directory = output_directory.expanduser().resolve()
    if output_directory.exists() or output_directory.is_symlink():
        raise PackageError("output directory already exists; refusing to overwrite it")
    try:
        output_directory.parent.mkdir(parents=True, exist_ok=True)
        output_directory.mkdir()
    except OSError as error:
        raise PackageError("could not create the package output directory") from error

    try:
        staged_binary = output_directory / ARCHIVE_BINARY
        shutil.copyfile(binary_path, staged_binary)
        staged_binary.chmod(0o755)
        usage_path = output_directory / ARCHIVE_USAGE
        usage_path.write_text(usage_text(version, target), encoding="utf-8")
        try:
            notice_artifacts = collect_cli_notices(
                repository=repository,
                output_directory=output_directory,
                target=target,
                metadata=metadata,
            )
        except NoticeCollectionError as error:
            raise PackageError(f"dependency notice collection failed: {error}") from error
        manifest = make_manifest(
            version=version,
            commit=commit,
            dirty=dirty,
            target=target,
            rust_toolchain=rust_toolchain,
            build_id=build_id,
            build_timestamp_utc=build_timestamp_utc,
            binary_sha256=sha256_file(staged_binary),
            notices_sha256=sha256_file(notice_artifacts.notices_path),
            dependency_inventory_sha256=sha256_file(notice_artifacts.inventory_path),
        )
        manifest_path = output_directory / ARCHIVE_MANIFEST
        manifest_path.write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        files = _archive_files(
            staged_binary,
            repository,
            usage_path,
            manifest_path,
            notice_artifacts.notices_path,
            notice_artifacts.inventory_path,
        )
        archive_name = f"daedalus-{version}-{target}-{build_id}.tar.gz"
        archive_path = output_directory / archive_name
        checksum_path = output_directory / f"{archive_name}.sha256"
        _write_archive(archive_path, files)
        checksum_path.write_text(
            f"{sha256_file(archive_path)}  {archive_path.name}\n", encoding="ascii"
        )
        verified_manifest = verify_archive(archive_path, run_smoke=True)
        return PackagePaths(output_directory, archive_path, checksum_path, verified_manifest)
    except Exception:
        shutil.rmtree(output_directory, ignore_errors=True)
        raise


def _default_output_directory(
    root: Path,
    version: str,
    target: str,
    commit: str,
    dirty: bool,
    build_id: str,
) -> Path:
    source_state = "dirty" if dirty else "clean"
    return root / "dist" / "local" / (
        f"daedalus-{version}-{target}-{commit[:12]}-{source_state}-{build_id}"
    )


def _parse_args(argv: Sequence[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build and package a native Daedalus CLI executable."
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--verify-archive",
        type=Path,
        metavar="ARCHIVE",
        help="verify an existing archive and its checksum without rebuilding",
    )
    parser.add_argument(
        "--out",
        type=Path,
        help="new package output directory (must not already exist); defaults under dist/local",
    )
    parser.add_argument("--tag", help="validate a v<CLI SemVer> tag before packaging")
    parser.add_argument(
        "--target",
        help="native host target triple; cross-target packaging is rejected",
    )
    parser.add_argument(
        "--metadata-only",
        action="store_true",
        help="skip executing an archive binary (for cross-platform release aggregation)",
    )
    parser.add_argument("--commit", help="expected exact commit when verifying an archive")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    arguments = _parse_args(argv)
    repository = Path(__file__).resolve().parents[1]
    try:
        if arguments.verify_archive is not None:
            if arguments.out is not None or arguments.target is not None:
                raise PackageError("--out and --target apply only when building a package")
            archive_path = arguments.verify_archive
            if not archive_path.is_absolute():
                archive_path = Path.cwd() / archive_path
            manifest = verify_archive(
                archive_path,
                expected_tag=arguments.tag,
                expected_commit=arguments.commit,
                run_smoke=not arguments.metadata_only,
            )
            print(
                json.dumps(
                    {
                        "archive": archive_path.name,
                        "cli_version": manifest["cli_version"],
                        "target_triple": manifest["target_triple"],
                        "exact_commit": manifest["exact_commit"],
                    },
                    sort_keys=True,
                )
            )
            return 0
        if arguments.metadata_only or arguments.commit is not None:
            raise PackageError("--metadata-only and --commit require --verify-archive")

        version = read_cli_version(repository)
        validate_tag(arguments.tag, version)
        rust_toolchain = read_rust_identity(repository)
        host_target = rust_toolchain["rustc_host"]
        target = arguments.target or host_target
        validate_target(target, host_target)
        commit, dirty = read_git_identity(repository)
        if arguments.tag is not None and dirty:
            raise PackageError("tagged package builds require a clean Git worktree")
        build_id, build_timestamp_utc = make_build_id()
        binary_path = build_cli(repository, target)
        post_build_commit, post_build_dirty = read_git_identity(repository)
        if post_build_commit != commit:
            raise PackageError("Git HEAD changed while building; refusing to package a different source revision")
        if arguments.tag is not None and post_build_dirty:
            raise PackageError("tagged package builds require a clean Git worktree after building")
        dirty = dirty or post_build_dirty
        output_directory = arguments.out
        if output_directory is None:
            output_directory = _default_output_directory(
                repository, version, target, commit, dirty, build_id
            )
        elif not output_directory.is_absolute():
            output_directory = Path.cwd() / output_directory
        package = create_package(
            binary_path=binary_path,
            output_directory=output_directory,
            repository=repository,
            version=version,
            commit=commit,
            dirty=dirty,
            target=target,
            rust_toolchain=rust_toolchain,
            build_id=build_id,
            build_timestamp_utc=build_timestamp_utc,
        )
        print(f"archive: {package.archive}")
        print(f"checksum: {package.checksum}")
        print(f"manifest: {package.output_directory / ARCHIVE_MANIFEST}")
        return 0
    except PackageError as error:
        print(f"package_cli: {error}", file=sys.stderr)
        return error.exit_code


if __name__ == "__main__":
    raise SystemExit(main())
