# Testing

Daedalus has an early Rust engine workspace. Tests cover implemented contracts and small engine fixtures; they do not certify commercial game quality, 144 fps, or every planned platform. The [implementation checklist](docs/04-plan/implementation-checklist.md) separates working code from remaining integrations.

## Current checks

Run these commands from the repository root:

```sh
python3 scripts/check_docs.py
python3 -m unittest discover -s scripts -p 'test_*.py'
```

The documentation checker verifies UTF-8 Markdown, local link targets, repository path boundaries, and use of an obsolete project label. Its unit tests cover link resolution, code masking, reference links, path traversal, and encoding/name errors. GitHub Actions runs these checks in the `docs` job on pull requests and pushes to `main`. It does not validate remote URLs, heading anchors, factual claims, or the whole CommonMark grammar; those need review or additional tools when relevant.

## Rust workspace checks

The repository pins Rust 1.94.0, rustfmt, and Clippy in rust-toolchain.toml. Run:

```sh
cargo check --workspace --all-features --locked
cargo fmt --all --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

The initial tests exercise generation/session handles, stale revisions, command saturation/deduplication, fixed-clock catch-up, task cancellation/panic/shutdown, snapshot retention, scene patch rollback, greedy-mesh boundaries, fluid conservation, light removal, and bounded occupancy paths. A 100,000-entity fixture checks ECS data and handles; it is not proof of zero races or frame-time performance.

The ECS schedule now uses Bevy's parallel executor with an explicit process-global compute worker budget and a separate per-engine bounded Rayon pool. An integration test uses two compute workers to verify independent-system overlap and conflicting mutable-access serialization. Conflicting global-pool configurations return errors. Reported CPU totals are per-engine estimates, not a limit on every process, driver, physics, or audio thread. Snapshot publication and result polling use nonwaiting entry points, but the whole engine is not formally lock-free or wait-free.

## Native session-control library

`daedalus-runtime` exports `SessionControlService`, attached clients, typed operations, and owned request tickets. The native actor owns one real `ProjectSession`; Manual Step and Realtime have separate clock rules. Focused checks are:

```sh
cargo test -p daedalus-runtime session_control::tests --locked
cargo test -p daedalus-runtime session_primitives::tests --locked
cargo test -p daedalus-world --lib --locked
```

The latest local all-feature coordinator run passes 187 CPU tests with three hardware tests ignored by default. Service tests cover real authority admission versus application, multiple clients, wrong-session/stale revisions, bounded pages, reserved Step capacity under saturation, reader-held result/frame lifetimes, duplicate replay and eviction, nonwaiting contention, timeouts, shutdown, and injected actor failures. Core retention also counts an independently retained entity-array Arc or cloned snapshot value. Frame-cap-one progress and pin/release recovery use a separate producer budget; native service producer capacity must be 2–64. A controlled actor pause verifies that ACK and last-reader release free Step credit before actor bookkeeping cleanup. The first Linux PR run exposed that race; subsequent CI is recorded separately in the worklog. Unknown post-failure command outcomes are reported explicitly. The native project viewer and protocol-2 adapter now use this service; MCP and managed attach are still pending. The legacy window shares mailbox/stop primitives but retains its old count-only result guard. See the [service contract](docs/03-architecture/session-control-service.md) for exact limits and remaining migration.

## Current CLI smoke

```sh
cargo run -p daedalus-cli -- inspect --entities 2
cargo run -p daedalus-cli -- run --ticks 3 --entities 2
cargo run -p daedalus-cli -- step --ticks 2
cargo run -p daedalus-cli -- --version
cargo run -p daedalus-cli -- version --json
cargo run -p daedalus-cli -- serve
```

The first commands create bounded inspection sessions and do not initialize a GPU. `serve` keeps one real engine/session alive over stdin/stdout JSON Lines; its simulation advances through explicit `step` requests. A line has `protocol: 1`, `request_id`, and a nested `request` with a snake_case operation such as `spawn_transform`, `get_transform`, `set_transform`, `step`, `latest_snapshot`, or `exit`. Command acceptance is distinct from the applied outcomes returned by a step. Malformed/oversized input is isolated, outputs are bounded, and EOF shuts down the workers. This is local control, not the multiplayer server or an MCP implementation.

`scene validate <input>` and `scene patch <input> <patch> <output>` use the real content schema and revisioned atomic patch validation. Scene validation alone does not establish that a component has a runtime adapter; the project loader separately enforces its supported component set.

## External data-only projects

```sh
cargo run -p daedalus-cli -- project init /tmp/daedalus-empty --name Empty
cargo run -p daedalus-cli -- project validate /tmp/daedalus-empty
cargo run -p daedalus-cli -- project inspect /tmp/daedalus-empty
cargo run -p daedalus-cli -- project run /tmp/daedalus-empty --ticks 2
cargo run -p daedalus-cli -- project serve /tmp/daedalus-empty
```

`init` requires a new child directory under an existing parent and never overwrites a project. It writes an empty manifest and scene; it does not insert a cube or game. Filesystem and byte-based preparation share semantic validation, budgets, source hashes, and one immutable prepared-project model. See the [project format and support contract](docs/03-architecture/project-lifecycle.md).

For authored content, the current project loader accepts root entities, `daedalus.transform`, `daedalus.mesh_renderer`, and `daedalus.mesh` indexed JSON geometry with normals and opaque scalar materials. General model/texture/sprite import, hierarchy/prefabs, attached C# modules, live reload/save, MCP, and web export are not implemented. Unsupported data is an explicit error rather than a substituted fixture.

`project serve` uses the existing bounded JSONL control path with the same authority engine. A metadata request is:

```json
{"protocol":1,"request_id":"info","request":{"op":"project_info","offset":0,"limit":100}}
```

Use the returned handle in `get_transform` under `entity`, and in `set_transform` under `target`. Read the authority revision before submitting a change, then `step` and inspect its applied or rejected outcome and snapshot. Authored scene revision and current entity revision are different. Metadata includes source identity and the current tick; stable-ID pages omit stale copies of mutable transform state. Bare `serve` remains supported, but its `project_info` returns `project_not_loaded`.

An authored project's GPU capture uses its actual prepared geometry and the same newly created session's snapshot:

```sh
cargo run -p daedalus-cli --features capture -- project capture /path/to/authored-project --output /tmp/project.png --width 513 --height 289 --ticks 2
```

The report records source digest, authored scene revision, authority tick, snapshot tick, geometry counts, and actual adapter. A batch capture opens a new session; it is not a capture of another running `serve` or the fixture window. No adapter is an error. The 2026-10-16 coordinator check rendered an external one-triangle project with its own material on Apple M3 Pro/Metal, and independently checked real command application, stale-revision rejection, and matching snapshots. Whole-engine and game acceptance remain open.

Reuse the CPU project smoke with an already-built binary:

```sh
cargo build -p daedalus-cli --locked
python3 scripts/smoke_project.py --cli target/debug/daedalus
```

The helper does not build or initialize a GPU. It creates and removes external temporary projects, derives handles from the active session, and checks empty initialization/no-overwrite, shared load identity, accepted versus applied commands, actual transforms/revisions/snapshots, stale-command rejection, and unsupported-component diagnostics. Processes and JSONL responses have finite timeouts; malformed, oversized, and stalled responses are failure cases with child cleanup tests. The success JSON includes the executable hash, 18 checks, and observed wall time. It does not certify gameplay, live-window behavior, or product acceptance. Native archive verification reuses this same helper on the extracted executable.

An optional GPU CLI build is explicit:

```sh
cargo run -p daedalus-cli --features capture -- capture --output /tmp/daedalus-capture.png --width 320 --height 180
```

This captures the renderer's named material validation fixture, not a game or a general scene importer. The coordinator's recorded GPU adapter is Apple M3 Pro/Metal; no-adapter is an error.

## Real GPU smoke

The external project viewer uses the same authority service as its optional protocol-2 adapter:

```sh
cargo test -p daedalus-cli --features window project_view_control::tests --locked
cargo run -p daedalus-cli --features window -- project view /path/to/project --stdio-control --clock manual --report /tmp/project-view.json
python3 scripts/smoke_project.py --cli target/debug/daedalus --view
```

The first command runs CPU protocol tests without opening a window. The last command requires an already-built window-enabled CLI, a real GPU, and a desktop. It reuses the default 18 CPU checks, then verifies a native manual-clock edit/Step/query against the report's actually presented packet and a separate graceful Close with stdin still open. The coordinator's 2026-10-16 run passed all 20 checks on Apple M3 Pro/Metal. It does not certify live capture, MCP, loading UI, blocked OS stdout, or the P02 product gate. Default smoke and package verification remain CPU-only. The [viewer contract](docs/03-architecture/project-view-control.md) describes string-encoded IDs, owned submit/poll/ack/forget tickets, bounded I/O, and best-effort Close receipts.

The GPU test is ignored in the default suite so CPU CI does not silently depend on a graphics adapter. Run it explicitly:

```sh
cargo test -p daedalus-renderer --features test-fixtures --test gpu_capture -- --ignored --nocapture
```

It renders a small material/depth fixture through a real wgpu adapter, reads back 513×289 pixels with padded rows, writes a PNG in the system temporary directory, and decodes the pixels for validation. `DAEDALUS_RENDERER_CAPTURE_PATH` selects another output path. No adapter is an explicit failure in this requested test; a placeholder image is not a fallback.

## Native live session

The optional `window` CLI feature includes GPU capture. Run the actual engine window on a desktop with a supported GPU:

```sh
cargo run -p daedalus-cli --features window -- view
cargo run -p daedalus-cli --features window -- view --seconds 5 --report /tmp/daedalus-live.json
```

WASD/QE moves the camera; left-drag or wheel changes the view. Arrow keys submit movement commands to the authority ECS, R requests a new terrain generation, and Escape closes the window. Dimensions are requested logical points; the report records the actual physical render size. `--work-iterations 10000` increases actual bounded generate/mesh work for loading/cancellation checks. The last owned scene remains visible during regeneration. There is no synthetic delay substituted for terrain work.

For manual QA, observe loading animation and advancing tick/generation in the title, move the camera and entity while loading, request overlapping regenerations, resize, then close. Inspect the JSON for accepted/applied/stopped outcomes, unread results recovered at shutdown, real terrain completions/cancellations, input drops, loading frames/input, camera changes, resize events, and bounded frame interval samples. An accepted command may finish after the last displayed snapshot; compare the authority's final shutdown state as well.

`--capture-output /tmp/daedalus-live.png` captures the final owned scene after the window exits. It is an offscreen render, not proof of an interactive window or its overlay. Physical-window screenshots and the session report provide separate evidence. Frame interval samples measure CPU/present spacing; they do not certify GPU completion latency or 144 fps. Mac evidence does not establish browser, Windows, mobile, or console support. The [live session contract](docs/03-architecture/native-live-session.md) defines ownership and failure behavior.

The earlier 2026-10-16 live-window checkpoint passed 102 all-feature CPU tests, formatting, and Clippy, plus the separately requested real Metal capture test. Actual Mac window input verified camera movement/orbit, authority ECS movement during terrain loading, supersession, resize, and close during a job. A clean window-enabled release archive also passed extracted CPU smoke and a real window/time-limit run. The [live checkpoint record](docs/06-worklog/2026-10-16-native-live-session.md) contains the exact source, environment, report counters, and scope limits. The subsequent project foundation passes 128 all-feature CPU tests plus formatting, check, and strict Clippy; its direct project capture and authority checks are recorded in the [readiness worklog](docs/06-worklog/2026-10-16-product-readiness-audit.md).

## Local CLI packaging and future releases

Python 3.11 or newer is required for the packaging scripts. On a native macOS arm64 or Linux x64 host, run:

```sh
python3 scripts/package_cli.py
```

The script builds `daedalus-cli` in the release profile with `--locked --features window`, creates a new versioned directory under ignored `dist/local/`, and prints the archive, checksum, and manifest paths. It refuses to overwrite an existing output directory. The manifest records the exact commit, dirty state, native target, features, toolchain, build ID, and hashes of the executable, notices, and dependency inventory. The archive contains no .NET runtime or managed SDK.

Recheck a generated archive using its printed path:

```sh
python3 scripts/package_cli.py --verify-archive /path/to/daedalus-package.tar.gz
```

Verification checks the archive SHA-256, entry names and types, embedded manifest and file hashes, executable permissions, extracted CLI `--version` and CPU `inspect`, and the shared external-project CPU smoke. This does not exercise GPU capture; use the separate real GPU check above. The dependency inventory is a conservative Cargo graph with recorded target/features, not a binary-symbol SBOM. Missing license texts fail packaging unless an exact package/checksum has explicitly curated upstream or declared-SPDX evidence; canonical reference texts are labeled separately from upstream originals.

The `release` workflow performs native packaging on macOS arm64 and Linux x64 for PRs and manual runs with read-only repository permissions. A future pushed `v<CLI SemVer>` tag must match the crate version, resolve to a commit in main history, and build cleanly on both targets before the publish job creates a GitHub Release. Only that job receives write permission. Tag publication, signing/notarization, Windows packaging, and managed runtime distribution have not been verified by a PR dry build. No tag or public release is created as part of the initial setup.

The package and notice tests are included in the Python discovery command above. They exercise malformed/mismatched tags, archive integrity, failed builds, overwrite rejection, dependency provenance, and the publish command through a local stub rather than publishing a real release.

The packaging checkpoints on 2026-10-16 include 36 Python tests (8 documentation, 14 notice collection, 14 packaging), a real local macOS archive and GPU capture, and PR native builds on macOS 15 arm64 and Ubuntu 24.04 x64. Follow-ups normalize compiler source paths through a build-only remapping environment and compare Git identity before/after compilation. The manifest records its fixed neutral policy without personal paths. The exact source checkpoints, artifact verification, and latest PR checks are linked in the [packaging record](docs/06-worklog/2026-10-16-cli-packaging.md). These runner checks do not establish compatibility with every older OS or Linux distribution.

## Managed/native integration

Use the pinned .NET SDK 10.0.402 with C# 14. `global.json` disables SDK roll-forward; ensure the intended `dotnet` is on PATH as described in [managed/README.md](managed/README.md).

```sh
cargo test -p daedalus-bridge --locked
cargo build -p daedalus-bridge --locked
dotnet build managed/Daedalus.slnx --configuration Release
DAEDALUS_NATIVE_PATH="$PWD/target/debug/libdaedalus_bridge.dylib" \
  dotnet run --no-build --project managed/Daedalus.ManagedHost --configuration Release -- --self-test
