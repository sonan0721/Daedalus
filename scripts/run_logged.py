#!/usr/bin/env python3
"""Run one noninteractive command while preserving its raw output exclusively.

Launcher and evidence-finalization errors use exit status 125. Child exit statuses
are preserved; a child terminated by signal N maps to the conventional 128 + N
wrapper status while its original negative return code remains in the JSON record.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Sequence


LAUNCHER_ERROR_EXIT = 125
HASH_CHUNK_BYTES = 1024 * 1024


def _json_line(record: dict[str, object]) -> bytes:
    return (json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")


def _write_record(
    log_file, record: dict[str, object], *, prefix_newline: bool = False
) -> None:
    data = _json_line(record)
    if prefix_newline:
        data = b"\n" + data
    written = log_file.write(data)
    if written != len(data):
        raise OSError("short write while finalizing the run log")
    log_file.flush()


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as log_file:
        while chunk := log_file.read(HASH_CHUNK_BYTES):
            digest.update(chunk)
    return digest.hexdigest()


def _emit_result(record: dict[str, object]) -> bool:
    try:
        sys.stdout.write(json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")
        sys.stdout.flush()
    except OSError:
        return False
    return True


def _result(
    *,
    log_path: Path,
    log_sha256: str | None,
    launched: bool,
    status: str,
    exit_code: int,
    child_returncode: int | None = None,
    elapsed_seconds: float | None = None,
    error: str | None = None,
) -> dict[str, object]:
    record: dict[str, object] = {
        "log_path": os.fspath(log_path),
        "log_sha256": log_sha256,
        "launched": launched,
        "status": status,
        "exit_code": exit_code,
        "child_returncode": child_returncode,
    }
    if elapsed_seconds is not None:
        record["elapsed_seconds"] = elapsed_seconds
    if error is not None:
        record["error"] = error
    return record


def run_logged(log_path: Path, argv: Sequence[str]) -> int:
    """Run argv with stdout and stderr directed to a newly created raw log.

    The log is opened with exclusive creation before the child is launched. A
    failure to open it never starts the child. A missing footer or missing hash
    means the run is not complete evidence, even if the child itself returned 0.
    """

    if not argv or any(not isinstance(argument, str) for argument in argv):
        record = _result(
            log_path=log_path,
            log_sha256=None,
            launched=False,
            status="launcher_error",
            exit_code=LAUNCHER_ERROR_EXIT,
            error="a non-empty string argument vector is required",
        )
        _emit_result(record)
        return LAUNCHER_ERROR_EXIT

    try:
        log_file = log_path.open("xb")
    except OSError as error:
        record = _result(
            log_path=log_path,
            log_sha256=None,
            launched=False,
            status="launcher_error",
            exit_code=LAUNCHER_ERROR_EXIT,
            error=f"could not exclusively create log: {type(error).__name__}",
        )
        _emit_result(record)
        return LAUNCHER_ERROR_EXIT

    started_at = dt.datetime.now(dt.timezone.utc).isoformat()
    started_clock = time.perf_counter()
    finalization_error: str | None = None
    child_returncode: int | None = None
    launched = False
    launcher_error: str | None = None

    try:
        _write_record(
            log_file,
            {"event": "start", "started_at_utc": started_at, "argv": list(argv)},
        )
    except OSError as error:
        finalization_error = f"could not write start metadata: {type(error).__name__}"

    if finalization_error is None:
        try:
            child = subprocess.Popen(
                list(argv),
                stdin=subprocess.DEVNULL,
                stdout=log_file,
                stderr=log_file,
                shell=False,
            )
            launched = True
        except OSError as error:
            launcher_error = f"could not launch child: {type(error).__name__}"
        else:
            try:
                child_returncode = child.wait()
            except OSError as error:
                finalization_error = f"could not collect child exit status: {type(error).__name__}"

    elapsed_seconds = time.perf_counter() - started_clock
    if finalization_error is None:
        if launcher_error is not None:
            footer: dict[str, object] = {
                "event": "launcher_error",
                "elapsed_seconds": elapsed_seconds,
                "error": launcher_error,
            }
        else:
            footer = {
                "event": "child_exit",
                "elapsed_seconds": elapsed_seconds,
                "child_returncode": child_returncode,
            }
        try:
            _write_record(log_file, footer, prefix_newline=True)
        except OSError as error:
            finalization_error = f"could not write completion footer: {type(error).__name__}"

    try:
        log_file.close()
    except OSError as error:
        if finalization_error is None:
            finalization_error = f"could not close run log: {type(error).__name__}"

    log_sha256: str | None = None
    try:
        log_sha256 = _sha256_file(log_path)
    except OSError as error:
        if finalization_error is None:
            finalization_error = f"could not hash completed run log: {type(error).__name__}"

    if finalization_error is not None:
        record = _result(
            log_path=log_path,
            log_sha256=log_sha256,
            launched=launched,
            status="finalization_error",
            exit_code=LAUNCHER_ERROR_EXIT,
            child_returncode=child_returncode,
            elapsed_seconds=elapsed_seconds,
            error=finalization_error,
        )
        _emit_result(record)
        return LAUNCHER_ERROR_EXIT

    if launcher_error is not None:
        record = _result(
            log_path=log_path,
            log_sha256=log_sha256,
            launched=False,
            status="launcher_error",
            exit_code=LAUNCHER_ERROR_EXIT,
            elapsed_seconds=elapsed_seconds,
            error=launcher_error,
        )
        _emit_result(record)
        return LAUNCHER_ERROR_EXIT

    assert child_returncode is not None
    if child_returncode < 0:
        wrapper_exit = min(255, 128 + -child_returncode)
        status = "child_signaled"
    else:
        wrapper_exit = child_returncode
        status = "child_exit"
    record = _result(
        log_path=log_path,
        log_sha256=log_sha256,
        launched=True,
        status=status,
        exit_code=wrapper_exit,
        child_returncode=child_returncode,
        elapsed_seconds=elapsed_seconds,
    )
    if not _emit_result(record):
        return LAUNCHER_ERROR_EXIT
    return wrapper_exit


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Run one command with exclusive JSONL start/footer metadata and raw output capture."
    )
    parser.add_argument("--log", required=True, type=Path, help="new log path; existing files are refused")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    try:
        separator = arguments.index("--")
    except ValueError:
        _parser().error("expected -- before the child argv")
    namespace = _parser().parse_args(arguments[:separator])
    child_argv = arguments[separator + 1 :]
    if not child_argv:
        _parser().error("a child argv is required after --")
    return run_logged(namespace.log, child_argv)


if __name__ == "__main__":
    raise SystemExit(main())
