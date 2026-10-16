#!/usr/bin/env python3
"""Run a bounded CPU smoke against an already-built Daedalus CLI binary."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import selectors
import struct
import subprocess
import sys
import tempfile
import time
import wave
import zlib
from pathlib import Path
from typing import Any, Sequence


PROTOCOL_VERSION = 1
VIEW_PROTOCOL_VERSION = 2
DEFAULT_TIMEOUT_SECONDS = 30.0
MAX_TIMEOUT_SECONDS = 300.0
MAX_JSONL_RESPONSE_BYTES = 1024 * 1024
# Frozen v1 cache envelope layout; mutate only payload bytes so the type/key
# identity stays valid and recovery exercises the stored payload digest.
CACHE_ENVELOPE_HEADER_BYTES = 8 + 2 + 1 + 1 + 32 + 8 + 32
NATIVE_VIEW_SECONDS = 3
NATIVE_OBSERVER_SECONDS = 2
NATIVE_VIEW_WIDTH = 640
NATIVE_VIEW_HEIGHT = 360
MAX_STDOUT_PIPE_FILL_BYTES = 8 * 1024 * 1024
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
        suffix = "" if context is None else f"; response: {json.dumps(context, sort_keys=True, default=str)}"
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


def _png_rgba(width: int, height: int, color: tuple[int, int, int, int]) -> bytes:
    row = b"\x00" + bytes(color) * width
    pixels = row * height

    def chunk(kind: bytes, payload: bytes) -> bytes:
        body = kind + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(pixels, level=9))
        + chunk(b"IEND", b"")
    )


def _triangle_binary(first_x: float = -0.5) -> bytes:
    positions = (first_x, -0.5, 0.0, 0.5, -0.5, 0.0, 0.0, 0.5, 0.0)
    normals = (0.0, 0.0, 1.0) * 3
    texcoords = (0.0, 0.0, 1.0, 0.0, 0.5, 1.0)
    return struct.pack("<9f9f6f3H", *positions, *normals, *texcoords, 0, 1, 2)


def _gltf_triangle_document(image_uri: str) -> dict[str, Any]:
    return {
        "asset": {"version": "2.0", "generator": "Daedalus offline asset smoke"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0, "translation": [0.0, 0.0, 0.0]}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3, "material": 0}]}],
        "materials": [{"pbrMetallicRoughness": {"baseColorFactor": [1.0, 1.0, 1.0, 1.0], "metallicFactor": 0.0, "roughnessFactor": 0.8, "baseColorTexture": {"index": 0}}}],
        "textures": [{"source": 0, "sampler": 0}],
        "samplers": [{"magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497}],
        "images": [{"uri": image_uri}],
        "buffers": [{"uri": "triangle.bin", "byteLength": 102}],
        "bufferViews": [
            {"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962},
            {"buffer": 0, "byteOffset": 36, "byteLength": 36, "target": 34962},
            {"buffer": 0, "byteOffset": 72, "byteLength": 24, "target": 34962},
            {"buffer": 0, "byteOffset": 96, "byteLength": 6, "target": 34963},
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [-0.5, -0.5, 0.0], "max": [0.5, 0.5, 0.0]},
            {"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3"},
            {"bufferView": 2, "componentType": 5126, "count": 3, "type": "VEC2"},
            {"bufferView": 3, "componentType": 5123, "count": 3, "type": "SCALAR", "min": [0], "max": [2]},
        ],
    }


def _encode_glb(document: dict[str, Any], binary: bytes) -> bytes:
    document = json.loads(json.dumps(document))
    document["buffers"][0]["byteLength"] = len(binary)
    json_chunk = json.dumps(document, separators=(",", ":")).encode("utf-8")
    json_chunk += b" " * ((-len(json_chunk)) % 4)
    binary_chunk = binary + b"\x00" * ((-len(binary)) % 4)
    total_length = 12 + 8 + len(json_chunk) + 8 + len(binary_chunk)
    return (
        b"glTF"
        + struct.pack("<II", 2, total_length)
        + struct.pack("<II", len(json_chunk), 0x4E4F534A)
        + json_chunk
        + struct.pack("<II", len(binary_chunk), 0x004E4942)
        + binary_chunk
    )


def _decode_glb(data: bytes) -> tuple[dict[str, Any], bytes]:
    if data[:4] != b"glTF" or len(data) < 20:
        raise SmokeError("offline smoke generated an invalid GLB header")
    declared_length = struct.unpack_from("<I", data, 8)[0]
    if declared_length != len(data):
        raise SmokeError("offline smoke generated a GLB with the wrong declared length")
    offset = 12
    document: dict[str, Any] | None = None
    binary: bytes | None = None
    while offset < len(data):
        chunk_length, chunk_type = struct.unpack_from("<II", data, offset)
        offset += 8
        chunk = data[offset : offset + chunk_length]
        offset += chunk_length
        if chunk_type == 0x4E4F534A:
            document = json.loads(chunk.decode("utf-8").rstrip(" \x00"))
        elif chunk_type == 0x004E4942:
            binary = chunk
    if document is None or binary is None:
        raise SmokeError("offline smoke generated a GLB without JSON or BIN chunks")
    return document, binary


def _write_asset_project(directory: Path) -> None:
    for subdirectory in ("models", "images", "atlases", "audio"):
        (directory / subdirectory).mkdir(parents=True, exist_ok=True)
    base_png = _png_rgba(2, 2, (210, 80, 40, 255))
    atlas_png = _png_rgba(16, 16, (40, 180, 70, 255))
    model_png = _png_rgba(2, 2, (40, 80, 210, 255))
    (directory / "images" / "base.png").write_bytes(base_png)
    (directory / "images" / "atlas.png").write_bytes(atlas_png)
    external_document = _gltf_triangle_document("../images/base.png")
    (directory / "models" / "external.gltf").write_text(
        json.dumps(external_document, separators=(",", ":")) + "\n", encoding="utf-8"
    )
    (directory / "models" / "triangle.bin").write_bytes(_triangle_binary())

    embedded_binary = _triangle_binary()
    image_offset = (len(embedded_binary) + 3) & ~3
    embedded_binary += b"\x00" * (image_offset - len(embedded_binary)) + model_png
    embedded_document = _gltf_triangle_document("unused.png")
    embedded_document["buffers"] = [{"byteLength": len(embedded_binary)}]
    embedded_document["images"] = [
        {"bufferView": 4, "mimeType": "image/png"}
    ]
    embedded_document["bufferViews"].append(
        {"buffer": 0, "byteOffset": image_offset, "byteLength": len(model_png)}
    )
    (directory / "models" / "embedded.glb").write_bytes(
        _encode_glb(embedded_document, embedded_binary)
    )

    atlas = {
        "schema_version": 1,
        "image_asset": "asset:image-atlas",
        "filter_policy": "nearest_no_mip_v1",
        "regions": [
            {
                "id": "sprite:stone",
                "rect": [0, 0, 8, 8],
                "pivot": [0.5, 0.5],
                "pixels_per_unit": 16.0,
            }
        ],
    }
    (directory / "atlases" / "sprites.json").write_text(
        json.dumps(atlas, separators=(",", ":")) + "\n", encoding="utf-8"
    )
    with wave.open(os.fspath(directory / "audio" / "tone.wav"), "wb") as audio:
        audio.setnchannels(1)
        audio.setsampwidth(2)
        audio.setframerate(8_000)
        audio.writeframes(struct.pack("<4h", 0, 1_000, -1_000, 0))

    manifest = {"schema_version": 1, "name": "offline-asset-smoke", "scene": "scene.json"}
    scene = {
        "schema_version": 1,
        "revision": 0,
        "entities": [],
        "assets": [
            {"id": "asset:model-external", "kind": "daedalus.model", "uri": "models/external.gltf", "metadata": {"schema_version": 1}},
            {"id": "asset:model-embedded", "kind": "daedalus.model", "uri": "models/embedded.glb", "metadata": {"schema_version": 1}},
            {"id": "asset:image-base", "kind": "daedalus.image", "uri": "images/base.png", "metadata": {"schema_version": 1}},
            {"id": "asset:sprite-atlas", "kind": "daedalus.sprite_atlas", "uri": "atlases/sprites.json", "metadata": {"schema_version": 1}},
            {"id": "asset:image-atlas", "kind": "daedalus.image", "uri": "images/atlas.png", "metadata": {"schema_version": 1, "role": "sprite_atlas_srgb", "mip_policy": "none", "sampler": {"min_filter": "nearest", "mag_filter": "nearest", "mipmap_filter": "none", "wrap_u": "clamp_to_edge", "wrap_v": "clamp_to_edge"}}},
            {"id": "asset:audio-tone", "kind": "daedalus.audio", "uri": "audio/tone.wav", "metadata": {"schema_version": 1}},
        ],
    }
    (directory / "daedalus.project.json").write_text(
        json.dumps(manifest, separators=(",", ":")) + "\n", encoding="utf-8"
    )
    (directory / "scene.json").write_text(
        json.dumps(scene, separators=(",", ":")) + "\n", encoding="utf-8"
    )


def _write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, separators=(",", ":")) + "\n", encoding="utf-8")


def _cook_project(
    binary: Path,
    project: Path,
    *,
    cwd: Path,
    timeout: float,
    cache_dir: Path | None = None,
    expect_success: bool = True,
) -> dict[str, Any]:
    arguments = ["project", "cook", os.fspath(project)]
    if cache_dir is not None:
        arguments.extend(("--cache-dir", os.fspath(cache_dir)))
    result = _run_cli(
        binary,
        arguments,
        cwd=cwd,
        timeout=timeout,
        expect_success=expect_success,
    )
    response_bytes = result.stdout.encode("utf-8")
    _expect(
        len(response_bytes) <= MAX_JSONL_RESPONSE_BYTES,
        "project cook response exceeded the 1 MiB JSON line limit",
        len(response_bytes),
    )
    output = _json_output(result, arguments)
    _expect(output.get("command") == "project_cook", "project cook returned wrong command", output)
    _expect(output.get("ok") is expect_success, "project cook returned wrong result status", output)
    return output


def _cook_metrics(output: dict[str, Any]) -> dict[str, Any]:
    metrics = output.get("metrics")
    _expect(isinstance(metrics, dict), "project cook omitted its metrics", output)
    return metrics


def _cook_expect_unchanged_work(output: dict[str, Any], *, label: str) -> None:
    metrics = _cook_metrics(output)
    for name in (
        "modelGeometryCooks",
        "pngBaseDecodes",
        "imageCooks",
        "atlasCooks",
        "audioCooks",
    ):
        _expect(metrics.get(name) == 0, f"{label} unexpectedly repeated {name}", metrics)


def _cook_expect_only(
    output: dict[str, Any],
    *,
    label: str,
    changed_metric: str,
    minimum: int = 1,
) -> None:
    metrics = _cook_metrics(output)
    _expect(
        isinstance(metrics.get(changed_metric), int) and metrics[changed_metric] >= minimum,
        f"{label} did not perform the expected {changed_metric}",
        metrics,
    )
    for name in ("modelGeometryCooks", "imageCooks", "atlasCooks", "audioCooks"):
        if name != changed_metric:
            _expect(metrics.get(name) == 0, f"{label} unexpectedly repeated {name}", metrics)
    expected_decodes = 1 if changed_metric == "imageCooks" else 0
    decode_count = metrics.get("pngBaseDecodes")
    if expected_decodes:
        _expect(
            isinstance(decode_count, int) and decode_count >= expected_decodes,
            f"{label} did not decode the changed PNG source",
            metrics,
        )
    else:
        _expect(decode_count == 0, f"{label} unexpectedly decoded an unchanged PNG", metrics)


def _geometry_keys(output: dict[str, Any]) -> set[str]:
    assets = output.get("assets")
    _expect(isinstance(assets, list), "project cook omitted asset summaries", output)
    return {
        item["key"]
        for asset in assets
        if asset.get("kind") == "model"
        for item in asset.get("resourceKeys", [])
        if item.get("kind") == "geometry"
    }


def _run_asset_cook_smoke(
    binary: Path,
    *,
    root: Path,
    cwd: Path,
    timeout: float,
) -> dict[str, Any]:
    project = root / "offline-asset-project"
    _write_asset_project(project)
    cache_root = project / ".daedalus" / "cache"

    original_sources = {
        path.relative_to(project).as_posix(): _sha256_file(path)
        for path in project.rglob("*")
        if path.is_file()
    }
    initial = _cook_project(binary, project, cwd=cwd, timeout=timeout)
    _expect(initial.get("ok") is True, "project cook did not report a valid catalog", initial)
    _expect(
        Path(initial.get("cacheRoot", "")) == cache_root,
        "project cook did not use the default project-local cache root",
        initial.get("cacheRoot"),
    )
    _expect(initial.get("cacheNamespace") == "v1", "project cook omitted its versioned cache namespace", initial)
    counts = initial.get("counts", {})
    _expect(
        counts.get("assetsByKind")
        == {"models": 2, "images": 2, "spriteAtlases": 1, "audio": 1},
        "asset fixture did not cook glTF, GLB, PNG, atlas, and WAV content",
        counts,
    )
    summaries = initial.get("assets")
    _expect(isinstance(summaries, list) and len(summaries) == 6, "asset fixture returned the wrong catalog size", initial)
    model_summaries = [asset for asset in summaries if asset.get("kind") == "model"]
    _expect(
        len(model_summaries) == 2
        and all(asset.get("nodes") == 1 and asset.get("primitives") == 1 for asset in model_summaries)
        and all(asset.get("vertices") == 3 and asset.get("indices") == 3 for asset in model_summaries),
        "model summaries omitted actual cooked triangle data",
        model_summaries,
    )
    atlas_summary = next((asset for asset in summaries if asset.get("kind") == "sprite_atlas"), None)
    audio_summary = next((asset for asset in summaries if asset.get("kind") == "audio"), None)
    _expect(atlas_summary is not None and atlas_summary.get("atlasRegions") == 1, "atlas summary omitted its region", atlas_summary)
    _expect(
        audio_summary is not None
        and audio_summary.get("audioFrames") == 4
        and audio_summary.get("audioChannels") == 1
        and audio_summary.get("audioSampleRate") == 8_000,
        "WAV summary omitted its decoded PCM metadata",
        audio_summary,
    )
    image_dimensions = {
        (asset.get("width"), asset.get("height"))
        for asset in summaries
        if asset.get("kind") == "image"
    }
    _expect((2, 2) in image_dimensions and (16, 16) in image_dimensions, "PNG summaries omitted decoded dimensions", image_dimensions)
    first_geometry_keys = _geometry_keys(initial)
    _expect(
        len(first_geometry_keys) == 1,
        "equivalent glTF and GLB primitives did not share one canonical geometry identity",
        first_geometry_keys,
    )
    _expect(
        _cook_metrics(initial).get("modelGeometryCooks") == 1,
        "equivalent glTF and GLB primitives performed more than one geometry cook",
        _cook_metrics(initial),
    )
    source_ids = {source.get("sourceId") for source in initial.get("sources", [])}
    _expect(
        {"models/external.gltf", "models/triangle.bin", "models/embedded.glb", "images/base.png", "images/atlas.png", "audio/tone.wav"}.issubset(source_ids),
        "cook report omitted transitive source identities",
        source_ids,
    )
    _expect(
        original_sources
        == {
            path.relative_to(project).as_posix(): _sha256_file(path)
            for path in project.rglob("*")
            if path.is_file() and ".daedalus" not in path.parts
        },
        "project cook modified authored project source files",
        project,
    )

    warm = _cook_project(binary, project, cwd=cwd, timeout=timeout)
    _cook_expect_unchanged_work(warm, label="warm no-change cook")
    _expect(_cook_metrics(warm).get("cacheHits", 0) > 0, "warm no-change cook reported no cache reuse", warm)
    checks = ["asset_cook_formats_and_typed_payload_summary", "asset_cook_warm_no_change_reuse"]

    base_png = project / "images" / "base.png"
    base_png.write_bytes(_png_rgba(2, 2, (211, 81, 41, 255)))
    external_image = _cook_project(binary, project, cwd=cwd, timeout=timeout)
    _cook_expect_only(external_image, label="external PNG edit", changed_metric="imageCooks")
    _expect(_geometry_keys(external_image) == first_geometry_keys, "external PNG edit changed geometry identity", external_image)
    checks.append("asset_cook_external_png_keeps_geometry")

    glb_path = project / "models" / "embedded.glb"
    glb_document, glb_binary = _decode_glb(glb_path.read_bytes())
    image_view = glb_document["bufferViews"][4]
    image_start = image_view["byteOffset"]
    embedded_png = _png_rgba(2, 2, (41, 81, 211, 255))
    image_view["byteLength"] = len(embedded_png)
    glb_path.write_bytes(_encode_glb(glb_document, glb_binary[:image_start] + embedded_png))
    embedded_image = _cook_project(binary, project, cwd=cwd, timeout=timeout)
    _cook_expect_only(embedded_image, label="GLB embedded PNG edit", changed_metric="imageCooks")
    _expect(_geometry_keys(embedded_image) == first_geometry_keys, "embedded PNG edit changed geometry identity", embedded_image)
    checks.append("asset_cook_embedded_png_keeps_geometry")

    external_gltf_path = project / "models" / "external.gltf"
    external_document = json.loads(external_gltf_path.read_text(encoding="utf-8"))
    external_document["nodes"][0]["translation"] = [0.25, 0.0, 0.0]
    _write_json(external_gltf_path, external_document)
    node_edit = _cook_project(binary, project, cwd=cwd, timeout=timeout)
    _cook_expect_unchanged_work(node_edit, label="node transform edit")
    _expect(_geometry_keys(node_edit) == first_geometry_keys, "node transform edit changed geometry identity", node_edit)
    checks.append("asset_cook_node_transform_keeps_geometry")

    external_document["materials"][0]["pbrMetallicRoughness"]["baseColorFactor"] = [0.75, 0.8, 0.9, 1.0]
    _write_json(external_gltf_path, external_document)
    material_edit = _cook_project(binary, project, cwd=cwd, timeout=timeout)
    _cook_expect_unchanged_work(material_edit, label="material factor edit")
    _expect(_geometry_keys(material_edit) == first_geometry_keys, "material factor edit changed geometry identity", material_edit)
    checks.append("asset_cook_material_keeps_geometry")

    (project / "models" / "triangle.bin").write_bytes(_triangle_binary(-0.4))
    external_document["accessors"][0]["min"][0] = -0.4
    _write_json(external_gltf_path, external_document)
    geometry_edit = _cook_project(binary, project, cwd=cwd, timeout=timeout)
    _cook_expect_only(geometry_edit, label="geometry payload edit", changed_metric="modelGeometryCooks")
    changed_geometry_keys = _geometry_keys(geometry_edit)
    _expect(
        len(changed_geometry_keys) == 2 and first_geometry_keys.issubset(changed_geometry_keys),
        "geometry edit did not add one distinct identity beside the shared unchanged GLB geometry",
        geometry_edit,
    )
    checks.append("asset_cook_geometry_payload_invalidates")

    atlas_path = project / "atlases" / "sprites.json"
    atlas_document = json.loads(atlas_path.read_text(encoding="utf-8"))
    atlas_document["regions"][0]["rect"] = [4, 4, 8, 8]
    _write_json(atlas_path, atlas_document)
    atlas_edit = _cook_project(binary, project, cwd=cwd, timeout=timeout)
    _cook_expect_only(atlas_edit, label="atlas rectangle edit", changed_metric="atlasCooks")
    checks.append("asset_cook_atlas_metadata_keeps_png")

    with wave.open(os.fspath(project / "audio" / "tone.wav"), "wb") as audio:
        audio.setnchannels(1)
        audio.setsampwidth(2)
        audio.setframerate(8_000)
        audio.writeframes(struct.pack("<4h", 0, 1_001, -1_001, 0))
    audio_edit = _cook_project(binary, project, cwd=cwd, timeout=timeout)
    _cook_expect_only(audio_edit, label="WAV sample edit", changed_metric="audioCooks")
    checks.append("asset_cook_wav_edit_keeps_geometry")

    model_asset = next(
        asset for asset in audio_edit["assets"] if asset.get("assetId") == "asset:model-embedded"
    )
    geometry_key = next(
        key["key"] for key in model_asset["resourceKeys"] if key.get("kind") == "geometry"
    )
    cache_entry = cache_root / "v1" / "cook" / f"{geometry_key}.bin"
    _expect(cache_entry.is_file(), "shared geometry cooked cache entry was not published", cache_entry)
    corrupt_entry = bytearray(cache_entry.read_bytes())
    _expect(
        len(corrupt_entry) > CACHE_ENVELOPE_HEADER_BYTES,
        "shared geometry cache entry had no payload to corrupt",
        cache_entry,
    )
    corrupt_entry[-1] ^= 0x01
    cache_entry.write_bytes(corrupt_entry)
    cache_recovery = _cook_project(binary, project, cwd=cwd, timeout=timeout)
    _expect(
        _cook_metrics(cache_recovery).get("cacheCorrupt", 0) >= 1,
        "cook did not report and rebuild a corrupt cached payload",
        cache_recovery,
    )
    _expect(
        _cook_metrics(cache_recovery).get("modelGeometryCooks", 0) >= 1,
        "cook reported payload corruption without rebuilding shared geometry",
        cache_recovery,
    )
    checks.append("asset_cook_corrupt_payload_cache_rebuilt")

    cache_entry.write_bytes(b"corrupt offline smoke cache envelope")
    header_recovery = _cook_project(binary, project, cwd=cwd, timeout=timeout)
    _expect(
        _cook_metrics(header_recovery).get("cacheCorrupt", 0) >= 1,
        "cook did not report a malformed cache envelope",
        header_recovery,
    )
    _expect(
        _cook_metrics(header_recovery).get("modelGeometryCooks", 0) >= 1,
        "cook reported a malformed envelope without rebuilding shared geometry",
        header_recovery,
    )
    checks.append("asset_cook_malformed_header_cache_rebuilt")

    override_cache = root / "asset-cache-override"
    overridden = _cook_project(
        binary,
        project,
        cwd=cwd,
        timeout=timeout,
        cache_dir=override_cache,
    )
    _expect(
        Path(overridden.get("cacheRoot", "")) == override_cache,
        "project cook ignored the explicit cache root",
        overridden.get("cacheRoot"),
    )
    checks.append("asset_cook_cache_root_override")

    bad_document = json.loads(external_gltf_path.read_text(encoding="utf-8"))
    bad_document["extensionsRequired"] = ["KHR_draco_mesh_compression"]
    _write_json(external_gltf_path, bad_document)
    unsupported = _cook_project(
        binary,
        project,
        cwd=cwd,
        timeout=timeout,
        expect_success=False,
    )
    error = unsupported.get("error", {})
    _expect(error.get("code") == "unsupported", "unsupported glTF capability lacked its typed error code", unsupported)
    _expect(
        error.get("source_id") == "models/external.gltf" and error.get("path"),
        "unsupported glTF diagnostic omitted source/path location",
        unsupported,
    )
    checks.append("asset_cook_unsupported_feature_has_typed_location")
    return {
        "checks": checks,
        "asset_counts": counts.get("assetsByKind"),
        "warm_cache_hits": _cook_metrics(warm).get("cacheHits"),
        "corrupt_cache_rebuilds": _cook_metrics(cache_recovery).get("cacheCorrupt"),
        "unsupported_error_code": error.get("code"),
    }


class _JsonlProjectServer:
    """One bounded project JSONL process with request-ID and response validation."""

    def __init__(
        self,
        binary: Path,
        project: Path,
        *,
        cwd: Path,
        timeout: float,
        command: Sequence[str] | None = None,
        protocol_version: int = PROTOCOL_VERSION,
        process_label: str = "project serve",
        stdin_target: Any = subprocess.PIPE,
        stdout_target: int | None = None,
    ) -> None:
        self.timeout = timeout
        self.protocol_version = protocol_version
        self.process_label = process_label
        self.process_stderr = tempfile.TemporaryFile(mode="w+b")
        process_arguments = command or ("project", "serve", os.fspath(project))
        try:
            self.process = subprocess.Popen(
                [os.fspath(binary), *process_arguments],
                cwd=cwd,
                stdin=stdin_target,
                stdout=subprocess.PIPE if stdout_target is None else stdout_target,
                stderr=self.process_stderr,
                bufsize=0,
            )
        except OSError as error:
            self.process_stderr.close()
            raise SmokeError(f"could not start {self.process_label}: {error}") from error
        self._stdin = self.process.stdin
        if stdin_target == subprocess.PIPE:
            assert self._stdin is not None
        self._stdout = self.process.stdout
        if stdout_target is None:
            assert self._stdout is not None
            self._selector: selectors.BaseSelector | None = selectors.DefaultSelector()
            self._selector.register(self._stdout, selectors.EVENT_READ)
        else:
            self._selector = None
        self._buffer = bytearray()
        self._closed = False

    def _server_diagnostic(self) -> str:
        try:
            self.process_stderr.flush()
            self.process_stderr.seek(0)
            return self.process_stderr.read().decode("utf-8", errors="replace")[-2000:]
        except OSError:
            return ""

    def send_only(self, request_id: str, operation: dict[str, Any]) -> None:
        if self._stdin is None:
            raise SmokeError(f"{self.process_label} has no writable stdin")
        envelope = {
            "protocol": self.protocol_version,
            "request_id": request_id,
            "request": operation,
        }
        encoded = json.dumps(envelope, separators=(",", ":")).encode("utf-8") + b"\n"
        try:
            self._stdin.write(encoded)
            self._stdin.flush()
        except (BrokenPipeError, OSError) as error:
            raise SmokeError(
                f"{self.process_label} closed while sending request {request_id!r}; "
                f"{self._server_diagnostic()}"
            ) from error

    def request(
        self,
        request_id: str,
        operation: dict[str, Any],
        *,
        timeout: float | None = None,
        allow_error: bool = False,
    ) -> dict[str, Any]:
        if self._stdout is None or self._selector is None:
            raise SmokeError(f"{self.process_label} stdout is intentionally not captured")
        self.send_only(request_id, operation)
        wait_timeout = self.timeout if timeout is None else min(timeout, self.timeout)
        deadline = time.monotonic() + wait_timeout
        while True:
            newline = self._buffer.find(b"\n")
            if newline >= 0:
                if newline > MAX_JSONL_RESPONSE_BYTES:
                    raise SmokeError(
                        f"{self.process_label} response exceeded {MAX_JSONL_RESPONSE_BYTES} bytes"
                    )
                raw = bytes(self._buffer[:newline])
                del self._buffer[: newline + 1]
                return self._validate_response(raw, request_id, allow_error=allow_error)
            if len(self._buffer) > MAX_JSONL_RESPONSE_BYTES:
                raise SmokeError(
                    f"{self.process_label} response exceeded {MAX_JSONL_RESPONSE_BYTES} bytes"
                )
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SmokeError(
                    f"timed out after {wait_timeout:g}s waiting for {self.process_label} request "
                    f"{request_id!r}; {self._server_diagnostic()}"
                )
            ready = self._selector.select(remaining)
            if not ready:
                raise SmokeError(
                    f"timed out after {wait_timeout:g}s waiting for {self.process_label} request "
                    f"{request_id!r}; {self._server_diagnostic()}"
                )
            try:
                chunk = os.read(self._stdout.fileno(), 64 * 1024)
            except OSError as error:
                raise SmokeError(f"could not read {self.process_label} response: {error}") from error
            if not chunk:
                raise SmokeError(
                    f"{self.process_label} exited before responding to {request_id!r} "
                    f"(exit {self.process.poll()}); {self._server_diagnostic()}"
                )
            self._buffer.extend(chunk)

    def _validate_response(
        self, raw: bytes, request_id: str, *, allow_error: bool = False
    ) -> dict[str, Any]:
        try:
            response = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise SmokeError(
                f"{self.process_label} returned invalid JSON for {request_id!r}: "
                f"{raw[:2000]!r}; {self._server_diagnostic()}"
            ) from error
        _expect(isinstance(response, dict), "project serve response must be an object", response)
        _expect(
            response.get("protocol") == self.protocol_version,
            f"{self.process_label} response has unsupported protocol for {request_id!r}",
            response,
        )
        _expect(
            response.get("request_id") == request_id,
            f"{self.process_label} response request ID does not match {request_id!r}",
            response,
        )
        _expect(
            type(response.get("ok")) is bool,
            f"{self.process_label} response has no boolean success flag for {request_id!r}",
            response,
        )
        if response["ok"]:
            _expect(
                isinstance(response.get("result"), dict),
                f"{self.process_label} response has no result object for {request_id!r}",
                response,
            )
        else:
            _expect(
                isinstance(response.get("error"), dict),
                f"{self.process_label} error response has no error object for {request_id!r}",
                response,
            )
            _expect(
                allow_error,
                f"{self.process_label} rejected request {request_id!r}",
                response,
            )
        return response

    def wait_for_exit(self, *, timeout: float | None = None) -> int:
        """Wait for the child while leaving stdin open, proving it exits independently of EOF."""

        wait_timeout = self.timeout if timeout is None else min(timeout, self.timeout)
        try:
            return_code = self.process.wait(timeout=wait_timeout)
        except subprocess.TimeoutExpired as error:
            raise SmokeError(
                f"{self.process_label} did not exit within {wait_timeout:g}s; "
                f"{self._server_diagnostic()}"
            ) from error
        if return_code != 0:
            raise SmokeError(
                f"{self.process_label} exited with code {return_code}; {self._server_diagnostic()}"
            )
        return return_code

    def finish(self) -> None:
        if self._closed:
            return
        try:
            self._close_stdin()
            try:
                return_code = self.process.wait(timeout=self.timeout)
            except subprocess.TimeoutExpired as error:
                raise SmokeError(
                    f"{self.process_label} did not exit within {self.timeout:g}s after input closed; "
                    f"{self._server_diagnostic()}"
                ) from error
            if return_code != 0:
                raise SmokeError(
                    f"{self.process_label} exited with code {return_code}; {self._server_diagnostic()}"
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
                raise SmokeError(f"could not reap terminated {self.process_label} process") from error

    def _close_streams(self) -> None:
        if self._closed:
            return
        self._closed = True
        if self._selector is not None:
            try:
                self._selector.unregister(self._stdout)
            except (KeyError, ValueError):
                pass
            self._selector.close()
        self._close_stdin()
        if self._stdout is not None:
            self._stdout.close()
        self.process_stderr.close()

    def _close_stdin(self) -> None:
        if self._stdin is None:
            return
        try:
            self._stdin.close()
        except OSError:
            pass

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


def _v2_result(
    server: _JsonlProjectServer,
    request_id: str,
    request: dict[str, Any],
    *,
    kind: str | None = None,
    timeout: float | None = None,
) -> dict[str, Any]:
    response = server.request(request_id, request, timeout=timeout)
    _expect(response.get("ok") is True, f"project view rejected {request_id!r}", response)
    result = response["result"]
    if kind is not None:
        _expect(result.get("kind") == kind, f"{request_id!r} returned the wrong result kind", response)
    data = result.get("data")
    _expect(isinstance(data, dict), f"{request_id!r} returned no result data object", response)
    return data


def _v2_submit(
    server: _JsonlProjectServer,
    request_id: str,
    operation: dict[str, Any],
) -> tuple[str, bool]:
    deadline = time.monotonic() + server.timeout
    attempt = 0
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError(f"timed out submitting {request_id!r}")
        response = server.request(
            request_id,
            {"op": "submit", "operation": operation},
            timeout=remaining,
            allow_error=True,
        )
        if not response.get("ok"):
            error = response.get("error", {})
            if error.get("code") == "busy":
                attempt += 1
                time.sleep(min(0.01, max(0.0, remaining)))
                continue
            raise SmokeError(f"project view rejected submit {request_id!r}; {json.dumps(response, sort_keys=True)}")
        result = response["result"]
        _expect(result.get("kind") == "submitted", f"{request_id!r} returned the wrong result kind", response)
        data = result.get("data")
        _expect(isinstance(data, dict), "submit returned no result data object", response)
        _expect(attempt <= int(server.timeout * 100) + 1, f"submit {request_id!r} exceeded its bounded retry count")
        break
    ticket = data.get("ticket")
    _expect(isinstance(ticket, str) and ticket, "submit returned no ticket", data)
    _expect(type(data.get("request_id_reused")) is bool, "submit returned no reuse flag", data)
    return ticket, data["request_id_reused"]


def _v2_poll_until(
    server: _JsonlProjectServer,
    ticket: str,
    *,
    request_prefix: str,
    phase: str,
    timeout: float,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    for attempt in range(int(timeout * 100) + 1):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError(f"timed out waiting for ticket {ticket!r} to reach {phase}")
        response = server.request(
            f"{request_prefix}-{attempt}",
            {"op": "poll", "ticket": ticket},
            timeout=remaining,
            allow_error=True,
        )
        if not response.get("ok"):
            error = response.get("error", {})
            if error.get("code") == "busy":
                time.sleep(min(0.01, max(0.0, remaining)))
                continue
            raise SmokeError(f"project view poll failed for {ticket!r}; {json.dumps(response, sort_keys=True)}")
        result = response["result"]
        _expect(result.get("kind") == "poll", f"{request_prefix!r} returned the wrong result kind", response)
        data = result.get("data")
        _expect(isinstance(data, dict), "poll returned no result data object", response)
        current_phase = data.get("phase")
        _expect(
            current_phase in {"queued", "authority_accepted", "terminal"},
            f"poll returned an unknown phase for {ticket!r}",
            data,
        )
        if current_phase == phase:
            return data
        if current_phase == "terminal":
            raise SmokeError(
                f"ticket {ticket!r} became terminal before reaching {phase}; "
                f"poll: {json.dumps(data, sort_keys=True)}"
            )
        time.sleep(min(0.01, max(0.0, remaining)))
    raise SmokeError(f"ticket {ticket!r} exceeded the bounded poll count while waiting for {phase}")


def _v2_terminal_result(poll: dict[str, Any], *, context: str) -> dict[str, Any]:
    terminal = poll.get("terminal")
    _expect(isinstance(terminal, dict), f"{context} poll has no terminal outcome", poll)
    _expect(terminal.get("ok") is True, f"{context} operation failed", poll)
    result = terminal.get("result")
    _expect(isinstance(result, dict), f"{context} terminal result is missing", poll)
    _expect(isinstance(result.get("kind"), str), f"{context} result has no kind", result)
    return result


def _v2_ack(server: _JsonlProjectServer, request_id: str, ticket: str) -> None:
    deadline = time.monotonic() + server.timeout
    for attempt in range(int(server.timeout * 100) + 1):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError(f"timed out acknowledging ticket {ticket!r}")
        response = server.request(
            f"{request_id}-{attempt}",
            {"op": "ack", "ticket": ticket},
            timeout=remaining,
            allow_error=True,
        )
        if not response.get("ok"):
            if response.get("error", {}).get("code") == "busy":
                time.sleep(min(0.01, max(0.0, remaining)))
                continue
            raise SmokeError(f"ack request failed for {ticket!r}; {json.dumps(response, sort_keys=True)}")
        _expect(response.get("result", {}).get("kind") == "acked", "ack returned another result kind", response)
        return
    raise SmokeError(f"ticket {ticket!r} exceeded its bounded acknowledgement retry count")


def _v2_latest_frame(server: _JsonlProjectServer, *, timeout: float) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    for attempt in range(int(timeout * 100) + 1):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError("timed out waiting for latest_frame")
        response = server.request(
            f"view-latest-frame-{attempt}",
            {"op": "latest_frame"},
            timeout=remaining,
            allow_error=True,
        )
        if not response.get("ok"):
            code = response.get("error", {}).get("code")
            if code in {"busy", "frame_unavailable"}:
                time.sleep(min(0.01, max(0.0, remaining)))
                continue
            raise SmokeError(f"latest_frame request failed; {json.dumps(response, sort_keys=True)}")
        result = response["result"]
        _expect(result.get("kind") == "latest_frame", "latest_frame returned another result kind", response)
        data = result.get("data")
        _expect(isinstance(data, dict), "latest_frame returned no frame identity", response)
        return data
    raise SmokeError("latest_frame exceeded its bounded retry count")


def _read_native_report(path: Path) -> dict[str, Any]:
    try:
        size = path.stat().st_size
        _expect(size <= MAX_JSONL_RESPONSE_BYTES, "native project view report exceeded 1 MiB", size)
        report = json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        raise SmokeError(f"native project view did not write its report at {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise SmokeError(f"native project view report is invalid JSON at {path}: {error}") from error
    _expect(isinstance(report, dict), "native project view report must be an object", report)
    return report


def _packet_identity(packet: dict[str, Any], *, context: str) -> dict[str, str]:
    keys = ("session", "tick", "sequence", "project_generation", "source_digest")
    identity: dict[str, str] = {}
    for key in keys:
        value = packet.get(key)
        _expect(isinstance(value, str) and value, f"{context} has invalid {key}", packet)
        identity[key] = value
    return identity


def _validate_native_view_report(
    report: dict[str, Any],
    *,
    expected_session: str,
    expected_source_digest: str,
    expected_tick: str,
    expected_exit_reason: str,
    latest_frame: dict[str, Any] | None = None,
) -> dict[str, Any]:
    _expect(report.get("command") == "project_view", "native report is from another command", report)
    _expect(report.get("session") == expected_session, "native report belongs to another session", report)
    _expect(
        report.get("source_digest") == expected_source_digest,
        "native report source digest differs from the authored project",
        report,
    )
    _expect(
        report.get("authority_final_tick") == expected_tick,
        "native report has an unexpected final authority tick",
        report,
    )
    window = report.get("window")
    _expect(isinstance(window, dict), "native report has no window summary", report)
    _expect(
        window.get("exit_reason") == expected_exit_reason,
        f"native window exited for {window.get('exit_reason')!r}, expected {expected_exit_reason}",
        report,
    )
    coordinator = report.get("coordinator")
    _expect(isinstance(coordinator, dict), "native report has no coordinator counters", report)
    _expect(
        type(coordinator.get("protocol_requests")) is int and coordinator["protocol_requests"] >= 1,
        "native report did not record protocol requests",
        coordinator,
    )
    if expected_exit_reason == "TimeLimit":
        _expect(
            type(coordinator.get("duplicate_submissions")) is int
            and coordinator["duplicate_submissions"] >= 1
            and type(coordinator.get("protocol_errors")) is int
            and coordinator["protocol_errors"] >= 1,
            "native report did not record the duplicate and retained-ID collision checks",
            coordinator,
        )

    accepted = report.get("accepted_packet")
    presented = report.get("last_presented")
    _expect(isinstance(accepted, dict), "native report has no accepted packet identity", report)
    _expect(isinstance(presented, dict), "native viewer never reported an actually presented packet", report)
    accepted_identity = _packet_identity(accepted, context="accepted packet")
    presented_identity = _packet_identity(presented, context="last presented packet")
    _expect(
        accepted_identity == presented_identity,
        "last presented packet does not match the final accepted packet",
        {"accepted_packet": accepted_identity, "last_presented": presented_identity},
    )
    _expect(
        presented_identity["session"] == expected_session
        and presented_identity["source_digest"] == expected_source_digest
        and presented_identity["tick"] == expected_tick,
        "presented packet does not match the final authority/session state",
        presented_identity,
    )
    _expect(
        type(presented.get("physical_width")) is int
        and presented["physical_width"] > 0
        and type(presented.get("physical_height")) is int
        and presented["physical_height"] > 0
        and isinstance(presented.get("camera"), dict),
        "presented packet has invalid surface dimensions or camera metadata",
        presented,
    )

    if expected_exit_reason == "TimeLimit":
        _expect(isinstance(latest_frame, dict), "latest-frame identity was not supplied", report)
        _expect(
            _packet_identity(latest_frame, context="latest frame") == accepted_identity,
            "latest runtime frame differs from the packet presented by the window",
            {"latest_frame": latest_frame, "accepted_packet": accepted_identity},
        )
        resources = report.get("resources")
        _expect(isinstance(resources, dict), "native report has no resource counters", report)
        for counter in (
            "active_bytes",
            "active_entries",
            "geometry_upload_count",
            "geometry_upload_bytes",
            "instance_write_count",
            "material_write_count",
            "cache_hit_count",
            "cache_miss_count",
            "cache_eviction_count",
        ):
            value = resources.get(counter)
            _expect(type(value) is int and value >= 0, f"invalid native resource counter {counter}", resources)
        _expect(
            resources["geometry_upload_count"] == 1 and resources["geometry_upload_bytes"] > 0,
            "the single authored mesh was not uploaded exactly once",
            resources,
        )
        _expect(
            type(window.get("rendered_frames")) is int and window["rendered_frames"] >= 1,
            "native window did not report a rendered frame",
            window,
        )
    return report


def _project_view_command(project: Path, report: Path, *, seconds: int) -> list[str]:
    return [
        "project",
        "view",
        os.fspath(project),
        "--stdio-control",
        "--clock",
        "manual",
        "--seconds",
        str(seconds),
        "--report",
        os.fspath(report),
        "--width",
        str(NATIVE_VIEW_WIDTH),
        "--height",
        str(NATIVE_VIEW_HEIGHT),
    ]


def _run_native_project_view(
    binary: Path,
    project: Path,
    *,
    cwd: Path,
    root: Path,
    timeout: float,
    expected_source_digest: str,
) -> dict[str, Any]:
    report_path = root / "native-view-report.json"
    command = _project_view_command(project, report_path, seconds=NATIVE_VIEW_SECONDS)
    server = _JsonlProjectServer(
        binary,
        project,
        cwd=cwd,
        timeout=timeout,
        command=command,
        protocol_version=VIEW_PROTOCOL_VERSION,
        process_label="project view",
    )
    with server:
        info_ticket, reused = _v2_submit(
            server,
            "view-info",
            {"op": "project_info", "offset": 0, "limit": 10},
        )
        _expect(not reused, "initial project_info submit was unexpectedly reused")
        info_poll = _v2_poll_until(
            server,
            info_ticket,
            request_prefix="view-info-poll",
            phase="terminal",
            timeout=timeout,
        )
        info_result = _v2_terminal_result(info_poll, context="project_info")
        _expect(info_result.get("kind") == "project_info", "project_info returned another result kind", info_result)
        info = info_result.get("data")
        _expect(isinstance(info, dict), "project_info terminal result has no data", info_result)
        session = info.get("session")
        _expect(isinstance(session, str) and session, "project_info returned no decimal session ID", info)
        _expect(info.get("tick") == "0", "native view did not start at authority tick zero", info)
        _expect(
            info.get("source_digest") == expected_source_digest,
            "native project view loaded a different source digest",
            info,
        )
        _v2_ack(server, "view-info-ack", info_ticket)

        get_operation = {"op": "get_transform", "target": {"stable_id": ENTITY_ID}}
        get_ticket, reused = _v2_submit(server, "view-get-zero", get_operation)
        _expect(not reused, "initial transform submit was unexpectedly reused")
        duplicate_ticket, was_reused = _v2_submit(server, "view-get-zero", get_operation)
        _expect(
            was_reused and duplicate_ticket == get_ticket,
            "retained duplicate submit did not reuse its original ticket",
            {"first": get_ticket, "duplicate": duplicate_ticket, "reused": was_reused},
        )
        collision = server.request(
            "view-get-zero",
            {"op": "submit", "operation": {"op": "status"}},
            allow_error=True,
        )
        _expect(
            collision.get("ok") is False
            and collision.get("error", {}).get("code") == "request_id_collision",
            "retained request ID collision was not rejected",
            collision,
        )
        get_poll = _v2_poll_until(
            server,
            get_ticket,
            request_prefix="view-get-zero-poll",
            phase="terminal",
            timeout=timeout,
        )
        get_result = _v2_terminal_result(get_poll, context="tick-zero get_transform")
        _expect(get_result.get("kind") == "transform", "get_transform returned another result kind", get_result)
        initial_state = get_result.get("data")
        _expect(isinstance(initial_state, dict), "get_transform returned no state", get_result)
        _expect(initial_state.get("observed_tick") == "0", "get_transform did not observe tick zero", initial_state)
        initial_entity = initial_state.get("entity")
        _expect(isinstance(initial_entity, dict), "get_transform returned no entity", initial_state)
        _expect(initial_entity.get("stable_id") == ENTITY_ID, "get_transform returned another stable entity", initial_entity)
        _expect(initial_entity.get("revision") == "0", "authored transform did not begin at revision zero", initial_entity)
        _expect(initial_entity.get("handle", {}).get("session") == session, "get_transform handle belongs to another session", initial_entity)
        _expect(
            initial_entity.get("transform")
            == {
                "translation": [0.0, 0.0, 0.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0],
            },
            "tick-zero get_transform did not contain the authored transform",
            initial_entity,
        )
        _v2_ack(server, "view-get-zero-ack", get_ticket)

        set_ticket, reused = _v2_submit(
            server,
            "view-set-transform",
            {
                "op": "set_transform",
                "target": {"stable_id": ENTITY_ID},
                "expected_revision": "0",
                "transform": TRANSFORM_AFTER_COMMAND,
            },
        )
        _expect(not reused, "set_transform submit was unexpectedly reused")
        accepted_poll = _v2_poll_until(
            server,
            set_ticket,
            request_prefix="view-set-poll",
            phase="authority_accepted",
            timeout=timeout,
        )
        authority = accepted_poll.get("authority")
        _expect(
            isinstance(authority, dict)
            and authority.get("submission") == "enqueued"
            and authority.get("apply_tick") == "1",
            "set_transform was not accepted for authority tick one",
            accepted_poll,
        )

        step_ticket, reused = _v2_submit(server, "view-step-one", {"op": "step", "count": 1})
        _expect(not reused, "step submit was unexpectedly reused")
        step_poll = _v2_poll_until(
            server,
            step_ticket,
            request_prefix="view-step-one-poll",
            phase="terminal",
            timeout=timeout,
        )
        step_result = _v2_terminal_result(step_poll, context="manual Step")
        _expect(step_result.get("kind") == "step", "manual Step returned another result kind", step_result)
        steps = step_result.get("data")
        _expect(isinstance(steps, list) and len(steps) == 1, "manual Step did not return one tick", step_result)
        _expect(steps[0].get("tick") == "1", "manual Step did not reach authority tick one", steps)
        step_commands = steps[0].get("commands")
        _expect(isinstance(step_commands, list) and len(step_commands) == 1, "manual Step did not resolve one command", steps[0])
        _expect(
            step_commands[0].get("outcome") == {"kind": "applied", "data": {"revision": "1"}},
            "manual Step did not apply the transform at revision one",
            step_commands[0],
        )
        _v2_ack(server, "view-step-one-ack", step_ticket)

        command_poll = _v2_poll_until(
            server,
            set_ticket,
            request_prefix="view-set-terminal-poll",
            phase="terminal",
            timeout=timeout,
        )
        command_result = _v2_terminal_result(command_poll, context="set_transform")
        _expect(command_result.get("kind") == "command", "set_transform returned another terminal kind", command_result)
        command_data = command_result.get("data")
        _expect(
            isinstance(command_data, dict)
            and command_data.get("outcome") == {"kind": "applied", "data": {"revision": "1"}}
            and command_data.get("resolve_tick") == "1",
            "set_transform terminal outcome did not confirm Applied at tick one",
            command_result,
        )
        _v2_ack(server, "view-set-transform-ack", set_ticket)

        final_get_ticket, reused = _v2_submit(
            server,
            "view-get-final",
            {"op": "get_transform", "target": {"stable_id": ENTITY_ID}},
        )
        _expect(not reused, "final transform submit was unexpectedly reused")
        final_get_poll = _v2_poll_until(
            server,
            final_get_ticket,
            request_prefix="view-get-final-poll",
            phase="terminal",
            timeout=timeout,
        )
        final_get_result = _v2_terminal_result(final_get_poll, context="final get_transform")
        final_state = final_get_result.get("data")
        _expect(
            final_get_result.get("kind") == "transform"
            and isinstance(final_state, dict)
            and final_state.get("observed_tick") == "1"
            and final_state.get("entity", {}).get("revision") == "1"
            and final_state.get("entity", {}).get("stable_id") == ENTITY_ID
            and final_state.get("entity", {}).get("handle", {}).get("session") == session
            and final_state.get("entity", {}).get("transform") == TRANSFORM_AFTER_COMMAND,
            "final get_transform does not contain the authoritative tick-one state",
            final_get_result,
        )
        _v2_ack(server, "view-get-final-ack", final_get_ticket)

        latest_frame = _v2_latest_frame(server, timeout=timeout)
        _expect(
            latest_frame.get("session") == session
            and latest_frame.get("tick") == "1"
            and latest_frame.get("source_digest") == expected_source_digest,
            "latest_frame does not reflect the authority/session state",
            latest_frame,
        )
        # LatestFrame is runtime metadata only. The viewer report must separately
        # identify the packet that actually reached the native surface.
        server.wait_for_exit(timeout=timeout)

    report = _read_native_report(report_path)
    return _validate_native_view_report(
        report,
        expected_session=session,
        expected_source_digest=expected_source_digest,
        expected_tick="1",
        expected_exit_reason="TimeLimit",
        latest_frame=latest_frame,
    )


def _run_native_project_close(
    binary: Path,
    project: Path,
    *,
    cwd: Path,
    root: Path,
    timeout: float,
    expected_source_digest: str,
) -> dict[str, Any]:
    report_path = root / "native-close-report.json"
    command = _project_view_command(project, report_path, seconds=NATIVE_VIEW_SECONDS)
    server = _JsonlProjectServer(
        binary,
        project,
        cwd=cwd,
        timeout=timeout,
        command=command,
        protocol_version=VIEW_PROTOCOL_VERSION,
        process_label="project view close case",
    )
    with server:
        server.send_only("native-close", {"op": "close"})
        # Do not wait for or consume the CloseRequested JSONL receipt. A successful
        # exit is established by the report written after the window handles Close.
        server.wait_for_exit(timeout=timeout)

    report = _read_native_report(report_path)
    _expect(report.get("command") == "project_view", "native close report is from another command", report)
    _expect(
        isinstance(report.get("session"), str) and report["session"],
        "native close report has no session identity",
        report,
    )
    _expect(
        report.get("source_digest") == expected_source_digest,
        "native close report source digest differs from the authored project",
        report,
    )
    window = report.get("window")
    _expect(isinstance(window, dict), "native close report has no window summary", report)
    _expect(window.get("exit_reason") == "CloseRequested", "explicit Close did not close the project window", report)
    coordinator = report.get("coordinator")
    _expect(
        isinstance(coordinator, dict)
        and type(coordinator.get("protocol_requests")) is int
        and coordinator["protocol_requests"] >= 1
        and coordinator.get("protocol_detach_reason") is None,
        "Close case detached before the window handled its request",
        coordinator,
    )
    return report


def _validate_native_liveness_report(
    report: dict[str, Any],
    *,
    expected_source_digest: str,
    expected_detach_reason: str | None,
    expected_protocol_requests: int,
) -> dict[str, Any]:
    _expect(report.get("command") == "project_view", "native liveness report is from another command", report)
    session = report.get("session")
    _expect(isinstance(session, str) and session, "native liveness report has no session", report)
    _expect(report.get("source_digest") == expected_source_digest, "native liveness source digest changed", report)
    _expect(report.get("authority_final_tick") == "0", "native liveness case advanced authority", report)
    _expect(report.get("termination") == "StopRequested", "native liveness session did not shut down normally", report)

    window = report.get("window")
    _expect(isinstance(window, dict), "native liveness report has no window summary", report)
    _expect(window.get("exit_reason") == "TimeLimit", "native liveness viewer did not reach its time limit", report)
    _expect(
        type(window.get("rendered_frames")) is int and window["rendered_frames"] > 0,
        "native liveness viewer presented no surface frames",
        window,
    )

    accepted = report.get("accepted_packet")
    presented = report.get("last_presented")
    _expect(isinstance(accepted, dict) and isinstance(presented, dict), "native liveness report has no packet presentation", report)
    accepted_identity = _packet_identity(accepted, context="liveness accepted packet")
    presented_identity = _packet_identity(presented, context="liveness presented packet")
    _expect(accepted_identity == presented_identity, "liveness packet was accepted but not presented", report)
    _expect(
        presented_identity["session"] == session
        and presented_identity["tick"] == "0"
        and presented_identity["source_digest"] == expected_source_digest,
        "liveness presentation does not match the initial authority state",
        presented_identity,
    )
    _expect(
        type(presented.get("physical_width")) is int
        and presented["physical_width"] > 0
        and type(presented.get("physical_height")) is int
        and presented["physical_height"] > 0,
        "liveness presentation has invalid surface dimensions",
        presented,
    )

    coordinator = report.get("coordinator")
    _expect(isinstance(coordinator, dict), "native liveness report has no coordinator summary", report)
    _expect(
        coordinator.get("protocol_detach_reason") == expected_detach_reason,
        "native liveness control detach reason did not match the transport case",
        coordinator,
    )
    _expect(
        coordinator.get("protocol_requests") == expected_protocol_requests
        and coordinator.get("protocol_errors") == 0,
        "native liveness protocol counters did not match the transport case",
        coordinator,
    )

    resources = report.get("resources")
    _expect(isinstance(resources, dict), "native liveness report has no resource counters", report)
    _expect(
        resources.get("geometry_upload_count") == 1
        and type(resources.get("geometry_upload_bytes")) is int
        and resources["geometry_upload_bytes"] > 0,
        "native liveness viewer did not retain the one authored mesh resource",
        resources,
    )
    return report


def _fill_stdout_pipe(write_fd: int) -> int:
    """Fill a POSIX pipe without blocking, restoring its original write mode afterward."""

    if os.name != "posix":
        raise SmokeError("the native stdout-backpressure case requires POSIX pipes")
    try:
        import fcntl
    except ImportError as error:
        raise SmokeError("the native stdout-backpressure case requires fcntl") from error

    try:
        original_flags = fcntl.fcntl(write_fd, fcntl.F_GETFL)
        fcntl.fcntl(write_fd, fcntl.F_SETFL, original_flags | os.O_NONBLOCK)
    except OSError as error:
        raise SmokeError(f"could not configure native stdout pipe: {error}") from error

    written = 0
    pipe_is_full = False
    chunk = b"d" * (64 * 1024)
    try:
        while written < MAX_STDOUT_PIPE_FILL_BYTES:
            try:
                count = os.write(
                    write_fd,
                    chunk[: min(len(chunk), MAX_STDOUT_PIPE_FILL_BYTES - written)],
                )
            except InterruptedError:
                continue
            except BlockingIOError:
                pipe_is_full = True
                break
            if count <= 0:
                raise SmokeError("native stdout pipe accepted no bytes before its capacity check")
            written += count
    except OSError as error:
        raise SmokeError(f"could not fill native stdout pipe: {error}") from error
    finally:
        try:
            fcntl.fcntl(write_fd, fcntl.F_SETFL, original_flags)
        except OSError as error:
            raise SmokeError(f"could not restore native stdout pipe mode: {error}") from error

    if not pipe_is_full or written == 0:
        raise SmokeError(
            f"could not fill native stdout pipe within the {MAX_STDOUT_PIPE_FILL_BYTES}-byte bound"
        )
    return written


def _close_fd(fd: int | None) -> None:
    if fd is None:
        return
    try:
        os.close(fd)
    except OSError:
        pass


def _run_native_project_eof(
    binary: Path,
    project: Path,
    *,
    cwd: Path,
    root: Path,
    timeout: float,
    expected_source_digest: str,
) -> dict[str, Any]:
    report_path = root / "native-eof-report.json"
    command = _project_view_command(project, report_path, seconds=NATIVE_OBSERVER_SECONDS)
    server = _JsonlProjectServer(
        binary,
        project,
        cwd=cwd,
        timeout=timeout,
        command=command,
        protocol_version=VIEW_PROTOCOL_VERSION,
        process_label="project view stdin EOF case",
        stdin_target=subprocess.DEVNULL,
    )
    with server:
        server.wait_for_exit(timeout=timeout)

    report = _read_native_report(report_path)
    return _validate_native_liveness_report(
        report,
        expected_source_digest=expected_source_digest,
        expected_detach_reason="stdin_eof",
        expected_protocol_requests=0,
    )


def _run_native_project_blocked_stdout(
    binary: Path,
    project: Path,
    *,
    cwd: Path,
    root: Path,
    timeout: float,
    expected_source_digest: str,
) -> dict[str, Any]:
    if os.name != "posix":
        raise SmokeError("the native stdout-backpressure case requires POSIX pipes")

    report_path = root / "native-blocked-stdout-report.json"
    command = _project_view_command(project, report_path, seconds=NATIVE_OBSERVER_SECONDS)
    read_fd, write_fd = os.pipe()
    server: _JsonlProjectServer | None = None
    fill_bytes = 0
    try:
        fill_bytes = _fill_stdout_pipe(write_fd)
        server = _JsonlProjectServer(
            binary,
            project,
            cwd=cwd,
            timeout=timeout,
            command=command,
            protocol_version=VIEW_PROTOCOL_VERSION,
            process_label="project view blocked stdout case",
            stdout_target=write_fd,
        )
        os.close(write_fd)
        write_fd = None
        try:
            with server:
                server.send_only("blocked-stdout-capabilities", {"op": "capabilities"})
                # Keep the pipe read end open and unread while the finite viewer exits.
                server.wait_for_exit(timeout=timeout)
        finally:
            if not server._closed:
                try:
                    server._terminate_if_running()
                finally:
                    server._close_streams()
    finally:
        _close_fd(write_fd)
        _close_fd(read_fd)

    report = _read_native_report(report_path)
    validated = _validate_native_liveness_report(
        report,
        expected_source_digest=expected_source_digest,
        expected_detach_reason=None,
        expected_protocol_requests=1,
    )
    return {"stdout_pipe_bytes_filled": fill_bytes, "report": validated}


def run_project_smoke(
    cli_binary: str | Path,
    *,
    cwd: str | Path | None = None,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
    view: bool = False,
    assets: bool = False,
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

        asset_results: dict[str, Any] | None = None
        if assets:
            asset_results = _run_asset_cook_smoke(
                binary,
                root=root,
                cwd=working_directory,
                timeout=timeout,
            )
            checks.extend(asset_results.pop("checks"))

        view_results: dict[str, Any] | None = None
        if view:
            timed_view = _run_native_project_view(
                binary,
                project,
                cwd=working_directory,
                root=root,
                timeout=timeout,
                expected_source_digest=validated["source_digest"],
            )
            checks.append("native_view_presented_authority_state")
            close_view = _run_native_project_close(
                binary,
                project,
                cwd=working_directory,
                root=root,
                timeout=timeout,
                expected_source_digest=validated["source_digest"],
            )
            checks.append("native_view_explicit_close_exit")
            eof_view = _run_native_project_eof(
                binary,
                project,
                cwd=working_directory,
                root=root,
                timeout=timeout,
                expected_source_digest=validated["source_digest"],
            )
            checks.append("native_view_eof_detaches_without_stopping_window")
            blocked_stdout_view = _run_native_project_blocked_stdout(
                binary,
                project,
                cwd=working_directory,
                root=root,
                timeout=timeout,
                expected_source_digest=validated["source_digest"],
            )
            checks.append("native_view_blocked_stdout_does_not_block_shutdown")
            view_results = {
                "time_limit_report": timed_view,
                "close_report": close_view,
                "eof_report": eof_view,
                "blocked_stdout": blocked_stdout_view,
            }

    result = {
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
    if view_results is not None:
        result["native_view"] = view_results
    if asset_results is not None:
        result["asset_cook"] = asset_results
    return result


def _parse_args(argv: Sequence[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run bounded CPU project lifecycle checks against an existing Daedalus CLI binary; --view adds native GPU/window checks."
    )
    parser.add_argument("--cli", type=Path, required=True, help="already-built daedalus executable")
    parser.add_argument(
        "--timeout",
        type=_validate_timeout,
        default=DEFAULT_TIMEOUT_SECONDS,
        help=f"timeout per process/JSONL response in seconds (default: {DEFAULT_TIMEOUT_SECONDS:g})",
    )
    parser.add_argument(
        "--view",
        action="store_true",
        help="also open the authored project in an already-built native-window CLI and verify presentation/Close reports",
    )
    parser.add_argument(
        "--assets",
        action="store_true",
        help="also run offline multi-format CPU asset-cook and selective-cache-reuse checks",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    arguments = _parse_args(argv)
    try:
        result = run_project_smoke(
            arguments.cli,
            timeout=arguments.timeout,
            view=arguments.view,
            assets=arguments.assets,
        )
        print(json.dumps(result, sort_keys=True))
        return 0
    except SmokeError as error:
        print(f"smoke_project: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
