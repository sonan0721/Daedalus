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
    _v2_poll_until,
    _v2_submit,
    _validate_native_view_report,
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
        self.assertFalse(arguments.view)
        view_arguments = _parse_args(["--cli", "/bin/true", "--view"])
        self.assertTrue(view_arguments.view)
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

    def test_protocol_two_request_and_exit_wait_keep_stdin_open(self) -> None:
        binary = self.create_server_stub(
            "import json, sys, time\n"
            "request = json.loads(sys.stdin.readline())\n"
            "assert request['protocol'] == 2\n"
            "assert request['request'] == {'op': 'poll', 'ticket': 't1'}\n"
            "print(json.dumps({'protocol': 2, 'request_id': request['request_id'], 'ok': True, "
            "'result': {'kind': 'poll', 'data': {'phase': 'terminal'}}}), flush=True)\n"
            "time.sleep(0.05)"
        )
        server = _JsonlProjectServer(
            binary,
            self.project,
            cwd=self.root,
            timeout=1.0,
            command=["project", "view", str(self.project)],
            protocol_version=2,
            process_label="project view",
        )
        with server:
            response = server.request("p2-poll", {"op": "poll", "ticket": "t1"})
            self.assertEqual(response["result"]["data"]["phase"], "terminal")
            self.assertEqual(server.wait_for_exit(timeout=1.0), 0)
            self.assertFalse(server._stdin.closed)
        self.assertTrue(server._stdin.closed)
        self.assertEqual(server.process.returncode, 0)

    def test_default_server_preserves_protocol_one_envelope(self) -> None:
        binary = self.create_server_stub(
            "import json, sys\n"
            "request = json.loads(sys.stdin.readline())\n"
            "assert request == {'protocol': 1, 'request_id': 'legacy-info', "
            "'request': {'op': 'project_info', 'offset': 0, 'limit': 10}}\n"
            "print(json.dumps({'protocol': 1, 'request_id': request['request_id'], 'ok': True, "
            "'result': {'project_name': 'legacy'}}), flush=True)"
        )
        server = _JsonlProjectServer(binary, self.project, cwd=self.root, timeout=1.0)
        with server:
            response = server.request(
                "legacy-info",
                {"op": "project_info", "offset": 0, "limit": 10},
            )
            self.assertEqual(response["protocol"], 1)
            self.assertEqual(response["result"]["project_name"], "legacy")

    def test_error_envelopes_are_available_only_when_requested(self) -> None:
        binary = self.create_server_stub(
            "import json, sys, time\n"
            "request = json.loads(sys.stdin.readline())\n"
            "print(json.dumps({'protocol': 2, 'request_id': request['request_id'], 'ok': False, "
            "'error': {'code': 'request_id_collision', 'message': 'collision'}}), flush=True)\n"
            "time.sleep(0.05)"
        )
        server = _JsonlProjectServer(
            binary,
            self.project,
            cwd=self.root,
            timeout=1.0,
            protocol_version=2,
            process_label="project view",
        )
        with server:
            response = server.request(
                "collision",
                {"op": "submit", "operation": {"op": "status"}},
                allow_error=True,
            )
            self.assertFalse(response["ok"])
            self.assertEqual(response["error"]["code"], "request_id_collision")

    def test_protocol_two_submit_retries_a_bounded_busy_response(self) -> None:
        binary = self.create_server_stub(
            "import json, sys\n"
            "for attempt in range(2):\n"
            "    request = json.loads(sys.stdin.readline())\n"
            "    assert request['protocol'] == 2 and request['request_id'] == 'submit-id'\n"
            "    if attempt == 0:\n"
            "        response = {'protocol': 2, 'request_id': 'submit-id', 'ok': False, "
            "'error': {'code': 'busy', 'message': 'try again', 'retryable': True}}\n"
            "    else:\n"
            "        response = {'protocol': 2, 'request_id': 'submit-id', 'ok': True, "
            "'result': {'kind': 'submitted', 'data': {'ticket': 't9-1-1', 'request_id_reused': False}}}\n"
            "    print(json.dumps(response), flush=True)"
        )
        server = _JsonlProjectServer(
            binary,
            self.project,
            cwd=self.root,
            timeout=1.0,
            protocol_version=2,
            process_label="project view",
        )
        with server:
            ticket, reused = _v2_submit(server, "submit-id", {"op": "status"})
            self.assertEqual(ticket, "t9-1-1")
            self.assertFalse(reused)

    def test_protocol_two_poll_retries_busy_without_losing_ticket(self) -> None:
        binary = self.create_server_stub(
            "import json, sys\n"
            "for attempt in range(2):\n"
            "    request = json.loads(sys.stdin.readline())\n"
            "    assert request['protocol'] == 2 and request['request']['ticket'] == 'ticket-1'\n"
            "    if attempt == 0:\n"
            "        response = {'protocol': 2, 'request_id': request['request_id'], 'ok': False, "
            "'error': {'code': 'busy', 'message': 'try again', 'retryable': True}}\n"
            "    else:\n"
            "        response = {'protocol': 2, 'request_id': request['request_id'], 'ok': True, "
            "'result': {'kind': 'poll', 'data': {'phase': 'terminal', 'authority': None, "
            "'terminal': {'ok': True, 'result': {'kind': 'status', 'data': {}}, 'error': None}}}}\n"
            "    print(json.dumps(response), flush=True)"
        )
        server = _JsonlProjectServer(
            binary,
            self.project,
            cwd=self.root,
            timeout=1.0,
            protocol_version=2,
            process_label="project view",
        )
        with server:
            result = _v2_poll_until(
                server,
                "ticket-1",
                request_prefix="poll",
                phase="terminal",
                timeout=1.0,
            )
            self.assertEqual(result["phase"], "terminal")
            self.assertTrue(result["terminal"]["ok"])

    def test_close_can_exit_without_consuming_or_requiring_jsonl_ack(self) -> None:
        binary = self.create_server_stub(
            "import json, sys, time\n"
            "request = json.loads(sys.stdin.readline())\n"
            "assert request['protocol'] == 2 and request['request']['op'] == 'close'\n"
            "time.sleep(0.05)"
        )
        server = _JsonlProjectServer(
            binary,
            self.project,
            cwd=self.root,
            timeout=1.0,
            command=["project", "view", str(self.project)],
            protocol_version=2,
            process_label="project view close case",
        )
        with server:
            server.send_only("explicit-close", {"op": "close"})
            self.assertEqual(server.wait_for_exit(timeout=1.0), 0)
            self.assertFalse(server._stdin.closed)
        self.assertEqual(server.process.returncode, 0)

    def test_exit_wait_timeout_still_terminates_and_reaps_child(self) -> None:
        binary = self.create_server_stub("import time\ntime.sleep(30)")
        server = _JsonlProjectServer(binary, self.project, cwd=self.root, timeout=1.0)
        started = time.monotonic()
        with self.assertRaisesRegex(SmokeError, "did not exit"):
            with server:
                server.wait_for_exit(timeout=0.1)
        self.assertIsNotNone(server.process.poll())
        self.assertTrue(server._stdin.closed)
        self.assertLess(time.monotonic() - started, 2.0)

    def test_native_report_requires_presented_identity_to_match_authority_and_latest_frame(self) -> None:
        identity = {
            "session": "9",
            "tick": "1",
            "sequence": "2",
            "project_generation": "3",
            "source_digest": "a" * 64,
        }
        report = {
            "command": "project_view",
            "session": "9",
            "source_digest": "a" * 64,
            "authority_final_tick": "1",
            "accepted_packet": dict(identity),
            "last_presented": {
                **identity,
                "camera": {},
                "physical_width": 640,
                "physical_height": 360,
            },
            "window": {"exit_reason": "TimeLimit", "rendered_frames": 1},
            "coordinator": {"protocol_requests": 5, "duplicate_submissions": 1, "protocol_errors": 1},
            "resources": {
                "active_bytes": 12,
                "active_entries": 1,
                "geometry_upload_count": 1,
                "geometry_upload_bytes": 12,
                "instance_write_count": 1,
                "material_write_count": 1,
                "cache_hit_count": 1,
                "cache_miss_count": 1,
                "cache_eviction_count": 0,
            },
        }
        validated = _validate_native_view_report(
            report,
            expected_session="9",
            expected_source_digest="a" * 64,
            expected_tick="1",
            expected_exit_reason="TimeLimit",
            latest_frame=identity,
        )
        self.assertIs(validated, report)

        mismatched = {**report, "last_presented": {**report["last_presented"], "tick": "0"}}
        with self.assertRaisesRegex(SmokeError, "does not match the final accepted packet"):
            _validate_native_view_report(
                mismatched,
                expected_session="9",
                expected_source_digest="a" * 64,
                expected_tick="1",
                expected_exit_reason="TimeLimit",
                latest_frame=identity,
            )

        mismatched_latest = {**identity, "sequence": "1"}
        with self.assertRaisesRegex(SmokeError, "latest runtime frame differs"):
            _validate_native_view_report(
                report,
                expected_session="9",
                expected_source_digest="a" * 64,
                expected_tick="1",
                expected_exit_reason="TimeLimit",
                latest_frame=mismatched_latest,
            )


if __name__ == "__main__":
    unittest.main()
