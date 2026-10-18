# Daedalus

**An independent, code-first game engine being designed for demanding 2D and 3D worlds.**

Daedalus aims to combine a Rust core, responsive loading and streaming, modern GPU rendering, and authoritative dedicated servers. The selected gameplay and PC mod SDK baseline is C# 14 with .NET 10 LTS/CoreCLR. Rust gameplay can be compiled into a game build, and C++ libraries can be integrated through explicit native adapters. Development is driven through code, CLI, MCP, and a visible runtime window.

**Status: engine foundation under construction.** The Rust workspace contains a parallel ECS runtime, bounded CPU tasks and snapshots, persistent JSON CLI control, external data-only project loading, a native window connected to live ECS and background terrain work, content algorithms, an independent wgpu renderer, Jolt/Rapier physics adapters, Kira audio, and a .NET gameplay host with tested module replacement. The managed SDK currently covers transforms. Advanced rendering, full gameplay integration, web support, and performance targets remain incomplete. Game development follows engine product acceptance in separate repositories.

**Delivery stage: D1 engine implementation; product acceptance is pending.** The full objective is a usable AI-driven engine, followed by separate Minecraft-quality 3D and Terraria-quality 2D games consuming it. Project lifecycle, asset production, UI/animation, multiplayer/persistence, and game export still need integration. Component tests, the native validation window, and packaged CLI do not close that objective. See the [product acceptance contract](docs/04-plan/product-delivery-and-acceptance.md) and [readiness audit](docs/06-worklog/2026-10-16-product-readiness-audit.md). Game projects follow the engine gate and stay outside this repository.

Run the current engine CLI with `cargo run -p daedalus-cli -- inspect --entities 2`. Build and validation commands, GPU requirements, and known limits are in [TESTING.md](TESTING.md). Module progress is tracked in the [implementation checklist](docs/04-plan/implementation-checklist.md).

To open the native engine validation window:

```sh
cargo run -p daedalus-cli --features window -- view
```

Use WASD/QE and mouse controls for the camera, arrow keys to move the ECS entity, R to regenerate terrain, and Escape to close. The window connects one live simulation session to bounded background voxel generation and meshing; it is a small engine fixture. `view --help` lists the workload, automatic exit, and JSON report options. This does not yet implement an infinite world or a complete UI system. See the [live session contract](docs/03-architecture/native-live-session.md) for ownership and failure handling.

## Work with an external project

Create an empty data-only project in a new directory, then validate or run its real ECS session:

```sh
cargo run -p daedalus-cli -- project init /tmp/my-daedalus-project --name MyProject
cargo run -p daedalus-cli -- project validate /tmp/my-daedalus-project
cargo run -p daedalus-cli -- project run /tmp/my-daedalus-project --ticks 2
cargo run -p daedalus-cli -- project serve /tmp/my-daedalus-project
```

The project manifest references a versioned scene. Current runnable content is root entities with transforms, indexed JSON meshes with scalar opaque materials, static glTF/GLB models with opaque base-color PNG textures, and atlas sprites with an authority-owned orthographic camera. Models use cooked mip levels and per-primitive samplers; the first sprite subset uses nearest-filtered atlases. `project serve` retains one authority session; `project_info` exposes authored stable IDs and handles, and normal control operations query and change actual ECS state. `project capture` in a capture-enabled build renders the project's own geometry and snapshot to PNG. Initialization inserts no fixture objects.

This is the first runnable project foundation: general scene hierarchy/prefabs, attached gameplay modules, live project reload, live image capture, saving, MCP, and game export remain unfinished. The existing bare `view` window still runs its named validation fixture. See the [project contract](docs/03-architecture/project-lifecycle.md) and [verification commands](TESTING.md).

The native Rust library provides a bounded `SessionControlService` with multiple clients, owned request results, Manual/Realtime clocks, and snapshots from one real project engine. A window-enabled build opens the same project session for native viewing and optional protocol-2 JSONL control:

```sh
cargo run -p daedalus-cli --features window -- project view /path/to/project
cargo run -p daedalus-cli --features window -- project view /path/to/project --clock manual --stdio-control --report /tmp/daedalus-project-view.json
```

The [viewer contract](docs/03-architecture/project-view-control.md) describes tickets, explicit Step, bounded I/O, and shutdown. Camera controls work without a selected entity; `--controlled-entity <stable-id>` enables diagnostic arrow movement, and manual mode uses F10 for one tick. Protocol 1 remains available through `project serve`. Live image capture, MCP, and managed attach to this viewer are still pending.

For automated viewing, add `--input-source protocol`. This disables physical keyboard, pointer, and wheel controls while preserving protocol edits, Step, resize, and OS close. Both legacy and managed project views report the selected input source. Interactive viewing defaults to `--input-source native`; see the [input policy](docs/03-architecture/native-view-input-policy.md).

