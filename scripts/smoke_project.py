#!/usr/bin/env python3
"""Run a bounded CPU smoke against an already-built Daedalus CLI binary."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import selectors
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Sequence


PROTOCOL_VERSION = 1
DEFAULT_TIMEOUT_SECONDS = 30.0
MAX_TIMEOUT_SECONDS = 300.0
MAX_JSONL_RESPONSE_BYTES = 1024 * 1024
ENTITY_ID = "entity:smoke-mesh"
PROJECT_NAME = "smoke-mesh-project"
TRANSFORM_AFTER_COMMAND = {
    "translation": [7.0, 2.0, -3.0],
    "rotation": [0.0, 0.0, 0.0, 1.0],
    "scale": [1.5, 2.0, 0.75],
}


class SmokeError(Exception):
    """A smoke failure with the command or protocol context that caused it."""


def _validate_timeout(value: str | float) -> float:
    try:
        timeout = float(value)
    except (TypeError, ValueError) as error:
        raise argparse.ArgumentTypeError("timeout must be a number of seconds") from error
    if not math.isfinite(timeout) or not 0 < timeout <= MAX_TIMEOUT_SECONDS:
        raise argparse.ArgumentTypeError(
            f"timeout must be greater than zero and at most {MAX_TIMEOUT_SECONDS:g} seconds"
        )
    return timeout


def _diagnostic(stdout: str, stderr: str) -> str:
    parts = []
    if stdout.strip():
        parts.append(f"stdout: {stdout.strip()[-2000:]}")
    if stderr.strip():
        parts.append(f"stderr: {stderr.strip()[-2000:]}")
    return "; ".join(parts) or "no output"


def _run_cli(
    binary: Path,
    arguments: Sequence[str],
    *,
    cwd: Path,
    timeout: float,
    expect_success: bool = True,
) -> subprocess.CompletedProcess[str]:
    command = [os.fspath(binary), *arguments]
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            capture_output=True,
            text=True,
            check=False,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as error:
        stdout = _decode_output(error.stdout)
        stderr = _decode_output(error.stderr)
        raise SmokeError(
            f"CLI command timed out after {timeout:g}s ({arguments!r}); "
            f"{_diagnostic(stdout, stderr)}"
        ) from error
    except OSError as error:
        raise SmokeError(f"could not run CLI command {arguments!r}: {error}") from error

    if expect_success and result.returncode != 0:
        raise SmokeError(
            f"CLI command {arguments!r} exited {result.returncode}; "
            f"{_diagnostic(result.stdout, result.stderr)}"
        )
    if not expect_success and result.returncode == 0:
        raise SmokeError(
            f"CLI command {arguments!r} unexpectedly succeeded; "
            f"{_diagnostic(result.stdout, result.stderr)}"
        )
    return result


def _decode_output(value: str | bytes | None) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return value


def _json_output(result: subprocess.CompletedProcess[str], arguments: Sequence[str]) -> dict[str, Any]:
    try:
        output = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise SmokeError(
            f"CLI command {arguments!r} did not return JSON; "
            f"{_diagnostic(result.stdout, result.stderr)}"
        ) from error
    if not isinstance(output, dict):
        raise SmokeError(f"CLI command {arguments!r} returned JSON that is not an object")
    return output


def _expect(condition: bool, message: str, context: Any = None) -> None:
    if not condition:
        suffix = "" if context is None else f"; response: {json.dumps(context, sort_keys=True)}"
        raise SmokeError(message + suffix)


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        raise SmokeError(f"could not identify CLI binary {path}: {error}") from error
    return digest.hexdigest()


def _write_project(directory: Path, *, unsupported_component: bool = False) -> None:
    directory.mkdir()
    (directory / "assets").mkdir()
    manifest = {
        "schema_version": 1,
        "name": PROJECT_NAME if not unsupported_component else "unsupported-component-project",
        "scene": "scene.json",
    }
    component_type = "daedalus.unsupported_smoke_component" if unsupported_component else "daedalus.transform"
    components: list[dict[str, Any]] = [
        {
            "id": "component:smoke-transform",
            "type_id": component_type,
            "data": {
                "translation": [0.0, 0.0, 0.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0],
            },
        },
        {
            "id": "component:smoke-mesh-renderer",
            "type_id": "daedalus.mesh_renderer",
            "asset_refs": ["asset:smoke-mesh"],
            "data": {
                "mesh": "asset:smoke-mesh",
                "material": {
                    "base_color": [0.68, 0.22, 0.1],
                    "metallic": 0.15,
                    "roughness": 0.7,
                    "emissive": [0.0, 0.0, 0.0],
                },
            },
        },
    ]
    scene = {
        "schema_version": 1,
        "revision": 3,
        "entities": [
            {
                "id": ENTITY_ID,
                "name": "Smoke Mesh",
                "components": components,
            }
        ],
        "assets": [
            {
                "id": "asset:smoke-mesh",
                "kind": "daedalus.mesh",
                "uri": "assets/smoke-mesh.json",
                "metadata": {},
            }
        ],
    }
    mesh = {
        "schema_version": 1,
        "vertices": [
            {"position": [-0.7, -0.6, 0.0], "normal": [0.0, 0.0, 1.0]},
            {"position": [0.7, -0.6, 0.0], "normal": [0.0, 0.0, 1.0]},
            {"position": [0.0, 0.7, 0.0], "normal": [0.0, 0.0, 1.0]},
        ],
        "indices": [0, 1, 2],
    }
    (directory / "daedalus.project.json").write_text(
        json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
    )
    (directory / "scene.json").write_text(json.dumps(scene, indent=2) + "\n", encoding="utf-8")
    (directory / "assets" / "smoke-mesh.json").write_text(
        json.dumps(mesh, indent=2) + "\n", encoding="utf-8"
    )


class _JsonlProjectServer:
    """One bounded project serve process with request-ID and response validation."""

    def __init__(self, binary: Path, project: Path, *, cwd: Path, timeout: float) -> None:
        self.timeout = timeout
        self.process_stderr = tempfile.TemporaryFile(mode="w+b")
        try:
            self.process = subprocess.Popen(
                [os.fspath(binary), "project", "serve", os.fspath(project)],
                cwd=cwd,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=self.process_stderr,
                bufsize=0,
            )
        except OSError as error:
            self.process_stderr.close()
            raise SmokeError(f"could not start project serve: {error}") from error
        assert self.process.stdin is not None
        assert self.process.stdout is not None
        self._stdout = self.process.stdout
        self._stdin = self.process.stdin
        self._selector = selectors.DefaultSelector()
        self._selector.register(self._stdout, selectors.EVENT_READ)
        self._buffer = bytearray()
        self._closed = False

    def _server_diagnostic(self) -> str:
        try:
            self.process_stderr.flush()
            self.process_stderr.seek(0)
            return self.process_stderr.read().decode("utf-8", errors="replace")[-2000:]
        except OSError:
            return ""

    def request(self, request_id: str, operation: dict[str, Any]) -> dict[str, Any]:
        envelope = {
            "protocol": PROTOCOL_VERSION,
            "request_id": request_id,
            "request": operation,
        }
        encoded = json.dumps(envelope, separators=(",", ":")).encode("utf-8") + b"\n"
        try:
            self._stdin.write(encoded)
            self._stdin.flush()
        except (BrokenPipeError, OSError) as error:
            raise SmokeError(
                f"project serve closed while sending request {request_id!r}; "
                f"{self._server_diagnostic()}"
            ) from error

        deadline = time.monotonic() + self.timeout
        while True:
            newline = self._buffer.find(b"\n")
            if newline >= 0:
                if newline > MAX_JSONL_RESPONSE_BYTES:
                    raise SmokeError(
                        f"project serve response exceeded {MAX_JSONL_RESPONSE_BYTES} bytes"
                    )
                raw = bytes(self._buffer[:newline])
                del self._buffer[: newline + 1]
                return self._validate_response(raw, request_id)
            if len(self._buffer) > MAX_JSONL_RESPONSE_BYTES:
                raise SmokeError(
                    f"project serve response exceeded {MAX_JSONL_RESPONSE_BYTES} bytes"
                )
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SmokeError(
                    f"timed out after {self.timeout:g}s waiting for project serve request "
                    f"{request_id!r}; {self._server_diagnostic()}"
                )
            ready = self._selector.select(remaining)
            if not ready:
                raise SmokeError(
                    f"timed out after {self.timeout:g}s waiting for project serve request "
                    f"{request_id!r}; {self._server_diagnostic()}"
                )
            try:
                chunk = os.read(self._stdout.fileno(), 64 * 1024)
            except OSError as error:
                raise SmokeError(f"could not read project serve response: {error}") from error
            if not chunk:
                raise SmokeError(
                    f"project serve exited before responding to {request_id!r} "
                    f"(exit {self.process.poll()}); {self._server_diagnostic()}"
                )
            self._buffer.extend(chunk)

    def _validate_response(self, raw: bytes, request_id: str) -> dict[str, Any]:
        try:
            response = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise SmokeError(
                f"project serve returned invalid JSON for {request_id!r}: "
                f"{raw[:2000]!r}; {self._server_diagnostic()}"
            ) from error
        _expect(isinstance(response, dict), "project serve response must be an object", response)
        _expect(
            response.get("protocol") == PROTOCOL_VERSION,
            f"project serve response has unsupported protocol for {request_id!r}",
            response,
        )
        _expect(
            response.get("request_id") == request_id,
            f"project serve response request ID does not match {request_id!r}",
            response,
        )
        _expect(
            type(response.get("ok")) is bool,
            f"project serve response has no boolean success flag for {request_id!r}",
            response,
        )
        _expect(response["ok"], f"project serve rejected request {request_id!r}", response)
        _expect(
            isinstance(response.get("result"), dict),
            f"project serve response has no result object for {request_id!r}",
            response,
        )
        return response

    def finish(self) -> None:
        if self._closed:
            return
        try:
            self._stdin.close()
            try:
                return_code = self.process.wait(timeout=self.timeout)
            except subprocess.TimeoutExpired as error:
                raise SmokeError(
                    f"project serve did not exit within {self.timeout:g}s after exit request; "
                    f"{self._server_diagnostic()}"
                ) from error
            if return_code != 0:
                raise SmokeError(
                    f"project serve exited with code {return_code}; {self._server_diagnostic()}"
                )
        finally:
            self._terminate_if_running()
            self._close_streams()

    def _terminate_if_running(self) -> None:
        if self.process.poll() is not None:
            return
        self.process.terminate()
        try:
            self.process.wait(timeout=min(self.timeout, 2.0))
        except subprocess.TimeoutExpired:
            self.process.kill()
            try:
                self.process.wait(timeout=min(self.timeout, 2.0))
            except subprocess.TimeoutExpired as error:
                raise SmokeError("could not reap terminated project serve process") from error

    def _close_streams(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            self._selector.unregister(self._stdout)
        except (KeyError, ValueError):
            pass
        self._selector.close()
        self._stdin.close()
        self._stdout.close()
        self.process_stderr.close()

    def __enter__(self) -> _JsonlProjectServer:
        return self

    def __exit__(self, exc_type: Any, exc: Any, traceback: Any) -> None:
        if exc_type is None:
            self.finish()
        else:
            try:
                self._terminate_if_running()
            finally:
                self._close_streams()


def _project_handle(project_info: dict[str, Any]) -> dict[str, Any]:
    entities = project_info.get("entities")
    _expect(isinstance(entities, list), "project_info entities must be a list", project_info)
    matches = [entry for entry in entities if isinstance(entry, dict) and entry.get("id") == ENTITY_ID]
    _expect(len(matches) == 1, f"project_info must return exactly one {ENTITY_ID!r}", project_info)
    handle = matches[0].get("handle")
    _expect(
        isinstance(handle, dict)
        and isinstance(handle.get("session"), str)
        and handle.get("session") == project_info.get("session")
        and type(handle.get("index")) is int
        and type(handle.get("generation")) is int,
        "project_info returned an invalid dynamic session/entity handle",
        matches[0],
    )
    return handle


def _check_step_application(
    response: dict[str, Any],
    *,
    command_name: str,
    command_id: str,
) -> dict[str, Any]:
    result = response["result"]
    reports = result.get("reports")
    _expect(isinstance(reports, list) and len(reports) == 1, "step must report one tick", response)
    commands = reports[0].get("commands")
    _expect(isinstance(commands, list) and len(commands) == 1, "step must report one command", response)
    command = commands[0]
    _expect(
        isinstance(command, dict) and command.get("command_id") == command_id,
        "step reported a different command ID",
        response,
    )
    outcome = command.get("outcome")
    _expect(
        isinstance(outcome, dict) and set(outcome) == {command_name},
        f"step reported an outcome other than {command_name}",
        response,
    )
    return outcome


def _run_jsonl_lifecycle(binary: Path, project: Path, *, cwd: Path, timeout: float) -> list[str]:
    checks: list[str] = []
    with _JsonlProjectServer(binary, project, cwd=cwd, timeout=timeout) as server:
        info_response = server.request(
            "info-before",
            {"op": "project_info", "offset": 0, "limit": 10},
        )
        info = info_response["result"]
        _expect(info.get("project_name") == PROJECT_NAME, "project_info returned wrong project", info)
        _expect(info.get("source_scene_revision") == 3, "project_info returned wrong source revision", info)
        session = info.get("session")
        _expect(isinstance(session, str) and session, "project_info returned no dynamic session ID", info)
        handle = _project_handle(info)
        _expect(handle["session"] == session, "project_info handle belongs to another session", info)
        checks.append("serve_project_info_dynamic_session_handle")

        accepted = server.request(
            "set-transform",
            {
                "op": "set_transform",
                "target": handle,
                "expected_revision": 0,
                "command_id": "41001",
                "transform": TRANSFORM_AFTER_COMMAND,
            },
        )
        accepted_result = accepted["result"]
        _expect(accepted_result.get("submission") == "accepted", "transform command was not accepted", accepted)
        _expect(accepted_result.get("command_id") == "41001", "accepted command ID changed", accepted)
        _expect(accepted_result.get("apply_at") == 1, "transform command has unexpected apply tick", accepted)
        checks.append("serve_transform_command_accepted_before_apply")

        before_step = server.request(
            "get-before-step",
            {"op": "get_transform", "entity": handle},
        )["result"]
        _expect(
            before_step.get("revision") == 0
            and before_step.get("transform") == {
                "translation": [0.0, 0.0, 0.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0],
            },
            "accepted transform changed state before its tick was applied",
            before_step,
        )
        checks.append("serve_acceptance_precedes_authoritative_state_change")

        stepped = server.request("step-one", {"op": "step", "count": 1})
        _expect(stepped["result"]["reports"][0].get("tick") == 1, "first step did not reach tick 1", stepped)
        applied = _check_step_application(
            stepped,
            command_name="Applied",
            command_id="41001",
        )
        _expect(
            applied["Applied"] == {"revision": 1},
            "command did not apply at revision 1",
            stepped,
        )
        checks.append("serve_step_applied_transform_revision_one")

        transform_response = server.request(
            "get-transform",
            {"op": "get_transform", "entity": handle},
        )
        transform_result = transform_response["result"]
        _expect(transform_result.get("entity") == handle, "get_transform returned another entity", transform_response)
        _expect(transform_result.get("revision") == 1, "get_transform returned wrong revision", transform_response)
        _expect(
            transform_result.get("transform") == TRANSFORM_AFTER_COMMAND,
            "get_transform does not contain the applied transform",
            transform_response,
        )
        checks.append("serve_get_transform_uses_entity_and_current_revision")

        snapshot_response = server.request("snapshot", {"op": "latest_snapshot"})
        snapshot = snapshot_response["result"].get("snapshot")
        _expect(isinstance(snapshot, dict), "latest_snapshot returned no snapshot", snapshot_response)
        _expect(snapshot.get("tick") == 1, "latest_snapshot returned a stale tick", snapshot_response)
        snapshot_entities = snapshot.get("entities")
        _expect(isinstance(snapshot_entities, list), "latest_snapshot entities must be a list", snapshot_response)
        snapshot_matches = [
            entity for entity in snapshot_entities
            if isinstance(entity, dict) and entity.get("entity") == handle
        ]
        _expect(len(snapshot_matches) == 1, "snapshot does not contain the returned entity handle", snapshot_response)
        _expect(
            snapshot_matches[0].get("transform") == TRANSFORM_AFTER_COMMAND,
            "snapshot transform differs from applied transform",
            snapshot_response,
        )
        checks.append("serve_snapshot_contains_applied_transform_at_tick_one")

        current_info_response = server.request(
            "info-current",
            {"op": "project_info", "offset": 0, "limit": 10},
        )
        current_info = current_info_response["result"]
        _expect(current_info.get("tick") == 1, "project_info did not report the current serve tick", current_info_response)
        _expect(current_info.get("session") == session, "project_info changed the active session", current_info_response)
        current_handle = _project_handle(current_info)
        _expect(current_handle == handle, "project_info changed the authored handle mapping", current_info_response)
        _expect(
            "transform" not in current_info["entities"][0] and "revision" not in current_info["entities"][0],
            "project_info must expose static bindings, not stale transform state",
            current_info_response,
        )
        checks.append("serve_project_info_reports_current_tick_without_stale_entity_state")

        stale_command = server.request(
            "set-transform-stale",
            {
                "op": "set_transform",
                "target": handle,
                "expected_revision": 0,
                "command_id": "41002",
                "transform": {
                    **TRANSFORM_AFTER_COMMAND,
                    "translation": [99.0, 0.0, 0.0],
                },
            },
        )
        _expect(stale_command["result"].get("submission") == "accepted", "stale command should reach authoritative tick validation", stale_command)
        _expect(stale_command["result"].get("command_id") == "41002", "stale command ID changed", stale_command)
        before_stale_step = server.request(
            "get-before-stale-step",
            {"op": "get_transform", "entity": handle},
        )["result"]
        _expect(
            before_stale_step.get("revision") == 1
            and before_stale_step.get("transform") == TRANSFORM_AFTER_COMMAND,
            "stale command changed state before its tick was applied",
            before_stale_step,
        )
        stale_step = server.request("step-stale", {"op": "step", "count": 1})
        _expect(stale_step["result"]["reports"][0].get("tick") == 2, "stale command step did not reach tick 2", stale_step)
        stale_outcome = _check_step_application(
            stale_step,
            command_name="Rejected",
            command_id="41002",
        )
        rejected = stale_outcome["Rejected"]
        _expect(
            isinstance(rejected, dict) and set(rejected) == {"RevisionConflict"},
            "stale revision did not produce a revision conflict",
            stale_step,
        )
        conflict = rejected["RevisionConflict"]
        _expect(
            isinstance(conflict, dict)
            and conflict.get("expected") == 0
            and conflict.get("actual") == 1,
            "stale revision conflict reported unexpected revisions",
            stale_step,
        )
        checks.append("serve_stale_revision_rejected_at_application")

        after_stale = server.request("get-after-stale", {"op": "get_transform", "entity": handle})
        after_stale_result = after_stale["result"]
        _expect(after_stale_result.get("revision") == 1, "stale command changed entity revision", after_stale)
        _expect(
            after_stale_result.get("transform") == TRANSFORM_AFTER_COMMAND,
            "stale command changed entity transform",
            after_stale,
        )
        latest_after_stale = server.request("snapshot-after-stale", {"op": "latest_snapshot"})["result"]["snapshot"]
        _expect(latest_after_stale.get("tick") == 2, "snapshot did not advance with rejected command tick", latest_after_stale)
        checks.append("serve_stale_revision_did_not_change_transform_or_revision")

        current_info_after_stale = server.request(
            "info-after-stale",
            {"op": "project_info", "offset": 0, "limit": 10},
        )["result"]
        _expect(current_info_after_stale.get("tick") == 2, "project_info is not current after second step", current_info_after_stale)
        checks.append("serve_project_info_tick_tracks_current_authoritative_tick")

        exited = server.request("exit", {"op": "exit"})
        _expect(exited["result"].get("closed") is True, "serve exit did not confirm shutdown", exited)
    checks.append("serve_clean_exit_and_child_reaped")
    return checks


def run_project_smoke(
    cli_binary: str | Path,
    *,
    cwd: str | Path | None = None,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
) -> dict[str, Any]:
    """Exercise empty and authored external projects using an existing CLI binary."""

    if not 0 < timeout <= MAX_TIMEOUT_SECONDS:
        raise SmokeError(f"timeout must be greater than zero and at most {MAX_TIMEOUT_SECONDS:g} seconds")
    binary = Path(cli_binary).expanduser()
    if not binary.is_absolute():
        binary = Path.cwd() / binary
    try:
        binary = binary.resolve(strict=True)
    except OSError as error:
        raise SmokeError(f"CLI binary does not exist: {binary}") from error
    if binary.is_symlink() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise SmokeError(f"CLI path is not an executable regular file: {binary}")
    working_directory = Path(cwd or Path.cwd()).expanduser().resolve()
    started = time.monotonic()

    version_result = _run_cli(binary, ["--version"], cwd=working_directory, timeout=timeout)
    version = version_result.stdout.strip()
    _expect(bool(version), "CLI --version returned no identity", version_result.stdout)
    checks = ["cli_version_identity"]

    repository = Path(__file__).resolve().parents[1]
    with tempfile.TemporaryDirectory(prefix="daedalus-project-smoke-") as temporary:
        root = Path(temporary).resolve()
        try:
            root.relative_to(repository)
        except ValueError:
            pass
        else:
            raise SmokeError(f"temporary smoke directory must be outside the engine repository: {root}")

        empty_project = root / "empty-project"
        init_args = ["project", "init", os.fspath(empty_project), "--name", "smoke-empty-project"]
        empty_init = _json_output(
            _run_cli(binary, init_args, cwd=working_directory, timeout=timeout), init_args
        )
        _expect(empty_init.get("command") == "project_init", "project init returned wrong command", empty_init)
        _expect(empty_init.get("entity_count") == 0 and empty_init.get("asset_count") == 0, "project init must create an empty project", empty_init)
        _expect(
            {path.name for path in empty_project.iterdir()} == {"daedalus.project.json", "scene.json"},
            "empty project contains unexpected generated content",
            sorted(path.name for path in empty_project.iterdir()),
        )
        checks.append("empty_project_init")

        empty_validate_args = ["project", "validate", os.fspath(empty_project)]
        empty_validate = _json_output(
            _run_cli(binary, empty_validate_args, cwd=working_directory, timeout=timeout),
            empty_validate_args,
        )
        _expect(empty_validate.get("valid") is True, "empty project did not validate", empty_validate)
        _expect(empty_validate.get("entity_count") == 0, "validated empty project has entities", empty_validate)
        checks.append("empty_project_shared_loader_validation")

        manifest_path = empty_project / "daedalus.project.json"
        original_manifest = manifest_path.read_bytes()
        duplicate_init = _run_cli(
            binary,
            ["project", "init", os.fspath(empty_project), "--name", "must-not-overwrite"],
            cwd=working_directory,
            timeout=timeout,
            expect_success=False,
        )
        diagnostic = f"{duplicate_init.stdout}\n{duplicate_init.stderr}".lower()
        _expect("already exists" in diagnostic or "never overwrites" in diagnostic, "duplicate init did not explain no-overwrite policy", diagnostic)
        _expect(manifest_path.read_bytes() == original_manifest, "duplicate init modified existing project", empty_project)
        checks.append("empty_project_init_no_overwrite")

        project = root / "authored-mesh-project"
        _write_project(project)
        validate_args = ["project", "validate", os.fspath(project)]
        validated = _json_output(
            _run_cli(binary, validate_args, cwd=working_directory, timeout=timeout),
            validate_args,
        )
        _expect(validated.get("valid") is True, "authored mesh project did not validate", validated)
        _expect(validated.get("project_name") == PROJECT_NAME, "validation returned wrong project identity", validated)
        _expect(validated.get("entity_count") == 1 and validated.get("asset_count") == 1, "validation returned wrong authored counts", validated)
        _expect(validated.get("source_scene_revision") == 3, "validation returned wrong source revision", validated)
        checks.append("authored_mesh_project_shared_loader_validation")

        run_args = ["project", "run", os.fspath(project), "--ticks", "2"]
        run_output = _json_output(
            _run_cli(binary, run_args, cwd=working_directory, timeout=timeout), run_args
        )
        _expect(run_output.get("command") == "project_run", "project run returned wrong command", run_output)
        _expect(run_output.get("project_name") == PROJECT_NAME, "project run returned wrong project identity", run_output)
        _expect(run_output.get("tick") == 2 and run_output.get("entity_count") == 1, "project run returned wrong tick/entity count", run_output)
        _expect(run_output.get("source_scene_revision") == 3, "project run returned wrong source revision", run_output)
        _expect(
            run_output.get("source_digest") == validated.get("source_digest"),
            "validate and run did not use the same loaded source identity",
            {"validate": validated, "run": run_output},
        )
        run_entities = run_output.get("entities")
        _expect(isinstance(run_entities, list) and len(run_entities) == 1, "project run did not report one entity", run_output)
        _expect(run_entities[0].get("id") == ENTITY_ID, "project run returned the wrong stable ID", run_entities[0])
        run_session = run_output.get("session")
        run_handle = run_entities[0].get("handle")
        _expect(
            isinstance(run_session, str)
            and isinstance(run_handle, dict)
            and run_handle.get("session") == run_session,
            "project run returned an invalid session/handle identity",
            run_output,
        )
        checks.append("project_run_identity_matches_validate_source")

        checks.extend(_run_jsonl_lifecycle(binary, project, cwd=working_directory, timeout=timeout))

        unsupported_project = root / "unsupported-component-project"
        _write_project(unsupported_project, unsupported_component=True)
        unsupported_args = ["project", "validate", os.fspath(unsupported_project)]
        unsupported = _run_cli(
            binary,
            unsupported_args,
            cwd=working_directory,
            timeout=timeout,
            expect_success=False,
        )
        unsupported_diagnostic = f"{unsupported.stdout}\n{unsupported.stderr}".lower()
        _expect(
            "unsupported" in unsupported_diagnostic
            and "daedalus.unsupported_smoke_component" in unsupported_diagnostic,
            "unsupported project component did not fail with a clear source diagnostic",
            unsupported_diagnostic,
        )
        checks.append("unsupported_component_explicit_failure")

    return {
        "schema_version": 1,
        "smoke": "external_project_cpu",
        "status": "passed",
        "cli": {
            "path": os.fspath(binary),
            "sha256": _sha256_file(binary),
            "version_output": version,
        },
        "checks": [{"name": name, "status": "passed"} for name in checks],
        "check_count": len(checks),
        "wall_time_seconds": round(time.monotonic() - started, 6),
    }


def _parse_args(argv: Sequence[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run bounded CPU project lifecycle checks against an existing Daedalus CLI binary."
    )
    parser.add_argument("--cli", type=Path, required=True, help="already-built daedalus executable")
    parser.add_argument(
        "--timeout",
        type=_validate_timeout,
        default=DEFAULT_TIMEOUT_SECONDS,
        help=f"timeout per process/JSONL response in seconds (default: {DEFAULT_TIMEOUT_SECONDS:g})",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    arguments = _parse_args(argv)
    try:
        result = run_project_smoke(arguments.cli, timeout=arguments.timeout)
        print(json.dumps(result, sort_keys=True))
        return 0
    except SmokeError as error:
        print(f"smoke_project: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
