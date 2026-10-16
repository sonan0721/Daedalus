from __future__ import annotations

import argparse
from contextlib import redirect_stderr
from io import StringIO
import json
import os
import struct
import subprocess
import sys
import tempfile
import time
import unittest
import wave
from pathlib import Path

from smoke_project import (
    MAX_JSONL_RESPONSE_BYTES,
    MAX_STDOUT_PIPE_FILL_BYTES,
    SmokeError,
    _JsonlProjectServer,
    _asset_activation_command,
    _asset_activation_expect_terminal_error,
    _asset_activation_status,
    _asset_preparation_command,
    _asset_preparation_status,
    _fill_stdout_pipe,
    _parse_args,
    _project_view_command,
    _v2_poll_until,
    _v2_submit,
    _decode_glb,
    _write_asset_project,
    _write_asset_preparation_view_project,
    _write_audio_project,
    _write_model_view_project,
    _write_sprite_view_project,
    ENTITY_ID,
    _validate_native_view_report,
    _validate_native_liveness_report,
    _validate_timeout,
)


# These cases exercise protocol behavior with a fresh Python child process.
# A one-second wall-clock bound can expire before the stub is scheduled on a
# busy shared runner, even though the child has no meaningful work to do.
# Keep ordinary request/close cases bounded while leaving the explicit
# timeout-failure tests below at their deliberately short limits.
STUB_TIMEOUT_SECONDS = 5.0


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

    def test_offline_asset_fixture_contains_external_and_embedded_sources(self) -> None:
        project = self.root / "asset-fixture"
        _write_asset_project(project)
        scene = json.loads((project / "scene.json").read_text(encoding="utf-8"))
        self.assertEqual(
            {asset["kind"] for asset in scene["assets"]},
            {
                "daedalus.model",
                "daedalus.image",
                "daedalus.sprite_atlas",
                "daedalus.audio",
            },
        )
        external = json.loads(
            (project / "models" / "external.gltf").read_text(encoding="utf-8")
        )
        self.assertEqual(external["images"][0]["uri"], "../images/base.png")
        external_geometry = (project / "models" / "triangle.bin").read_bytes()
        self.assertEqual(len(external_geometry), 102)
        embedded, binary = _decode_glb((project / "models" / "embedded.glb").read_bytes())
        self.assertEqual(binary[: len(external_geometry)], external_geometry)
        self.assertEqual(embedded["images"][0]["mimeType"], "image/png")
        image_view = embedded["bufferViews"][4]
        start = image_view["byteOffset"]
        self.assertTrue(binary[start : start + 8].startswith(b"\x89PNG\r\n\x1a\n"))
        self.assertTrue((project / "atlases" / "sprites.json").is_file())
        with wave.open(os.fspath(project / "audio" / "tone.wav"), "rb") as audio:
            self.assertEqual((audio.getnchannels(), audio.getframerate(), audio.getnframes()), (1, 8_000, 4))

    def test_audio_fixture_has_two_aliases_for_one_finite_pcm_source(self) -> None:
        project = self.root / "audio-fixture"
        project.mkdir()
        _write_audio_project(project)
        manifest = json.loads((project / "daedalus.project.json").read_text(encoding="utf-8"))
        scene = json.loads((project / "scene.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["name"], "offline-audio-smoke")
        self.assertEqual(scene["entities"], [])
        self.assertEqual(
            [(asset["id"], asset["uri"]) for asset in scene["assets"]],
            [
                ("asset:smoke-tone", "audio/tone.wav"),
                ("asset:smoke-tone-alias", "audio/tone.wav"),
            ],
        )
        with wave.open(os.fspath(project / "audio" / "tone.wav"), "rb") as audio:
            self.assertEqual(
                (audio.getnchannels(), audio.getsampwidth(), audio.getframerate(), audio.getnframes()),
                (1, 2, 8_000, 2_000),
            )
            samples = struct.unpack("<2000h", audio.readframes(2_000))
        self.assertGreater(max(samples), 10_000)
        self.assertLess(min(samples), -10_000)

    def test_model_view_fixture_reuses_offline_textured_gltf_and_authority_entity(self) -> None:
        project = self.root / "model-view-fixture"
        _write_model_view_project(project)
        manifest = json.loads((project / "daedalus.project.json").read_text(encoding="utf-8"))
        scene = json.loads((project / "scene.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["name"], "offline-model-view-smoke")
        self.assertEqual(
            {asset["id"] for asset in scene["assets"]},
            {"asset:model-external"},
        )
        self.assertEqual(len(scene["entities"]), 1)
        entity = scene["entities"][0]
        self.assertEqual(entity["id"], ENTITY_ID)
        components = {component["type_id"]: component for component in entity["components"]}
        self.assertEqual(
            components["daedalus.transform"]["data"],
            {
                "translation": [0.0, 0.0, 0.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0],
            },
        )
        self.assertEqual(
            components["daedalus.model_renderer"]["asset_refs"],
            ["asset:model-external"],
        )
        self.assertEqual(
            components["daedalus.model_renderer"]["data"],
            {"model": "asset:model-external"},
        )
        model = json.loads((project / "models" / "external.gltf").read_text(encoding="utf-8"))
        self.assertEqual(model["images"][0]["uri"], "../images/base.png")
        self.assertTrue((project / "images" / "base.png").is_file())

    def test_sprite_view_fixture_authors_orthographic_camera_and_nearest_atlas(self) -> None:
        project = self.root / "sprite-view-fixture"
        _write_sprite_view_project(project)
        manifest = json.loads((project / "daedalus.project.json").read_text(encoding="utf-8"))
        scene = json.loads((project / "scene.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["name"], "offline-sprite-view-smoke")
        self.assertEqual(
            {asset["id"] for asset in scene["assets"]},
            {"asset:sprite-atlas", "asset:image-atlas"},
        )
        self.assertEqual(len(scene["entities"]), 2)
        camera, sprite = scene["entities"]
        self.assertEqual(camera["id"], ENTITY_ID)
        camera_components = {item["type_id"]: item for item in camera["components"]}
        self.assertEqual(
            camera_components["daedalus.camera_2d"]["data"],
            {"vertical_size": 10.0, "near": 0.1, "far": 100.0},
        )
        self.assertEqual(sprite["id"], "entity:smoke-sprite")
        sprite_components = {item["type_id"]: item for item in sprite["components"]}
        self.assertEqual(
            sprite_components["daedalus.sprite_renderer"]["data"]["region"],
            "sprite:stone",
        )
        self.assertEqual(
            sprite_components["daedalus.transform"]["data"]["translation"],
            [0.0, 0.0, -5.0],
        )
        atlas = json.loads((project / "atlases" / "sprites.json").read_text(encoding="utf-8"))
        self.assertEqual(atlas["regions"][0]["rect"], [0, 0, 16, 16])
        self.assertEqual(atlas["regions"][0]["pivot"], [0.25, 0.75])
        image = (project / "images" / "atlas.png").read_bytes()
        self.assertEqual(image[:8], b"\x89PNG\r\n\x1a\n")
        self.assertEqual(struct.unpack_from(">II", image, 16), (16, 16))

    def test_timeout_parser_rejects_zero_non_finite_and_non_numeric_values(self) -> None:
        self.assertEqual(_validate_timeout("1"), 1.0)
        for value in ("0", "nan", "bad"):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                _validate_timeout(value)

    def test_argparse_accepts_positive_timeout_and_rejects_invalid_values(self) -> None:
        arguments = _parse_args(["--cli", "/bin/true", "--timeout", "1"])
        self.assertEqual(arguments.timeout, 1.0)
        self.assertFalse(arguments.view)
        self.assertFalse(arguments.assets)
        self.assertFalse(arguments.models)
        self.assertFalse(arguments.sprites)
        self.assertFalse(arguments.audio)
        self.assertFalse(arguments.audio_native)
        self.assertFalse(arguments.asset_preparation)
        self.assertFalse(arguments.asset_activation)
        view_arguments = _parse_args(["--cli", "/bin/true", "--view"])
        self.assertTrue(view_arguments.view)
        asset_arguments = _parse_args(["--cli", "/bin/true", "--assets"])
        self.assertTrue(asset_arguments.assets)
        model_arguments = _parse_args(["--cli", "/bin/true", "--models"])
        self.assertTrue(model_arguments.models)
        sprite_arguments = _parse_args(["--cli", "/bin/true", "--sprites"])
        self.assertTrue(sprite_arguments.sprites)
        audio_arguments = _parse_args(["--cli", "/bin/true", "--audio"])
        self.assertTrue(audio_arguments.audio)
        native_audio_arguments = _parse_args(["--cli", "/bin/true", "--audio-native"])
        self.assertTrue(native_audio_arguments.audio_native)
        asset_preparation_arguments = _parse_args(
            ["--cli", "/bin/true", "--asset-preparation"]
        )
        self.assertTrue(asset_preparation_arguments.asset_preparation)
        asset_activation_arguments = _parse_args(
            ["--cli", "/bin/true", "--asset-activation"]
        )
        self.assertTrue(asset_activation_arguments.asset_activation)
        combined_arguments = _parse_args(["--cli", "/bin/true", "--assets", "--view"])
        self.assertTrue(combined_arguments.assets)
        self.assertTrue(combined_arguments.view)
        combined_models = _parse_args(["--cli", "/bin/true", "--assets", "--view", "--models"])
        self.assertTrue(combined_models.assets)
        self.assertTrue(combined_models.view)
        self.assertTrue(combined_models.models)
        combined_sprites = _parse_args(["--cli", "/bin/true", "--models", "--sprites"])
        self.assertTrue(combined_sprites.models)
        self.assertTrue(combined_sprites.sprites)
        combined_audio = _parse_args(["--cli", "/bin/true", "--assets", "--audio", "--audio-native"])
        self.assertTrue(combined_audio.assets)
        self.assertTrue(combined_audio.audio)
        self.assertTrue(combined_audio.audio_native)
        combined_preparation = _parse_args(
            ["--cli", "/bin/true", "--asset-preparation", "--view"]
        )
        self.assertTrue(combined_preparation.asset_preparation)
        self.assertTrue(combined_preparation.view)
        combined_audio_preparation = _parse_args(
            ["--cli", "/bin/true", "--audio", "--asset-preparation"]
        )
        self.assertTrue(combined_audio_preparation.audio)
        self.assertTrue(combined_audio_preparation.asset_preparation)
        combined_activation = _parse_args(
            ["--cli", "/bin/true", "--asset-preparation", "--asset-activation"]
        )
        self.assertTrue(combined_activation.asset_preparation)
        self.assertTrue(combined_activation.asset_activation)
        for value in ("0", "nan", "bad"):
            with self.subTest(value=value), redirect_stderr(StringIO()), self.assertRaises(SystemExit):
                _parse_args(["--cli", "/bin/true", "--timeout", value])

    def test_asset_preparation_view_flag_is_opt_in_and_fixture_keeps_all_assets(self) -> None:
        project = self.root / "asset-preparation-view-fixture"
        _write_asset_preparation_view_project(project)
        manifest = json.loads((project / "daedalus.project.json").read_text(encoding="utf-8"))
        scene = json.loads((project / "scene.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["name"], "offline-asset-preparation-view-smoke")
        self.assertEqual(len(scene["assets"]), 6)
        self.assertEqual(len(scene["entities"]), 1)
        components = {item["type_id"]: item for item in scene["entities"][0]["components"]}
        self.assertEqual(
            components["daedalus.model_renderer"]["data"],
            {"model": "asset:model-external"},
        )

        default = _project_view_command(project, self.root / "view.json", seconds=3)
        prepared = _project_view_command(
            project,
            self.root / "view-prepared.json",
            seconds=3,
            asset_preparation=True,
        )
        self.assertNotIn("--asset-preparation", default)
        self.assertNotIn("--audio", default)
        self.assertIn("--stdio-control", prepared)
        self.assertEqual(prepared[-1], "--asset-preparation")
        prepared_audio = _project_view_command(
            project,
            self.root / "view-prepared-audio.json",
            seconds=3,
            asset_preparation=True,
            audio=True,
        )
        self.assertEqual(
            prepared_audio[prepared_audio.index("--audio") : prepared_audio.index("--audio") + 2],
            ["--audio", "offline"],
        )
        self.assertEqual(prepared_audio[-1], "--asset-preparation")

    def test_asset_preparation_status_uses_bounded_ticket_parser(self) -> None:
        binary = self.create_server_stub(
            "import json, sys\n"
            "status = {'session':'7','active_generation':'1','active_source_digest':'sha256:test',"
            "'active_cooked_accounted_bytes':123,'desired':None,'running':None,'pending':None,"
            "'state':'idle','ready_candidate':None,'latest_failure':None}\n"
            "request = json.loads(sys.stdin.readline())\n"
            "print(json.dumps({'protocol':2,'request_id':request['request_id'],'ok':True,"
            "'result':{'kind':'submitted','data':{'ticket':'t1','request_id_reused':False}}}), flush=True)\n"
            "request = json.loads(sys.stdin.readline())\n"
            "terminal = {'ok':True,'result':{'kind':'asset_preparation','data':{'kind':'status','data':status}}}\n"
            "print(json.dumps({'protocol':2,'request_id':request['request_id'],'ok':True,"
            "'result':{'kind':'poll','data':{'phase':'terminal','authority':None,'terminal':terminal}}}), flush=True)\n"
            "request = json.loads(sys.stdin.readline())\n"
            "print(json.dumps({'protocol':2,'request_id':request['request_id'],'ok':True,"
            "'result':{'kind':'acked'}}), flush=True)\n"
        )
        server = _JsonlProjectServer(
            binary,
            self.project,
            cwd=self.root,
            timeout=STUB_TIMEOUT_SECONDS,
            protocol_version=2,
            process_label="asset-preparation status stub",
        )
        with server:
            status, result = _asset_preparation_status(
                server,
                "asset-status-test",
                timeout=STUB_TIMEOUT_SECONDS,
            )
            self.assertEqual(status["state"], "idle")
            self.assertEqual(status["session"], "7")
            self.assertEqual(result["kind"], "asset_preparation")
        self.assertIsNotNone(server.process.returncode)

    def test_asset_preparation_retries_only_terminal_busy_after_ack(self) -> None:
        binary = self.create_server_stub(
            "import json, sys\n"
            "def read(): return json.loads(sys.stdin.readline())\n"
            "def reply(request, result):\n"
            " print(json.dumps({'protocol':2,'request_id':request['request_id'],'ok':True,'result':result}), flush=True)\n"
            "operation = {'op':'asset_preparation_prepare','command_id':'44'}\n"
            "first_submit = read()\n"
            "assert first_submit['request']['operation'] == operation\n"
            "reply(first_submit, {'kind':'submitted','data':{'ticket':'busy-ticket','request_id_reused':False}})\n"
            "poll = read()\n"
            "busy = {'ok':False,'error':{'code':'busy','message':'try lock busy'}}\n"
            "reply(poll, {'kind':'poll','data':{'phase':'terminal','authority':None,'terminal':busy}})\n"
            "ack = read()\n"
            "assert ack['request']['op'] == 'ack' and ack['request']['ticket'] == 'busy-ticket'\n"
            "reply(ack, {'kind':'acked'})\n"
            "second_submit = read()\n"
            "assert second_submit['request_id'] != first_submit['request_id']\n"
            "assert second_submit['request']['operation'] == operation\n"
            "reply(second_submit, {'kind':'submitted','data':{'ticket':'ready-ticket','request_id_reused':False}})\n"
            "poll = read()\n"
            "command = {'command_id':'44','observed_tick':'0','outcome':{'kind':'prepare_accepted','data':{'candidate':{'session':'7','epoch':'1'}}}}\n"
            "terminal = {'ok':True,'result':{'kind':'asset_preparation','data':{'kind':'command','data':command}}}\n"
            "reply(poll, {'kind':'poll','data':{'phase':'terminal','authority':None,'terminal':terminal}})\n"
            "ack = read()\n"
            "assert ack['request']['op'] == 'ack' and ack['request']['ticket'] == 'ready-ticket'\n"
            "reply(ack, {'kind':'acked'})\n"
        )
        server = _JsonlProjectServer(
            binary,
            self.project,
            cwd=self.root,
            timeout=STUB_TIMEOUT_SECONDS,
            protocol_version=2,
            process_label="asset-preparation Busy retry stub",
        )
        with server:
            command, _ = _asset_preparation_command(
                server,
                "asset-preparation-busy-retry",
                {"op": "asset_preparation_prepare", "command_id": "44"},
                command_id="44",
                outcome_kind="prepare_accepted",
                timeout=STUB_TIMEOUT_SECONDS,
            )
        self.assertEqual(command["command_id"], "44")
        self.assertEqual(command["outcome"]["data"]["candidate"]["epoch"], "1")
        self.assertIsNotNone(server.process.returncode)

    def test_asset_preparation_timeout_terminates_and_reaps_server(self) -> None:
        binary = self.create_server_stub(
            "import json, sys, time\n"
            "request = json.loads(sys.stdin.readline())\n"
            "print(json.dumps({'protocol':2,'request_id':request['request_id'],'ok':True,"
            "'result':{'kind':'submitted','data':{'ticket':'t1','request_id_reused':False}}}), flush=True)\n"
            "json.loads(sys.stdin.readline())\n"
            "time.sleep(30)\n"
        )
        server = _JsonlProjectServer(
            binary,
            self.project,
            cwd=self.root,
            timeout=1.0,
            protocol_version=2,
            process_label="asset-preparation stalled status stub",
        )
        started = time.monotonic()
        with self.assertRaisesRegex(SmokeError, "timed out"):
            with server:
                _asset_preparation_status(server, "asset-status-timeout", timeout=0.15)

        self.assertIsNotNone(server.process.poll())
        self.assertTrue(server._stdin.closed)
        self.assertTrue(server._stdout.closed)
        self.assertLess(time.monotonic() - started, 2.0)

    def test_asset_preparation_terminal_failure_surfaces_code_and_reaps_server(self) -> None:
        binary = self.create_server_stub(
            "import json, sys\n"
            "request = json.loads(sys.stdin.readline())\n"
            "print(json.dumps({'protocol':2,'request_id':request['request_id'],'ok':True,"
            "'result':{'kind':'submitted','data':{'ticket':'t1','request_id_reused':False}}}), flush=True)\n"
            "request = json.loads(sys.stdin.readline())\n"
            "terminal = {'ok':False,'error':{'code':'asset_preparation_unavailable','message':'disabled'}}\n"
            "print(json.dumps({'protocol':2,'request_id':request['request_id'],'ok':True,"
            "'result':{'kind':'poll','data':{'phase':'terminal','authority':None,'terminal':terminal}}}), flush=True)\n"
            "import time; time.sleep(30)\n"
        )
        server = _JsonlProjectServer(
            binary,
            self.project,
            cwd=self.root,
            timeout=STUB_TIMEOUT_SECONDS,
            protocol_version=2,
            process_label="asset-preparation failed status stub",
        )
        with self.assertRaisesRegex(SmokeError, "asset_preparation_unavailable"):
            with server:
                _asset_preparation_status(server, "asset-status-failure", timeout=STUB_TIMEOUT_SECONDS)

        self.assertIsNotNone(server.process.poll())
        self.assertTrue(server._stdin.closed)
        self.assertTrue(server._stdout.closed)

    def test_asset_activation_status_and_stage_use_bounded_ticket_ack(self) -> None:
        binary = self.create_server_stub(
            "import json, sys\n"
            "def read(): return json.loads(sys.stdin.readline())\n"
            "def reply(request, result):\n"
            " print(json.dumps({'protocol':2,'request_id':request['request_id'],'ok':True,'result':result}), flush=True)\n"
            "status = {'session':'7','state':'idle','active_generation':'1','active_source_digest':'sha256:active',"
            "'current_activation':None,'latest_failure':None,'memory':{'active_cooked_accounted_bytes':20,"
            "'candidate_cooked_accounted_bytes':0,'retired_cooked_accounted_bytes':0,'total_cooked_accounted_bytes':20,"
            "'max_cooked_accounted_bytes':100,'retired_generation_count':0}}\n"
            "first = read(); assert first['request']['operation'] == {'op':'asset_activation_status'}\n"
            "reply(first, {'kind':'submitted','data':{'ticket':'status-ticket','request_id_reused':False}})\n"
            "poll = read(); reply(poll, {'kind':'poll','data':{'phase':'terminal','authority':None,'terminal':"
            "{'ok':True,'result':{'kind':'asset_activation','data':{'kind':'status','data':status}}}}})\n"
            "ack = read(); assert ack['request']['ticket'] == 'status-ticket'; reply(ack, {'kind':'acked'})\n"
            "stage_operation = {'op':'asset_activation_stage','candidate':{'session':'7','epoch':'3'},"
            "'expected_active_generation':'1','command_id':'44'}\n"
            "stage = read(); assert stage['request']['operation'] == stage_operation\n"
            "reply(stage, {'kind':'submitted','data':{'ticket':'stage-ticket','request_id_reused':False}})\n"
            "poll = read(); command = {'command_id':'44','observed_tick':'2','outcome':{'kind':'stage_accepted',"
            "'data':{'activation':{'session':'7','sequence':'1'},'candidate':{'session':'7','epoch':'3'}}}}\n"
            "terminal = {'ok':True,'result':{'kind':'asset_activation','data':{'kind':'command','data':command}}}\n"
            "reply(poll, {'kind':'poll','data':{'phase':'terminal','authority':None,'terminal':terminal}})\n"
            "ack = read(); assert ack['request']['ticket'] == 'stage-ticket'; reply(ack, {'kind':'acked'})\n"
        )
        server = _JsonlProjectServer(
            binary,
            self.project,
            cwd=self.root,
            timeout=STUB_TIMEOUT_SECONDS,
            protocol_version=2,
            process_label="asset-activation result stub",
        )
        with server:
            status, _ = _asset_activation_status(
                server,
                "asset-activation-status-test",
                timeout=STUB_TIMEOUT_SECONDS,
            )
            self.assertEqual(status["active_generation"], "1")
            command, _ = _asset_activation_command(
                server,
                "asset-activation-stage-test",
                {
                    "op": "asset_activation_stage",
                    "candidate": {"session": "7", "epoch": "3"},
                    "expected_active_generation": "1",
                    "command_id": "44",
                },
                command_id="44",
                outcome_kind="stage_accepted",
                timeout=STUB_TIMEOUT_SECONDS,
            )
        self.assertEqual(command["outcome"]["data"]["activation"]["sequence"], "1")
        self.assertIsNotNone(server.process.returncode)

    def test_asset_activation_expected_generation_error_is_acked(self) -> None:
        binary = self.create_server_stub(
            "import json, sys\n"
            "def read(): return json.loads(sys.stdin.readline())\n"
            "def reply(request, result):\n"
            " print(json.dumps({'protocol':2,'request_id':request['request_id'],'ok':True,'result':result}), flush=True)\n"
            "stale = read(); stale_operation = stale['request']['operation']\n"
            "assert stale_operation['command_id'] == '44' and stale_operation['expected_active_generation'] == '1'\n"
            "reply(stale, {'kind':'submitted','data':{'ticket':'stale-ticket','request_id_reused':False}})\n"
            "poll = read(); terminal = {'ok':False,'error':{'code':'active_generation_mismatch','message':'stale generation'}}\n"
            "reply(poll, {'kind':'poll','data':{'phase':'terminal','authority':None,'terminal':terminal}})\n"
            "ack = read(); assert ack['request']['op'] == 'ack' and ack['request']['ticket'] == 'stale-ticket'\n"
            "reply(ack, {'kind':'acked'})\n"
            "retry = read(); retry_operation = retry['request']['operation']\n"
            "assert retry_operation['command_id'] == stale_operation['command_id']\n"
            "assert retry_operation['expected_active_generation'] == '2'\n"
            "reply(retry, {'kind':'submitted','data':{'ticket':'retry-ticket','request_id_reused':False}})\n"
            "poll = read(); command = {'command_id':'44','observed_tick':'2','outcome':{'kind':'stage_accepted',"
            "'data':{'activation':{'session':'7','sequence':'1'},'candidate':{'session':'7','epoch':'3'}}}}\n"
            "terminal = {'ok':True,'result':{'kind':'asset_activation','data':{'kind':'command','data':command}}}\n"
            "reply(poll, {'kind':'poll','data':{'phase':'terminal','authority':None,'terminal':terminal}})\n"
            "ack = read(); assert ack['request']['op'] == 'ack' and ack['request']['ticket'] == 'retry-ticket'\n"
            "reply(ack, {'kind':'acked'})\n"
        )
        server = _JsonlProjectServer(
            binary,
            self.project,
            cwd=self.root,
            timeout=STUB_TIMEOUT_SECONDS,
            protocol_version=2,
            process_label="asset-activation generation mismatch stub",
        )
        with server:
            poll = _asset_activation_expect_terminal_error(
                server,
                "asset-activation-generation-mismatch-test",
                {
                    "op": "asset_activation_stage",
                    "candidate": {"session": "7", "epoch": "3"},
                    "expected_active_generation": "1",
                    "command_id": "44",
                },
                code="active_generation_mismatch",
                timeout=STUB_TIMEOUT_SECONDS,
            )
            command, _ = _asset_activation_command(
                server,
                "asset-activation-generation-mismatch-retry",
                {
                    "op": "asset_activation_stage",
                    "candidate": {"session": "7", "epoch": "3"},
                    "expected_active_generation": "2",
                    "command_id": "44",
                },
                command_id="44",
                outcome_kind="stage_accepted",
                timeout=STUB_TIMEOUT_SECONDS,
            )
        self.assertEqual(poll["terminal"]["error"]["code"], "active_generation_mismatch")
        self.assertEqual(command["command_id"], "44")
        self.assertEqual(command["outcome"]["kind"], "stage_accepted")
        self.assertIsNotNone(server.process.returncode)

    def test_asset_activation_status_timeout_terminates_and_reaps_server(self) -> None:
        binary = self.create_server_stub(
            "import json, sys, time\n"
            "request = json.loads(sys.stdin.readline())\n"
            "print(json.dumps({'protocol':2,'request_id':request['request_id'],'ok':True,"
            "'result':{'kind':'submitted','data':{'ticket':'t1','request_id_reused':False}}}), flush=True)\n"
            "json.loads(sys.stdin.readline())\n"
            "time.sleep(30)\n"
        )
        server = _JsonlProjectServer(
            binary,
            self.project,
            cwd=self.root,
            timeout=1.0,
            protocol_version=2,
            process_label="asset-activation stalled status stub",
        )
        started = time.monotonic()
        with self.assertRaisesRegex(SmokeError, "timed out"):
            with server:
                _asset_activation_status(
                    server,
                    "asset-activation-status-timeout",
                    timeout=0.15,
                )
        self.assertIsNotNone(server.process.poll())
        self.assertTrue(server._stdin.closed)
        self.assertTrue(server._stdout.closed)
        self.assertLess(time.monotonic() - started, 2.0)

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
            timeout=STUB_TIMEOUT_SECONDS,
            command=["project", "view", str(self.project)],
            protocol_version=2,
            process_label="project view",
        )
        with server:
            response = server.request("p2-poll", {"op": "poll", "ticket": "t1"})
            self.assertEqual(response["result"]["data"]["phase"], "terminal")
            self.assertEqual(server.wait_for_exit(timeout=STUB_TIMEOUT_SECONDS), 0)
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
        server = _JsonlProjectServer(binary, self.project, cwd=self.root, timeout=STUB_TIMEOUT_SECONDS)
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
            timeout=STUB_TIMEOUT_SECONDS,
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
            timeout=STUB_TIMEOUT_SECONDS,
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
            timeout=STUB_TIMEOUT_SECONDS,
            protocol_version=2,
            process_label="project view",
        )
        with server:
            result = _v2_poll_until(
                server,
                "ticket-1",
                request_prefix="poll",
                phase="terminal",
                timeout=STUB_TIMEOUT_SECONDS,
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
            timeout=STUB_TIMEOUT_SECONDS,
            command=["project", "view", str(self.project)],
            protocol_version=2,
            process_label="project view close case",
        )
        with server:
            server.send_only("explicit-close", {"op": "close"})
            self.assertEqual(server.wait_for_exit(timeout=STUB_TIMEOUT_SECONDS), 0)
            self.assertFalse(server._stdin.closed)
        self.assertEqual(server.process.returncode, 0)

    def test_exit_wait_timeout_still_terminates_and_reaps_child(self) -> None:
        binary = self.create_server_stub("import time\ntime.sleep(30)")
        server = _JsonlProjectServer(binary, self.project, cwd=self.root, timeout=STUB_TIMEOUT_SECONDS)
        started = time.monotonic()
        with self.assertRaisesRegex(SmokeError, "did not exit"):
            with server:
                server.wait_for_exit(timeout=0.1)
        self.assertIsNotNone(server.process.poll())
        self.assertTrue(server._stdin.closed)
        self.assertLess(time.monotonic() - started, 2.0)

    def test_devnull_eof_and_uncaptured_stdout_server_cleanup(self) -> None:
        read_fd, write_fd = os.pipe()
        binary = self.create_server_stub("import sys\nassert sys.stdin.read() == ''")
        server = None
        try:
            server = _JsonlProjectServer(
                binary,
                self.project,
                cwd=self.root,
                timeout=STUB_TIMEOUT_SECONDS,
                stdin_target=subprocess.DEVNULL,
                stdout_target=write_fd,
            )
            os.close(write_fd)
            write_fd = -1
            with server:
                self.assertIsNone(server._stdin)
                self.assertIsNone(server._stdout)
                self.assertIsNone(server._selector)
                self.assertEqual(server.wait_for_exit(timeout=STUB_TIMEOUT_SECONDS), 0)
        finally:
            if server is not None and not server._closed:
                try:
                    server._terminate_if_running()
                finally:
                    server._close_streams()
            for fd in (read_fd, write_fd):
                if fd >= 0:
                    os.close(fd)

    def test_filled_stdout_pipe_stays_unread_while_child_exits(self) -> None:
        if os.name != "posix":
            self.skipTest("native stdout backpressure uses POSIX pipes")
        import fcntl

        read_fd, write_fd = os.pipe()
        server = None
        try:
            original_flags = fcntl.fcntl(write_fd, fcntl.F_GETFL)
            filled = _fill_stdout_pipe(write_fd)
            self.assertGreater(filled, 0)
            self.assertLessEqual(filled, MAX_STDOUT_PIPE_FILL_BYTES)
            restored_flags = fcntl.fcntl(write_fd, fcntl.F_GETFL)
            self.assertEqual(restored_flags & os.O_NONBLOCK, original_flags & os.O_NONBLOCK)

            binary = self.create_server_stub(
                "import json, os, sys, threading, time\n"
                "request = json.loads(sys.stdin.readline())\n"
                "assert request['request']['op'] == 'capabilities'\n"
                "def exit_after_timeout():\n"
                "    time.sleep(0.1)\n"
                "    os._exit(0)\n"
                "threading.Thread(target=exit_after_timeout, daemon=True).start()\n"
                "os.write(1, b'x' * (1024 * 1024))"
            )
            server = _JsonlProjectServer(
                binary,
                self.project,
                cwd=self.root,
                timeout=2.0,
                protocol_version=2,
                process_label="blocked stdout stub",
                stdout_target=write_fd,
            )
            os.close(write_fd)
            write_fd = -1
            with server:
                server.send_only("capabilities", {"op": "capabilities"})
                self.assertEqual(server.wait_for_exit(timeout=2.0), 0)
                self.assertFalse(server._stdin.closed)
                self.assertIsNone(server._stdout)
            self.assertEqual(server.process.returncode, 0)
        finally:
            if server is not None and not server._closed:
                try:
                    server._terminate_if_running()
                finally:
                    server._close_streams()
            for fd in (read_fd, write_fd):
                if fd >= 0:
                    os.close(fd)

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
                "render_view": {"camera": {}, "projection": {"kind": "perspective"}},
                "physical_width": 640,
                "physical_height": 360,
            },
            "window": {
                "exit_reason": "TimeLimit",
                "rendered_frames": 1,
                "final_camera": {},
                "final_render_view": {"camera": {}, "projection": {"kind": "perspective"}},
            },
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

    def test_native_liveness_report_requires_normal_time_limit_and_transport_identity(self) -> None:
        identity = {
            "session": "11",
            "tick": "0",
            "sequence": "1",
            "project_generation": "2",
            "source_digest": "b" * 64,
        }
        report = {
            "command": "project_view",
            "session": "11",
            "source_digest": "b" * 64,
            "authority_final_tick": "0",
            "termination": "StopRequested",
            "accepted_packet": dict(identity),
            "last_presented": {
                **identity,
                "physical_width": 640,
                "physical_height": 360,
            },
            "window": {"exit_reason": "TimeLimit", "rendered_frames": 2},
            "coordinator": {
                "protocol_detach_reason": "stdin_eof",
                "protocol_requests": 0,
                "protocol_errors": 0,
            },
            "resources": {"geometry_upload_count": 1, "geometry_upload_bytes": 12},
        }
        validated = _validate_native_liveness_report(
            report,
            expected_source_digest="b" * 64,
            expected_detach_reason="stdin_eof",
            expected_protocol_requests=0,
        )
        self.assertIs(validated, report)

        blocked_stdout = {
            **report,
            "coordinator": {
                "protocol_detach_reason": None,
                "protocol_requests": 1,
                "protocol_errors": 0,
            },
        }
        validated_blocked = _validate_native_liveness_report(
            blocked_stdout,
            expected_source_digest="b" * 64,
            expected_detach_reason=None,
            expected_protocol_requests=1,
        )
        self.assertIs(validated_blocked, blocked_stdout)

        not_presented = {**report, "window": {"exit_reason": "TimeLimit", "rendered_frames": 0}}
        with self.assertRaisesRegex(SmokeError, "presented no surface frames"):
            _validate_native_liveness_report(
                not_presented,
                expected_source_digest="b" * 64,
                expected_detach_reason="stdin_eof",
                expected_protocol_requests=0,
            )


if __name__ == "__main__":
    unittest.main()
