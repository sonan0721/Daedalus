#!/usr/bin/env python3
"""Run bounded external-project smoke checks against an existing Daedalus CLI binary."""

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
from typing import Any, Callable, Sequence


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
SPRITE_ENTITY_ID = "entity:smoke-sprite"
PROJECT_NAME = "smoke-mesh-project"
TRANSFORM_AFTER_COMMAND = {
    "translation": [7.0, 2.0, -3.0],
    "rotation": [0.0, 0.0, 0.0, 1.0],
    "scale": [1.5, 2.0, 0.75],
}
SPRITE_CAMERA_TRANSFORM_AFTER_COMMAND = {
    "translation": [0.25, 0.5, 0.0],
    "rotation": [0.0, 0.0, 0.0, 1.0],
    "scale": [1.0, 1.0, 1.0],
}
ASSET_PREPARATION_MEMORY_FIELDS = (
    "schemaVersion",
    "cookedAccountedBytes",
    "decodedBaseImageBytes",
    "parserScratchPeakBytes",
    "uniqueSourceCount",
    "capturedSourceBytes",
)
ASSET_PREPARATION_METRIC_FIELDS = (
    "sourceReads",
    "sourceBytes",
    "containerParses",
    "rangeHashBytes",
    "pngBaseDecodes",
    "modelGeometryCooks",
    "imageCooks",
    "atlasCooks",
    "audioCooks",
    "cacheHits",
    "cacheMisses",
    "cacheCorrupt",
    "cacheWrites",
    "cacheBytesCurrent",
    "cacheEntriesCurrent",
    "catalogMetadataBytes",
)
ASSET_PREPARATION_SOURCE_PATHS = (
    "daedalus.project.json",
    "scene.json",
    "models/external.gltf",
    "models/triangle.bin",
    "models/embedded.glb",
    "images/base.png",
    "images/atlas.png",
    "atlases/sprites.json",
    "audio/tone.wav",
)


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


def _write_pcm16_mono(path: Path, sample_rate: int, samples: Sequence[int]) -> None:
    """Write one checked mono PCM16 fixture without duplicating WAV setup."""

    if sample_rate <= 0 or not samples or any(
        type(sample) is not int or not -32_768 <= sample <= 32_767 for sample in samples
    ):
        raise SmokeError("offline smoke received invalid mono PCM16 fixture data")
    path.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(os.fspath(path), "wb") as audio:
        audio.setnchannels(1)
        audio.setsampwidth(2)
        audio.setframerate(sample_rate)
        audio.writeframes(struct.pack(f"<{len(samples)}h", *samples))


def _write_audio_project(directory: Path) -> None:
    """Create an external project whose only imported resource is a finite WAV clip."""

    (directory / "audio").mkdir(parents=True)
    sample_rate = 8_000
    frame_count = sample_rate // 4
    samples = [
        int(12_000 * math.sin(2.0 * math.pi * 440.0 * frame / sample_rate))
        for frame in range(frame_count)
    ]
    _write_pcm16_mono(directory / "audio" / "tone.wav", sample_rate, samples)

    manifest = {"schema_version": 1, "name": "offline-audio-smoke", "scene": "scene.json"}
    scene = {
        "schema_version": 1,
        "revision": 0,
        "entities": [],
        "assets": [
            {
                "id": "asset:smoke-tone",
                "kind": "daedalus.audio",
                "uri": "audio/tone.wav",
                "metadata": {"schema_version": 1},
            },
            {
                "id": "asset:smoke-tone-alias",
                "kind": "daedalus.audio",
                "uri": "audio/tone.wav",
                "metadata": {"schema_version": 1},
            },
        ],
    }
    (directory / "daedalus.project.json").write_text(
        json.dumps(manifest, separators=(",", ":")) + "\n", encoding="utf-8"
    )
    (directory / "scene.json").write_text(
        json.dumps(scene, separators=(",", ":")) + "\n", encoding="utf-8"
    )


def _png_rgba(width: int, height: int, color: tuple[int, int, int, int]) -> bytes:
    return _png_rgba_pixels(width, height, bytes(color) * width * height)


def _png_rgba_pixels(width: int, height: int, pixels: bytes) -> bytes:
    if width <= 0 or height <= 0 or len(pixels) != width * height * 4:
        raise SmokeError("offline smoke received invalid RGBA fixture dimensions")
    rows = b"".join(
        b"\x00" + pixels[row * width * 4 : (row + 1) * width * 4]
        for row in range(height)
    )

    def chunk(kind: bytes, payload: bytes) -> bytes:
        body = kind + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rows, level=9))
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
    _write_pcm16_mono(directory / "audio" / "tone.wav", 8_000, (0, 1_000, -1_000, 0))

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


def _write_model_view_project(directory: Path) -> None:
    """Build the offline textured glTF fixture with the shared authority entity shape."""

    _write_asset_project(directory)
    manifest_path = directory / "daedalus.project.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    manifest["name"] = "offline-model-view-smoke"
    manifest_path.write_text(
        json.dumps(manifest, separators=(",", ":")) + "\n", encoding="utf-8"
    )

    scene_path = directory / "scene.json"
    scene = json.loads(scene_path.read_text(encoding="utf-8"))
    scene["assets"] = [
        asset for asset in scene["assets"] if asset["id"] == "asset:model-external"
    ]
    scene["entities"] = [
        {
            "id": ENTITY_ID,
            "name": "Smoke Textured Model",
            "components": [
                {
                    "id": "component:smoke-transform",
                    "type_id": "daedalus.transform",
                    "data": {
                        "translation": [0.0, 0.0, 0.0],
                        "rotation": [0.0, 0.0, 0.0, 1.0],
                        "scale": [1.0, 1.0, 1.0],
                    },
                },
                {
                    "id": "component:smoke-model-renderer",
                    "type_id": "daedalus.model_renderer",
                    "asset_refs": ["asset:model-external"],
                    "data": {"model": "asset:model-external"},
                },
            ],
        }
    ]
    scene_path.write_text(json.dumps(scene, separators=(",", ":")) + "\n", encoding="utf-8")


def _write_asset_preparation_view_project(directory: Path) -> None:
    """Build the complete four-format fixture with one live model-rendered entity."""

    _write_asset_project(directory)
    manifest_path = directory / "daedalus.project.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    manifest["name"] = "offline-asset-preparation-view-smoke"
    manifest_path.write_text(
        json.dumps(manifest, separators=(",", ":")) + "\n", encoding="utf-8"
    )

    scene_path = directory / "scene.json"
    scene = json.loads(scene_path.read_text(encoding="utf-8"))
    scene["entities"] = [
        {
            "id": ENTITY_ID,
            "name": "Asset Preparation View Model",
            "components": [
                {
                    "id": "component:preparation-view-transform",
                    "type_id": "daedalus.transform",
                    "data": {
                        "translation": [0.0, 0.0, 0.0],
                        "rotation": [0.0, 0.0, 0.0, 1.0],
                        "scale": [1.0, 1.0, 1.0],
                    },
                },
                {
                    "id": "component:preparation-view-model-renderer",
                    "type_id": "daedalus.model_renderer",
                    "asset_refs": ["asset:model-external"],
                    "data": {"model": "asset:model-external"},
                },
            ],
        }
    ]
    scene_path.write_text(json.dumps(scene, separators=(",", ":")) + "\n", encoding="utf-8")


def _write_managed_activation_view_project(directory: Path) -> None:
    """Build one model-and-sprite scene with an authored orthographic camera."""

    _write_asset_project(directory)
    manifest_path = directory / "daedalus.project.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    manifest["name"] = "offline-managed-activation-view-smoke"
    manifest_path.write_text(
        json.dumps(manifest, separators=(",", ":")) + "\n", encoding="utf-8"
    )

    atlas_colors = (
        (230, 30, 20, 255),
        (20, 210, 40, 255),
        (25, 50, 235, 160),
        (250, 220, 30, 0),
    )
    atlas_pixels = bytearray()
    for y in range(16):
        for x in range(16):
            quadrant = (0 if y < 8 else 2) + (0 if x < 8 else 1)
            atlas_pixels.extend(atlas_colors[quadrant])
    (directory / "images" / "atlas.png").write_bytes(
        _png_rgba_pixels(16, 16, bytes(atlas_pixels))
    )

    scene_path = directory / "scene.json"
    scene = json.loads(scene_path.read_text(encoding="utf-8"))
    scene["entities"] = [
        {
            "id": "entity:managed-activation-camera",
            "name": "Managed Activation Orthographic Camera",
            "components": [
                {
                    "id": "component:managed-camera-transform",
                    "type_id": "daedalus.transform",
                    "data": {
                        "translation": [0.0, 0.0, 0.0],
                        "rotation": [0.0, 0.0, 0.0, 1.0],
                        "scale": [1.0, 1.0, 1.0],
                    },
                },
                {
                    "id": "component:managed-camera-2d",
                    "type_id": "daedalus.camera_2d",
                    "asset_refs": [],
                    "data": {"vertical_size": 10.0, "near": 0.1, "far": 100.0},
                },
            ],
        },
        {
            "id": ENTITY_ID,
            "name": "Managed Activation Textured Model",
            "components": [
                {
                    "id": "component:managed-model-transform",
                    "type_id": "daedalus.transform",
                    "data": {
                        "translation": [-1.5, 0.0, -3.0],
                        "rotation": [0.0, 0.0, 0.0, 1.0],
                        "scale": [1.0, 1.0, 1.0],
                    },
                },
                {
                    "id": "component:managed-model-renderer",
                    "type_id": "daedalus.model_renderer",
                    "asset_refs": ["asset:model-external"],
                    "data": {"model": "asset:model-external"},
                },
            ],
        },
        {
            "id": SPRITE_ENTITY_ID,
            "name": "Managed Activation Atlas Sprite",
            "components": [
                {
                    "id": "component:managed-sprite-transform",
                    "type_id": "daedalus.transform",
                    "data": {
                        "translation": [1.5, 0.0, -5.0],
                        "rotation": [0.0, 0.0, 0.0, 1.0],
                        "scale": [1.0, 1.0, 1.0],
                    },
                },
                {
                    "id": "component:managed-sprite-renderer",
                    "type_id": "daedalus.sprite_renderer",
                    "asset_refs": ["asset:sprite-atlas"],
                    "data": {
                        "atlas": "asset:sprite-atlas",
                        "region": "sprite:stone",
                        "tint": [1.0, 1.0, 1.0, 1.0],
                        "sorting_layer": 0,
                        "order_in_layer": 0,
                    },
                },
            ],
        },
    ]
    scene_path.write_text(json.dumps(scene, separators=(",", ":")) + "\n", encoding="utf-8")

    atlas_path = directory / "atlases" / "sprites.json"
    atlas = json.loads(atlas_path.read_text(encoding="utf-8"))
    atlas["regions"] = [
        {
            "id": "sprite:stone",
            "rect": [0, 0, 16, 16],
            "pivot": [0.5, 0.5],
            "pixels_per_unit": 16.0,
        }
    ]
    atlas_path.write_text(json.dumps(atlas, separators=(",", ":")) + "\n", encoding="utf-8")


def _write_sprite_view_project(directory: Path) -> None:
    """Build a small offline atlas scene with an authority-owned orthographic camera."""

    _write_asset_project(directory)
    manifest_path = directory / "daedalus.project.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    manifest["name"] = "offline-sprite-view-smoke"
    manifest_path.write_text(
        json.dumps(manifest, separators=(",", ":")) + "\n", encoding="utf-8"
    )

    colors = (
        (230, 30, 20, 255),
        (20, 210, 40, 255),
        (25, 50, 235, 160),
        (250, 220, 30, 0),
    )
    pixels = bytearray()
    for y in range(16):
        for x in range(16):
            quadrant = (0 if y < 8 else 2) + (0 if x < 8 else 1)
            pixels.extend(colors[quadrant])
    (directory / "images" / "atlas.png").write_bytes(
        _png_rgba_pixels(16, 16, bytes(pixels))
    )

    scene_path = directory / "scene.json"
    scene = json.loads(scene_path.read_text(encoding="utf-8"))
    scene["assets"] = [
        asset
        for asset in scene["assets"]
        if asset["id"] in {"asset:sprite-atlas", "asset:image-atlas"}
    ]
    scene["entities"] = [
        {
            "id": ENTITY_ID,
            "name": "Smoke Orthographic Camera",
            "components": [
                {
                    "id": "component:smoke-camera-transform",
                    "type_id": "daedalus.transform",
                    "data": {
                        "translation": [0.0, 0.0, 0.0],
                        "rotation": [0.0, 0.0, 0.0, 1.0],
                        "scale": [1.0, 1.0, 1.0],
                    },
                },
                {
                    "id": "component:smoke-camera-2d",
                    "type_id": "daedalus.camera_2d",
                    "asset_refs": [],
                    "data": {"vertical_size": 10.0, "near": 0.1, "far": 100.0},
                },
            ],
        },
        {
            "id": SPRITE_ENTITY_ID,
            "name": "Smoke Atlas Sprite",
            "components": [
                {
                    "id": "component:smoke-sprite-transform",
                    "type_id": "daedalus.transform",
                    "data": {
                        "translation": [0.0, 0.0, -5.0],
                        "rotation": [0.0, 0.0, 0.0, 1.0],
                        "scale": [1.0, 1.0, 1.0],
                    },
                },
                {
                    "id": "component:smoke-sprite-renderer",
                    "type_id": "daedalus.sprite_renderer",
                    "asset_refs": ["asset:sprite-atlas"],
                    "data": {
                        "atlas": "asset:sprite-atlas",
                        "region": "sprite:stone",
                        "tint": [1.0, 1.0, 1.0, 1.0],
                        "sorting_layer": 0,
                        "order_in_layer": 0,
                    },
                },
            ],
        },
    ]
    scene_path.write_text(json.dumps(scene, separators=(",", ":")) + "\n", encoding="utf-8")

    atlas_path = directory / "atlases" / "sprites.json"
    atlas = json.loads(atlas_path.read_text(encoding="utf-8"))
    atlas["regions"] = [
        {
            "id": "sprite:stone",
            "rect": [0, 0, 16, 16],
            "pivot": [0.25, 0.75],
            "pixels_per_unit": 16.0,
        }
    ]
    atlas_path.write_text(json.dumps(atlas, separators=(",", ":")) + "\n", encoding="utf-8")


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

    _write_pcm16_mono(project / "audio" / "tone.wav", 8_000, (0, 1_001, -1_001, 0))
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
    *,
    timeout: float | None = None,
) -> tuple[str, bool]:
    submit_timeout = server.timeout if timeout is None else min(timeout, server.timeout)
    deadline = time.monotonic() + submit_timeout
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


def _v2_ack(
    server: _JsonlProjectServer,
    request_id: str,
    ticket: str,
    *,
    timeout: float | None = None,
) -> None:
    ack_timeout = server.timeout if timeout is None else min(timeout, server.timeout)
    deadline = time.monotonic() + ack_timeout
    for attempt in range(int(ack_timeout * 100) + 1):
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


def _asset_preparation_terminal_poll(
    server: _JsonlProjectServer,
    request_prefix: str,
    operation: dict[str, Any],
    *,
    timeout: float,
) -> tuple[str, dict[str, Any], str, float]:
    """Retry only pre-effect actor-lock Busy terminals, acknowledging each ticket first."""

    duration = min(timeout, server.timeout)
    deadline = time.monotonic() + duration
    max_attempts = int(duration * 100) + 1
    for attempt in range(max_attempts):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError(f"timed out submitting asset-preparation operation {request_prefix!r}")
        attempt_prefix = request_prefix if attempt == 0 else f"{request_prefix}-retry-{attempt}"
        ticket, reused = _v2_submit(
            server,
            f"{attempt_prefix}-submit",
            operation,
            timeout=remaining,
        )
        _expect(not reused, "asset-preparation submit unexpectedly reused a request ID", attempt_prefix)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError(f"timed out polling asset-preparation operation {attempt_prefix!r}")
        poll = _v2_poll_until(
            server,
            ticket,
            request_prefix=f"{attempt_prefix}-poll",
            phase="terminal",
            timeout=remaining,
        )
        terminal = poll.get("terminal")
        error = terminal.get("error") if isinstance(terminal, dict) else None
        if not (
            isinstance(terminal, dict)
            and terminal.get("ok") is False
            and isinstance(error, dict)
            and error.get("code") == "busy"
        ):
            return ticket, poll, attempt_prefix, deadline

        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError(f"timed out acknowledging busy asset-preparation operation {attempt_prefix!r}")
        _v2_ack(server, f"{attempt_prefix}-ack", ticket, timeout=remaining)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError(f"timed out retrying busy asset-preparation operation {request_prefix!r}")
        time.sleep(min(0.01, remaining))
    raise SmokeError(f"asset-preparation operation {request_prefix!r} exceeded its bounded Busy retry count")


def _asset_preparation_request(
    server: _JsonlProjectServer,
    request_prefix: str,
    operation: dict[str, Any],
    *,
    expected_kind: str,
    timeout: float,
    outer_result_kind: str = "asset_preparation",
) -> tuple[dict[str, Any], dict[str, Any]]:
    ticket, poll, attempt_prefix, deadline = _asset_preparation_terminal_poll(
        server, request_prefix, operation, timeout=timeout
    )
    result = _v2_terminal_result(poll, context=request_prefix)
    _expect(
        result.get("kind") == outer_result_kind,
        f"{request_prefix!r} operation returned another result kind",
        result,
    )
    tagged = result.get("data")
    _expect(
        isinstance(tagged, dict) and tagged.get("kind") == expected_kind,
        f"asset-preparation result did not contain {expected_kind!r}",
        result,
    )
    payload = tagged.get("data")
    _expect(isinstance(payload, dict), "asset-preparation result omitted its bounded data object", result)
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise SmokeError(f"timed out acknowledging asset-preparation request {request_prefix!r}")
    _v2_ack(server, f"{attempt_prefix}-ack", ticket, timeout=remaining)
    return payload, result


def _asset_preparation_status(
    server: _JsonlProjectServer,
    request_prefix: str,
    *,
    timeout: float,
) -> tuple[dict[str, Any], dict[str, Any]]:
    status, result = _asset_preparation_request(
        server,
        request_prefix,
        {"op": "asset_preparation_status"},
        expected_kind="status",
        timeout=timeout,
    )
    return status, result


def _asset_preparation_command(
    server: _JsonlProjectServer,
    request_prefix: str,
    operation: dict[str, Any],
    *,
    command_id: str,
    outcome_kind: str,
    timeout: float,
    outer_result_kind: str = "asset_preparation",
) -> tuple[dict[str, Any], dict[str, Any]]:
    command, result = _asset_preparation_request(
        server,
        request_prefix,
        operation,
        expected_kind="command",
        timeout=timeout,
        outer_result_kind=outer_result_kind,
    )
    _expect(
        command.get("command_id") == command_id
        and isinstance(command.get("observed_tick"), str)
        and command["observed_tick"].isdecimal(),
        "asset-preparation command omitted its decimal ID or observed tick",
        command,
    )
    outcome = command.get("outcome")
    _expect(
        isinstance(outcome, dict) and outcome.get("kind") == outcome_kind,
        f"asset-preparation command did not return {outcome_kind!r}",
        command,
    )
    return command, result


