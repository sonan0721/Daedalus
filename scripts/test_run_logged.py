from __future__ import annotations

import hashlib
import json
import os
import signal
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from unittest.mock import patch

import run_logged


class RunLoggedTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="daedalus-run-logged-test-")
        self.root = Path(self.temporary.name)
        self.addCleanup(self.temporary.cleanup)

    def invoke(self, log_path: Path, child_argv: list[str]) -> tuple[int, dict[str, object]]:
        output = StringIO()
        with redirect_stdout(output):
            exit_code = run_logged.main(["--log", str(log_path), "--", *child_argv])
        lines = output.getvalue().splitlines()
        self.assertEqual(len(lines), 1, "the launcher should emit one compact JSON result")
        return exit_code, json.loads(lines[0])

    def assert_log_hash(self, log_path: Path, result: dict[str, object]) -> None:
        self.assertEqual(
            result["log_sha256"], hashlib.sha256(log_path.read_bytes()).hexdigest()
        )

    def test_captures_both_streams_and_preserves_nonzero_child_exit(self) -> None:
        log_path = self.root / "child.log"
        child_code = (
            "import sys; sys.stdout.write('child stdout without newline'); "
            "sys.stderr.write('child stderr without newline'); "
            "raise SystemExit(17)"
        )

        exit_code, result = self.invoke(log_path, [sys.executable, "-c", child_code])

        self.assertEqual(exit_code, 17)
        self.assertEqual(result["status"], "child_exit")
        self.assertEqual(result["child_returncode"], 17)
        self.assertTrue(result["launched"])
        raw_log = log_path.read_text(encoding="utf-8")
        self.assertIn('"event":"start"', raw_log)
        self.assertIn("child stdout", raw_log)
        self.assertIn("child stderr", raw_log)
        self.assertIn('"event":"child_exit"', raw_log)
        records = raw_log.splitlines()
        self.assertEqual(json.loads(records[0])["event"], "start")
        self.assertEqual(json.loads(records[-1])["event"], "child_exit")
        self.assertNotIn('"environment"', raw_log)
        self.assert_log_hash(log_path, result)

    def test_existing_log_is_unchanged_and_child_is_never_started(self) -> None:
        log_path = self.root / "existing.log"
        marker = self.root / "child-ran"
        original = b"prior raw evidence\x00\xff\n"
        log_path.write_bytes(original)
        child_code = (
            "from pathlib import Path; import sys; "
            "Path(sys.argv[1]).write_text('ran', encoding='utf-8')"
        )

        exit_code, result = self.invoke(
            log_path, [sys.executable, "-c", child_code, str(marker)]
        )

        self.assertEqual(exit_code, run_logged.LAUNCHER_ERROR_EXIT)
        self.assertEqual(result["status"], "launcher_error")
        self.assertFalse(result["launched"])
        self.assertIsNone(result["log_sha256"])
        self.assertEqual(log_path.read_bytes(), original)
        self.assertFalse(marker.exists())

    def test_child_launch_error_is_distinct_and_hashes_its_log(self) -> None:
        log_path = self.root / "launch-error.log"
        missing_program = self.root / "missing-child-program"

        exit_code, result = self.invoke(log_path, [str(missing_program)])

        self.assertEqual(exit_code, run_logged.LAUNCHER_ERROR_EXIT)
        self.assertEqual(result["status"], "launcher_error")
        self.assertFalse(result["launched"])
        self.assertIsNone(result["child_returncode"])
        self.assertIn("could not launch child", str(result["error"]))
        raw_log = log_path.read_text(encoding="utf-8")
        self.assertIn('"event":"start"', raw_log)
        self.assertIn('"event":"launcher_error"', raw_log)
        self.assert_log_hash(log_path, result)

    def test_hash_finalization_failure_cannot_report_success(self) -> None:
        log_path = self.root / "hash-error.log"
        output = StringIO()
        with patch.object(run_logged, "_sha256_file", side_effect=OSError("injected")):
            with redirect_stdout(output):
                exit_code = run_logged.main(
                    ["--log", str(log_path), "--", sys.executable, "-c", "pass"]
                )

        result = json.loads(output.getvalue())
        self.assertEqual(exit_code, run_logged.LAUNCHER_ERROR_EXIT)
        self.assertEqual(result["status"], "finalization_error")
        self.assertTrue(result["launched"])
        self.assertEqual(result["child_returncode"], 0)
        self.assertIsNone(result["log_sha256"])
        self.assertIn('"event":"child_exit"', log_path.read_text(encoding="utf-8"))

    @unittest.skipUnless(os.name == "posix", "negative subprocess signal status is POSIX-specific")
    def test_signal_termination_keeps_negative_status_and_shell_mapping(self) -> None:
        log_path = self.root / "signaled.log"
        child_code = "import os, signal; os.kill(os.getpid(), signal.SIGTERM)"

        exit_code, result = self.invoke(log_path, [sys.executable, "-c", child_code])

        self.assertEqual(result["status"], "child_signaled")
        self.assertTrue(result["launched"])
        self.assertEqual(result["child_returncode"], -signal.SIGTERM)
        self.assertEqual(exit_code, 128 + signal.SIGTERM)
        self.assert_log_hash(log_path, result)


if __name__ == "__main__":
    unittest.main()
