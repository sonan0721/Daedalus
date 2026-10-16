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

The project manifest references a versioned scene. Current runnable content is root entities with transforms and indexed JSON meshes with scalar opaque materials. `project serve` retains one authority session; `project_info` exposes authored stable IDs and handles, and normal control operations query and change actual ECS state. `project capture` in a capture-enabled build renders the project's own geometry and snapshot to PNG. Initialization inserts no fixture objects.

This is the first runnable project foundation: hierarchy/prefabs, imported model/texture/sprite execution, attached gameplay modules, live project reload, same-window control/capture, saving, MCP, and game export remain unfinished. The existing `view` window still runs its named validation fixture. See the [project contract](docs/03-architecture/project-lifecycle.md) and [verification commands](TESTING.md).

The native Rust library provides a bounded `SessionControlService` with multiple clients, owned request results, Manual/Realtime clocks, and snapshots from one real project engine. A window-enabled build opens the same project session for native viewing and optional protocol-2 JSONL control:

```sh
cargo run -p daedalus-cli --features window -- project view /path/to/project
cargo run -p daedalus-cli --features window -- project view /path/to/project --clock manual --stdio-control --report /tmp/daedalus-project-view.json
```

The [viewer contract](docs/03-architecture/project-view-control.md) describes tickets, explicit Step, bounded I/O, and shutdown. Camera controls work without a selected entity; `--controlled-entity <stable-id>` enables diagnostic arrow movement, and manual mode uses F10 for one tick. Protocol 1 remains available through `project serve`. Live image capture, MCP, and managed attach to this viewer are still pending.

The renderer's additive [packet API](docs/03-architecture/renderer-resource-reuse.md) shares immutable geometry, retains device-local buffers, and updates changed instance/material rows. Small native GPU tests verify reuse and pixel parity with the original capture path. Broader rendering quality, live project reload, and complete P02/D1 acceptance remain open.

## CPU asset preparation

The asset-aware loader and CLI share bounded source resolution, immutable CPU resources, dependency identities, and a versioned disk cache:

```sh
cargo run -p daedalus-cli -- project cook /path/to/project
cargo run -p daedalus-cli -- project cook /path/to/project --cache-dir /path/to/cache
```

The first CPU subset covers static glTF/GLB triangles, PNG, versioned sprite-atlas JSON, and PCM/float WAV. The [import contract](docs/03-architecture/asset-import-foundation.md) defines settings, format limits, and unsupported features. The bounded JSON report exposes source hashes, resource keys, and per-invocation decode/cook/cache counters. Unchanged resources and geometry unaffected by image or material edits reuse their cooked identities. This command prepares CPU data; imported models, textures, sprites, and audio still need [runtime consumers](docs/03-architecture/asset-runtime-consumers.md) before they can be used in a running project. The report explicitly marks those consumers unavailable.

## Build a local CLI package

With the pinned Rust toolchain and Python 3.11 or newer, run:

```sh
python3 scripts/package_cli.py
```

This builds a native release CLI with window and GPU capture enabled, verifies the extracted executable, and stores a versioned archive, build manifest, dependency notices, and checksum under ignored `dist/local/`. Run the extracted `daedalus view` to open the engine window. Each build records its source commit and whether the working tree was dirty. The initial packaging targets are macOS arm64 and Linux x64, built on their respective native hosts.

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
