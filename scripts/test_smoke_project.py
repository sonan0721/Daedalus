from __future__ import annotations

import argparse
from contextlib import redirect_stderr
from io import StringIO
import sys
import tempfile
import time
import unittest
from pathlib import Path

from smoke_project import (
    MAX_JSONL_RESPONSE_BYTES,
    SmokeError,
    _JsonlProjectServer,
    _parse_args,
    _validate_timeout,
)


class ProjectSmokeProcessTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="daedalus-smoke-process-test-")
        self.root = Path(self.temporary.name)
        self.project = self.root / "external-project"
        self.project.mkdir()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def create_server_stub(self, body: str) -> Path:
        binary = self.root / "server-stub"
        binary.write_text(f"#!{sys.executable}\n{body}\n", encoding="utf-8")
        binary.chmod(0o755)
        return binary

    def test_timeout_parser_rejects_zero_non_finite_and_non_numeric_values(self) -> None:
        self.assertEqual(_validate_timeout("1"), 1.0)
        for value in ("0", "nan", "bad"):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                _validate_timeout(value)

    def test_argparse_accepts_positive_timeout_and_rejects_invalid_values(self) -> None:
        arguments = _parse_args(["--cli", "/bin/true", "--timeout", "1"])
        self.assertEqual(arguments.timeout, 1.0)
        for value in ("0", "nan", "bad"):
            with self.subTest(value=value), redirect_stderr(StringIO()), self.assertRaises(SystemExit):
                _parse_args(["--cli", "/bin/true", "--timeout", value])

    def test_invalid_jsonl_response_terminates_and_reaps_server_child(self) -> None:
        binary = self.create_server_stub(
            "import sys, time\n"
            "print('not-json', flush=True)\n"
            "time.sleep(30)"
        )
        server = _JsonlProjectServer(binary, self.project, cwd=self.root, timeout=2.0)
        started = time.monotonic()
        with self.assertRaisesRegex(SmokeError, "invalid JSON"):
            with server:
                server.request("malformed", {"op": "project_info"})

        self.assertIsNotNone(server.process.poll())
        self.assertTrue(server._stdin.closed)
        self.assertTrue(server._stdout.closed)
        self.assertLess(time.monotonic() - started, 3.0)

    def test_jsonl_timeout_terminates_and_reaps_unresponsive_server_child(self) -> None:
        binary = self.create_server_stub("import time\ntime.sleep(30)")
        server = _JsonlProjectServer(binary, self.project, cwd=self.root, timeout=0.15)
        started = time.monotonic()
        with self.assertRaisesRegex(SmokeError, "timed out"):
            with server:
                server.request("blocked", {"op": "project_info"})

        self.assertIsNotNone(server.process.poll())
        self.assertTrue(server._stdin.closed)
        self.assertTrue(server._stdout.closed)
        self.assertLess(time.monotonic() - started, 2.0)

    def test_oversized_jsonl_response_is_rejected_and_child_is_reaped(self) -> None:
        binary = self.create_server_stub(
            "import sys, time\n"
            f"sys.stdout.write('x' * ({MAX_JSONL_RESPONSE_BYTES} + 1) + '\\n')\n"
            "sys.stdout.flush()\n"
            "time.sleep(30)"
        )
        server = _JsonlProjectServer(binary, self.project, cwd=self.root, timeout=2.0)
        with self.assertRaisesRegex(SmokeError, "exceeded"):
            with server:
                server.request("oversized", {"op": "project_info"})

        self.assertIsNotNone(server.process.poll())
        self.assertTrue(server._stdin.closed)


if __name__ == "__main__":
    unittest.main()