def _asset_preparation_expect_terminal_error(
    server: _JsonlProjectServer,
    request_prefix: str,
    operation: dict[str, Any],
    *,
    code: str,
    timeout: float,
) -> dict[str, Any]:
    ticket, poll, attempt_prefix, deadline = _asset_preparation_terminal_poll(
        server, request_prefix, operation, timeout=timeout
    )
    terminal = poll.get("terminal")
    _expect(
        isinstance(terminal, dict)
        and terminal.get("ok") is False
        and terminal.get("error", {}).get("code") == code
        and "result" not in terminal,
        f"operation did not reach the expected terminal error {code!r}",
        poll,
    )
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise SmokeError(f"timed out acknowledging invalid candidate {request_prefix!r}")
    _v2_ack(server, f"{attempt_prefix}-ack", ticket, timeout=remaining)
    return poll


def _asset_preparation_project_info(
    server: _JsonlProjectServer,
    *,
    timeout: float,
    request_prefix: str = "asset-preparation-project-info",
) -> dict[str, Any]:
    ticket, poll, attempt_prefix, deadline = _asset_preparation_terminal_poll(
        server,
        request_prefix,
        {"op": "project_info", "offset": 0, "limit": 16},
        timeout=timeout,
    )
    result = _v2_terminal_result(poll, context="asset-preparation project_info")
    _expect(result.get("kind") == "project_info", "project_info returned another result kind", result)
    info = result.get("data")
    _expect(isinstance(info, dict), "project_info omitted its data object", result)
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise SmokeError(f"timed out acknowledging asset-preparation project_info {request_prefix!r}")
    _v2_ack(server, f"{attempt_prefix}-ack", ticket, timeout=remaining)
    return info


def _asset_activation_status(
    server: _JsonlProjectServer,
    request_prefix: str,
    *,
    timeout: float,
) -> tuple[dict[str, Any], dict[str, Any]]:
    status, result = _asset_preparation_request(
        server,
        request_prefix,
        {"op": "asset_activation_status"},
        expected_kind="status",
        timeout=timeout,
        outer_result_kind="asset_activation",
    )
    return status, result


def _asset_activation_command(
    server: _JsonlProjectServer,
    request_prefix: str,
    operation: dict[str, Any],
    *,
    command_id: str,
    outcome_kind: str,
    timeout: float,
) -> tuple[dict[str, Any], dict[str, Any]]:
    return _asset_preparation_command(
        server,
        request_prefix,
        operation,
        command_id=command_id,
        outcome_kind=outcome_kind,
        timeout=timeout,
        outer_result_kind="asset_activation",
    )


def _asset_activation_expect_terminal_error(
    server: _JsonlProjectServer,
    request_prefix: str,
    operation: dict[str, Any],
    *,
    code: str,
    timeout: float,
) -> dict[str, Any]:
    return _asset_preparation_expect_terminal_error(
        server,
        request_prefix,
        operation,
        code=code,
        timeout=timeout,
    )


def _asset_activation_validate_id(
    value: Any,
    *,
    session: str,
    sequence_field: str,
    context: str,
) -> dict[str, str]:
    _expect(
        isinstance(value, dict)
        and value.get("session") == session
        and isinstance(value.get(sequence_field), str)
        and value[sequence_field].isdecimal()
        and int(value[sequence_field]) > 0,
        f"{context} has an invalid decimal identity",
        value,
    )
    return {"session": session, sequence_field: value[sequence_field]}


def _asset_activation_validate_status(
    status: dict[str, Any],
    *,
    session: str,
    active_generation: str | None = None,
    active_source_digest: str | None = None,
) -> dict[str, Any]:
    fields = {
        "session",
        "state",
        "active_generation",
        "active_source_digest",
        "current_activation",
        "latest_failure",
        "memory",
    }
    _expect(
        set(status) == fields,
        "asset-activation status has an unexpected or missing wire field",
        status,
    )
    generation = status.get("active_generation")
    source_digest = status.get("active_source_digest")
    _expect(
        status.get("session") == session
        and isinstance(generation, str)
        and generation.isdecimal()
        and int(generation) > 0
        and isinstance(source_digest, str)
        and source_digest,
        "asset-activation status has invalid active identity",
        status,
    )
    if active_generation is not None:
        _expect(
            generation == active_generation,
            "asset activation unexpectedly changed the active generation",
            status,
        )
    if active_source_digest is not None:
        _expect(
            source_digest == active_source_digest,
            "asset activation unexpectedly changed the active source digest",
            status,
        )
    allowed_states = {
        "idle",
        "stage_pending",
        "staging",
        "staged",
        "cancel_requested",
        "cancelled",
        "superseded",
        "committed",
        "failed",
        "stopping",
        "stopped",
    }
    _expect(status.get("state") in allowed_states, "asset activation returned an unknown state", status)

    memory = status.get("memory")
    memory_fields = {
        "active_cooked_accounted_bytes",
        "candidate_cooked_accounted_bytes",
        "retired_cooked_accounted_bytes",
        "total_cooked_accounted_bytes",
        "max_cooked_accounted_bytes",
        "retired_generation_count",
    }
    _expect(
        isinstance(memory, dict) and set(memory) == memory_fields,
        "asset-activation status has an unexpected or missing memory field",
        memory,
    )
    _expect(
        all(type(memory.get(field)) is int and memory[field] >= 0 for field in memory_fields)
        and memory["active_cooked_accounted_bytes"] > 0
        and memory["total_cooked_accounted_bytes"] <= memory["max_cooked_accounted_bytes"]
        and memory["retired_generation_count"] <= 1,
        "asset-activation status has invalid bounded memory accounting",
        memory,
    )

    current = status.get("current_activation")
    if current is not None:
        _expect(
            isinstance(current, dict)
            and set(current)
            == {
                "id",
                "candidate",
                "expected_active_generation",
                "source_digest",
                "committed_generation",
            },
            "asset-activation summary has an unexpected or missing field",
            current,
        )
        _asset_activation_validate_id(
            current.get("id"),
            session=session,
            sequence_field="sequence",
            context="activation summary ID",
        )
        candidate = current.get("candidate")
        _expect(
            isinstance(candidate, dict)
            and candidate.get("session") == session
            and isinstance(candidate.get("epoch"), str)
            and candidate["epoch"].isdecimal()
            and int(candidate["epoch"]) > 0
            and isinstance(current.get("expected_active_generation"), str)
            and current["expected_active_generation"].isdecimal()
            and isinstance(current.get("source_digest"), str)
            and current["source_digest"]
            and (
                current.get("committed_generation") is None
                or (
                    isinstance(current.get("committed_generation"), str)
                    and current["committed_generation"].isdecimal()
                )
            ),
            "asset-activation summary has invalid candidate/source metadata",
            current,
        )
    failure = status.get("latest_failure")
    if failure is not None:
        _expect(
            isinstance(failure, dict)
            and isinstance(failure.get("code"), str)
            and failure["code"],
            "asset-activation failure summary has no bounded error code",
            failure,
        )
    _asset_preparation_assert_no_payload(status)
    return memory


def _asset_activation_wait_for_state(
    server: _JsonlProjectServer,
    request_prefix: str,
    *,
    activation_id: dict[str, str],
    session: str,
    expected_state: str,
    active_generation: str,
    active_source_digest: str,
    timeout: float,
) -> tuple[dict[str, Any], dict[str, Any]]:
    deadline = time.monotonic() + min(timeout, server.timeout)
    for attempt in range(int(min(timeout, server.timeout) * 100) + 1):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError(
                f"timed out waiting for activation {activation_id!r} to reach {expected_state}"
            )
        status, result = _asset_activation_status(
            server,
            f"{request_prefix}-status-{attempt}",
            timeout=remaining,
        )
        _asset_activation_validate_status(
            status,
            session=session,
            active_generation=active_generation,
            active_source_digest=active_source_digest,
        )
        current = status.get("current_activation")
        if status.get("state") == expected_state:
            _expect(
                isinstance(current, dict) and current.get("id") == activation_id,
                "asset activation reached the requested state for another activation ID",
                status,
            )
            return status, result
        if status.get("state") in {"failed", "cancelled", "superseded", "stopped"}:
            raise SmokeError(
                f"activation {activation_id!r} reached {status.get('state')} before "
                f"{expected_state}: {json.dumps(status, sort_keys=True)}"
            )
        time.sleep(min(0.01, max(0.0, remaining)))
    raise SmokeError(
        f"activation {activation_id!r} exceeded the bounded poll count for {expected_state}"
    )


def _asset_preparation_assert_no_payload(value: Any) -> None:
    payload_keys = {
        "payload",
        "payloadbytes",
        "pcm",
        "pcmsamples",
        "positions",
        "normals",
        "uv0",
        "indices",
        "vertices",
        "texturedata",
        "imagedata",
        "imagebytes",
    }
    if isinstance(value, dict):
        for key, item in value.items():
            _expect(
                key.lower() not in payload_keys,
                "asset-preparation report exposed an asset payload field",
                key,
            )
            _asset_preparation_assert_no_payload(item)
    elif isinstance(value, list):
        for item in value:
            _asset_preparation_assert_no_payload(item)


def _asset_preparation_validate_identity(
    status: dict[str, Any],
    *,
    session: str,
    active_source_digest: str,
    active_cooked_accounted_bytes: int | None,
    active_generation: str = "1",
) -> int:
    required_fields = {
        "session",
        "active_generation",
        "active_source_digest",
        "active_cooked_accounted_bytes",
        "desired",
        "running",
        "pending",
        "state",
        "ready_candidate",
        "latest_failure",
    }
    _expect(
        set(status) == required_fields,
        "asset-preparation status has an unexpected or missing wire field",
        status,
    )
    _expect(
        status.get("session") == session
        and status.get("active_generation") == active_generation
        and status.get("active_source_digest") == active_source_digest,
        "preparation status changed the active session/generation/source identity",
        status,
    )
    active_bytes = status.get("active_cooked_accounted_bytes")
    _expect(
        type(active_bytes) is int and active_bytes >= 0,
        "preparation status returned an invalid active cooked-byte count",
        status,
    )
    if active_cooked_accounted_bytes is not None:
        _expect(
            active_bytes == active_cooked_accounted_bytes,
            "preparation changed the active cooked-byte ledger",
            status,
        )
    for name in ("desired", "running", "pending"):
        candidate_id = status.get(name)
        if candidate_id is not None:
            _expect(
                isinstance(candidate_id, dict)
                and candidate_id.get("session") == session
                and isinstance(candidate_id.get("epoch"), str)
                and candidate_id["epoch"].isdecimal(),
                f"preparation status returned an invalid {name} ID",
                status,
            )
    _expect(
        status.get("state")
        in {
            "idle",
            "pending",
            "preparing",
            "cpu_prepared",
            "cancel_requested",
            "cancelled",
            "superseded",
            "failed",
            "stopping",
            "stopped",
        },
        "preparation status returned an unknown lifecycle state",
        status,
    )
    _asset_preparation_assert_no_payload(status)
    return active_bytes


def _asset_preparation_validate_candidate(
    candidate: dict[str, Any],
    *,
    project: Path,
    session: str,
    entity_count: int,
) -> None:
    expected_fields = {
        "id",
        "is_current",
        "source_digest",
        "catalog_digest",
        "source_scene_revision",
        "asset_count",
        "entity_count",
        "memory",
        "metrics",
    }
    _expect(
        set(candidate) == expected_fields,
        "ready-candidate summary has an unexpected or missing field",
        candidate,
    )
    candidate_id = candidate.get("id")
    _expect(
        isinstance(candidate_id, dict)
        and candidate_id.get("session") == session
        and isinstance(candidate_id.get("epoch"), str)
        and candidate_id["epoch"].isdecimal(),
        "ready candidate omitted its decimal session/epoch identity",
        candidate,
    )
    _expect(
        candidate.get("is_current") is True
        and isinstance(candidate.get("source_digest"), str)
        and candidate["source_digest"]
        and isinstance(candidate.get("catalog_digest"), str)
        and candidate["catalog_digest"]
        and isinstance(candidate.get("source_scene_revision"), str)
        and candidate["source_scene_revision"].isdecimal()
        and candidate.get("asset_count") == 6
        and candidate.get("entity_count") == entity_count,
        "ready candidate omitted valid project/catalog metadata",
        candidate,
    )
    memory = candidate.get("memory")
    _expect(isinstance(memory, dict), "ready candidate omitted its shared content memory summary", candidate)
    for field in ASSET_PREPARATION_MEMORY_FIELDS:
        value = memory.get(field)
        _expect(
            type(value) is int and value >= 0,
            f"ready candidate memory summary has invalid {field}",
            memory,
        )
    expected_sources = len(ASSET_PREPARATION_SOURCE_PATHS)
    expected_bytes = sum((project / source_path).stat().st_size for source_path in ASSET_PREPARATION_SOURCE_PATHS)
    _expect(
        memory.get("schemaVersion") == 1
        and memory.get("cookedAccountedBytes", 0) > 0
        and memory.get("parserScratchPeakBytes", 0) > 0
        and memory.get("uniqueSourceCount") == expected_sources
        and memory.get("capturedSourceBytes") == expected_bytes,
        "ready candidate memory summary does not match its captured project source identities",
        {"memory": memory, "expected_sources": expected_sources, "expected_bytes": expected_bytes},
    )
    metrics = candidate.get("metrics")
    _expect(isinstance(metrics, dict), "ready candidate omitted shared content import metrics", candidate)
    for field in ASSET_PREPARATION_METRIC_FIELDS:
        value = metrics.get(field)
        _expect(
            type(value) is int and value >= 0,
            f"ready candidate metrics have invalid {field}",
            metrics,
        )
    _asset_preparation_assert_no_payload(candidate)


def _asset_preparation_wait_for_candidate(
    server: _JsonlProjectServer,
    request_prefix: str,
    *,
    accepted_candidate: dict[str, Any],
    project: Path,
    session: str,
    active_source_digest: str,
    active_cooked_accounted_bytes: int,
    entity_count: int,
    timeout: float,
    active_generation: str = "1",
) -> tuple[dict[str, Any], dict[str, Any]]:
    deadline = time.monotonic() + timeout
    for attempt in range(int(timeout * 100) + 1):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError(f"timed out waiting for asset-preparation candidate {accepted_candidate!r}")
        status, result = _asset_preparation_status(
            server,
            f"{request_prefix}-status-{attempt}",
            timeout=remaining,
        )
        _asset_preparation_validate_identity(
            status,
            session=session,
            active_source_digest=active_source_digest,
            active_cooked_accounted_bytes=active_cooked_accounted_bytes,
            active_generation=active_generation,
        )
        candidate = status.get("ready_candidate")
        if (
            status.get("state") == "cpu_prepared"
            and isinstance(candidate, dict)
            and candidate.get("id") == accepted_candidate
        ):
            _asset_preparation_validate_candidate(
                candidate,
                project=project,
                session=session,
                entity_count=entity_count,
            )
            return status, result
        if status.get("state") == "failed":
            raise SmokeError(
                "asset-preparation worker reported a failure instead of CpuPrepared; "
                f"status: {json.dumps(status, sort_keys=True)}"
            )
        time.sleep(min(0.01, max(0.0, remaining)))
    raise SmokeError(f"asset-preparation candidate exceeded the bounded status-poll count: {accepted_candidate!r}")


def _asset_preparation_wait_for_cancelled(
    server: _JsonlProjectServer,
    request_prefix: str,
    *,
    candidate_id: dict[str, Any],
    session: str,
    active_source_digest: str,
    active_cooked_accounted_bytes: int,
    timeout: float,
) -> tuple[dict[str, Any], dict[str, Any]]:
    deadline = time.monotonic() + timeout
    for attempt in range(int(timeout * 100) + 1):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError(f"timed out waiting for actual cancellation of {candidate_id!r}")
        status, result = _asset_preparation_status(
            server,
            f"{request_prefix}-status-{attempt}",
            timeout=remaining,
        )
        _asset_preparation_validate_identity(
            status,
            session=session,
            active_source_digest=active_source_digest,
            active_cooked_accounted_bytes=active_cooked_accounted_bytes,
        )
        if status.get("state") == "cancelled":
            _expect(
                status.get("ready_candidate") is None,
                "cancelled candidate remained exposed as ready",
                status,
            )
            return status, result
        if status.get("state") in {"failed", "superseded", "stopped"}:
            raise SmokeError(
                f"candidate reached {status.get('state')!r} instead of actual cancellation; "
                f"status: {json.dumps(status, sort_keys=True)}"
            )
        time.sleep(min(0.01, max(0.0, remaining)))
    raise SmokeError(f"asset-preparation candidate exceeded the bounded cancellation-poll count: {candidate_id!r}")