The renderer's additive [packet API](docs/03-architecture/renderer-resource-reuse.md) shares immutable geometry, retains device-local buffers, and updates changed instance/material rows. Small native GPU tests verify reuse and pixel parity with the original capture path. Broader rendering quality, live project reload, and complete P02/D1 acceptance remain open.

## CPU asset preparation

The asset-aware loader and CLI share bounded source resolution, immutable CPU resources, dependency identities, and a versioned disk cache:

```sh
cargo run -p daedalus-cli -- project cook /path/to/project
cargo run -p daedalus-cli -- project cook /path/to/project --cache-dir /path/to/cache
```

The first CPU subset covers static glTF/GLB triangles, PNG, versioned sprite-atlas JSON, and PCM/float WAV. The [import contract](docs/03-architecture/asset-import-foundation.md) defines settings, format limits, and unsupported features. The bounded JSON report exposes source hashes, resource keys, and per-invocation decode/cook/cache counters. Unchanged resources and geometry unaffected by image or material edits reuse their cooked identities. This command prepares CPU data and does not render or play audio; its `runtimeConsumers` flags describe that invocation. Static models and base-color textures use the same packet renderer in `project view` and `project capture`. Add `daedalus.model_renderer` with one matching `asset_refs` entry and `data.model`; the [model execution contract](docs/03-architecture/model-texture-runtime.md) defines the supported material subset and bounded resource lifetime.

For 2D, add `daedalus.sprite_renderer` with `data.atlas`, `data.region`, one matching atlas reference, optional linear RGBA tint and painter order. A project containing sprites requires one `daedalus.camera_2d` entity. The [sprite contract](docs/03-architecture/sprite-runtime-and-orthographic-view.md) defines coordinates, pivot/PPU, stable sorting, camera fields, and limits. Sprites share one quad and the model renderer's texture/resource lifetime. PNG capture preserves straight alpha; ordinary native windows are opaque. The 2D diagnostic camera uses WASD/drag to pan and the wheel to zoom. Capture and native reports include the actual camera/projection pair. Animation and tile rendering/lighting remain unfinished under the [consumer plan](docs/03-architecture/asset-runtime-consumers.md).

Imported WAV clips can be controlled through the same session's protocol 2:

```sh
cargo run -p daedalus-cli --features audio -- project serve /path/to/project --protocol 2 --clock manual --audio offline
cargo run -p daedalus-cli --features native-audio -- project serve /path/to/project --protocol 2 --clock manual --audio native
```

The [audio contract](docs/03-architecture/imported-audio-runtime.md) defines Play, Stop, volume/pan, full-clip looping, bounded voice queries, and caller-ID replay. Offline mode mixes actual imported PCM into bounded summaries without an output device. Native mode opens the actual backend and fails explicitly if unavailable. Clips are shared by cooked identity and do not auto-play. Gameplay audio components, spatial audio, compressed streaming, live bank replacement, and managed bindings remain pending.

An opt-in native worker can prepare CPU asset candidates while the same project session remains alive:

```sh
cargo run -p daedalus-cli -- project serve /path/to/project --protocol 2 --asset-preparation
```

Protocol 2 exposes `asset_preparation_prepare`, `asset_preparation_status`, and `asset_preparation_cancel` through the existing submit/poll/ack tickets. One worker owns the cache and coalesces requests to the latest candidate; status includes bounded source, memory, and cook metrics. The [preparation contract](docs/03-architecture/project-asset-preparation.md) defines cancellation, replay, and shutdown. This option prepares candidates only. Native viewing uses it together with `--stdio-control`; automatic file watching and live renderer/audio activation remain unfinished.

A native headless session can also enable CPU source activation:

```sh
cargo run -p daedalus-cli -- project serve /path/to/project --protocol 2 --asset-activation
```

This includes preparation and adds `asset_activation_status`, `asset_activation_stage`, `asset_activation_commit`, and `asset_activation_cancel` to the same ticket/replay service. A compatible candidate can replace the source while retaining the Engine, entity handles, live transforms, and authority tick. Stage acceptance precedes actual staging; clients wait for Staged before Commit. Previous CPU backing remains accounted until the worker reclaims it. The [headless activation contract](docs/03-architecture/headless-asset-generation.md) defines IDs, errors, bounds, and shutdown. This mode rejects attached audio and native viewing; successful CPU commit does not mean new pixels were displayed or an audio bank changed.

## Native source activation (experimental)

A window build can use the reusable `daedalus-project-host` library to prepare and activate compatible asset changes in the same project session:

```sh
cargo run -p daedalus-cli --features window -- project view /path/to/project --clock manual --stdio-control --asset-activation
```