DAEDALUS_NATIVE_PATH="$PWD/target/debug/libdaedalus_bridge.dylib" \
  dotnet run --no-build --project managed/Daedalus.ManagedIntegrationTests --configuration Release
```

Linux uses `libdaedalus_bridge.so`. The coordinator verified 7 bridge tests, a warning-free Release managed build, the real Rust-backed self-test, and 2 integration scenarios covering state migration, staged failure rollback, cleanup, and repeated ALC collection. This is synchronous whole-module replacement with a transform SDK; automatic source compilation/watch, general gameplay APIs, MetadataUpdater edits, runtime distribution, and browser C# are not implemented. See the [E04 record](docs/06-worklog/2026-10-15-e04-managed.md).

## Physics/audio integration

The initial physics/audio adapters can be checked independently:

```sh
cargo test -p daedalus-physics -p daedalus-audio --locked -- --nocapture
cargo test -p daedalus-audio inspect_native_default_output_device_without_playback --locked -- --ignored --nocapture
```

The coordinator verified 7 physics and 3 audio tests, plus the separately requested hardware probe on this Mac (48 kHz stereo). The probe does not open a stream or play audio. These tests exercise actual Jolt/Rapier bodies and Kira PCM, not completed character, terrain-collider, audio streaming, or ECS integration. See the [E06 record](docs/06-worklog/2026-10-15-e06-physics-audio.md). Native dependencies and source-build requirements are in the [dependency baseline](docs/02-research/implementation-dependency-baseline.md); Linux CPAL builds need ALSA development headers.

## Evidence and remaining coverage

On 2026-10-15 the coordinator independently ran the initial all-feature suite (50 passed), expanded suite (72 passed), and post-review ten-crate suite (80 passed). Two hardware tests are ignored by default: GPU capture and output-device probe; both passed separately on this Mac. Formatting and all-target/all-feature Clippy with warnings denied passed. Documentation checker tests remain 8. These are recorded checkpoints, not evidence for every later change. See the [integration record](docs/06-worklog/2026-10-15-runtime-integration.md).

Fixtures stay inside the engine test suite and do not grow into sample games. No web conformance, advanced renderer quality/performance, 30-player server, complete gameplay SDK, full UI/IME, complete physics/audio behavior, or long-running stability result is implied by the foundation checks. Add executable commands and evidence here as those integrations become real.