def _asset_preparation_audio_composition(
    server: _JsonlProjectServer,
    request_prefix: str,
    *,
    session: str,
    active_source_digest: str,
    active_cooked_accounted_bytes: int,
    timeout: float,
) -> dict[str, Any]:
    capabilities = _v2_audio_capabilities(
        server, request_id=f"{request_prefix}-capabilities"
    )
    audio_status, _ = _v2_audio_result(
        server,
        f"{request_prefix}-status-before",
        {"op": "audio_status"},
        expected_kind="status",
        timeout=timeout,
    )
    _expect(
        audio_status.get("session") == session
        and isinstance(audio_status.get("backend"), dict)
        and audio_status["backend"].get("kind") == "offline"
        and audio_status["backend"].get("data", {}).get("sample_rate") == 48_000
        and audio_status.get("alias_count") == 1
        and audio_status.get("unique_clip_count") == 1
        and audio_status.get("converted_clip_count") == 1
        and audio_status.get("active_voice_count") == 0
        and audio_status.get("offline_progressed_frames") == "0",
        "preparation service did not initialize the imported one-clip offline audio bank",
        audio_status,
    )
    project_before = _asset_preparation_project_info(
        server,
        timeout=timeout,
        request_prefix=f"{request_prefix}-project-before-audio",
    )
    tick_before = project_before.get("tick")
    _expect(
        project_before.get("session") == session
        and project_before.get("source_digest") == active_source_digest
        and isinstance(tick_before, str)
        and tick_before.isdecimal(),
        "preparation/audio service project_info omitted its active identity or decimal tick",
        project_before,
    )

    played, _ = _v2_audio_result(
        server,
        f"{request_prefix}-play-looping-imported-tone",
        {
            "op": "audio_command",
            "mutation": {
                "kind": "play",
                "clip": "asset:audio-tone",
                "volume": 1.0,
                "pan": 0.0,
                "looping": True,
            },
        },
        expected_kind="command",
        timeout=timeout,
    )
    play_outcome = played.get("outcome")
    voice = play_outcome.get("data", {}).get("voice") if isinstance(play_outcome, dict) else None
    _expect(
        played.get("command_id")
        and isinstance(played.get("command_id"), str)
        and played["command_id"].isdecimal()
        and int(played["command_id"]) >= (1 << 127)
        and isinstance(play_outcome, dict)
        and play_outcome.get("kind") == "played"
        and isinstance(voice, dict)
        and voice.get("session") == session
        and isinstance(voice.get("sequence"), str)
        and voice["sequence"].isdecimal()
        and played.get("observed_tick") == tick_before,
        "same-session automatic audio Play did not start the imported looping WAV",
        played,
    )

    rendered, _ = _v2_audio_result(
        server,
        f"{request_prefix}-render-offline",
        {
            "op": "audio_command",
            "mutation": {"kind": "render_offline", "frames": 256},
        },
        expected_kind="command",
        timeout=timeout,
    )
    render_summary = rendered.get("outcome", {}).get("data")
    peaks = (
        render_summary.get("left_peak"),
        render_summary.get("right_peak"),
    ) if isinstance(render_summary, dict) else (None, None)
    _expect(
        isinstance(render_summary, dict)
        and rendered.get("outcome", {}).get("kind") == "offline"
        and rendered.get("observed_tick") == tick_before
        and render_summary.get("frames") == 256
        and render_summary.get("sample_rate") == 48_000
        and render_summary.get("channels") == 2
        and render_summary.get("cumulative_frames") == "256"
        and render_summary.get("nonzero_samples", 0) > 0
        and all(type(peak) in (int, float) and math.isfinite(peak) for peak in peaks)
        and max(peaks) > 0.0
        and max(peaks) < 1.0,
        "same-session offline render did not produce bounded finite PCM from the imported looping WAV",
        rendered,
    )
    audio_status_after_render, _ = _v2_audio_result(
        server,
        f"{request_prefix}-status-after-render",
        {"op": "audio_status"},
        expected_kind="status",
        timeout=timeout,
    )
    project_after = _asset_preparation_project_info(
        server,
        timeout=timeout,
        request_prefix=f"{request_prefix}-project-after-audio",
    )
    _expect(
        audio_status_after_render.get("session") == session
        and audio_status_after_render.get("offline_progressed_frames") == "256"
        and audio_status_after_render.get("active_voice_count") == 1
        and project_after.get("session") == session
        and project_after.get("source_digest") == active_source_digest
        and project_after.get("tick") == tick_before,
        "offline audio frames were not independent from the current authority tick/project identity",
        {
            "audio_status": audio_status_after_render,
            "project_before": project_before,
            "project_after": project_after,
        },
    )

    stopped, _ = _v2_audio_result(
        server,
        f"{request_prefix}-stop-looping-tone",
        {"op": "audio_command", "mutation": {"kind": "stop", "voice": voice}},
        expected_kind="command",
        timeout=timeout,
    )
    _expect(
        stopped.get("outcome", {}).get("kind") == "stop_requested",
        "same-session Stop did not report its typed request outcome",
        stopped,
    )
    stop_rendered, _ = _v2_audio_result(
        server,
        f"{request_prefix}-render-after-stop",
        {
            "op": "audio_command",
            "mutation": {"kind": "render_offline", "frames": 64},
        },
        expected_kind="command",
        timeout=timeout,
    )
    ended_voice, _ = _v2_audio_result(
        server,
        f"{request_prefix}-ended-voice",
        {"op": "audio_voice", "voice": voice},
        expected_kind="voice",
        timeout=timeout,
    )
    final_audio_status, _ = _v2_audio_result(
        server,
        f"{request_prefix}-status-after-stop",
        {"op": "audio_status"},
        expected_kind="status",
        timeout=timeout,
    )
    final_prep_status, _ = _asset_preparation_status(
        server,
        f"{request_prefix}-preparation-status-after-audio",
        timeout=timeout,
    )
    final_active_cooked_bytes = _asset_preparation_validate_identity(
        final_prep_status,
        session=session,
        active_source_digest=active_source_digest,
        active_cooked_accounted_bytes=active_cooked_accounted_bytes,
    )
    project_after_stop = _asset_preparation_project_info(
        server,
        timeout=timeout,
        request_prefix=f"{request_prefix}-project-after-stop",
    )
    _expect(
        stop_rendered.get("outcome", {}).get("data", {}).get("cumulative_frames") == "320"
        and stop_rendered.get("outcome", {}).get("kind") == "offline"
        and stop_rendered.get("outcome", {}).get("data", {}).get("frames") == 64
        and ended_voice.get("state") == "ended"
        and ended_voice.get("voice") == voice
        and final_audio_status.get("offline_progressed_frames") == "320"
        and final_audio_status.get("active_voice_count") == 0
        and final_active_cooked_bytes == active_cooked_accounted_bytes
        and final_prep_status.get("state") == "idle"
        and final_prep_status.get("desired") is None
        and final_prep_status.get("running") is None
        and final_prep_status.get("pending") is None
        and final_prep_status.get("ready_candidate") is None
        and final_prep_status.get("latest_failure") is None
        and project_after_stop.get("session") == session
        and project_after_stop.get("source_digest") == active_source_digest
        and project_after_stop.get("tick") == tick_before,
        "same-session audio Stop did not reap its voice while preserving the active preparation project",
        {
            "stop_render": stop_rendered,
            "ended_voice": ended_voice,
            "audio_status": final_audio_status,
            "preparation_status": final_prep_status,
            "project_after_stop": project_after_stop,
        },
    )
    return {
        "capabilities": capabilities,
        "audio_status": audio_status,
        "played": played,
        "voice": voice,
        "offline_render": rendered,
        "status_after_render": audio_status_after_render,
        "project_info_before_audio": project_before,
        "project_info_after_audio": project_after,
        "project_info_after_stop": project_after_stop,
        "stopped": stopped,
        "stop_render": stop_rendered,
        "ended_voice": ended_voice,
        "status_after_stop": final_audio_status,
        "authority_tick": tick_before,
    }


def _run_asset_preparation_session(
    server: _JsonlProjectServer,
    project: Path,
    *,
    session: str,
    active_source_digest: str,
    audio: bool,
    entity_count: int,
    timeout: float,
    request_prefix: str,
) -> dict[str, Any]:
    checks: list[str] = []
    initial_status, initial_result = _asset_preparation_status(
        server,
        f"{request_prefix}-status-initial",
        timeout=timeout,
    )
    active_cooked_bytes = _asset_preparation_validate_identity(
        initial_status,
        session=session,
        active_source_digest=active_source_digest,
        active_cooked_accounted_bytes=None,
    )
    _expect(
        initial_status.get("state") == "idle"
        and initial_status.get("desired") is None
        and initial_status.get("running") is None
        and initial_status.get("pending") is None
        and initial_status.get("ready_candidate") is None
        and initial_status.get("latest_failure") is None,
        "new asset-preparation session did not start idle",
        initial_status,
    )
    checks.append("asset_preparation_initial_status_preserves_active_identity")

    audio_composition = None
    if audio:
        audio_composition = _asset_preparation_audio_composition(
            server,
            f"{request_prefix}-audio",
            session=session,
            active_source_digest=active_source_digest,
            active_cooked_accounted_bytes=active_cooked_bytes,
            timeout=timeout,
        )
        checks.append("asset_preparation_same_actor_offline_audio_is_independent_and_stops")

    def prepare(label: str, command_id: str) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
        command, _ = _asset_preparation_command(
            server,
            f"{request_prefix}-{label}-prepare",
            {"op": "asset_preparation_prepare", "command_id": command_id},
            command_id=command_id,
            outcome_kind="prepare_accepted",
            timeout=timeout,
        )
        outcome = command["outcome"].get("data")
        accepted_id = outcome.get("candidate") if isinstance(outcome, dict) else None
        _expect(
            isinstance(accepted_id, dict)
            and accepted_id.get("session") == session
            and isinstance(accepted_id.get("epoch"), str)
            and accepted_id["epoch"].isdecimal(),
            "Prepare receipt omitted its accepted session/epoch candidate",
            command,
        )
        status, _ = _asset_preparation_wait_for_candidate(
            server,
            f"{request_prefix}-{label}",
            accepted_candidate=accepted_id,
            project=project,
            session=session,
            active_source_digest=active_source_digest,
            active_cooked_accounted_bytes=active_cooked_bytes,
            entity_count=entity_count,
            timeout=timeout,
        )
        candidate = status["ready_candidate"]
        return command, status, candidate

    # The native preparation owner first loads this source into its persistent cook cache.
    # Editing one real input after startup proves that a candidate can invalidate only the
    # affected image leaf while geometry and unrelated formats remain reusable.
    (project / "images" / "base.png").write_bytes(_png_rgba(2, 2, (44, 88, 220, 255)))
    png_command, png_status, png_candidate = prepare("png-edit", "900001")
    png_metrics = png_candidate["metrics"]
    _expect(
        png_candidate["source_digest"] != active_source_digest
        and png_metrics["pngBaseDecodes"] >= 1
        and png_metrics["imageCooks"] >= 1
        and png_metrics["modelGeometryCooks"] == 0
        and png_metrics["atlasCooks"] == 0
        and png_metrics["audioCooks"] == 0,
        "PNG edit did not invalidate image cooking while retaining model geometry and other formats",
        png_candidate,
    )
    checks.append("asset_preparation_png_edit_invalidates_image_only")

    warm_command, warm_status, warm_candidate = prepare("warm", "900002")
    warm_metrics = warm_candidate["metrics"]
    _expect(
        warm_candidate["source_digest"] == png_candidate["source_digest"]
        and warm_candidate["catalog_digest"] == png_candidate["catalog_digest"]
        and warm_metrics["sourceReads"] > 0
        and warm_metrics["sourceBytes"] > 0
        and warm_metrics["containerParses"] > 0,
        "unchanged warm preparation did not preserve identity or report honest source reads/parses",
        warm_candidate,
    )
    for metric in ("pngBaseDecodes", "modelGeometryCooks", "imageCooks", "atlasCooks", "audioCooks"):
        _expect(warm_metrics.get(metric) == 0, f"unchanged warm preparation unexpectedly repeated {metric}", warm_metrics)
    _expect(
        int(warm_candidate["id"]["epoch"]) > int(png_candidate["id"]["epoch"]),
        "a new warm Prepare did not receive a newer candidate epoch",
        {"png": png_candidate["id"], "warm": warm_candidate["id"]},
    )
    checks.append("asset_preparation_unchanged_warm_reports_reads_without_recooking")

    replay_command, _ = _asset_preparation_command(
        server,
        f"{request_prefix}-warm-replay",
        {"op": "asset_preparation_prepare", "command_id": "900002"},
        command_id="900002",
        outcome_kind="prepare_accepted",
        timeout=timeout,
    )
    replay_status, replay_result = _asset_preparation_status(
        server,
        f"{request_prefix}-status-after-replay",
        timeout=timeout,
    )
    _asset_preparation_validate_identity(
        replay_status,
        session=session,
        active_source_digest=active_source_digest,
        active_cooked_accounted_bytes=active_cooked_bytes,
    )
    _expect(
        replay_command == warm_command
        and isinstance(replay_status.get("ready_candidate"), dict)
        and replay_status["state"] == "cpu_prepared"
        and replay_status["ready_candidate"].get("id") == warm_candidate["id"]
        and replay_status["ready_candidate"].get("is_current") is True
        and replay_status.get("desired") == warm_candidate["id"]
        and replay_status.get("running") is None
        and replay_status.get("pending") is None,
        "retained Prepare command-ID replay created a new candidate epoch or import",
        {"original": warm_command, "replay": replay_command, "status": replay_status},
    )
    checks.append("asset_preparation_command_id_replay_does_not_create_epoch")

    external_gltf = project / "models" / "external.gltf"
    document = json.loads(external_gltf.read_text(encoding="utf-8"))
    document["nodes"][0]["translation"] = [0.5, 0.0, 0.0]
    _write_json(external_gltf, document)
    node_command, node_status, node_candidate = prepare("node-edit", "900003")
    node_metrics = node_candidate["metrics"]
    _expect(
        node_candidate["source_digest"] != warm_candidate["source_digest"]
        and node_candidate["catalog_digest"] != warm_candidate["catalog_digest"]
        and node_metrics["containerParses"] > 0
        and node_metrics["modelGeometryCooks"] == 0
        and node_metrics["imageCooks"] == 0
        and node_metrics["atlasCooks"] == 0
        and node_metrics["audioCooks"] == 0,
        "glTF node edit failed to reparse/rekey the model wrapper while reusing geometry",
        node_candidate,
    )
    checks.append("asset_preparation_gltf_node_edit_reuses_geometry")

    atlas_path = project / "atlases" / "sprites.json"
    atlas_document = json.loads(atlas_path.read_text(encoding="utf-8"))
    atlas_document["regions"][0]["pivot"] = [0.25, 0.75]
    _write_json(atlas_path, atlas_document)
    atlas_command, atlas_status, atlas_candidate = prepare("atlas-edit", "900004")
    atlas_metrics = atlas_candidate["metrics"]
    _expect(
        atlas_candidate["source_digest"] != node_candidate["source_digest"]
        and atlas_candidate["catalog_digest"] != node_candidate["catalog_digest"]
        and atlas_metrics["atlasCooks"] >= 1
        and atlas_metrics["pngBaseDecodes"] == 0
        and atlas_metrics["modelGeometryCooks"] == 0
        and atlas_metrics["imageCooks"] == 0
        and atlas_metrics["audioCooks"] == 0,
        "atlas pivot edit did not invalidate only atlas metadata cooking",
        atlas_candidate,
    )
    checks.append("asset_preparation_atlas_edit_invalidates_atlas_only")

    _write_pcm16_mono(project / "audio" / "tone.wav", 8_000, (0, 1_001, -1_001, 0))
    audio_command, audio_status, audio_candidate = prepare("wav-edit", "900005")
    audio_metrics = audio_candidate["metrics"]
    _expect(
        audio_candidate["source_digest"] != atlas_candidate["source_digest"]
        and audio_candidate["catalog_digest"] != atlas_candidate["catalog_digest"]
        and audio_metrics["audioCooks"] >= 1
        and audio_metrics["pngBaseDecodes"] == 0
        and audio_metrics["modelGeometryCooks"] == 0
        and audio_metrics["imageCooks"] == 0
        and audio_metrics["atlasCooks"] == 0,
        "WAV edit did not invalidate only audio cooking",
        audio_candidate,
    )
    checks.append("asset_preparation_wav_edit_invalidates_audio_only")

    ready_id = audio_candidate["id"]
    future_id = {"session": session, "epoch": str(int(ready_id["epoch"]) + 1)}
    future_error = _asset_preparation_expect_terminal_error(
        server,
        f"{request_prefix}-future-candidate",
        {
            "op": "asset_preparation_cancel",
            "command_id": "900006",
            "candidate": future_id,
        },
        code="invalid_candidate",
        timeout=timeout,
    )
    foreign_id = {"session": str(int(session) + 1), "epoch": ready_id["epoch"]}
    foreign_error = _asset_preparation_expect_terminal_error(
        server,
        f"{request_prefix}-foreign-candidate",
        {
            "op": "asset_preparation_cancel",
            "command_id": "900007",
            "candidate": foreign_id,
        },
        code="invalid_candidate",
        timeout=timeout,
    )
    still_ready_status, still_ready_result = _asset_preparation_status(
        server,
        f"{request_prefix}-status-after-invalid-candidates",
        timeout=timeout,
    )
    _asset_preparation_validate_identity(
        still_ready_status,
        session=session,
        active_source_digest=active_source_digest,
        active_cooked_accounted_bytes=active_cooked_bytes,
    )
    _expect(
        isinstance(still_ready_status.get("ready_candidate"), dict)
        and still_ready_status["ready_candidate"].get("id") == ready_id
        and still_ready_status.get("state") == "cpu_prepared",
        "invalid future/foreign candidates changed the ready candidate",
        still_ready_status,
    )
    cancel_command, _ = _asset_preparation_command(
        server,
        f"{request_prefix}-cancel",
        {
            "op": "asset_preparation_cancel",
            "command_id": "900006",
            "candidate": ready_id,
        },
        command_id="900006",
        outcome_kind="cancel_accepted",
        timeout=timeout,
    )
    accepted_cancel_id = cancel_command["outcome"].get("data", {}).get("candidate")
    _expect(
        accepted_cancel_id == ready_id,
        "Cancel receipt did not identify the requested ready candidate after invalid-ID retries",
        cancel_command,
    )
    cancelled_status, cancelled_result = _asset_preparation_wait_for_cancelled(
        server,
        f"{request_prefix}-cancelled",
        candidate_id=ready_id,
        session=session,
        active_source_digest=active_source_digest,
        active_cooked_accounted_bytes=active_cooked_bytes,
        timeout=timeout,
    )
    checks.extend(
        [
            "asset_preparation_cancel_receipt_is_distinct_from_actual_cancel",
            "asset_preparation_future_candidate_rejection_does_not_consume_command_id",
        ]
    )
    final_status, final_result = _asset_preparation_status(
        server,
        f"{request_prefix}-status-final",
        timeout=timeout,
    )
    _asset_preparation_validate_identity(
        final_status,
        session=session,
        active_source_digest=active_source_digest,
        active_cooked_accounted_bytes=active_cooked_bytes,
    )
    _expect(
        final_status.get("state") == "cancelled"
        and final_status.get("ready_candidate") is None,
        "invalid future/foreign candidates changed the cancelled preparation state",
        final_status,
    )
    checks.extend(
        [
            "asset_preparation_future_candidate_rejected_before_effect",
            "asset_preparation_foreign_candidate_rejected_before_effect",
            "asset_preparation_active_project_unchanged_after_candidate_edits",
        ]
    )

    return {
        "session": session,
        "active_generation": initial_status["active_generation"],
        "active_source_digest": active_source_digest,
        "active_cooked_accounted_bytes": active_cooked_bytes,
        "same_actor_audio": audio_composition,
        "checks": checks,
        "initial_status": initial_result,
        "prepare_receipts": {
            "png_edit": png_command,
            "warm": warm_command,
            "node_edit": node_command,
            "atlas_edit": atlas_command,
            "wav_edit": audio_command,
            "cancel": cancel_command,
        },
        "candidate_statuses": {
            "png_edit": png_status,
            "warm": warm_status,
            "after_replay": replay_status,
            "node_edit": node_status,
            "atlas_edit": atlas_status,
            "wav_edit": audio_status,
            "still_ready_after_invalid_candidates": still_ready_status,
            "cancelled": cancelled_status,
            "final": final_status,
        },
        "acknowledgements": {
            "replay_status": replay_result,
            "still_ready_status": still_ready_result,
            "cancelled_status": cancelled_result,
            "final_status": final_result,
        },
        "invalid_candidate_rejections": {
            "future": future_error,
            "foreign": foreign_error,
        },
    }


def _run_asset_preparation_smoke(
    binary: Path,
    *,
    root: Path,
    cwd: Path,
    timeout: float,
    audio: bool = False,
) -> dict[str, Any]:
    project = root / "offline-asset-preparation-project"
    _write_asset_project(project)
    command = [
        "project",
        "serve",
        os.fspath(project),
        "--protocol",
        "2",
        "--asset-preparation",
    ]
    if audio:
        command.extend(["--audio", "offline"])
    server = _JsonlProjectServer(
        binary,
        project,
        cwd=cwd,
        timeout=timeout,
        command=command,
        protocol_version=VIEW_PROTOCOL_VERSION,
        process_label=(
            "project serve --asset-preparation --audio offline"
            if audio
            else "project serve --asset-preparation"
        ),
    )
    with server:
        info = _asset_preparation_project_info(server, timeout=timeout)
        session = info.get("session")
        source_digest = info.get("source_digest")
        _expect(
            isinstance(session, str)
            and session.isdecimal()
            and isinstance(source_digest, str)
            and source_digest,
            "asset-preparation serve returned invalid session/source identities",
            info,
        )
        source_files = info.get("source_files")
        _expect(isinstance(source_files, list), "asset-preparation project_info omitted source identities", info)
        source_ids = {item.get("source_id") for item in source_files if isinstance(item, dict)}
        _expect(
            source_ids == set(ASSET_PREPARATION_SOURCE_PATHS),
            "asset-preparation fixture did not expose exactly its authored source set",
            source_ids,
        )
        _expect(info.get("next_offset") is None, "asset-preparation source list exceeded its bounded page", info)
        session_result = _run_asset_preparation_session(
            server,
            project,
            session=session,
            active_source_digest=source_digest,
            audio=audio,
            entity_count=0,
            timeout=timeout,
            request_prefix="asset-preparation-serve",
        )
        exit_response = server.request("asset-preparation-exit", {"op": "close"}, allow_error=True)
        _expect(
            exit_response.get("ok") is True,
            "asset-preparation serve did not accept bounded session close",
            exit_response,
        )
        server.wait_for_exit(timeout=timeout)
    return {
        "project_source_digest": source_digest,
        "authored_source_files": source_files,
        "session": session_result,
        "exit": exit_response,
    }


