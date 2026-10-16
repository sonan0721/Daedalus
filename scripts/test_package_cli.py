from __future__ import annotations

import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from contextlib import redirect_stderr
from io import StringIO
from pathlib import Path
from unittest.mock import patch

import package_cli


REPOSITORY = Path(__file__).resolve().parents[1]
VERSION = "0.1.0"
TARGET = "aarch64-apple-darwin"
COMMIT = "a" * 40
BUILD_ID = "20261015T120000Z-" + "b" * 32
BUILD_TIMESTAMP = "2026-10-15T12:00:00Z"
RUST_TOOLCHAIN = {
    "rustc_version": "1.94.0",
    "rustc_commit_hash": "c" * 40,
    "rustc_host": TARGET,
    "llvm_version": "21.1.0",
    "cargo_version": "cargo 1.94.0",
}


def fixture_binary(version: str) -> bytes:
    source = f'''#!/usr/bin/env python3
import json
import sys

if sys.argv[1:] == ["--version"]:
    print("daedalus {version}")
elif sys.argv[1:] == ["inspect", "--entities", "2"]:
    print(json.dumps({{"command": "inspect", "entity_count": 2}}))
else:
    raise SystemExit(2)
'''
    return source.encode("utf-8")


def empty_cli_metadata(repository: Path) -> dict:
    cli_manifest = repository / "crates" / "daedalus-cli" / "Cargo.toml"
    root_id = f"path+file://{repository}#daedalus-cli@{VERSION}"
    return {
        "version": 1,
        "packages": [
            {
                "id": root_id,
                "name": "daedalus-cli",
                "version": VERSION,
                "source": None,
                "manifest_path": str(cli_manifest),
            }
        ],
        "workspace_members": [root_id],
        "resolve": {
            "nodes": [{"id": root_id, "features": ["audio", "capture", "native-audio", "window"], "deps": []}]
        },
    }


class PackageCliTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="daedalus-package-test-")
        self.root = Path(self.temporary.name)
        self.cargo_home = self.root / "cargo-home"
        self.cargo_home.mkdir()
        self.binary = self.root / "fixture-daedalus"
        self.binary.write_bytes(fixture_binary(VERSION))
        self.binary.chmod(0o755)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def create_package(self, output_directory: Path, binary: Path | None = None) -> package_cli.PackagePaths:
        with patch.dict(os.environ, {"CARGO_HOME": str(self.cargo_home)}):
            # This fixture only implements --version and inspect. Keep archive/version checks
            # real, but make the external-project smoke boundary explicit instead of claiming
            # this stub exercised JSONL or project lifecycle behavior.
            with patch.object(
                package_cli,
                "run_project_smoke",
                return_value={
                    "status": "passed",
                    "checks": [{"name": "mocked-unit-boundary"}],
                    "audio_offline": {"invocation_kind": "headless_protocol2_manual_offline"},
                },
            ) as project_smoke:
                package = package_cli.create_package(
                    binary_path=binary or self.binary,
                    output_directory=output_directory,
                    repository=REPOSITORY,
                    version=VERSION,
                    commit=COMMIT,
                    dirty=True,
                    target=TARGET,
                    rust_toolchain=RUST_TOOLCHAIN,
                    build_id=BUILD_ID,
                    build_timestamp_utc=BUILD_TIMESTAMP,
                    metadata=empty_cli_metadata(REPOSITORY),
                )
                self.project_smoke_mock_calls = project_smoke.call_count
                self.project_smoke_mock_arguments = project_smoke.call_args_list
                return package

    def test_packages_archive_with_stub_cli_and_preserves_manifest_and_checksums(self) -> None:
        output_directory = self.root / "output"
        package = self.create_package(output_directory)

        manifest = package_cli.verify_archive(
            package.archive,
            expected_tag=f"v{VERSION}",
            expected_commit=COMMIT,
            run_smoke=False,
        )
        self.assertEqual(self.project_smoke_mock_calls, 1)
        smoke_binary = self.project_smoke_mock_arguments[0].args[0]
        self.assertEqual(smoke_binary.name, package_cli.ARCHIVE_BINARY)
        self.assertEqual(self.project_smoke_mock_arguments[0].kwargs["cwd"], smoke_binary.parent)
        self.assertIs(self.project_smoke_mock_arguments[0].kwargs["audio"], True)
        self.assertEqual(manifest["cli_version"], VERSION)
        self.assertEqual(manifest["exact_commit"], COMMIT)
        self.assertTrue(manifest["dirty"])
        self.assertEqual(manifest["target_triple"], TARGET)
        self.assertEqual(manifest["cargo_profile"], "release")
        self.assertEqual(manifest["cargo_features"], ["native-audio", "window"])
        self.assertEqual(manifest["rust_path_remapping"], package_cli.RUST_PATH_REMAP_POLICY)
        self.assertEqual(manifest["build_id"], BUILD_ID)
        self.assertEqual(manifest["binary_name"], "daedalus")
        self.assertEqual(len(manifest["third_party_notices_sha256"]), 64)
        self.assertEqual(len(manifest["dependency_inventory_sha256"]), 64)
        package_usage = (output_directory / package_cli.ARCHIVE_USAGE).read_text(encoding="utf-8")
        self.assertIn("--audio offline", package_usage)
        self.assertIn("native-audio", package_usage)
        self.assertIn("libasound.so.2", package_cli.usage_text(VERSION, "x86_64-unknown-linux-gnu"))

        with tarfile.open(package.archive, "r:gz") as archive:
            members = archive.getmembers()
            self.assertEqual(
                {member.name for member in members},
                {
                    "daedalus",
                    "LICENSE-MIT",
                    "LICENSE-APACHE",
                    "USAGE.txt",
                    "THIRD_PARTY_NOTICES.txt",
                    "dependency-inventory.json",
                    "build-manifest.json",
                },
            )
            binary_member = next(member for member in members if member.name == "daedalus")
            self.assertTrue(binary_member.mode & 0o111)
            self.assertTrue(all(Path(member.name).name == member.name for member in members))
            self.assertFalse(any("/Users/" in member.name or "\\\\Users\\\\" in member.name for member in members))

        with tarfile.open(package.archive, "r:gz") as archive:
            inventory = json.load(archive.extractfile("dependency-inventory.json"))
            notices = archive.extractfile("THIRD_PARTY_NOTICES.txt").read().decode("utf-8")
        self.assertEqual(inventory["target_triple"], TARGET)
        self.assertEqual(inventory["dependency_count"], 0)
        self.assertIn("not a binary-symbol SBOM", inventory["feature_resolution"])
        self.assertIn("Daedalus CLI third-party notices", notices)
        self.assertNotIn(str(REPOSITORY), notices)
        self.assertNotIn(str(REPOSITORY), json.dumps(manifest))

        checksum_fields = package.checksum.read_text(encoding="ascii").strip().split()
        self.assertEqual(checksum_fields[1], package.archive.name)
        self.assertEqual(len(checksum_fields[0]), 64)
        self.assertEqual(manifest["binary_sha256"], package_cli.sha256_file(self.binary))

    def test_rejects_malformed_and_mismatched_tags(self) -> None:
        for tag in ("release-0.1.0", "v0.2.0", "v0.1"):
            with self.subTest(tag=tag), self.assertRaises(package_cli.PackageError):
                package_cli.validate_tag(tag, VERSION)

    def test_rejects_a_binary_whose_version_disagrees_with_the_manifest(self) -> None:
        wrong_binary = self.root / "wrong-version-daedalus"
        wrong_binary.write_bytes(fixture_binary("9.9.9"))
        wrong_binary.chmod(0o755)
        output_directory = self.root / "wrong-version-package"

        with self.assertRaisesRegex(package_cli.PackageError, "--version"):
            self.create_package(output_directory, wrong_binary)
        self.assertFalse(output_directory.exists())

    def test_rejects_symlinked_archive_input(self) -> None:
        package = self.create_package(self.root / "package")
        archive_link = self.root / "linked-package.tar.gz"
        try:
            archive_link.symlink_to(package.archive)
        except (NotImplementedError, OSError) as error:
            self.skipTest(f"symlinks are unavailable: {error}")

        with self.assertRaisesRegex(package_cli.PackageError, "regular file"):
            package_cli.verify_archive(archive_link)

    def test_existing_output_is_preserved_and_never_overwritten(self) -> None:
        output_directory = self.root / "existing"
        output_directory.mkdir()
        preserved_file = output_directory / "keep.txt"
        preserved_file.write_text("existing package data", encoding="utf-8")

        with self.assertRaisesRegex(package_cli.PackageError, "refusing to overwrite"):
            self.create_package(output_directory)
        self.assertEqual(preserved_file.read_text(encoding="utf-8"), "existing package data")

    def test_rejects_cross_target_and_unverified_target(self) -> None:
        with self.assertRaisesRegex(package_cli.PackageError, "cross-target"):
            package_cli.validate_target("x86_64-unknown-linux-gnu", TARGET)
        with self.assertRaisesRegex(package_cli.PackageError, "no native packaging contract"):
            package_cli.validate_target("x86_64-pc-windows-msvc", "x86_64-pc-windows-msvc")

    def test_preserves_failed_build_exit_code_from_a_real_child_process(self) -> None:
        cargo_stub = self.root / "cargo-failing"
        arguments_log = self.root / "cargo-arguments.json"
        cargo_stub.write_text(
            "#!/usr/bin/env python3\n"
            "import json, pathlib, sys\n"
            f"pathlib.Path({str(arguments_log)!r}).write_text(json.dumps(sys.argv[1:]))\n"
            "raise SystemExit(23)\n",
            encoding="utf-8",
        )
        cargo_stub.chmod(0o755)

        with self.assertRaises(package_cli.PackageError) as error:
            package_cli.build_cli(self.root, TARGET, os.fspath(cargo_stub))
        self.assertEqual(error.exception.exit_code, 23)
        self.assertEqual(
            json.loads(arguments_log.read_text(encoding="utf-8")),
            [
                "build",
                "--release",
                "-p",
                "daedalus-cli",
                "--features",
                "native-audio,window",
                "--locked",
                "--target-dir",
                str(self.root / "target"),
                "--target",
                TARGET,
            ],
        )

    def test_build_cli_remaps_source_paths_and_preserves_cargo_flag_precedence(self) -> None:
        home = self.root / "build home with spaces"
        repository = home / "workspace" / "Daedalus"
        cargo_home = home / "cargo cache"
        rustup_home = home / "rustup cache"
        repository.mkdir(parents=True)
        cargo_home.mkdir()
        rustup_home.mkdir()
        binary = repository / "target" / TARGET / "release" / "daedalus"
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"controlled build output")

        cargo_stub = self.root / "cargo recorder"
        cargo_stub.write_text(
            f"#!{sys.executable}\n"
            "import json, os, pathlib, sys\n"
            "pathlib.Path(os.environ['PACKAGE_TEST_LOG']).write_text(json.dumps({"
            "'arguments': sys.argv[1:], "
            "'encoded': os.environ.get('CARGO_ENCODED_RUSTFLAGS'), "
            "'rustflags': os.environ.get('RUSTFLAGS')}))\n",
            encoding="utf-8",
        )
        cargo_stub.chmod(0o755)

        cases = (
            {
                "CARGO_ENCODED_RUSTFLAGS": "--cfg\x1ffeature=\"two words\"",
                "RUSTFLAGS": "--cfg ignored_by_encoded",
                "expected_flags": ["--cfg", 'feature="two words"'],
            },
            {
                "CARGO_ENCODED_RUSTFLAGS": "",
                "RUSTFLAGS": "--cfg ignored_by_empty_encoded",
                "expected_flags": [],
            },
            {
                "RUSTFLAGS": "-C opt-level=2 --cfg whitespace_split",
                "expected_flags": ["-C", "opt-level=2", "--cfg", "whitespace_split"],
            },
        )
        log_path = self.root / "cargo-environment.json"
        for index, case in enumerate(cases):
            with self.subTest(index=index):
                environment = {
                    "HOME": str(home),
                    "CARGO_HOME": str(cargo_home),
                    "RUSTUP_HOME": str(rustup_home),
                    "PACKAGE_TEST_LOG": str(log_path),
                }
                environment.update(
                    {
                        key: value
                        for key, value in case.items()
                        if key in {"CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS"}
                    }
                )
                with patch.dict(os.environ, environment):
                    if "CARGO_ENCODED_RUSTFLAGS" not in case:
                        os.environ.pop("CARGO_ENCODED_RUSTFLAGS", None)
                    package_cli.build_cli(repository, TARGET, os.fspath(cargo_stub))

                captured = json.loads(log_path.read_text(encoding="utf-8"))
                self.assertEqual(
                    captured["arguments"],
                    [
                        "build",
                        "--release",
                        "-p",
                        "daedalus-cli",
                        "--features",
                        "native-audio,window",
                        "--locked",
                        "--target-dir",
                        str(repository / "target"),
                        "--target",
                        TARGET,
                    ],
                )
                self.assertEqual(captured["rustflags"], case.get("RUSTFLAGS"))
                self.assertEqual(
                    captured["encoded"].split("\x1f")[: len(case["expected_flags"])],
                    case["expected_flags"],
                )
                remaps = captured["encoded"].split("\x1f")[len(case["expected_flags"]) :]
                expected_remaps = [
                    "--remap-path-prefix",
                    f"{home}=/daedalus/build-home",
                    "--remap-path-prefix",
                    f"{cargo_home}=/daedalus/cargo",
                    "--remap-path-prefix",
                    f"{rustup_home}=/daedalus/rustup",
                    "--remap-path-prefix",
                    f"{repository.resolve()}=/daedalus/source",
                ]
                self.assertEqual(remaps, expected_remaps)

    def test_tagged_build_rejects_dirty_worktree_before_build(self) -> None:
        output_directory = self.root / "tagged-package"
        errors = StringIO()
        with (
            patch.object(package_cli, "read_rust_identity", return_value=RUST_TOOLCHAIN),
            patch.object(package_cli, "read_git_identity", return_value=(COMMIT, True)),
            patch.object(package_cli, "build_cli") as build,
            redirect_stderr(errors),
        ):
            result = package_cli.main(
                ["--tag", f"v{VERSION}", "--target", TARGET, "--out", str(output_directory)]
            )

        self.assertEqual(result, 1)
        self.assertIn("clean Git worktree", errors.getvalue())
        build.assert_not_called()
        self.assertFalse(output_directory.exists())

    def test_build_rejects_git_head_change_before_packaging(self) -> None:
        errors = StringIO()
        package = package_cli.PackagePaths(
            self.root, self.root / "archive", self.root / "checksum", {}
        )
        with (
            patch.object(package_cli, "read_rust_identity", return_value=RUST_TOOLCHAIN),
            patch.object(
                package_cli,
                "read_git_identity",
                side_effect=[(COMMIT, False), ("b" * 40, False)],
            ) as read_git,
            patch.object(package_cli, "build_cli", return_value=self.binary) as build,
            patch.object(package_cli, "create_package", return_value=package) as create,
            redirect_stderr(errors),
        ):
            result = package_cli.main(
                ["--target", TARGET, "--out", str(self.root / "changed-head-package")]
            )

        self.assertEqual(result, 1)
        self.assertIn("HEAD changed while building", errors.getvalue())
        self.assertEqual(read_git.call_count, 2)
        build.assert_called_once()
        create.assert_not_called()

    def test_tagged_build_rejects_post_build_dirtiness(self) -> None:
        errors = StringIO()
        package = package_cli.PackagePaths(
            self.root, self.root / "archive", self.root / "checksum", {}
        )
        with (
            patch.object(package_cli, "read_rust_identity", return_value=RUST_TOOLCHAIN),
            patch.object(
                package_cli,
                "read_git_identity",
                side_effect=[(COMMIT, False), (COMMIT, True)],
            ) as read_git,
            patch.object(package_cli, "build_cli", return_value=self.binary) as build,
            patch.object(package_cli, "create_package", return_value=package) as create,
            redirect_stderr(errors),
        ):
            result = package_cli.main(
                [
                    "--tag",
                    f"v{VERSION}",
                    "--target",
                    TARGET,
                    "--out",
                    str(self.root / "post-dirty-tagged-package"),
                ]
            )

        self.assertEqual(result, 1)
        self.assertIn("clean Git worktree after building", errors.getvalue())
        self.assertEqual(read_git.call_count, 2)
        build.assert_called_once()
        create.assert_not_called()

    def test_local_package_dirty_flag_or_covers_before_and_after_build(self) -> None:
        package = package_cli.PackagePaths(
            self.root, self.root / "archive", self.root / "checksum", {}
        )
        for index, (initially_dirty, dirty_after_build) in enumerate(
            ((False, True), (True, False))
        ):
            with self.subTest(initially_dirty=initially_dirty, dirty_after_build=dirty_after_build):
                with (
                    patch.object(package_cli, "read_rust_identity", return_value=RUST_TOOLCHAIN),
                    patch.object(
                        package_cli,
                        "read_git_identity",
                        side_effect=[
                            (COMMIT, initially_dirty),
                            (COMMIT, dirty_after_build),
                        ],
                    ),
                    patch.object(package_cli, "build_cli", return_value=self.binary),
                    patch.object(
                        package_cli, "create_package", return_value=package
                    ) as create,
                ):
                    result = package_cli.main(
                        [
                            "--target",
                            TARGET,
                            "--out",
                            str(self.root / f"local-dirty-{index}"),
                        ]
                    )

                self.assertEqual(result, 0)
                self.assertTrue(create.call_args.kwargs["dirty"])

    def test_release_workflow_handles_prerelease_metadata_and_explicit_repo(self) -> None:
        workflow = (REPOSITORY / ".github" / "workflows" / "release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("libasound2-dev", workflow)
        self.assertIn("libasound\\.so", workflow)
        self.assertIn("--audio", workflow)
        self.assertIn('index("native-audio")', workflow)
        lines = workflow.splitlines()
        start = next(
            index for index, line in enumerate(lines)
            if line.strip().startswith("release_args=(")
        )
        end = next(
            index for index in range(start, len(lines))
            if lines[index].strip().startswith("gh release create ")
        )
        release_command = "\n".join(line[10:] for line in lines[start : end + 1])
        self.assertIn('--repo "$REPOSITORY"', release_command)

        temporary_bin = self.root / "bin"
        temporary_bin.mkdir()
        captured_arguments = self.root / "gh-arguments.json"
        gh_stub = temporary_bin / "gh"
        gh_stub.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, pathlib, sys\n"
            "pathlib.Path(os.environ['GH_ARGS_OUTPUT']).write_text(json.dumps(sys.argv[1:]))\n",
            encoding="utf-8",
        )
        gh_stub.chmod(0o755)
        assets = self.root / "release-assets"
        assets.mkdir()
        (assets / "fixture.tar.gz").write_bytes(b"controlled asset")

        cases = (
            ("v1.0.0+build-test", False),
            ("v1.0.0-alpha.1+build-test", True),
        )
        for tag, is_prerelease in cases:
            with self.subTest(tag=tag):
                environment = os.environ.copy()
                environment.update(
                    {
                        "RELEASE_TAG": tag,
                        "REPOSITORY": "owner/Daedalus",
                        "GH_ARGS_OUTPUT": str(captured_arguments),
                        "PATH": f"{temporary_bin}{os.pathsep}{environment['PATH']}",
                    }
                )
                result = subprocess.run(
                    ["bash", "-c", release_command],
                    cwd=self.root,
                    env=environment,
                    check=False,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                arguments = json.loads(captured_arguments.read_text(encoding="utf-8"))
                self.assertIn("--repo", arguments)
                self.assertEqual(arguments[arguments.index("--repo") + 1], "owner/Daedalus")
                self.assertEqual("--prerelease" in arguments, is_prerelease)

    def test_records_exact_commit_and_dirty_state_from_git(self) -> None:
        repository = self.root / "git-repository"
        repository.mkdir()
        package_cli.run_command(["git", "init", "--quiet"], cwd=repository)
        package_cli.run_command(
            ["git", "config", "user.name", "Package Test"], cwd=repository
        )
        package_cli.run_command(
            ["git", "config", "user.email", "package-test@example.invalid"], cwd=repository
        )
        tracked_file = repository / "source.txt"
        tracked_file.write_text("source", encoding="utf-8")
        package_cli.run_command(["git", "add", "source.txt"], cwd=repository)
        package_cli.run_command(["git", "commit", "--quiet", "-m", "fixture"], cwd=repository)

        clean_commit, clean_dirty = package_cli.read_git_identity(repository)
        expected_commit = package_cli.run_command(
            ["git", "rev-parse", "HEAD"], cwd=repository, capture_output=True
        ).stdout.strip()
        self.assertEqual(clean_commit, expected_commit)
        self.assertFalse(clean_dirty)

        (repository / "untracked.txt").write_text("local-only", encoding="utf-8")
        dirty_commit, dirty = package_cli.read_git_identity(repository)
        self.assertEqual(dirty_commit, expected_commit)
        self.assertTrue(dirty)


if __name__ == "__main__":
    unittest.main()