Use protocol 2 preparation, Stage, and Commit operations; `view_status` reports the actual presented generation separately from queued work. The [host contract](docs/03-architecture/native-project-host.md) and [CLI contract](docs/03-architecture/native-project-view-activation.md) describe ownership, bounded work, and report version 2. A focused actual Mac run verified mixed model/sprite replacement, preserved entity state, and old-source retirement; [integration evidence](docs/06-worklog/2026-10-18-native-host-integration.md) records failures and the exact scope.

This path currently rejects attached audio. Automatic file watching, general scene reload, and live-window image capture remain unfinished; initial CPU loading still happens before the window opens. `view_status` provides metadata, and `live_capture` remains false. The new host/CLI product gate and full D1 engine acceptance remain open.

The [capture runtime foundation](docs/06-worklog/2026-10-18-live-capture-foundation.md) adds an opt-in typed handoff, one image reservation, cancellation and retained artifact/work capabilities to the same authority. CPU tests verify this ownership contract; the actual surface readback, artifact store and CLI capture operation are the next integration steps.

## Build a local CLI package

With the pinned Rust toolchain and Python 3.11 or newer, run:

```sh
python3 scripts/package_cli.py
```

This builds a native release CLI with window, GPU capture, and native/offline audio enabled, verifies the extracted executable including imported offline audio, and stores a versioned archive, build manifest, dependency notices, and checksum under ignored `dist/local/`. Run the extracted `daedalus view` to open the engine window. Each build records its source commit and whether the working tree was dirty. The initial packaging targets are macOS arm64 and Linux x64, built on their respective native hosts. Linux hosts must provide `libasound.so.2`; the archive does not bundle ALSA.

The release workflow builds packages for PR inspection. A future `v<CLI version>` tag on main history triggers GitHub Release publication after both platform builds succeed. No public release has been issued. The initial CLI archive does not bundle the C# SDK, .NET runtime, or game export tools. See the [CLI build and release contract](docs/03-architecture/cli-build-and-release.md) for boundaries.

## Direction

- Treat 2D tile simulation, 3D voxel streaming, and general engine capabilities as first-class design concerns.
- Isolate simulation, presentation, background work, and asset processing through explicit ownership and bounded communication.
- Keep loading UI and interaction responsive while long-running work progresses.
- Design for macOS and WebGPU browsers first; plan Windows, Linux, mobile, and later console ports with explicit capability and validation gates.
- Plan multiplayer and persistence from the beginning, targeting dedicated multiplayer sessions of about 30 players; activity and network budgets remain to be specified.
- Support stylized and physically based art through an extensible material and rendering architecture. Art references express intended quality, not shipped feature parity.
- Target 144 fps or higher under a defined workload and quality profile; preserve game functionality, save semantics, and scripting API across native and web exports, while allowing device-specific graphics settings.
- Design live playtesting and C# logic reload as separate capabilities. Downloadable mods focus on desktop PC first; equivalent dynamic mod loading on web or mobile is not part of that initial contract.

These are design objectives. Dependency choices and measurable quality targets are still under investigation.

## Start here

| Document | Purpose |
| --- | --- |
| [Documentation index](docs/README.md) | Read order, file map, and document status |
| [Project charter](docs/00-vision/project-charter.md) | Confirmed intent and engine-only boundaries |
| [Feature and quality scope](docs/00-vision/feature-quality-matrix.md) | What a complete engine must account for |
| [Questionnaire and answers](docs/01-discovery/answer-log.md) | User decisions and remaining uncertainties |
| [Original proposal audit](docs/02-research/claim-audit.md) | Claims requiring correction or measurement |
| [Architecture overview](docs/03-architecture/system-context.md) | Module boundaries and platform responsibilities |
| [Gameplay and mod SDK](docs/03-architecture/gameplay-and-mod-sdk.md) | C# wrapping, compiled Rust gameplay, and C++ integration |
| [Autopilot and live testing](docs/03-architecture/autopilot-and-live-testing.md) | Control a running session, verify results, and inject new C# logic |
| [Goals](docs/04-plan/goal-map.md) | Bounded research and implementation objectives |
| [Waterfall masterplan](docs/04-plan/waterfall-masterplan.md) | Deliverables, dependencies, and exit gates |

The detailed design documents are initially written in Korean for the founding maintainer. English and Korean issues and pull requests are welcome.

## Contribute and validate

Read [CONTRIBUTING.md](CONTRIBUTING.md), [GOVERNANCE.md](GOVERNANCE.md), and [TESTING.md](TESTING.md). Architectural changes begin with a documented problem and evidence. The current local documentation check is:

```sh
python3 scripts/check_docs.py
```

The project-owned AI coding workflow uses Luna with xhigh reasoning, with coordinator review. Contributors remain responsible for the changes they submit.

## License

Original engine source, tooling, and repository documentation are available under **MIT OR Apache-2.0**, at your option. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE). External libraries, assets, fonts, and proprietary platform SDKs retain their own terms; they are not relicensed by this repository.

The project is not affiliated with the games or studios mentioned as engineering or visual references.