def _asset_activation_v2_terminal_request(
    server: _JsonlProjectServer,
    request_prefix: str,
    operation: dict[str, Any],
    *,
    result_kind: str,
    timeout: float,
) -> dict[str, Any]:
    ticket, poll, attempt_prefix, deadline = _asset_preparation_terminal_poll(
        server,
        request_prefix,
        operation,
        timeout=timeout,
    )
    result = _v2_terminal_result(poll, context=request_prefix)
    _expect(
        result.get("kind") == result_kind,
        f"{request_prefix!r} returned another terminal result kind",
        result,
    )
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise SmokeError(f"timed out acknowledging {request_prefix!r}")
    _v2_ack(server, f"{attempt_prefix}-ack", ticket, timeout=remaining)
    return result


def _asset_activation_get_transform(
    server: _JsonlProjectServer,
    request_prefix: str,
    *,
    timeout: float,
) -> dict[str, Any]:
    result = _asset_activation_v2_terminal_request(
        server,
        request_prefix,
        {"op": "get_transform", "target": {"stable_id": ENTITY_ID}},
        result_kind="transform",
        timeout=timeout,
    )
    data = result.get("data")
    _expect(isinstance(data, dict), "get_transform omitted its data", result)
    return data


def _asset_activation_apply_authority_transform(
    server: _JsonlProjectServer,
    *,
    session: str,
    command_id: str,
    timeout: float,
    initial_transform: dict[str, Any] | None = None,
    target_transform: dict[str, Any] = TRANSFORM_AFTER_COMMAND,
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    expected_initial_transform = initial_transform or {
        "translation": [0.0, 0.0, 0.0],
        "rotation": [0.0, 0.0, 0.0, 1.0],
        "scale": [1.0, 1.0, 1.0],
    }
    initial = _asset_activation_get_transform(
        server,
        "asset-activation-transform-initial",
        timeout=timeout,
    )
    initial_entity = initial.get("entity")
    _expect(
        initial.get("observed_tick") == "0"
        and isinstance(initial_entity, dict)
        and initial_entity.get("stable_id") == ENTITY_ID
        and initial_entity.get("revision") == "0"
        and initial_entity.get("handle", {}).get("session") == session
        and initial_entity.get("transform") == expected_initial_transform,
        "activation smoke did not start with the authored live entity state",
        initial,
    )
    handle = initial_entity.get("handle")
    _expect(isinstance(handle, dict), "get_transform returned no live entity handle", initial_entity)

    set_ticket, reused = _v2_submit(
        server,
        "asset-activation-transform-set",
        {
            "op": "set_transform",
            "target": {"stable_id": ENTITY_ID},
            "command_id": command_id,
            "expected_revision": "0",
            "transform": target_transform,
        },
        timeout=timeout,
    )
    _expect(not reused, "activation transform submit unexpectedly reused a ticket")
    accepted = _v2_poll_until(
        server,
        set_ticket,
        request_prefix="asset-activation-transform-authority",
        phase="authority_accepted",
        timeout=timeout,
    )
    authority = accepted.get("authority")
    _expect(
        isinstance(authority, dict)
        and authority.get("submission") == "enqueued"
        and authority.get("apply_tick") == "1",
        "live transform was not accepted for the next authority tick",
        accepted,
    )

    step_ticket, reused = _v2_submit(
        server,
        "asset-activation-authority-step",
        {"op": "step", "count": 1},
        timeout=timeout,
    )
    _expect(not reused, "activation authority Step unexpectedly reused a ticket")
    step_poll = _v2_poll_until(
        server,
        step_ticket,
        request_prefix="asset-activation-authority-step-poll",
        phase="terminal",
        timeout=timeout,
    )
    step = _v2_terminal_result(step_poll, context="activation authority Step")
    step_data = step.get("data")
    step_commands = (
        step_data[0].get("commands")
        if isinstance(step_data, list) and len(step_data) == 1
        else None
    )
    _expect(
        step.get("kind") == "step"
        and isinstance(step_data, list)
        and len(step_data) == 1
        and step_data[0].get("tick") == "1"
        and isinstance(step_commands, list)
        and len(step_commands) == 1
        and step_commands[0].get("command_id") == command_id
        and step_commands[0].get("outcome")
        == {"kind": "applied", "data": {"revision": "1"}},
        "authority Step did not apply the live transform at revision one",
        step,
    )
    _v2_ack(server, "asset-activation-authority-step-ack", step_ticket, timeout=timeout)

    set_poll = _v2_poll_until(
        server,
        set_ticket,
        request_prefix="asset-activation-transform-terminal",
        phase="terminal",
        timeout=timeout,
    )
    set_result = _v2_terminal_result(set_poll, context="activation set_transform")
    set_data = set_result.get("data")
    _expect(
        set_result.get("kind") == "command"
        and isinstance(set_data, dict)
        and set_data.get("command_id") == command_id
        and set_data.get("outcome") == {"kind": "applied", "data": {"revision": "1"}}
        and set_data.get("resolve_tick") == "1",
        "set_transform did not confirm its authoritative application",
        set_result,
    )
    _v2_ack(server, "asset-activation-transform-set-ack", set_ticket, timeout=timeout)

    state = _asset_activation_get_transform(
        server,
        "asset-activation-transform-after-step",
        timeout=timeout,
    )
    entity = state.get("entity")
    _expect(
        state.get("observed_tick") == "1"
        and isinstance(entity, dict)
        and entity.get("stable_id") == ENTITY_ID
        and entity.get("handle") == handle
        and entity.get("revision") == "1"
        and entity.get("transform") == target_transform,
        "live transform state did not survive the authority Step",
        state,
    )
    return state, handle, step_data[0]


def _asset_activation_prepare_candidate(
    server: _JsonlProjectServer,
    request_prefix: str,
    *,
    command_id: str,
    project: Path,
    session: str,
    timeout: float,
    entity_count: int = 1,
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    active_status, _ = _asset_activation_status(
        server,
        f"{request_prefix}-active-before-prepare",
        timeout=timeout,
    )
    active_memory = _asset_activation_validate_status(active_status, session=session)
    active_generation = active_status["active_generation"]
    active_source_digest = active_status["active_source_digest"]
    preparation_status, _ = _asset_preparation_status(
        server,
        f"{request_prefix}-preparation-before-prepare",
        timeout=timeout,
    )
    active_cooked_bytes = _asset_preparation_validate_identity(
        preparation_status,
        session=session,
        active_source_digest=active_source_digest,
        active_cooked_accounted_bytes=None,
        active_generation=active_generation,
    )
    _expect(
        active_memory["active_cooked_accounted_bytes"] > 0,
        "activation status reports no active cooked backing before preparation",
        active_status,
    )
    command, _ = _asset_preparation_command(
        server,
        f"{request_prefix}-prepare",
        {"op": "asset_preparation_prepare", "command_id": command_id},
        command_id=command_id,
        outcome_kind="prepare_accepted",
        timeout=timeout,
    )
    candidate_id = command.get("outcome", {}).get("data", {}).get("candidate")
    _expect(
        isinstance(candidate_id, dict)
        and candidate_id.get("session") == session
        and isinstance(candidate_id.get("epoch"), str)
        and candidate_id["epoch"].isdecimal(),
        "Prepare receipt omitted its candidate identity",
        command,
    )
    status, _ = _asset_preparation_wait_for_candidate(
        server,
        request_prefix,
        accepted_candidate=candidate_id,
        project=project,
        session=session,
        active_source_digest=active_source_digest,
        active_cooked_accounted_bytes=active_cooked_bytes,
        entity_count=entity_count,
        timeout=timeout,
        active_generation=active_generation,
    )
    latest_active, _ = _asset_activation_status(
        server,
        f"{request_prefix}-active-after-prepare",
        timeout=timeout,
    )
    _asset_activation_validate_status(
        latest_active,
        session=session,
        active_generation=active_generation,
        active_source_digest=active_source_digest,
    )
    return command, status, status["ready_candidate"]


def _asset_activation_stage_candidate(
    server: _JsonlProjectServer,
    request_prefix: str,
    *,
    command_id: str,
    candidate: dict[str, Any],
    session: str,
    expected_active_generation: str,
    active_source_digest: str,
    timeout: float,
) -> tuple[dict[str, str], dict[str, Any], dict[str, Any]]:
    candidate_id = candidate["id"]
    command, _ = _asset_activation_command(
        server,
        f"{request_prefix}-stage",
        {
            "op": "asset_activation_stage",
            "candidate": candidate_id,
            "expected_active_generation": expected_active_generation,
            "command_id": command_id,
        },
        command_id=command_id,
        outcome_kind="stage_accepted",
        timeout=timeout,
    )
    accepted = command["outcome"].get("data")
    _expect(
        isinstance(accepted, dict) and accepted.get("candidate") == candidate_id,
        "Stage receipt did not identify the prepared candidate",
        command,
    )
    activation_id = _asset_activation_validate_id(
        accepted.get("activation"),
        session=session,
        sequence_field="sequence",
        context="Stage activation ID",
    )
    staged, _ = _asset_activation_wait_for_state(
        server,
        request_prefix,
        activation_id=activation_id,
        session=session,
        expected_state="staged",
        active_generation=expected_active_generation,
        active_source_digest=active_source_digest,
        timeout=timeout,
    )
    current = staged.get("current_activation")
    _expect(
        isinstance(current, dict)
        and current.get("candidate") == candidate_id
        and current.get("expected_active_generation") == expected_active_generation
        and current.get("source_digest") == candidate["source_digest"]
        and current.get("committed_generation") is None,
        "staged activation summary did not retain its candidate, source digest, and expected generation",
        staged,
    )
    return activation_id, command, staged


def _asset_activation_commit_candidate(
    server: _JsonlProjectServer,
    request_prefix: str,
    *,
    activation_id: dict[str, str],
    candidate: dict[str, Any],
    command_id: str,
    expected_active_generation: str,
    session: str,
    timeout: float,
) -> tuple[dict[str, Any], dict[str, Any]]:
    command, _ = _asset_activation_command(
        server,
        request_prefix,
        {
            "op": "asset_activation_commit",
            "activation": activation_id,
            "expected_active_generation": expected_active_generation,
            "command_id": command_id,
        },
        command_id=command_id,
        outcome_kind="commit_applied",
        timeout=timeout,
    )
    committed = command["outcome"].get("data")
    expected_generation = str(int(expected_active_generation) + 1)
    _expect(
        isinstance(committed, dict)
        and committed.get("activation") == activation_id
        and committed.get("generation") == expected_generation,
        "Commit receipt did not identify the new project generation",
        command,
    )
    status, _ = _asset_activation_status(
        server,
        f"{request_prefix}-status-committed",
        timeout=timeout,
    )
    memory = _asset_activation_validate_status(
        status,
        session=session,
        active_generation=expected_generation,
        active_source_digest=candidate["source_digest"],
    )
    current = status.get("current_activation")
    _expect(
        status.get("state") == "committed"
        and isinstance(current, dict)
        and current.get("id") == activation_id
        and current.get("candidate") == candidate["id"]
        and current.get("source_digest") == candidate["source_digest"]
        and current.get("committed_generation") == expected_generation,
        "activation status did not retain the committed generation/source identity",
        status,
    )
    _expect(
        memory["active_cooked_accounted_bytes"] > 0,
        "committed activation has no active cooked backing",
        status,
    )
    return command, status


def _asset_activation_verify_live_state(
    server: _JsonlProjectServer,
    request_prefix: str,
    *,
    session: str,
    expected_handle: dict[str, Any],
    expected_source_digest: str,
    expected_generation: str,
    previous_frame: dict[str, Any],
    require_new_frame: bool,
    timeout: float,
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    info = _asset_preparation_project_info(
        server,
        timeout=timeout,
        request_prefix=f"{request_prefix}-project-info",
    )
    _expect(
        info.get("session") == session
        and info.get("source_digest") == expected_source_digest
        and info.get("tick") == "1"
        and info.get("entity_count") == 1,
        "project_info did not reflect the active source while retaining authority tick/entity count",
        info,
    )
    transform = _asset_activation_get_transform(
        server,
        f"{request_prefix}-transform",
        timeout=timeout,
    )
    entity = transform.get("entity")
    _expect(
        transform.get("observed_tick") == "1"
        and isinstance(entity, dict)
        and entity.get("stable_id") == ENTITY_ID
        and entity.get("handle") == expected_handle
        and entity.get("revision") == "1"
        and entity.get("transform") == TRANSFORM_AFTER_COMMAND,
        "project source replacement changed the live entity handle, transform, or revision",
        transform,
    )
    frame = _v2_latest_frame(
        server,
        timeout=timeout,
        request_prefix=f"{request_prefix}-latest-frame",
    )
    _expect(
        frame.get("session") == session
        and frame.get("tick") == "1"
        and frame.get("project_generation") == expected_generation
        and frame.get("source_digest") == expected_source_digest
        and isinstance(frame.get("source_scene_revision"), str)
        and frame["source_scene_revision"].isdecimal()
        and frame.get("source_scene_revision") == previous_frame.get("source_scene_revision")
        and frame.get("authority_snapshot_sequence") == previous_frame.get("authority_snapshot_sequence"),
        "published frame did not pair the active source with the preserved scene revision and authority snapshot",
        frame,
    )
    if require_new_frame:
        _expect(
            isinstance(frame.get("sequence"), str)
            and frame["sequence"].isdecimal()
            and isinstance(previous_frame.get("sequence"), str)
            and previous_frame["sequence"].isdecimal()
            and int(frame["sequence"]) > int(previous_frame["sequence"]),
            "commit did not publish a newer frame sequence",
            {"before": previous_frame, "after": frame},
        )
    else:
        _expect(
            frame.get("sequence") == previous_frame.get("sequence"),
            "replayed/no-effect activation published another frame",
            {"before": previous_frame, "after": frame},
        )
    return info, transform, frame


def _run_asset_activation_smoke(
    binary: Path,
    *,
    root: Path,
    cwd: Path,
    timeout: float,
) -> dict[str, Any]:
    option_rejections = _asset_activation_option_rejections(
        binary,
        root=root,
        cwd=cwd,
        timeout=timeout,
    )
    project = root / "offline-asset-activation-project"
    _write_asset_preparation_view_project(project)
    command = (
        "project",
        "serve",
        os.fspath(project),
        "--protocol",
        "2",
        "--asset-activation",
    )
    server = _JsonlProjectServer(
        binary,
        project,
        cwd=cwd,
        timeout=timeout,
        command=command,
        protocol_version=VIEW_PROTOCOL_VERSION,
        process_label="headless project serve --asset-activation",
    )
    checks = [item["check"] for item in option_rejections]
    with server:
        initial_info = _asset_preparation_project_info(
            server,
            timeout=timeout,
            request_prefix="asset-activation-project-info-initial",
        )
        session = initial_info.get("session")
        active_source_digest = initial_info.get("source_digest")
        source_files = initial_info.get("source_files")
        _expect(
            isinstance(session, str)
            and session.isdecimal()
            and isinstance(active_source_digest, str)
            and active_source_digest
            and initial_info.get("entity_count") == 1,
            "activation server did not load the authored project/entity identity",
            initial_info,
        )
        _expect(
            isinstance(source_files, list)
            and {item.get("source_id") for item in source_files if isinstance(item, dict)}
            == set(ASSET_PREPARATION_SOURCE_PATHS),
            "activation fixture did not expose exactly its authored source set",
            source_files,
        )
        status, initial_status_result = _asset_activation_status(
            server,
            "asset-activation-status-initial",
            timeout=timeout,
        )
        memory = _asset_activation_validate_status(
            status,
            session=session,
            active_generation="1",
            active_source_digest=active_source_digest,
        )
        initial_source_digest = active_source_digest
        _expect(
            status.get("state") == "idle" and status.get("current_activation") is None,
            "activation-enabled serve did not start with an idle activation ledger",
            status,
        )
        checks.append("asset_activation_one_headless_actor_starts_on_four_format_source")

        next_command_id = 930001
        issued_command_ids: list[dict[str, str]] = []

        def issue_command_id(label: str) -> str:
            nonlocal next_command_id
            command_id = str(next_command_id)
            next_command_id += 1
            issued_command_ids.append({"label": label, "command_id": command_id})
            return command_id

        authority_state, entity_handle, authority_step = _asset_activation_apply_authority_transform(
            server,
            session=session,
            command_id=issue_command_id("authority-transform"),
            timeout=timeout,
        )
        authority_frame = _v2_latest_frame(
            server,
            timeout=timeout,
            request_prefix="asset-activation-frame-after-authority-step",
        )
        _expect(
            authority_frame.get("session") == session
            and authority_frame.get("tick") == "1"
            and authority_frame.get("authority_snapshot_sequence", "0").isdecimal()
            and authority_frame.get("project_generation") == "1"
            and authority_frame.get("source_digest") == active_source_digest,
            "authority Step frame omitted the active source/tick identity",
            authority_frame,
        )
        checks.append("asset_activation_live_transform_applies_before_source_commit")

        commits: list[dict[str, Any]] = []
        next_generation = 1

        def prepare_and_commit(label: str) -> None:
            nonlocal active_source_digest, next_generation, memory, status, authority_frame
            prepare_command_id = issue_command_id(f"{label}-prepare")
            stage_command_id = issue_command_id(f"{label}-stage")
            commit_command_id = issue_command_id(f"{label}-commit")
            _, _, candidate = _asset_activation_prepare_candidate(
                server,
                f"asset-activation-{label}",
                command_id=prepare_command_id,
                project=project,
                session=session,
                timeout=timeout,
            )
            _expect(
                candidate.get("source_digest") != active_source_digest,
                f"{label} edit did not produce a distinct project source identity",
                candidate,
            )
            candidate_status, _ = _asset_activation_status(
                server,
                f"asset-activation-{label}-active-before-stage",
                timeout=timeout,
            )
            _asset_activation_validate_status(
                candidate_status,
                session=session,
                active_generation=str(next_generation),
                active_source_digest=active_source_digest,
            )
            activation_id, stage_command, staged_status = _asset_activation_stage_candidate(
                server,
                f"asset-activation-{label}",
                command_id=stage_command_id,
                candidate=candidate,
                session=session,
                expected_active_generation=str(next_generation),
                active_source_digest=active_source_digest,
                timeout=timeout,
            )
            commit_command, commit_status = _asset_activation_commit_candidate(
                server,
                f"asset-activation-{label}-commit",
                activation_id=activation_id,
                candidate=candidate,
                command_id=commit_command_id,
                expected_active_generation=str(next_generation),
                session=session,
                timeout=timeout,
            )
            new_generation = str(next_generation + 1)
            info, live_transform, frame = _asset_activation_verify_live_state(
                server,
                f"asset-activation-{label}-verify",
                session=session,
                expected_handle=entity_handle,
                expected_source_digest=candidate["source_digest"],
                expected_generation=new_generation,
                previous_frame=authority_frame,
                require_new_frame=True,
                timeout=timeout,
            )
            commits.append(
                {
                    "label": label,
                    "activation": activation_id,
                    "candidate_id": candidate["id"],
                    "source_digest": candidate["source_digest"],
                    "generation": new_generation,
                    "frame_sequence": frame["sequence"],
                    "catalog_digest": candidate["catalog_digest"],
                    "prepare_command_id": prepare_command_id,
                    "stage_command_id": stage_command_id,
                    "commit_command_id": commit_command_id,
                    "commit_command": commit_command,
                }
            )
            status = commit_status
            memory = _asset_activation_validate_status(
                status,
                session=session,
                active_generation=new_generation,
                active_source_digest=candidate["source_digest"],
            )
            active_source_digest = candidate["source_digest"]
            next_generation += 1
            authority_frame = frame

        # Edit source bytes and valid authored model/atlas/audio data in sequence. Each
        # candidate is prepared, staged to actual Staged, then committed by this actor.
        (project / "images" / "base.png").write_bytes(_png_rgba(2, 2, (44, 88, 220, 255)))
        prepare_and_commit("png-edit")
        checks.append("asset_activation_png_edit_is_committed_and_preserves_authority")

        first_commit = commits[0]
        replay, _ = _asset_activation_command(
            server,
            "asset-activation-commit-replay",
            {
                "op": "asset_activation_commit",
                "activation": first_commit["activation"],
                "expected_active_generation": "1",
                "command_id": first_commit["commit_command_id"],
            },
            command_id=first_commit["commit_command_id"],
            outcome_kind="commit_applied",
            timeout=timeout,
        )
        _expect(
            replay == first_commit["commit_command"]
            and replay["outcome"].get("data", {}).get("activation") == first_commit["activation"]
            and replay["outcome"].get("data", {}).get("generation") == "2",
            "commit replay returned a different activation receipt",
            replay,
        )
        replay_status, _ = _asset_activation_status(
            server,
            "asset-activation-status-after-commit-replay",
            timeout=timeout,
        )
        _asset_activation_validate_status(
            replay_status,
            session=session,
            active_generation="2",
            active_source_digest=active_source_digest,
        )
        _, _, replay_frame = _asset_activation_verify_live_state(
            server,
            "asset-activation-replay-verify",
            session=session,
            expected_handle=entity_handle,
            expected_source_digest=active_source_digest,
            expected_generation="2",
            previous_frame=authority_frame,
            require_new_frame=False,
            timeout=timeout,
        )
        checks.append("asset_activation_commit_replay_does_not_increment_generation_or_frame")

        external_gltf = project / "models" / "external.gltf"
        model_document = json.loads(external_gltf.read_text(encoding="utf-8"))
        model_document["nodes"][0]["translation"] = [0.5, 0.0, 0.0]
        _write_json(external_gltf, model_document)
        prepare_and_commit("model-node-edit")
        checks.append("asset_activation_model_edit_commits_without_recreating_live_entity")

        atlas_path = project / "atlases" / "sprites.json"
        atlas_document = json.loads(atlas_path.read_text(encoding="utf-8"))
        atlas_document["regions"][0]["pivot"] = [0.25, 0.75]
        _write_json(atlas_path, atlas_document)
        prepare_and_commit("atlas-edit")
        checks.append("asset_activation_atlas_edit_commits_without_recreating_live_entity")

        _write_pcm16_mono(project / "audio" / "tone.wav", 8_000, (0, 1_002, -1_002, 0))
        prepare_and_commit("wav-edit")
        checks.append("asset_activation_wav_edit_commits_without_recreating_live_entity")

        # A stale expected generation is rejected before Stage side effects. The same
        # caller ID is then retried at the current generation and produces one real stage.
        (project / "images" / "base.png").write_bytes(_png_rgba(2, 2, (180, 96, 28, 255)))
        cancel_prepare_command_id = issue_command_id("cancel-candidate-prepare")
        _, _, cancel_candidate = _asset_activation_prepare_candidate(
            server,
            "asset-activation-cancel-candidate",
            command_id=cancel_prepare_command_id,
            project=project,
            session=session,
            timeout=timeout,
        )
        stage_operation = {
            "op": "asset_activation_stage",
            "candidate": cancel_candidate["id"],
            "expected_active_generation": "1",
            "command_id": issue_command_id("stale-generation-stage"),
        }
        stale_command_id = stage_operation["command_id"]
        stale_generation = _asset_activation_expect_terminal_error(
            server,
            "asset-activation-stale-generation",
            stage_operation,
            code="active_generation_mismatch",
            timeout=timeout,
        )
        current_generation = str(next_generation)
        activation_id, _, staged_status = _asset_activation_stage_candidate(
            server,
            "asset-activation-cancel-candidate-retry",
            command_id=stale_command_id,
            candidate=cancel_candidate,
            session=session,
            expected_active_generation=current_generation,
            active_source_digest=active_source_digest,
            timeout=timeout,
        )
        cancel_command_id = issue_command_id("cancel-staged-activation")
        cancel_command, _ = _asset_activation_command(
            server,
            "asset-activation-cancel-staged",
            {
                "op": "asset_activation_cancel",
                "activation": activation_id,
                "command_id": cancel_command_id,
            },
            command_id=cancel_command_id,
            outcome_kind="cancel_accepted",
            timeout=timeout,
        )
        cancel_outcome = cancel_command["outcome"].get("data")
        _expect(
            isinstance(cancel_outcome, dict) and cancel_outcome.get("activation") == activation_id,
            "Cancel receipt omitted the staged activation identity",
            cancel_command,
        )
        cancelled_status, _ = _asset_activation_wait_for_state(
            server,
            "asset-activation-cancelled",
            activation_id=activation_id,
            session=session,
            expected_state="cancelled",
            active_generation=current_generation,
            active_source_digest=active_source_digest,
            timeout=timeout,
        )
        _expect(
            cancelled_status.get("current_activation", {}).get("committed_generation") is None,
            "cancelled staged activation unexpectedly reports a committed generation",
            cancelled_status,
        )
        after_cancel, _, after_cancel_frame = _asset_activation_verify_live_state(
            server,
            "asset-activation-cancel-verify",
            session=session,
            expected_handle=entity_handle,
            expected_source_digest=active_source_digest,
            expected_generation=current_generation,
            previous_frame=authority_frame,
            require_new_frame=False,
            timeout=timeout,
        )
        checks.append("asset_activation_stale_generation_retry_and_actual_staged_cancel_preserve_active_source")

        # A validly loaded candidate with a different authored entity set must fail the
        # shared compatibility worker and leave the last committed source/frame active.
        scene_path = project / "scene.json"
        scene_document = json.loads(scene_path.read_text(encoding="utf-8"))
        scene_document["entities"][0]["id"] = "entity:activation-topology-rejected"
        _write_json(scene_path, scene_document)
        topology_prepare_command_id = issue_command_id("topology-candidate-prepare")
        topology_prepare, _, topology_candidate = _asset_activation_prepare_candidate(
            server,
            "asset-activation-topology-candidate",
            command_id=topology_prepare_command_id,
            project=project,
            session=session,
            timeout=timeout,
        )
        topology_stage_command_id = issue_command_id("topology-stage")
        topology_stage, _ = _asset_activation_command(
            server,
            "asset-activation-topology-stage",
            {
                "op": "asset_activation_stage",
                "candidate": topology_candidate["id"],
                "expected_active_generation": current_generation,
                "command_id": topology_stage_command_id,
            },
            command_id=topology_stage_command_id,
            outcome_kind="stage_accepted",
            timeout=timeout,
        )
        topology_accepted = topology_stage["outcome"].get("data")
        _expect(
            isinstance(topology_accepted, dict)
            and topology_accepted.get("candidate") == topology_candidate["id"],
            "topology Stage receipt did not identify its prepared candidate",
            topology_stage,
        )
        topology_activation = _asset_activation_validate_id(
            topology_accepted.get("activation"),
            session=session,
            sequence_field="sequence",
            context="topology Stage activation ID",
        )
        failed_status, _ = _asset_activation_wait_for_state(
            server,
            "asset-activation-topology-failed",
            activation_id=topology_activation,
            session=session,
            expected_state="failed",
            active_generation=current_generation,
            active_source_digest=active_source_digest,
            timeout=timeout,
        )
        failure = failed_status.get("latest_failure")
        _expect(
            isinstance(failure, dict) and failure.get("code") == "incompatible_topology",
            "topology-changing candidate did not produce a bounded compatibility failure",
            failed_status,
        )
        _expect(
            topology_candidate["source_digest"] != active_source_digest
            and topology_stage["outcome"].get("kind") == "stage_accepted"
            and topology_prepare.get("command_id") == topology_prepare_command_id,
            "topology rejection did not occur after a real prepared/staged candidate",
            {"prepare": topology_prepare, "stage": topology_stage, "candidate": topology_candidate},
        )
        _, _, final_frame = _asset_activation_verify_live_state(
            server,
            "asset-activation-topology-last-good",
            session=session,
            expected_handle=entity_handle,
            expected_source_digest=active_source_digest,
            expected_generation=current_generation,
            previous_frame=after_cancel_frame,
            require_new_frame=False,
            timeout=timeout,
        )
        checks.append("asset_activation_incompatible_topology_keeps_last_good_source_and_frame")

        final_status, final_status_result = _asset_activation_status(
            server,
            "asset-activation-status-final",
            timeout=timeout,
        )
        memory = _asset_activation_validate_status(
            final_status,
            session=session,
            active_generation=current_generation,
            active_source_digest=active_source_digest,
        )
        exit_response = server.request(
            "asset-activation-close",
            {"op": "close"},
            allow_error=True,
        )
        _expect(exit_response.get("ok") is True, "activation serve did not accept bounded close", exit_response)
        _expect(server.wait_for_exit(timeout=timeout) == 0, "activation serve did not exit cleanly", exit_response)
        shutdown_diagnostic = server._server_diagnostic()
        _expect(
            "asset_activation_owner=stopped" in shutdown_diagnostic,
            "activation owner did not report a fully stopped worker after close",
            shutdown_diagnostic,
        )
        checks.append("asset_activation_owner_reports_stopped_after_bounded_close")

    issued_values = [int(entry["command_id"]) for entry in issued_command_ids]
    _expect(
        all(previous < current for previous, current in zip(issued_values, issued_values[1:])),
        "activation mutation command IDs did not advance in one session-wide sequence",
        issued_command_ids,
    )
    return {
        "scope": "headless_cpu_asset_source_activation",
        "option_rejections": option_rejections,
        "session": session,
        "authored_source_files": source_files,
        "initial_source_digest": initial_source_digest,
        "final_active_source_digest": active_source_digest,
        "initial_generation": "1",
        "final_generation": current_generation,
        "live_entity_handle": entity_handle,
        "authority_tick": authority_state["observed_tick"],
        "authority_step": authority_step,
        "authority_snapshot_sequence": authority_frame["authority_snapshot_sequence"],
        "command_id_trace": issued_command_ids,
        "reused_command_ids": {
            "first_commit_replay": first_commit["commit_command_id"],
            "stale_generation_retry": stale_command_id,
        },
        "commits": commits,
        "commit_replay": replay,
        "stale_generation_error": stale_generation["terminal"]["error"],
        "cancelled_activation": activation_id,
        "cancel_status": cancelled_status,
        "topology_candidate_source_digest": topology_candidate["source_digest"],
        "topology_failure": failure,
        "last_good_frame": final_frame,
        "memory": memory,
        "final_status": final_status_result["data"]["data"],
        "cancel_candidate_staged_status": staged_status,
        "pre_topology_info_after_cancel": after_cancel,
        "checks": checks,
    }


def _asset_activation_option_rejections(
    binary: Path,
    *,
    root: Path,
    cwd: Path,
    timeout: float,
) -> list[dict[str, Any]]:
    cases = (
        (
            "asset_activation_protocol_one_rejected_before_project_load",
            root / "missing-activation-protocol-one-project",
            ["--protocol", "1", "--asset-activation"],
            "--clock, --audio, --asset-preparation, and --asset-activation require `project serve --protocol 2`",
        ),
        (
            "asset_activation_offline_audio_rejected_before_project_load",
            root / "missing-activation-audio-project",
            ["--protocol", "2", "--audio", "offline", "--asset-activation"],
            "--asset-activation cannot be combined with --audio",
        ),
    )
    results: list[dict[str, Any]] = []
    for check, project, options, expected_error in cases:
        _expect(not project.exists(), "activation option rejection fixture unexpectedly exists", project)
        command = ["project", "serve", os.fspath(project), *options]
        result = _run_cli(
            binary,
            command,
            cwd=cwd,
            timeout=timeout,
            expect_success=False,
        )
        expected_stderr = f'Error: "{expected_error}"'
        _expect(
            result.stderr.strip() == expected_stderr,
            "activation option was rejected with an unexpected diagnostic",
            {"expected": expected_stderr, "actual": result.stderr},
        )
        _expect(
            not result.stdout.strip(),
            "activation option rejection wrote stdout before starting the service",
            result.stdout,
        )
        cache = project / ".daedalus"
        _expect(
            not project.exists() and not cache.exists(),
            "activation option rejection created a project or project cache before validation",
            {"project": project, "cache": cache},
        )
        results.append(
            {
                "check": check,
                "exit_code": result.returncode,
                "diagnostic": expected_error,
                "stderr": result.stderr.strip(),
                "project_created": project.exists(),
                "cache_created": cache.exists(),
            }
        )
    return results


def _run_asset_preparation_view_smoke(
    binary: Path,
    *,
    root: Path,
    cwd: Path,
    timeout: float,
    audio: bool = False,
) -> dict[str, Any]:
    project = root / "offline-asset-preparation-view-project"
    _write_asset_preparation_view_project(project)
    validation_args = ["project", "validate", os.fspath(project)]
    validated = _json_output(_run_cli(binary, validation_args, cwd=cwd, timeout=timeout), validation_args)
    _expect(
        validated.get("valid") is True
        and validated.get("entity_count") == 1
        and validated.get("asset_count") == 6,
        "asset-preparation native-view fixture did not validate with all four asset formats",
        validated,
    )
    source_digest = validated.get("source_digest")
    _expect(isinstance(source_digest, str) and source_digest, "asset-preparation view fixture omitted source identity", validated)
    session_capture: dict[str, Any] = {}

    def prepare_in_live_view(server: _JsonlProjectServer, session: str) -> None:
        initial, initial_result = _asset_preparation_status(
            server,
            "asset-preparation-view-status-initial",
            timeout=timeout,
        )
        active_cooked_bytes = _asset_preparation_validate_identity(
            initial,
            session=session,
            active_source_digest=source_digest,
            active_cooked_accounted_bytes=None,
        )
        _expect(initial.get("state") == "idle", "native view preparation service did not start idle", initial)
        audio_composition = None
        if audio:
            audio_composition = _asset_preparation_audio_composition(
                server,
                "asset-preparation-view-audio",
                session=session,
                active_source_digest=source_digest,
                active_cooked_accounted_bytes=active_cooked_bytes,
                timeout=timeout,
            )
        accepted, _ = _asset_preparation_command(
            server,
            "asset-preparation-view-prepare",
            {"op": "asset_preparation_prepare", "command_id": "920001"},
            command_id="920001",
            outcome_kind="prepare_accepted",
            timeout=timeout,
        )
        accepted_id = accepted["outcome"].get("data", {}).get("candidate")
        _expect(isinstance(accepted_id, dict), "native view Prepare receipt omitted candidate identity", accepted)
        ready_status, ready_result = _asset_preparation_wait_for_candidate(
            server,
            "asset-preparation-view",
            accepted_candidate=accepted_id,
            project=project,
            session=session,
            active_source_digest=source_digest,
            active_cooked_accounted_bytes=active_cooked_bytes,
            entity_count=1,
            timeout=timeout,
        )
        candidate = ready_status["ready_candidate"]
        _expect(
            candidate.get("source_digest") == source_digest
            and candidate.get("id", {}).get("session") == session,
            "native view candidate did not describe the same initial source/session",
            candidate,
        )
        session_capture.update(
            {
                "initial_status": initial_result,
                "same_actor_audio": audio_composition,
                "prepare_receipt": accepted,
                "ready_status": ready_result,
                "candidate": candidate,
                "active_generation": ready_status["active_generation"],
                "active_source_digest": ready_status["active_source_digest"],
            }
        )

    report = _run_native_project_view(
        binary,
        project,
        cwd=cwd,
        root=root,
        timeout=timeout,
        expected_source_digest=source_digest,
        on_ready=prepare_in_live_view,
        asset_preparation=True,
        audio=audio,
    )
    candidate = session_capture.get("candidate")
    presented = report.get("last_presented")
    _expect(
        isinstance(candidate, dict)
        and isinstance(presented, dict)
        and report.get("session") == candidate.get("id", {}).get("session")
        and report.get("source_digest") == candidate.get("source_digest")
        and presented.get("session") == candidate.get("id", {}).get("session")
        and presented.get("source_digest") == candidate.get("source_digest"),
        "CPU-prepared candidate and actual presented view did not share source/session identity",
        {"candidate": candidate, "report": report},
    )
    return {
        "invocation_kind": "same_session_asset_preparation_and_native_presentation",
        "source_digest": source_digest,
        "candidate": candidate,
        "preparation": session_capture,
        "native_view_report": report,
    }


def _v2_audio_result(
    server: _JsonlProjectServer,
    request_id: str,
    operation: dict[str, Any],
    *,
    expected_kind: str,
    timeout: float | None = None,
) -> tuple[dict[str, Any], dict[str, Any]]:
    result_timeout = server.timeout if timeout is None else min(timeout, server.timeout)
    ticket, reused = _v2_submit(server, request_id, operation, timeout=result_timeout)
    _expect(not reused, f"new audio request {request_id!r} unexpectedly reused a ticket")
    poll = _v2_poll_until(
        server,
        ticket,
        request_prefix=f"{request_id}-poll",
        phase="terminal",
        timeout=result_timeout,
    )
    terminal_result = _v2_terminal_result(poll, context=request_id)
    _expect(terminal_result.get("kind") == "audio", "audio operation returned another result kind", terminal_result)
    audio_result = terminal_result.get("data")
    _expect(isinstance(audio_result, dict), "audio operation returned no typed data", terminal_result)
    _expect(
        audio_result.get("kind") == expected_kind,
        f"audio operation returned {audio_result.get('kind')!r}, expected {expected_kind!r}",
        audio_result,
    )
    data = audio_result.get("data")
    _expect(isinstance(data, dict), "audio operation returned no result payload", audio_result)
    _v2_ack(server, f"{request_id}-ack", ticket, timeout=result_timeout)
    return data, terminal_result


def _v2_audio_capabilities(server: _JsonlProjectServer, *, request_id: str) -> dict[str, Any]:
    capabilities = _v2_result(
        server,
        request_id,
        {"op": "capabilities"},
        kind="capabilities",
    )
    _expect(capabilities.get("audio_enabled") is True, "requested project audio backend is not enabled", capabilities)
    operations = capabilities.get("operations")
    _expect(
        isinstance(operations, list) and "audio_command" in operations,
        "audio-enabled session omitted audio_command capability",
        capabilities,
    )
    return capabilities


def _v2_audio_command(
    server: _JsonlProjectServer,
    request_id: str,
    command_id: str,
    mutation: dict[str, Any],
) -> tuple[dict[str, Any], dict[str, Any]]:
    return _v2_audio_result(
        server,
        request_id,
        {"op": "audio_command", "command_id": command_id, "mutation": mutation},
        expected_kind="command",
    )


def _run_audio_project_smoke(
    binary: Path,
    *,
    cwd: Path,
    root: Path,
    timeout: float,
) -> dict[str, Any]:
    project = root / "authored-offline-audio-project"
    _write_audio_project(project)
    command = [
        "project",
        "serve",
        os.fspath(project),
        "--protocol",
        "2",
        "--clock",
        "manual",
        "--audio",
        "offline",
    ]
    checks: list[str] = []
    with _JsonlProjectServer(
        binary,
        project,
        cwd=cwd,
        timeout=timeout,
        command=command,
        protocol_version=VIEW_PROTOCOL_VERSION,
        process_label="offline audio project serve",
    ) as server:
        capabilities = _v2_audio_capabilities(server, request_id="offline-audio-capabilities")
        status, _ = _v2_audio_result(
            server,
            "offline-audio-status",
            {"op": "audio_status"},
            expected_kind="status",
        )
        session = status.get("session")
        backend = status.get("backend")
        _expect(isinstance(session, str) and session.isdigit(), "audio status omitted decimal session identity", status)
        _expect(
            isinstance(backend, dict)
            and backend.get("kind") == "offline"
            and backend.get("data", {}).get("sample_rate") == 48_000,
            "project did not select the requested 48 kHz offline backend",
            status,
        )
        _expect(
            status.get("alias_count") == 2
            and status.get("unique_clip_count") == 1
            and status.get("converted_clip_count") == 1,
            "two aliases of the imported WAV were not prepared as one shared clip",
            status,
        )
        checks.append("offline_imported_wav_bank_and_backend")

        clips, _ = _v2_audio_result(
            server,
            "offline-audio-clips",
            {"op": "audio_clips", "offset": 0, "limit": 8},
            expected_kind="clips",
        )
        clip_rows = clips.get("clips")
        _expect(
            isinstance(clip_rows, list)
            and [row.get("asset_id") for row in clip_rows] == ["asset:smoke-tone", "asset:smoke-tone-alias"]
            and all(row.get("channels") == 1 and row.get("frames") == "2000" and row.get("sample_rate") == 8_000 for row in clip_rows),
            "audio bank page did not expose both imported aliases and their PCM source shape",
            clips,
        )
        checks.append("offline_imported_wav_alias_page")

        play_operation = {
            "op": "audio_command",
            "command_id": "71001",
            "mutation": {
                "kind": "play",
                "clip": "asset:smoke-tone",
                "volume": 1.0,
                "pan": -1.0,
                "looping": True,
            },
        }
        played, played_terminal = _v2_audio_result(
            server,
            "offline-audio-play",
            play_operation,
            expected_kind="command",
        )
        _expect(played.get("command_id") == "71001", "Play returned another command ID", played)
        play_outcome = played.get("outcome")
        _expect(
            isinstance(play_outcome, dict) and play_outcome.get("kind") == "played",
            "imported WAV Play did not return a typed Played outcome",
            played,
        )
        voice = play_outcome.get("data", {}).get("voice")
        _expect(
            isinstance(voice, dict)
            and voice.get("session") == session
            and isinstance(voice.get("sequence"), str)
            and voice["sequence"].isdigit(),
            "Play returned an invalid session-scoped decimal voice ID",
            played,
        )

        duplicate_play, duplicate_play_terminal = _v2_audio_result(
            server,
            "offline-audio-play-replay",
            play_operation,
            expected_kind="command",
        )
        _expect(
            duplicate_play == played and duplicate_play_terminal == played_terminal,
            "replayed Play did not return the original typed result after ticket acknowledgement",
            {"first": played, "replay": duplicate_play},
        )
        voices, _ = _v2_audio_result(
            server,
            "offline-audio-voices",
            {"op": "audio_voices", "offset": 0, "limit": 8},
            expected_kind="voices",
        )
        voice_rows = voices.get("voices")
        _expect(
            isinstance(voice_rows, list)
            and len(voice_rows) == 1
            and voice_rows[0].get("voice") == voice
            and voice_rows[0].get("state") == "playing"
            and voice_rows[0].get("looping") is True,
            "duplicate Play created another voice or lost the requested loop state",
            voices,
        )
        checks.append("offline_play_replay_after_ack_is_idempotent")

        render_operation = {
            "op": "audio_command",
            "command_id": "71002",
            "mutation": {"kind": "render_offline", "frames": 1024},
        }
        rendered, rendered_terminal = _v2_audio_result(
            server,
            "offline-audio-render",
            render_operation,
            expected_kind="command",
        )
        summary = rendered.get("outcome", {}).get("data")
        _expect(
            rendered.get("command_id") == "71002"
            and rendered.get("outcome", {}).get("kind") == "offline"
            and isinstance(summary, dict)
            and summary.get("frames") == 1024
            and summary.get("sample_rate") == 48_000
            and summary.get("channels") == 2
            and summary.get("cumulative_frames") == "1024"
            and type(summary.get("left_peak")) in (int, float)
            and summary["left_peak"] > 0.0
            and type(summary.get("right_peak")) in (int, float)
            and summary["right_peak"] < 1.0e-6
            and summary.get("nonzero_samples", 0) > 0,
            "RenderOffline did not report the imported WAV in the hard-left stereo mix",
            rendered,
        )
        duplicate_render, duplicate_render_terminal = _v2_audio_result(
            server,
            "offline-audio-render-replay",
            render_operation,
            expected_kind="command",
        )
        _expect(
            duplicate_render == rendered and duplicate_render_terminal == rendered_terminal,
            "replayed RenderOffline advanced the mixer or changed its original summary",
            {"first": rendered, "replay": duplicate_render},
        )
        status_after = _v2_audio_result(
            server,
            "offline-audio-status-after-render",
            {"op": "audio_status"},
            expected_kind="status",
        )[0]
        _expect(
            status_after.get("offline_progressed_frames") == "1024"
            and status_after.get("active_voice_count") == 1,
            "replayed offline render advanced the mixer twice or lost its active voice",
            status_after,
        )
        checks.append("offline_render_reports_pcm_and_replay_does_not_advance_twice")

        volume_zero, _ = _v2_audio_command(
            server,
            "offline-audio-volume-zero",
            "71003",
            {"kind": "set_volume", "voice": voice, "volume": 0.0},
        )
        _expect(
            volume_zero.get("outcome", {}).get("kind") == "volume_set"
            and volume_zero.get("outcome", {}).get("data", {}).get("requested_volume") == 0.0,
            "SetVolume did not return its actual typed requested value",
            volume_zero,
        )
        volume_settle, _ = _v2_audio_command(
            server,
            "offline-audio-volume-settle",
            "71004",
            {"kind": "render_offline", "frames": 256},
        )
        _expect(
            volume_settle.get("outcome", {}).get("data", {}).get("cumulative_frames") == "1280",
            "SetVolume settle block did not advance the offline mixer by its actual frames",
            volume_settle,
        )
        silent_block, _ = _v2_audio_command(
            server,
            "offline-audio-volume-zero-measure",
            "71005",
            {"kind": "render_offline", "frames": 256},
        )
        silent_summary = silent_block.get("outcome", {}).get("data")
        _expect(
            silent_block.get("outcome", {}).get("kind") == "offline"
            and isinstance(silent_summary, dict)
            and silent_summary.get("cumulative_frames") == "1536"
            and silent_summary.get("left_peak", 1.0) < 1.0e-6
            and silent_summary.get("right_peak", 1.0) < 1.0e-6,
            "SetVolume(0) did not produce a silent measured offline block after settling",
            silent_block,
        )

        restored_volume, _ = _v2_audio_command(
            server,
            "offline-audio-volume-restore",
            "71006",
            {"kind": "set_volume", "voice": voice, "volume": 0.75},
        )
        right_pan, _ = _v2_audio_command(
            server,
            "offline-audio-pan-right",
            "71007",
            {"kind": "set_pan", "voice": voice, "pan": 1.0},
        )
        _expect(
            restored_volume.get("outcome", {}).get("kind") == "volume_set"
            and restored_volume.get("outcome", {}).get("data", {}).get("requested_volume") == 0.75
            and right_pan.get("outcome", {}).get("kind") == "pan_set"
            and right_pan.get("outcome", {}).get("data", {}).get("requested_pan") == 1.0,
            "SetVolume or SetPan did not preserve the requested audio controls",
            {"volume": restored_volume, "pan": right_pan},
        )
        pan_settle, _ = _v2_audio_command(
            server,
            "offline-audio-pan-settle",
            "71008",
            {"kind": "render_offline", "frames": 256},
        )
        _expect(
            pan_settle.get("outcome", {}).get("data", {}).get("cumulative_frames") == "1792",
            "SetPan settle block did not advance the offline mixer",
            pan_settle,
        )
        right_block, _ = _v2_audio_command(
            server,
            "offline-audio-pan-right-measure",
            "71009",
            {"kind": "render_offline", "frames": 512},
        )
        right_summary = right_block.get("outcome", {}).get("data")
        _expect(
            right_block.get("outcome", {}).get("kind") == "offline"
            and isinstance(right_summary, dict)
            and right_summary.get("cumulative_frames") == "2304"
            and right_summary.get("right_peak", 0.0) > 0.0
            and right_summary.get("left_peak", 1.0) < 1.0e-6,
            "SetPan(1) did not produce a measured hard-right offline block after settling",
            right_block,
        )

        stopped, _ = _v2_audio_command(
            server,
            "offline-audio-stop",
            "71010",
            {"kind": "stop", "voice": voice},
        )
        _expect(
            stopped.get("outcome", {}).get("kind") == "stop_requested",
            "Stop did not return its typed stop-request outcome",
            stopped,
        )
        stop_render, _ = _v2_audio_command(
            server,
            "offline-audio-stop-render",
            "71011",
            {"kind": "render_offline", "frames": 128},
        )
        ended_voice, _ = _v2_audio_result(
            server,
            "offline-audio-ended-voice",
            {"op": "audio_voice", "voice": voice},
            expected_kind="voice",
        )
        stopped_status, _ = _v2_audio_result(
            server,
            "offline-audio-status-after-stop",
            {"op": "audio_status"},
            expected_kind="status",
        )
        _expect(
            stop_render.get("outcome", {}).get("data", {}).get("cumulative_frames") == "2432"
            and ended_voice.get("voice") == voice
            and ended_voice.get("state") == "ended"
            and stopped_status.get("active_voice_count") == 0,
            "Stop plus a real offline render did not reap the ended voice",
            {"stop_render": stop_render, "voice": ended_voice, "status": stopped_status},
        )
        checks.append("offline_volume_pan_stop_and_voice_reap")

        failed_play_operation = {
            "op": "audio_command",
            "command_id": "71012",
            "mutation": {
                "kind": "play",
                "clip": "asset:missing-smoke-tone",
                "volume": 1.0,
                "pan": 0.0,
                "looping": False,
            },
        }
        rejected, rejected_terminal = _v2_audio_result(
            server,
            "offline-audio-typed-failure",
            failed_play_operation,
            expected_kind="command",
        )
        _expect(
            rejected.get("command_id") == "71012"
            and rejected.get("outcome", {}).get("kind") == "error"
            and rejected.get("outcome", {}).get("data", {}).get("code") == "unknown_clip",
            "accepted invalid Play did not preserve its explicit typed failure",
            rejected,
        )
        replayed_rejection, replayed_rejection_terminal = _v2_audio_result(
            server,
            "offline-audio-typed-failure-replay",
            failed_play_operation,
            expected_kind="command",
        )
        _expect(
            replayed_rejection == rejected and replayed_rejection_terminal == rejected_terminal,
            "typed audio failure replay changed or retried the original command",
            {"first": rejected, "replay": replayed_rejection},
        )
        checks.append("offline_typed_command_failure_replays_after_ack")

    return {
        "invocation_kind": "headless_protocol2_manual_offline",
        "concurrent_with_native_window": False,
        "project_name": "offline-audio-smoke",
        "capabilities": capabilities,
        "status": status,
        "clip_page": clips,
        "played": played,
        "duplicate_play": duplicate_play,
        "voices": voices,
        "offline_render": rendered,
        "volume_zero_render": silent_block,
        "pan_right_render": right_block,
        "stop_render": stop_render,
        "ended_voice": ended_voice,
        "status_after_stop": stopped_status,
        "typed_failure": rejected,
        "status_after_render": status_after,
        "checks": checks,
    }


def _run_native_audio_smoke(
    binary: Path,
    *,
    cwd: Path,
    root: Path,
    timeout: float,
) -> dict[str, Any]:
    project = root / "authored-native-audio-project"
    _write_audio_project(project)
    command = [
        "project",
        "serve",
        os.fspath(project),
        "--protocol",
        "2",
        "--clock",
        "manual",
        "--audio",
        "native",
    ]
    started = time.monotonic()
    with _JsonlProjectServer(
        binary,
        project,
        cwd=cwd,
        timeout=timeout,
        command=command,
        protocol_version=VIEW_PROTOCOL_VERSION,
        process_label="native audio project serve",
    ) as server:
        capabilities = _v2_audio_capabilities(server, request_id="native-audio-capabilities")
        status, _ = _v2_audio_result(
            server,
            "native-audio-status",
            {"op": "audio_status"},
            expected_kind="status",
        )
        _expect(
            status.get("backend", {}).get("kind") == "native"
            and status.get("alias_count") == 2
            and status.get("unique_clip_count") == 1,
            "native audio session did not open the imported project bank/backend",
            status,
        )
        played, _ = _v2_audio_result(
            server,
            "native-audio-play",
            {
                "op": "audio_command",
                "command_id": "72001",
                "mutation": {
                    "kind": "play",
                    "clip": "asset:smoke-tone",
                    "volume": 1.0,
                    "pan": 0.0,
                    "looping": False,
                },
            },
            expected_kind="command",
        )
        _expect(
            played.get("outcome", {}).get("kind") == "played",
            "native imported-WAV Play did not reach the selected audio backend",
            played,
        )
        voice = played.get("outcome", {}).get("data", {}).get("voice")
        _expect(
            isinstance(voice, dict)
            and voice.get("session") == status.get("session")
            and isinstance(voice.get("sequence"), str),
            "native Play returned an invalid session voice identity",
            played,
        )

        completion_timeout = min(timeout, 10.0)
        deadline = time.monotonic() + completion_timeout
        attempts = 0
        last_voice: dict[str, Any] | None = None
        while time.monotonic() < deadline and attempts < int(completion_timeout * 20) + 1:
            attempts += 1
            remaining = max(0.01, deadline - time.monotonic())
            last_voice, _ = _v2_audio_result(
                server,
                f"native-audio-voice-{attempts}",
                {"op": "audio_voice", "voice": voice},
                expected_kind="voice",
                timeout=min(0.5, remaining),
            )
            if last_voice.get("state") == "ended":
                break
            time.sleep(min(0.05, max(0.0, deadline - time.monotonic())))
        _expect(
            isinstance(last_voice, dict) and last_voice.get("state") == "ended",
            "finite imported native voice did not reach backend-observed completion before the deadline",
            {"status": status, "played": played, "last_voice": last_voice, "attempts": attempts},
        )
        _expect(last_voice.get("voice") == voice, "native completion query returned another voice", last_voice)

    return {
        "invocation_kind": "headless_protocol2_manual_native",
        "concurrent_with_native_window": False,
        "capabilities": capabilities,
        "backend": status.get("backend"),
        "session": status.get("session"),
        "played": played,
        "voice_completion": last_voice,
        "completion_poll_count": attempts,
        "completion_wait_seconds": round(time.monotonic() - started, 6),
        "evidence": "finite voice reached backend-reported ended state; no speaker loopback measurement",
    }


def _v2_latest_frame(
    server: _JsonlProjectServer,
    *,
    timeout: float,
    request_prefix: str = "view-latest-frame",
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    for attempt in range(int(timeout * 100) + 1):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError("timed out waiting for latest_frame")
        response = server.request(
            f"{request_prefix}-{attempt}",
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
    _expect(
        report.get("input_source") == "protocol",
        "automated native project view did not report protocol-only input routing",
        report,
    )
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
    render_view = presented.get("render_view")
    window_render_view = window.get("final_render_view")
    _expect(
        isinstance(render_view, dict)
        and isinstance(render_view.get("camera"), dict)
        and isinstance(render_view.get("projection"), dict)
        and isinstance(window_render_view, dict)
        and isinstance(window_render_view.get("camera"), dict)
        and isinstance(window_render_view.get("projection"), dict),
        "native report omitted the actual presented/final render view",
        {"last_presented": presented, "window": window},
    )
    _expect(
        presented["camera"] == render_view["camera"]
        and window.get("final_camera") == window_render_view["camera"],
        "legacy camera fields differ from the camera in the actual render view",
        {"last_presented": presented, "window": window},
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
            "the single authored draw geometry was not uploaded exactly once",
            resources,
        )
        _expect(
            type(window.get("rendered_frames")) is int and window["rendered_frames"] >= 1,
            "native window did not report a rendered frame",
            window,
        )
    return report


def _project_view_command(
    project: Path,
    report: Path,
    *,
    seconds: int,
    input_source: str = "protocol",
    asset_preparation: bool = False,
    asset_activation: bool = False,
    audio: bool = False,
) -> list[str]:
    if input_source not in {"native", "protocol"}:
        raise SmokeError("project view input_source must be 'native' or 'protocol'")
    command = [
        "project",
        "view",
        os.fspath(project),
        "--stdio-control",
        "--input-source",
        input_source,
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
    if audio:
        command.extend(["--audio", "offline"])
    if asset_preparation:
        command.append("--asset-preparation")
    if asset_activation:
        command.append("--asset-activation")
    return command


_HOST_CPU_STATUS_FIELDS = (
    "generation_slots_used",
    "generation_slots_limit",
    "frame_slots_used",
    "frame_slots_limit",
    "prepared_metadata_bytes",
    "prepared_metadata_reserved_bytes",
    "legacy_mesh_copy_bytes",
    "legacy_mesh_copy_reserved_bytes",
    "frame_bytes",
    "frame_reserved_bytes",
    "scratch_current_bytes",
    "scratch_peak_bytes",
)


def _validate_managed_view_status(
    status: dict[str, Any],
    *,
    expected_session: str | None = None,
) -> dict[str, Any]:
    expected_fields = {
        "lifecycle",
        "session",
        "view_started",
        "view_closed",
        "active_generation",
        "active_source_digest",
        "queued_publication",
        "last_presented",
        "cpu",
        "failure",
        "live_capture",
    }
    _expect(
        isinstance(status, dict) and set(status) == expected_fields,
        "managed view_status has an unexpected or missing field",
        status,
    )
    session = status.get("session")
    generation = status.get("active_generation")
    digest = status.get("active_source_digest")
    _expect(
        isinstance(session, str)
        and session.isdecimal()
        and int(session) > 0
        and (expected_session is None or session == expected_session)
        and isinstance(generation, str)
        and generation.isdecimal()
        and int(generation) > 0
        and isinstance(digest, str)
        and digest,
        "managed view_status has invalid session or active source identity",
        status,
    )
    _expect(
        status.get("lifecycle")
        in {
            "initializing",
            "initial_preparing",
            "cpu_prepared",
            "ready",
            "view_running",
            "view_closed",
            "stopping",
            "stopped",
            "failed",
        }
        and type(status.get("view_started")) is bool
        and type(status.get("view_closed")) is bool
        and status.get("view_closed") <= status.get("view_started")
        and status.get("live_capture") is False,
        "managed view_status has invalid lifecycle or capture metadata",
        status,
    )
    cpu = status.get("cpu")
    _expect(
        isinstance(cpu, dict)
        and set(cpu) == set(_HOST_CPU_STATUS_FIELDS)
        and all(type(cpu.get(field)) is int and cpu[field] >= 0 for field in _HOST_CPU_STATUS_FIELDS),
        "managed view_status has invalid CPU logical accounting",
        cpu,
    )

    queued = status.get("queued_publication")
    if queued is not None:
        _validate_host_publication_identity(queued, session=session)
    presented = status.get("last_presented")
    if presented is not None:
        _validate_host_presented_summary(presented, session=session)
    failure = status.get("failure")
    _expect(
        failure is None
        or (
            isinstance(failure, dict)
            and set(failure) == {"code", "message"}
            and isinstance(failure.get("code"), str)
            and failure["code"]
            and len(failure["code"].encode("utf-8")) <= 64
            and isinstance(failure.get("message"), str)
            and len(failure["message"].encode("utf-8")) <= 2_048
        ),
        "managed view_status has invalid bounded failure metadata",
        failure,
    )
    return status


def _validate_host_publication_identity(value: Any, *, session: str) -> dict[str, Any]:
    fields = {
        "session",
        "tick",
        "sequence",
        "authority_snapshot_sequence",
        "project_generation",
        "source_digest",
        "scene_revision",
    }
    _expect(
        isinstance(value, dict)
        and set(value) == fields
        and value.get("session") == session
        and all(
            isinstance(value.get(field), str) and value[field].isdecimal()
            for field in ("tick", "sequence", "authority_snapshot_sequence", "project_generation", "scene_revision")
        )
        and all(int(value[field]) > 0 for field in ("sequence", "project_generation"))
        and isinstance(value.get("source_digest"), str)
        and value["source_digest"],
        "managed queued publication has invalid identity metadata",
        value,
    )
    return value


def _validate_host_presented_summary(value: Any, *, session: str) -> dict[str, Any]:
    fields = {
        "session",
        "tick",
        "sequence",
        "authority_snapshot_sequence",
        "project_generation",
        "source_digest",
        "present_sequence",
        "physical_width",
        "physical_height",
        "render_view",
        "view_projection",
        "actual_view_available",
    }
    _expect(
        isinstance(value, dict)
        and set(value) == fields
        and value.get("session") == session
        and all(
            isinstance(value.get(field), str) and value[field].isdecimal()
            for field in ("tick", "sequence", "project_generation", "present_sequence")
        )
        and (
            value.get("authority_snapshot_sequence") is None
            or (
                isinstance(value.get("authority_snapshot_sequence"), str)
                and value["authority_snapshot_sequence"].isdecimal()
            )
        )
        and int(value["sequence"]) > 0
        and int(value["project_generation"]) > 0
        and int(value["present_sequence"]) > 0
        and isinstance(value.get("source_digest"), str)
        and value["source_digest"]
        and type(value.get("physical_width")) is int
        and value["physical_width"] > 0
        and type(value.get("physical_height")) is int
        and value["physical_height"] > 0
        and value.get("render_view") is None
        and value.get("view_projection") is None
        and value.get("actual_view_available") is False,
        "managed last-presented summary has invalid identity or compact-view metadata",
        value,
    )
    return value


def _managed_view_status(
    server: _JsonlProjectServer,
    request_id: str,
    *,
    timeout: float,
    expected_session: str | None = None,
) -> dict[str, Any] | None:
    response = server.request(
        request_id,
        {"op": "view_status"},
        timeout=timeout,
        allow_error=True,
    )
    if response.get("ok") is False:
        error = response.get("error")
        if isinstance(error, dict) and error.get("code") == "view_status_busy":
            _expect(error.get("retryable") is True, "view_status_busy is not marked retryable", response)
            return None
        raise SmokeError(f"managed view_status failed: {json.dumps(response, sort_keys=True)}")
    result = response.get("result")
    _expect(
        isinstance(result, dict) and result.get("kind") == "view_status",
        "direct view_status returned another result kind",
        response,
    )
    data = result.get("data")
    _expect(isinstance(data, dict), "direct view_status omitted bounded status data", result)
    return _validate_managed_view_status(data, expected_session=expected_session)


def _wait_managed_view_status(
    server: _JsonlProjectServer,
    request_prefix: str,
    predicate: Callable[[dict[str, Any]], bool],
    *,
    timeout: float,
    expected_session: str,
) -> dict[str, Any]:
    duration = min(timeout, server.timeout)
    deadline = time.monotonic() + duration
    for attempt in range(int(duration * 100) + 1):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        status = _managed_view_status(
            server,
            f"{request_prefix}-{attempt}",
            timeout=remaining,
            expected_session=expected_session,
        )
        if status is not None:
            if status["lifecycle"] in {"failed", "stopped"}:
                raise SmokeError(
                    f"managed view reached {status['lifecycle']} before {request_prefix}: "
                    f"{json.dumps(status, sort_keys=True)}"
                )
            if predicate(status):
                return status
        remaining = deadline - time.monotonic()
        if remaining > 0:
            time.sleep(min(0.01, remaining))
    raise SmokeError(f"timed out waiting for managed view status condition {request_prefix!r}")


def _run_native_project_view(
    binary: Path,
    project: Path,
    *,
    cwd: Path,
    root: Path,
    timeout: float,
    expected_source_digest: str,
    on_ready: Callable[[_JsonlProjectServer, str], None] | None = None,
    expected_entity_count: int = 1,
    authority_transform: dict[str, Any] = TRANSFORM_AFTER_COMMAND,
    asset_preparation: bool = False,
    audio: bool = False,
) -> dict[str, Any]:
    report_path = root / "native-view-report.json"
    command = _project_view_command(
        project,
        report_path,
        seconds=NATIVE_VIEW_SECONDS,
        asset_preparation=asset_preparation,
        audio=audio,
    )
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
        _expect(
            info.get("entity_count") == expected_entity_count,
            "native project view reported an unexpected authored entity count",
            info,
        )
        _v2_ack(server, "view-info-ack", info_ticket)

        if on_ready is not None:
            on_ready(server, session)

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
                "transform": authority_transform,
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
            and final_state.get("entity", {}).get("transform") == authority_transform,
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


def _run_project_packet_capture(
    binary: Path,
    project: Path,
    *,
    cwd: Path,
    root: Path,
    timeout: float,
    expected_source_digest: str,
    image_filename: str,
    expected_projection: dict[str, Any],
    asset_label: str,
) -> dict[str, Any]:
    image_path = root / image_filename
    command = [
        "project",
        "capture",
        os.fspath(project),
        "--output",
        os.fspath(image_path),
        "--width",
        str(NATIVE_VIEW_WIDTH),
        "--height",
        str(NATIVE_VIEW_HEIGHT),
        "--ticks",
        "1",
    ]
    output = _json_output(_run_cli(binary, command, cwd=cwd, timeout=timeout), command)
    _expect(output.get("command") == "project_capture", "project capture returned another command", output)
    _expect(
        isinstance(output.get("session"), str) and output["session"],
        "project capture omitted its new session identity",
        output,
    )
    _expect(
        output.get("authority_tick") == 1
        and output.get("snapshot_tick") == 1
        and output.get("source_digest") == expected_source_digest,
        "project capture identity/tick differs from its authored project",
        output,
    )
    projection = output.get("projection")
    _expect(isinstance(projection, dict), "project capture omitted packet projection counters", output)
    _expect(
        projection.get("object_count") == 1
        and isinstance(projection.get("packet_expanded_gpu_geometry_bytes"), int)
        and projection["packet_expanded_gpu_geometry_bytes"] > 0
        and projection.get("expanded_geometry_bytes_basis")
        == "per_draw_packed_geometry_plus_legacy_object_record",
        "project capture did not report one packed draw with the compatibility basis",
        projection,
    )
    resources = output.get("resources")
    _expect(
        isinstance(resources, dict)
        and resources.get("geometry_upload_count") == 1
        and isinstance(resources.get("geometry_upload_bytes"), int)
        and resources["geometry_upload_bytes"] > 0,
        "project capture did not upload its geometry exactly once",
        resources,
    )
    asset_resources = output.get("asset_resources")
    _expect(isinstance(asset_resources, dict), "project capture omitted imported asset GPU counters", output)
    _expect(
        asset_resources.get("texture_upload_count") == 1
        and isinstance(asset_resources.get("texture_upload_bytes"), int)
        and asset_resources["texture_upload_bytes"] > 0,
        f"project capture did not upload the {asset_label} texture",
        asset_resources,
    )
    _expect(image_path.is_file(), "project capture did not write a PNG artifact", image_path)
    image_bytes = image_path.stat().st_size
    _expect(image_bytes > 64, "project capture wrote an unexpectedly small PNG artifact", image_bytes)
    with image_path.open("rb") as image_file:
        _expect(
            image_file.read(8) == b"\x89PNG\r\n\x1a\n",
            "project capture output is not a PNG file",
            image_path,
        )
    capture = output.get("capture")
    _expect(
        isinstance(capture, dict)
        and capture.get("path") == os.fspath(image_path)
        and capture.get("width") == NATIVE_VIEW_WIDTH
        and capture.get("height") == NATIVE_VIEW_HEIGHT
        and capture.get("rgba8_bytes") == NATIVE_VIEW_WIDTH * NATIVE_VIEW_HEIGHT * 4,
        "project capture returned unexpected image dimensions",
        output,
    )
    render_view = capture.get("render_view")
    _expect(
        isinstance(render_view, dict)
        and isinstance(render_view.get("camera"), dict)
        and render_view.get("projection") == expected_projection,
        "project capture did not report the projection used by its rendered packet",
        capture,
    )
    return {
        "report": output,
        "image_artifact": {"format": "PNG", "byte_len": image_bytes},
    }


def _run_model_project_capture(
    binary: Path,
    project: Path,
    *,
    cwd: Path,
    root: Path,
    timeout: float,
    expected_source_digest: str,
) -> dict[str, Any]:
    return _run_project_packet_capture(
        binary,
        project,
        cwd=cwd,
        root=root,
        timeout=timeout,
        expected_source_digest=expected_source_digest,
        image_filename="textured-model-capture.png",
        expected_projection={"kind": "perspective"},
        asset_label="base-color",
    )


def _run_sprite_project_capture(
    binary: Path,
    project: Path,
    *,
    cwd: Path,
    root: Path,
    timeout: float,
    expected_source_digest: str,
) -> dict[str, Any]:
    return _run_project_packet_capture(
        binary,
        project,
        cwd=cwd,
        root=root,
        timeout=timeout,
        expected_source_digest=expected_source_digest,
        image_filename="orthographic-sprite-capture.png",
        expected_projection={"kind": "orthographic", "vertical_size": 10.0},
        asset_label="sprite-atlas",
    )


def _run_model_project_smoke(
    binary: Path,
    *,
    cwd: Path,
    root: Path,
    timeout: float,
) -> dict[str, Any]:
    project = root / "authored-textured-model-project"
    _write_model_view_project(project)
    validate_args = ["project", "validate", os.fspath(project)]
    validated = _json_output(_run_cli(binary, validate_args, cwd=cwd, timeout=timeout), validate_args)
    _expect(validated.get("valid") is True, "textured model project did not validate", validated)
    _expect(
        validated.get("entity_count") == 1 and validated.get("asset_count", 0) >= 1,
        "textured model project did not contain its authored entity/assets",
        validated,
    )
    source_digest = validated.get("source_digest")
    _expect(isinstance(source_digest, str) and source_digest, "model validation omitted source identity", validated)

    capture_during_view: dict[str, Any] = {}

    def capture_when_view_is_ready(server: _JsonlProjectServer, view_session: str) -> None:
        capture_during_view["result"] = _run_model_project_capture(
            binary,
            project,
            cwd=cwd,
            root=root,
            timeout=timeout,
            expected_source_digest=source_digest,
        )
        _expect(
            server.process.poll() is None,
            "native model viewer exited while its concurrent batch capture was running",
            {"view_session": view_session, "capture": capture_during_view["result"]},
        )
        capture_during_view["view_session"] = view_session

    view = _run_native_project_view(
        binary,
        project,
        cwd=cwd,
        root=root,
        timeout=timeout,
        expected_source_digest=source_digest,
        on_ready=capture_when_view_is_ready,
    )
    capture = capture_during_view.get("result")
    _expect(
        isinstance(capture, dict),
        "native model view did not run its concurrent batch capture after project_info",
        capture_during_view,
    )
    view_asset_resources = view.get("asset_resources")
    _expect(
        isinstance(view_asset_resources, dict)
        and view_asset_resources.get("texture_upload_count") == 1
        and isinstance(view_asset_resources.get("texture_upload_bytes"), int)
        and view_asset_resources["texture_upload_bytes"] > 0,
        "native model view did not upload its referenced base-color texture",
        view,
    )
    capture_session = capture["report"].get("session")
    view_session = view.get("session")
    _expect(
        isinstance(capture_session, str)
        and capture_session
        and isinstance(view_session, str)
        and view_session,
        "batch capture and native view must each report their own session identity",
        {"capture": capture["report"], "view": view},
    )
    return {
        "source_digest": source_digest,
        "capture": {
            "invocation_kind": "batch_capture_process_while_native_window_active",
            "concurrent_with_native_view": True,
            "native_view_session": capture_during_view["view_session"],
            **capture,
        },
        "native_view": {
            "invocation_kind": "native_window_process",
            "report": view,
        },
    }


def _run_sprite_project_smoke(
    binary: Path,
    *,
    cwd: Path,
    root: Path,
    timeout: float,
) -> dict[str, Any]:
    project = root / "authored-orthographic-sprite-project"
    _write_sprite_view_project(project)
    validate_args = ["project", "validate", os.fspath(project)]
    validated = _json_output(_run_cli(binary, validate_args, cwd=cwd, timeout=timeout), validate_args)
    _expect(validated.get("valid") is True, "orthographic sprite project did not validate", validated)
    _expect(
        validated.get("entity_count") == 2 and validated.get("asset_count") == 2,
        "orthographic sprite project must contain its camera, sprite, atlas, and image",
        validated,
    )
    source_digest = validated.get("source_digest")
    _expect(isinstance(source_digest, str) and source_digest, "sprite validation omitted source identity", validated)

    capture_during_view: dict[str, Any] = {}

    def capture_when_view_is_ready(server: _JsonlProjectServer, view_session: str) -> None:
        capture_during_view["result"] = _run_sprite_project_capture(
            binary,
            project,
            cwd=cwd,
            root=root,
            timeout=timeout,
            expected_source_digest=source_digest,
        )
        _expect(
            server.process.poll() is None,
            "native sprite viewer exited while its concurrent batch capture was running",
            {"view_session": view_session, "capture": capture_during_view["result"]},
        )
        capture_during_view["view_session"] = view_session

    view = _run_native_project_view(
        binary,
        project,
        cwd=cwd,
        root=root,
        timeout=timeout,
        expected_source_digest=source_digest,
        on_ready=capture_when_view_is_ready,
        expected_entity_count=2,
        authority_transform=SPRITE_CAMERA_TRANSFORM_AFTER_COMMAND,
    )
    capture = capture_during_view.get("result")
    _expect(
        isinstance(capture, dict),
        "native sprite view did not capture while the viewer was active",
        capture_during_view,
    )
    capture_view = capture["report"]["capture"]["render_view"]
    _expect(
        capture_view["camera"].get("position") == [0.0, 0.0, 0.0]
        and capture_view.get("projection") == {"kind": "orthographic", "vertical_size": 10.0},
        "batch sprite capture did not use the authored tick-zero orthographic camera",
        capture_view,
    )

    presented = view.get("last_presented")
    presented_view = presented.get("render_view") if isinstance(presented, dict) else None
    camera = presented_view.get("camera") if isinstance(presented_view, dict) else None
    expected_projection = {"kind": "orthographic", "vertical_size": 10.0}
    _expect(
        isinstance(camera, dict)
        and camera.get("position") == SPRITE_CAMERA_TRANSFORM_AFTER_COMMAND["translation"]
        and camera.get("target") == [0.25, 0.5, -1.0]
        and camera.get("up") == [0.0, 1.0, 0.0]
        and type(camera.get("vertical_fov_radians")) in (int, float)
        and math.isfinite(camera["vertical_fov_radians"])
        and camera.get("near") == struct.unpack("<f", struct.pack("<f", 0.1))[0]
        and camera.get("far") == 100.0
        and isinstance(presented_view, dict)
        and presented_view.get("projection") == expected_projection
        and view.get("window", {}).get("final_render_view") == presented_view,
        "presented and final window views did not follow the authority camera without local input",
        {"last_presented": presented, "window": view.get("window")},
    )
    geometry_resources = view.get("resources")
    # The viewer may coalesce the initial tick-zero packet before its first
    # surface draw. This smoke verifies initialized globals and the actual
    # final presented camera; the ordered GPU regression
    # asset_backpressure_preserves_ready_packet_and_does_not_count_busy_candidate_as_drawn
    # verifies the exact camera-refresh write delta.
    _expect(
        isinstance(geometry_resources, dict)
        and geometry_resources.get("geometry_upload_count") == 1
        and type(geometry_resources.get("global_write_count")) is int
        and geometry_resources["global_write_count"] >= 1,
        "sprite window did not initialize camera globals while reusing one unit quad",
        geometry_resources,
    )
    texture_resources = view.get("asset_resources")
    _expect(
        isinstance(texture_resources, dict)
        and texture_resources.get("texture_upload_count") == 1,
        "sprite window did not upload its atlas image exactly once",
        texture_resources,
    )
    return {
        "source_digest": source_digest,
        "capture": {
            "invocation_kind": "batch_capture_process_while_native_window_active",
            "concurrent_with_native_view": True,
            "native_view_session": capture_during_view["view_session"],
            **capture,
        },
        "native_view": {
            "invocation_kind": "native_window_process",
            "authority_camera_transform": SPRITE_CAMERA_TRANSFORM_AFTER_COMMAND,
            "report": view,
        },
    }


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


def _validate_native_activation_view_report(
    report: dict[str, Any],
    *,
    session: str,
    source_digest: str,
    generation: str,
    tick: str,
    status_presented: dict[str, Any],
) -> dict[str, Any]:
    _expect(
        report.get("command") == "project_view"
        and report.get("report_version") == 2
        and report.get("presentation_mode") == "managed_activation",
        "activation view wrote another report contract",
        report,
    )
    _expect(
        report.get("input_source") == "protocol",
        "automated activation view did not report protocol-only input routing",
        report,
    )
    _expect(
        "accepted_packet" not in report
        and report.get("session") == session
        and report.get("source_digest") == source_digest
        and report.get("source_entity_count") == 3
        and report.get("authority_final_tick") == tick
        and report.get("host_shutdown") == "stopped",
        "activation report did not retain the committed host/session state",
        report,
    )
    queued = report.get("queued_publication")
    _validate_host_publication_identity(queued, session=session)
    _expect(
        queued.get("project_generation") == generation
        and queued.get("source_digest") == source_digest
        and int(queued["sequence"]) >= int(status_presented["sequence"]),
        "activation report queued identity differs from the committed source/presentation",
        {"queued_publication": queued, "status_presented": status_presented},
    )
    presented = report.get("last_presented")
    presented_sequence = presented.get("present_sequence") if isinstance(presented, dict) else None
    status_present_sequence = status_presented.get("present_sequence")
    _expect(
        isinstance(presented, dict)
        and presented.get("session") == session
        and presented.get("tick") == tick
        and presented.get("project_generation") == generation
        and presented.get("source_digest") == source_digest
        and presented.get("sequence") == status_presented.get("sequence")
        and isinstance(presented_sequence, str)
        and presented_sequence.isdecimal()
        and isinstance(status_present_sequence, str)
        and status_present_sequence.isdecimal()
        and int(presented_sequence) >= int(status_present_sequence)
        and type(presented.get("physical_width")) is int
        and presented["physical_width"] > 0
        and type(presented.get("physical_height")) is int
        and presented["physical_height"] > 0
        and presented.get("actual_view_available") is True,
        "activation report did not match the live actual-present status",
        {"report_presented": presented, "view_status_presented": status_presented},
    )
    projection = presented.get("render_view", {}).get("projection")
    _expect(
        isinstance(projection, dict)
        and projection.get("kind") == "orthographic"
        and projection.get("vertical_size") == 10.0,
        "managed activation present did not use the authored orthographic camera",
        presented,
    )
    view_projection = presented.get("view_projection")
    _expect(
        isinstance(view_projection, list)
        and len(view_projection) == 16
        and all(type(value) in (int, float) and math.isfinite(value) for value in view_projection),
        "managed activation report omitted the finite actual view-projection matrix",
        presented,
    )
    window = report.get("window")
    _expect(
        isinstance(window, dict)
        and type(window.get("rendered_frames")) is int
        and window["rendered_frames"] > 0,
        "managed activation window did not report any presented surface frames",
        window,
    )
    _expect(
        isinstance(report.get("resources"), dict)
        and isinstance(report.get("asset_resources"), dict)
        and isinstance(report.get("coordinator"), dict)
        and report["coordinator"].get("protocol_detach_reason") is None,
        "managed activation report omitted native resource or coordinator evidence",
        report,
    )
    host_status = _validate_managed_view_status(report.get("host"), expected_session=session)
    final_presented = report["last_presented"]
    _expect(
        host_status["active_generation"] == generation
        and host_status["active_source_digest"] == source_digest
        and isinstance(host_status.get("last_presented"), dict)
        and host_status["last_presented"].get("sequence") == final_presented.get("sequence")
        and host_status["last_presented"].get("present_sequence")
        == final_presented.get("present_sequence"),
        "final report host snapshot differs from live direct view_status",
        {"report_host": host_status, "report_presented": final_presented},
    )
    return report


def _run_native_asset_activation_view(
    binary: Path,
    *,
    root: Path,
    cwd: Path,
    timeout: float,
) -> dict[str, Any]:
    project = root / "managed-activation-view-project"
    _write_managed_activation_view_project(project)
    validate_args = ["project", "validate", os.fspath(project)]
    validated = _json_output(_run_cli(binary, validate_args, cwd=cwd, timeout=timeout), validate_args)
    _expect(
        validated.get("valid") is True
        and validated.get("entity_count") == 3
        and validated.get("asset_count") == 6,
        "managed activation fixture did not validate with its model, orthographic camera, sprite, and four formats",
        validated,
    )
    initial_digest = validated.get("source_digest")
    _expect(isinstance(initial_digest, str) and initial_digest, "managed activation fixture has no source digest", validated)

    report_path = root / "managed-activation-view-report.json"
    command = _project_view_command(
        project,
        report_path,
        seconds=NATIVE_VIEW_SECONDS,
        asset_activation=True,
    )
    server = _JsonlProjectServer(
        binary,
        project,
        cwd=cwd,
        timeout=timeout,
        command=command,
        protocol_version=VIEW_PROTOCOL_VERSION,
        process_label="project view --asset-activation",
    )
    with server:
        capabilities = server.request("managed-view-capabilities", {"op": "capabilities"})
        cap_data = capabilities.get("result", {}).get("data")
        _expect(
            isinstance(cap_data, dict)
            and cap_data.get("view_status") is True
            and cap_data.get("asset_activation_enabled") is True
            and "view_status" not in cap_data.get("operations", []),
            "managed activation view did not expose only its direct view_status capability",
            capabilities,
        )

        info = _asset_preparation_project_info(
            server,
            timeout=timeout,
            request_prefix="managed-activation-project-info-initial",
        )
        session = info.get("session")
        _expect(
            isinstance(session, str)
            and session.isdecimal()
            and int(session) > 0
            and info.get("source_digest") == initial_digest
            and info.get("entity_count") == 3
            and {item.get("source_id") for item in info.get("source_files", []) if isinstance(item, dict)}
            == set(ASSET_PREPARATION_SOURCE_PATHS),
            "managed activation host did not load the same complete four-format project",
            info,
        )
        initial_activation_status, _ = _asset_activation_status(
            server,
            "managed-activation-status-initial",
            timeout=timeout,
        )
        _asset_activation_validate_status(
            initial_activation_status,
            session=session,
            active_generation="1",
            active_source_digest=initial_digest,
        )
        _expect(
            initial_activation_status.get("state") == "idle",
            "managed activation host did not start with an idle replacement ledger",
            initial_activation_status,
        )

        initial_view_status = _wait_managed_view_status(
            server,
            "managed-view-initial-present",
            lambda value: value["view_started"]
            and isinstance(value["last_presented"], dict)
            and value["last_presented"].get("project_generation") == "1"
            and value["last_presented"].get("source_digest") == initial_digest,
            timeout=timeout,
            expected_session=session,
        )
        initial_presented = initial_view_status["last_presented"]
        _expect(
            initial_view_status["lifecycle"] == "view_running"
            and initial_presented["tick"] == "0",
            "initial managed view did not present generation one at tick zero",
            initial_view_status,
        )

        authored_transform = {
            "translation": [-1.5, 0.0, -3.0],
            "rotation": [0.0, 0.0, 0.0, 1.0],
            "scale": [1.0, 1.0, 1.0],
        }
        live_transform = {
            "translation": [-1.0, 0.25, -3.0],
            "rotation": [0.0, 0.0, 0.0, 1.0],
            "scale": [1.1, 1.0, 1.0],
        }
        authority_state, entity_handle, authority_step = _asset_activation_apply_authority_transform(
            server,
            session=session,
            command_id="930001",
            timeout=timeout,
            initial_transform=authored_transform,
            target_transform=live_transform,
        )
        authority_view_status = _wait_managed_view_status(
            server,
            "managed-view-authority-present",
            lambda value: isinstance(value["last_presented"], dict)
            and value["last_presented"].get("project_generation") == "1"
            and value["last_presented"].get("source_digest") == initial_digest
            and value["last_presented"].get("tick") == "1"
            and int(value["last_presented"]["sequence"]) > int(initial_presented["sequence"]),
            timeout=timeout,
            expected_session=session,
        )
        authority_presented = authority_view_status["last_presented"]
        authority_frame = _v2_latest_frame(
            server,
            timeout=timeout,
            request_prefix="managed-activation-frame-after-authority-step",
        )
        _expect(
            authority_frame.get("session") == session
            and authority_frame.get("tick") == "1"
            and authority_frame.get("project_generation") == "1"
            and authority_frame.get("source_digest") == initial_digest
            and authority_frame.get("authority_snapshot_sequence")
            == authority_presented.get("authority_snapshot_sequence"),
            "native host authority frame and actual present diverged before replacement",
            {"authority_frame": authority_frame, "view_status": authority_view_status},
        )

        # Change source image pixels, model node placement, atlas pixels, and sprite region
        # metadata together. The same host then prepares, device-stages, and commits this
        # candidate while its existing model pose, orthographic camera, and session stay live.
        (project / "images" / "base.png").write_bytes(_png_rgba(2, 2, (44, 88, 220, 255)))
        model_path = project / "models" / "external.gltf"
        model_document = json.loads(model_path.read_text(encoding="utf-8"))
        model_document["nodes"][0]["translation"] = [0.2, 0.0, 0.0]
        _write_json(model_path, model_document)
        changed_atlas_colors = (
            (18, 32, 224, 255),
            (240, 160, 24, 255),
            (90, 18, 210, 160),
            (20, 220, 90, 0),
        )
        changed_pixels = bytearray()
        for y in range(16):
            for x in range(16):
                quadrant = (0 if y < 8 else 2) + (0 if x < 8 else 1)
                changed_pixels.extend(changed_atlas_colors[quadrant])
        (project / "images" / "atlas.png").write_bytes(
            _png_rgba_pixels(16, 16, bytes(changed_pixels))
        )
        atlas_path = project / "atlases" / "sprites.json"
        atlas_document = json.loads(atlas_path.read_text(encoding="utf-8"))
        atlas_document["regions"][0]["pivot"] = [0.25, 0.75]
        _write_json(atlas_path, atlas_document)

        _, preparation_status, candidate = _asset_activation_prepare_candidate(
            server,
            "managed-activation-source-change",
            command_id="930002",
            project=project,
            session=session,
            timeout=timeout,
            entity_count=3,
        )
        candidate_digest = candidate.get("source_digest")
        _expect(
            isinstance(candidate_digest, str)
            and candidate_digest
            and candidate_digest != initial_digest
            and preparation_status.get("ready_candidate", {}).get("source_digest") == candidate_digest,
            "image/model/sprite source edits did not produce one ready candidate identity",
            candidate,
        )
        active_before_stage, _ = _asset_activation_status(
            server,
            "managed-activation-active-before-stage",
            timeout=timeout,
        )
        _asset_activation_validate_status(
            active_before_stage,
            session=session,
            active_generation="1",
            active_source_digest=initial_digest,
        )
        activation_id, stage_command, staged_status = _asset_activation_stage_candidate(
            server,
            "managed-activation-source-change",
            command_id="930003",
            candidate=candidate,
            session=session,
            expected_active_generation="1",
            active_source_digest=initial_digest,
            timeout=timeout,
        )
        commit_command, commit_status = _asset_activation_commit_candidate(
            server,
            "managed-activation-source-commit",
            activation_id=activation_id,
            candidate=candidate,
            command_id="930004",
            expected_active_generation="1",
            session=session,
            timeout=timeout,
        )
        _expect(
            staged_status.get("state") == "staged"
            and stage_command.get("outcome", {}).get("kind") == "stage_accepted"
            and commit_command.get("outcome", {}).get("kind") == "commit_applied"
            and authority_step.get("tick") == "1"
            and authority_state.get("observed_tick") == "1",
            "replacement did not pass device-ready Stage and Commit after the authority pose was applied",
            {"stage": stage_command, "commit": commit_command},
        )

        committed_view_status = _wait_managed_view_status(
            server,
            "managed-view-committed-present",
            lambda value: isinstance(value["queued_publication"], dict)
            and value["queued_publication"].get("project_generation") == "2"
            and value["queued_publication"].get("source_digest") == candidate_digest
            and isinstance(value["last_presented"], dict)
            and value["last_presented"].get("project_generation") == "2"
            and value["last_presented"].get("source_digest") == candidate_digest
            and value["last_presented"].get("tick") == "1"
            and int(value["last_presented"]["sequence"]) > int(authority_presented["sequence"]),
            timeout=timeout,
            expected_session=session,
        )
        committed_presented = committed_view_status["last_presented"]
        _expect(
            int(committed_view_status["queued_publication"]["sequence"])
            >= int(committed_presented["sequence"]),
            "managed last queued publication regressed behind the actual presented identity",
            committed_view_status,
        )

        retirement_status: dict[str, Any] | None = None
        retirement_memory: dict[str, Any] | None = None
        deadline = time.monotonic() + min(timeout, server.timeout)
        for attempt in range(int(min(timeout, server.timeout) * 100) + 1):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            retirement_status, _ = _asset_activation_status(
                server,
                f"managed-activation-retirement-{attempt}",
                timeout=remaining,
            )
            retirement_memory = _asset_activation_validate_status(
                retirement_status,
                session=session,
                active_generation="2",
                active_source_digest=candidate_digest,
            )
            if (
                retirement_memory["retired_cooked_accounted_bytes"] == 0
                and retirement_memory["retired_generation_count"] == 0
            ):
                break
            remaining = deadline - time.monotonic()
            if remaining > 0:
                time.sleep(min(0.01, remaining))
        _expect(
            isinstance(retirement_memory, dict)
            and retirement_memory.get("retired_cooked_accounted_bytes") == 0
            and retirement_memory.get("retired_generation_count") == 0,
            "actual managed present did not release the previous source generation within the bounded wait",
            retirement_status,
        )

        final_info = _asset_preparation_project_info(
            server,
            timeout=timeout,
            request_prefix="managed-activation-project-info-final",
        )
        final_transform = _asset_activation_get_transform(
            server,
            "managed-activation-transform-final",
            timeout=timeout,
        )
        final_entity = final_transform.get("entity")
        latest_frame = _v2_latest_frame(
            server,
            timeout=timeout,
            request_prefix="managed-activation-latest-frame-final",
        )
        _expect(
            final_info.get("session") == session
            and final_info.get("source_digest") == candidate_digest
            and final_info.get("tick") == "1"
            and final_info.get("entity_count") == 3
            and final_transform.get("observed_tick") == "1"
            and isinstance(final_entity, dict)
            and final_entity.get("stable_id") == ENTITY_ID
            and final_entity.get("handle") == entity_handle
            and final_entity.get("revision") == "1"
            and final_entity.get("transform") == live_transform
            and latest_frame.get("session") == session
            and latest_frame.get("tick") == "1"
            and latest_frame.get("project_generation") == "2"
            and latest_frame.get("source_digest") == candidate_digest,
            "source activation changed the existing entity handle/pose/tick or failed to publish the committed source",
            {"project_info": final_info, "transform": final_transform, "latest_frame": latest_frame},
        )
        _expect(
            int(committed_presented["sequence"]) <= int(latest_frame["sequence"])
            and int(latest_frame["sequence"])
            <= int(committed_view_status["queued_publication"]["sequence"]),
            "actual presented, runtime latest, and queued publication sequence order is inconsistent",
            {
                "presented": committed_presented,
                "latest_frame": latest_frame,
                "queued": committed_view_status["queued_publication"],
            },
        )

        close = server.request("managed-activation-close", {"op": "close"}, allow_error=True)
        _expect(close.get("ok") is True, "managed activation view rejected Close", close)
        _expect(server.wait_for_exit(timeout=timeout) == 0, "managed activation view did not exit cleanly", close)

    report = _validate_native_activation_view_report(
        _read_native_report(report_path),
        session=session,
        source_digest=candidate_digest,
        generation="2",
        tick="1",
        status_presented=committed_presented,
    )
    return {
        "invocation_kind": "same_native_window_session_asset_activation",
        "session": session,
        "initial_source_digest": initial_digest,
        "committed_source_digest": candidate_digest,
        "initial_presented": initial_presented,
        "authority_presented": authority_presented,
        "candidate": candidate,
        "activation": activation_id,
        "commit_command": commit_command,
        "committed_view_status": committed_view_status,
        "retired_source_memory": retirement_memory,
        "live_entity_handle": entity_handle,
        "live_transform": live_transform,
        "latest_frame": latest_frame,
        "native_report": report,
        "pixel_capture": None,
        "pixel_capture_note": "window actual-present identity is verified; the smoke has no live-window pixel readback path",
    }


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
    models: bool = False,
    sprites: bool = False,
    audio: bool = False,
    audio_native: bool = False,
    asset_preparation: bool = False,
    asset_activation: bool = False,
) -> dict[str, Any]:
    """Exercise CPU lifecycle checks plus any explicitly requested native/model checks."""

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

        asset_preparation_results: dict[str, Any] | None = None
        if asset_preparation:
            asset_preparation_results = _run_asset_preparation_smoke(
                binary,
                root=root,
                cwd=working_directory,
                timeout=timeout,
                audio=audio,
            )
            checks.extend(asset_preparation_results["session"]["checks"])

        asset_activation_results: dict[str, Any] | None = None
        if asset_activation:
            asset_activation_results = _run_asset_activation_smoke(
                binary,
                root=root,
                cwd=working_directory,
                timeout=timeout,
            )
            checks.extend(asset_activation_results["checks"])

        audio_results: dict[str, Any] | None = None
        if audio:
            audio_results = _run_audio_project_smoke(
                binary,
                cwd=working_directory,
                root=root,
                timeout=timeout,
            )
            checks.extend(audio_results.pop("checks"))

        audio_native_results: dict[str, Any] | None = None
        if audio_native:
            audio_native_results = _run_native_audio_smoke(
                binary,
                cwd=working_directory,
                root=root,
                timeout=timeout,
            )
            checks.append("native_imported_voice_reaches_backend_completion")

        view_results: dict[str, Any] | None = None
        managed_activation_view_results: dict[str, Any] | None = None
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

            if asset_preparation:
                asset_preparation_view_results = _run_asset_preparation_view_smoke(
                    binary,
                    root=root,
                    cwd=working_directory,
                    timeout=timeout,
                    audio=audio,
                )
                checks.append("asset_preparation_active_view_and_cpu_candidate_share_session_and_source")
            else:
                asset_preparation_view_results = None

            if asset_activation:
                managed_activation_view_results = _run_native_asset_activation_view(
                    binary,
                    root=root,
                    cwd=working_directory,
                    timeout=timeout,
                )
                checks.append("managed_activation_view_presents_and_retires_committed_source_in_same_session")
        else:
            asset_preparation_view_results = None

        model_results: dict[str, Any] | None = None
        if models:
            model_results = _run_model_project_smoke(
                binary,
                cwd=working_directory,
                root=root,
                timeout=timeout,
            )
            checks.extend(
                [
                    "textured_model_project_batch_capture",
                    "textured_model_project_native_view_and_authority_control",
                ]
            )

        sprite_results: dict[str, Any] | None = None
        if sprites:
            sprite_results = _run_sprite_project_smoke(
                binary,
                cwd=working_directory,
                root=root,
                timeout=timeout,
            )
            checks.extend(
                [
                    "orthographic_sprite_project_batch_capture",
                    "orthographic_sprite_native_authority_view_and_atlas_upload",
                ]
            )

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
    if asset_preparation_results is not None:
        result["asset_preparation"] = asset_preparation_results
    if asset_activation_results is not None:
        result["asset_activation"] = asset_activation_results
    if managed_activation_view_results is not None:
        result["managed_activation_view"] = managed_activation_view_results
    if asset_preparation_view_results is not None:
        result["asset_preparation_view"] = asset_preparation_view_results
    if model_results is not None:
        result["models"] = model_results
    if sprite_results is not None:
        result["sprites"] = sprite_results
    if audio_results is not None:
        result["audio_offline"] = audio_results
    if audio_native_results is not None:
        result["audio_native"] = audio_native_results
    return result


def _parse_args(argv: Sequence[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run bounded CPU project lifecycle checks against an existing Daedalus CLI binary; --view/--models/--sprites add native GPU checks, --audio-native adds a native audio-device check, --asset-preparation exercises CPU asset preparation, and --asset-activation exercises headless CPU source replacement (combine with --view for same-window native activation)."
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
    parser.add_argument(
        "--models",
        action="store_true",
        help="also capture and open an offline textured glTF project in the native GPU viewer",
    )
    parser.add_argument(
        "--sprites",
        action="store_true",
        help="also capture and open an offline orthographic atlas-sprite project in the native GPU viewer",
    )
    parser.add_argument(
        "--audio",
        action="store_true",
        help="also import, play, and render a generated WAV through protocol-2 offline audio (no device required)",
    )
    parser.add_argument(
        "--audio-native",
        action="store_true",
        help="also open the imported finite WAV on the native audio backend and wait for backend-reported completion",
    )
    parser.add_argument(
        "--asset-preparation",
        action="store_true",
        help="also exercise protocol-2 live CPU asset-candidate preparation, reuse, edits, and cancellation",
    )
    parser.add_argument(
        "--asset-activation",
        action="store_true",
        help="also exercise protocol-2 headless CPU source replacement; combine with --view to verify same-window native Stage/Commit and actual presentation",
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
            models=arguments.models,
            sprites=arguments.sprites,
            audio=arguments.audio,
            audio_native=arguments.audio_native,
            asset_preparation=arguments.asset_preparation,
            asset_activation=arguments.asset_activation,
        )
        print(json.dumps(result, sort_keys=True))
        return 0
    except SmokeError as error:
        print(f"smoke_project: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
