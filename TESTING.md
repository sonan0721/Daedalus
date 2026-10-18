# Testing

Daedalus has an early Rust engine workspace. Tests cover implemented contracts and small engine fixtures; they do not certify commercial game quality, 144 fps, or every planned platform. The [implementation checklist](docs/04-plan/implementation-checklist.md) separates working code from remaining integrations.

## Current checks

Run these commands from the repository root:

```sh
python3 scripts/check_docs.py
python3 -m unittest discover -s scripts -p 'test_*.py'
```

The documentation checker verifies UTF-8 Markdown, local link targets, repository path boundaries, and use of an obsolete project label. Its unit tests cover link resolution, code masking, reference links, path traversal, and encoding/name errors. GitHub Actions runs these checks in the `docs` job on pull requests and pushes to `main`. It does not validate remote URLs, heading anchors, factual claims, or the whole CommonMark grammar; those need review or additional tools when relevant.

For noninteractive commands, the optional evidence runner creates a new log before starting the child:

```sh
python3 scripts/run_logged.py --log /tmp/daedalus-docs-check-001.log -- python3 scripts/check_docs.py
```

Choose a fresh path for every attempt; its parent directory must exist. Existing logs are refused without running the command. The runner captures both output streams and reports the child status and completed log SHA-256 as JSON. Child failures remain failures; launcher or evidence errors use exit 125. Stdin is closed, so this is for checks/builds rather than interactive CLI sessions. A missing completion footer or failed finalization is incomplete evidence. The runner does not select tests, retry, build implicitly, or coordinate Cargo workers. Its focused regressions run with `python3 -m unittest discover -s scripts -p test_run_logged.py` and are included in normal Python discovery.

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

The accepted PR19 all-feature CI checkpoint passes 359 CPU tests with 22 hardware tests ignored on Linux; the Mac run ignores 23, including its platform-specific audio diagnostic. The new renderer boundary is included in that total; see the [generation worklog](docs/06-worklog/2026-10-17-render-generation.md). Service tests cover real authority admission versus application, multiple clients, wrong-session/stale revisions, bounded pages, reserved Step capacity under saturation, reader-held result/frame lifetimes, duplicate replay and eviction, nonwaiting contention, timeouts, shutdown, and injected actor failures. Core retention also counts an independently retained entity-array Arc or cloned snapshot value. Frame-cap-one progress and pin/release recovery use a separate producer budget; native service producer capacity must be 2–64. A controlled actor pause verifies that ACK and last-reader release free Step credit before actor bookkeeping cleanup. The first Linux PR run exposed that race; subsequent CI is recorded separately in the worklog. Unknown post-failure command outcomes are reported explicitly. The native project viewer and protocol-2 adapter now use this service; MCP and managed attach are still pending. The legacy window now also reuses the count-and-byte CreditPool with RAII leases through its FIFO result channel. Its by-value results release engine credit at dequeue; they do not adopt the service ticket/reader lifetime. Failure-only OutcomeUnknown, injected post-apply/final-stop panics, bounded diagnostics, Stop saturation, and concurrent submit/stop are covered. See the [service contract](docs/03-architecture/session-control-service.md) for exact limits and remaining adapters.

The additive `NativeProjectActivationService::start_with_render_consumer` factory registers one runtime-neutral source consumer before startup. Its tests use the actual preparation worker and authority to check take-once source leases, bounded Ready/Failed reports, publication-token presentation ACKs, two generation commits with preserved live state, source-pin retention, cancellation, supersession, stop, panic cleanup, and progress with held ordinary result credits. These cases are included in the runtime commands above. Their Ready/presentation reports are trusted test inputs; they do not prove GPU upload or actual window presentation. The renderer, reusable native host, and CLI view activation connection remain under implementation; see the [consumer contract](docs/03-architecture/project-generation-consumer.md) and [current evidence](docs/06-worklog/2026-10-17-render-generation.md).

## Runtime capture foundation

The opt-in runtime capture foundation can be checked without a GPU or output device:

```sh
cargo test -p daedalus-runtime --all-features --locked capture_
cargo test -p daedalus-runtime --all-features --locked --test capture_public_api
```

`start_with_render_consumer_and_capture` adds a renderer-neutral handoff to the same actor and preparation service. Fourteen focused tests cover admission rollback, full ticket identity, source binding, independent Step capacity, timeout/abandonment, responder closure and injected actor failure. Two external public-API tests cover opt-in behavior and independent retained progress/capability/work ownership. Their consumer supplies trusted CPU metadata; they do not produce a PNG or perform readback. The final local workspace passed 414 cases with 34 hardware cases ignored, plus check, formatting, strict Clippy and Python72. Renderer/host/artifact/CLI capture integration remains open and `live_capture` stays false. See the [capture record](docs/06-worklog/2026-10-18-live-capture-foundation.md).

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

For runnable authored content, the legacy project loader accepts root entities, `daedalus.transform`, `daedalus.mesh_renderer`, and `daedalus.mesh` indexed JSON geometry with normals and opaque scalar materials, plus camera-only `daedalus.camera_2d` metadata. CLI project commands use the shared asset-aware loader and also accept `daedalus.model_renderer` for static glTF/GLB models and `daedalus.sprite_renderer` for atlases with one orthographic camera. The first model subset is opaque base-color PNG textures, mip levels, and actual primitive samplers; sprites support linear RGBA tint, deterministic painter order, and nearest-filtered atlas regions. Imported WAV playback is available through the optional shared audio consumer described below. General scene hierarchy/prefabs, attached C# modules, live reload/save, MCP, and web export remain unfinished. Unsupported data is an explicit error rather than a substituted fixture.

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

## CPU asset import and cook

```sh
cargo test -p daedalus-content --locked
cargo check -p daedalus-content --lib --target wasm32-unknown-unknown --locked
cargo run -p daedalus-cli -- project cook /path/to/project
python3 scripts/smoke_project.py --cli target/debug/daedalus --assets
```

The Wasm command requires the pinned toolchain's `wasm32-unknown-unknown` standard library; compilation does not establish a working browser renderer or web export. The asset-aware filesystem and byte-map APIs share the CPU importer/catalog and explicit unsupported-feature diagnostics. See the [format/settings/budget contract](docs/03-architecture/asset-import-foundation.md).

The opt-in `--assets` mode reuses the existing subprocess, temporary-project, timeout, binary-hash, and cleanup helpers. It adds offline glTF/GLB, PNG, atlas, and WAV preparation; same-geometry sharing; warm zero-decode/zero-cook behavior; external/embedded PNG, node, material, geometry, atlas, and WAV edits; payload/header cache corruption recovery; a cache-root override; and typed unsupported-extension diagnostics. It needs no GPU, audio device, network download, or extra Cargo build. Default CPU18 and native `--view` meanings remain unchanged. CPU catalog success does not establish textured rendering, sprite playback, audio output, reload, or P03/D1 acceptance.

The macOS/Linux package workflow is configured to run `--assets --audio --asset-preparation --asset-activation` in one combined step against its staged binary after the shared package backend succeeds. The activation change's actual CI outcome is recorded separately in its worklog. The helper runs supported combinations in separate sub-smokes; it does not attach audio to the headless activation service. Archive verification also uses the CPU lifecycle and imported offline-audio checks; it requires no output device. Locally, run the same helper against an extracted binary to verify the new CPU asset path in that archive. Original public assets are tested separately with [revision/license/hash provenance](docs/02-research/asset-validation-provenance.md); they are not downloaded in CI or bundled with the engine.

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

Automated helper views explicitly use `--input-source protocol`, so local physical controls cannot change the camera or trigger diagnostic gameplay operations during authority checks. Native input remains the CLI default and needs separate interactive verification. Protocol-only views still record actual observed input counters and handle focus, resize, and OS close; their reports must identify the selected policy.

The first command runs CPU protocol tests without opening a window. The last command requires an already-built window-enabled CLI, a real GPU, and a desktop. It reuses the default 18 CPU checks, then verifies a native manual-clock edit/Step/query against the report's actually presented packet, a separate graceful Close with stdin still open, EOF detaching control while the window stays alive, and finite shutdown with stdout deliberately blocked by a full unread POSIX pipe. The coordinator's 2026-10-16 run passed all 22 checks on Apple M3 Pro/Metal; the latest separate Python regression suite passed 56 tests. The two OS pipe cases require POSIX support and fail explicitly if setup is unavailable. These checks do not certify live capture, MCP, loading UI, or the P02 product gate. Default smoke and package verification remain CPU-only; imported offline mixing does not open an audio device. The [viewer contract](docs/03-architecture/project-view-control.md) describes string-encoded IDs, owned submit/poll/ack/forget tickets, bounded I/O, and best-effort Close receipts.

The GPU test is ignored in the default suite so CPU CI does not silently depend on a graphics adapter. Run it explicitly:

```sh
cargo test -p daedalus-renderer --features test-fixtures --test gpu_capture -- --ignored --nocapture
```

It renders a small material/depth fixture through a real wgpu adapter, reads back 513×289 pixels with padded rows, writes a PNG in the system temporary directory, and decodes the pixels for validation. `DAEDALUS_RENDERER_CAPTURE_PATH` selects another output path. No adapter is an explicit failure in this requested test; a placeholder image is not a fallback.

## Imported model and texture validation

Use an already-built capture/window-enabled CLI and a real desktop GPU:

```sh
python3 scripts/smoke_project.py --cli target/debug/daedalus --view --models
cargo test -p daedalus-renderer --all-features --locked -- --ignored --nocapture --test-threads=1
```

The opt-in `--models` mode reuses the offline glTF/PNG generator and native authority-control helper. It captures a textured project in a separate process while its native viewer is alive, then edits and queries the viewer authority and checks the presented packet. This verifies eager cache-pin release and both consumers; the batch PNG is a separate session, not live-window capture. Combined `--view --models` passes 24 checks locally; default CPU18, assets31, native22, and CPU-only package verification retain their meanings. The model test does not add network downloads or force hardware into default CI.

The model checkpoint passed four real Metal tests locally; the expanded renderer command now passes nine. Model pixels cover asymmetric UV orientation, sRGB decode and linear factor multiplication against an independent scalar reference, repeat/mirror/clamp, nearest/linear, and cooked mip selection. Resource checks cover shared geometry/texture, unchanged redraws, transform-only writes, and bounded submission/retirement. Backpressure tests use actual GPU draws with injected ledger/completion state for deterministic saturation; they do not measure a real GPU-stall duration. The public Khronos Cube was also rendered and visually inspected with its original texture. See the [model worklog](docs/06-worklog/2026-10-16-model-texture-runtime.md) for that checkpoint's identities, counters, limits, and package/CI status.

## Atlas sprites and orthographic views

```sh
python3 scripts/smoke_project.py --cli target/debug/daedalus --assets --view --models --sprites
cargo test -p daedalus-renderer --all-features --locked -- --ignored --nocapture --test-threads=1
```

`--sprites` reuses the offline atlas generator, bounded native authority-control helper, concurrent batch capture, and cleanup path. It checks the camera's actual authority update and reported orthographic projection, one shared quad/atlas upload, and globals-only camera changes. The batch image is a separate session while the window remains alive. Default CPU/package modes stay device-free.

The combined command above passed 39 checks locally in 14.288578 seconds. This includes CPU18, asset13, native4, model2, and sprite2 checks; the scope differs from earlier runs, so this is not a speedup comparison. The first sprite-native run exposed a float64-literal versus f32-wire expected-value mismatch in the smoke; it was corrected to the exact f32 round-trip without changing engine output or other assertions.

The nine explicit Metal tests include independent sRGB/unlit sprite references, asymmetric pivot/UV/mirror, painter ties/order, tint alpha, aspect resize, shared residency and dirty writes. Transparent PNG captures cover nonblack clears at alpha0/.25/1 with RGB4/alpha1-byte tolerances for UNORM8 quantization; opaque sprite pixels retain a three-byte RGB tolerance. Packet and legacy view targets are separately read back to verify opaque native policy. Early non-sRGB rejection and Busy fallback's actual ready projection/target are checked explicitly. The first run found three fixture errors, preserved in the [sprite worklog](docs/06-worklog/2026-10-16-sprite-runtime.md); the corrected run passed all nine. These checks do not establish a complete 2D renderer, live-window capture, or 144fps.

## Staged renderer library generations

The managed CPU owner and same-device staging path have separate [CPU](docs/03-architecture/render-cpu-generation.md) and [device](docs/03-architecture/render-device-generation.md) contracts. Local validation passes 359 workspace CPU tests, 19 real Metal renderer tests (ten new staging cases and nine existing cases), strict Clippy, formatting, and 70 Python tests. PR19's five checks, both packaged CPU62 smokes, downloaded Mac binary's existing native72 regression, and identical head/CI/main trees accepted this renderer library boundary. Focused renderer CPU configurations and every failed/fixed attempt are recorded in the worklog. Host/window/CLI activation and SDK integration remain pending.

The new native-adapter cases are ignored by CPU CI. Select them explicitly and serialize device use:

```sh
cargo test -p daedalus-renderer --lib --all-features --locked packet_renderer::tests::staged_gpu_tests:: -- --ignored --nocapture --test-threads=1
```

Check that the intended cases actually ran. These tests import generated geometry/PNG fixtures and exercise real device uploads, render targets, and completion, with explicit ledger injection only for deterministic saturation. Their snapshots are test inputs rather than a running authority host. This command's presence is not a passing GPU result; actual runs, errors, and source checkpoints belong in the [generation worklog](docs/06-worklog/2026-10-17-render-generation.md). Actual window presentation ACKs, host/CLI/SDK integration, and the engine product gate remain separate requirements.

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

The script builds `daedalus-cli` in the release profile with `--locked --features native-audio,window`, creates a new versioned directory under ignored `dist/local/`, and prints the archive, checksum, and manifest paths. It refuses to overwrite an existing output directory. The manifest records the exact commit, dirty state, native target, features, toolchain, build ID, and hashes of the executable, notices, and dependency inventory. The archive contains no .NET runtime or managed SDK.

Recheck a generated archive using its printed path:

```sh
python3 scripts/package_cli.py --verify-archive /path/to/daedalus-package.tar.gz
```

Verification checks the archive SHA-256, entry names and types, embedded manifest and file hashes, executable permissions, extracted CLI `--version` and CPU `inspect`, and the shared external-project CPU smoke including imported offline audio. This does not exercise GPU capture; use the separate real GPU check above. The dependency inventory is a conservative Cargo graph with recorded target/features, not a binary-symbol SBOM. Missing license texts fail packaging unless an exact package/checksum has explicitly curated upstream or declared-SPDX evidence; canonical reference texts are labeled separately from upstream originals.

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

## Imported project audio

```sh
cargo build -p daedalus-cli --features audio --locked
python3 scripts/smoke_project.py --cli target/debug/daedalus --audio
cargo build -p daedalus-cli --features native-audio,window --locked
python3 scripts/smoke_project.py --cli target/debug/daedalus --assets --view --models --sprites --audio --audio-native
```

`--audio` is CPU-only: it imports a finite WAV with two aliases, verifies one shared Frame conversion, sends Play/Stop/volume/pan through protocol 2, inspects actual mixed PCM summaries and voice reaping, and retries acknowledged caller command IDs without playing or advancing the audio clock twice. Its 24 checks include the unchanged CPU18. `--audio-native` explicitly opens the actual CPAL backend and waits for a finite imported voice to reach backend-reported Ended; it fails on device startup failure and does not substitute offline mixing. It adds one check to the selected modes. Completion is not a speaker loopback or audibility measurement, and actor join does not synchronously join Kira's detached CPAL stream-manager thread.

The optional runtime feature uses the same actor, owned tickets, bounded replay, and JSONL DTOs in headless protocol-2 serve and native viewing. Clips do not auto-play. Offline render has an independent sample clock; an engine Step does not advance it. See the [imported audio contract](docs/03-architecture/imported-audio-runtime.md) for limits, decimal IDs, requested control values, and unsupported spatial/streaming/managed behavior. The [worklog](docs/06-worklog/2026-10-16-imported-audio-runtime.md) records source/binary identities, real public-WAV and device evidence, failures, and the package/CI gate separately.

Native sprite smoke checks the final actual presented authority view. Initial packets may coalesce before the first draw, so its total globals-write count does not establish that the original camera was displayed before an edit. Ordered real-GPU tests separately verify the exact camera-change write delta. `latest_frame` reports a published actor snapshot, not a surface presentation acknowledgment.

## In-session CPU asset preparation

Use a prebuilt CLI; the helper does not invoke Cargo:

```sh
target/debug/daedalus project serve /path/to/project --protocol 2 --asset-preparation
python3 scripts/smoke_project.py --cli target/debug/daedalus --assets --audio --asset-preparation
python3 scripts/smoke_project.py --cli target/debug/daedalus --assets --view --models --sprites --audio --audio-native --asset-preparation
```

The second command needs the optional audio feature but no output device or GPU. The third explicitly needs a desktop, real GPU, native audio backend, and a CLI built with `native-audio,window`. `project view` enables preparation through `--stdio-control --asset-preparation`. Protocol 1 and a view without control reject the flag before loading the project or starting a worker/device.

The opt-in preparation mode adds twelve CPU checks using one persistent service/cache owner: real PNG/glTF/atlas/WAV edits, unchanged warm reuse with honest read/parse counters, candidate/source/memory identities, retained command replay, invalid candidates, and accepted versus completed cancellation. Selecting `--audio` as well adds one same-actor check: the preparation service uses offline audio, produces finite nonzero PCM, preserves the authority tick/source, and stops and reaps the imported looping voice. Preparation-only mode stays audio-free. With `--view`, one additional check correlates the CPU candidate and the same session's actual active presentation; the combined audio mode also checks that view's offline audio. It does not activate the candidate; the initial render/audio generation stays active. Initial source preparation remains synchronous startup work, and the worker retains its cache lock until it actually exits. See the [bounded ownership and control contract](docs/03-architecture/project-asset-preparation.md) and [attempt-by-attempt worklog](docs/06-worklog/2026-10-16-project-asset-preparation.md) for measured results, remaining gates, and failures.

The coordinator's local runs passed 30 checks with preparation alone, 50 combined CPU checks, and 60 full native checks. The same preparation actor produced 256 frames of finite nonzero PCM, retained the authority tick, and reaped the voice after Stop and another 64 frames. Its native view subsequently presented the same session/source at authority tick 1. The separate native audio check reached backend-reported Ended. Python discovery passed 61 tests; clean archive and CI acceptance are recorded separately in the preparation worklog.

## Native headless source-generation activation

The opt-in CPU path uses one existing Engine and preparation/cache worker:

```sh
cargo test -p daedalus-runtime --all-features --locked headless_activation
cargo test -p daedalus-cli --all-features --locked project_view_control::tests::activation_
cargo build -p daedalus-cli --all-features --locked
target/debug/daedalus project serve /path/to/project --protocol 2 --asset-activation
python3 scripts/smoke_project.py --cli target/debug/daedalus --asset-activation
python3 scripts/smoke_project.py --cli target/debug/daedalus --assets --audio --asset-preparation --asset-activation
```

`--asset-activation` includes CPU preparation. Protocol 1, native viewing, and audio attached to that service are rejected before project/cache/device startup. The helper needs no GPU, output device, or network assets. It edits PNG, model-node, atlas, and WAV source data in one session, waits for actual Staged before Commit, and checks generation/source/frame identity while preserving the live handle, transform, revision, and authority tick. Replay, stale-generation retry, staged cancellation, incompatible topology, and fully stopped owner are also checked. The first actual CLI run passed 28 checks (CPU18 plus activation10) in 1.005974 seconds.

This is CPU source activation. It does not update displayed pixels or an audio bank. `latest_frame.sequence` is the publication sequence; `authority_snapshot_sequence` identifies the underlying simulation snapshot. A deterministic actual-worker regression reproduced a Stage-result loss found by CI; the fix passed the corrected workspace (326 tests), CPU CLI smoke (62 checks), and all five PR checks including both packaged CPU smokes. The corrected clean macOS archive then failed the actual-device finite-audio completion check twice, temporarily holding PR acceptance. Later same-binary native recovery and final clean-package checks allowed PR16 acceptance; PR17's downloaded CI Mac binary also passed the combined native 72-check run with real Ended. No backend/device/volume/deadline change or root cause was established. These later results preserve the earlier failures; see the [final generation evidence](docs/06-worklog/2026-10-17-render-generation.md). The helper also checks actual invalid-option rejection before filesystem effects. See the [headless contract](docs/03-architecture/headless-asset-generation.md) and [activation worklog](docs/06-worklog/2026-10-16-project-asset-activation.md) for exact identities, coverage limits, and intermediate failures.

## Reusable native host and managed source activation

The host is a library consumer of one project authority, with an opt-in native window. Focused verification commands are:

```sh
cargo test -p daedalus-project-host --no-default-features --locked
cargo test -p daedalus-project-host --all-features --locked
cargo run -p daedalus-project-host --all-features --locked --example native_host_smoke -- --observe-ms 500 /path/to/project
cargo build -p daedalus-cli --all-features --locked
python3 scripts/smoke_project.py --cli target/debug/daedalus --view --asset-activation
```

The public SDK example checks actual presentation metadata, repeated redraw of one Manual publication, view closure with the same authority still alive, and whole-session shutdown. It needs a native window and the OS UI thread. The opt-in CLI smoke uses the existing external model/sprite fixture, mutates live authority state, edits PNG/model/atlas sources, waits for actual device-ready Stage before Commit, observes the new presented identity, and checks old-source retirement and final report identity. It does not capture the window's pixels.

The [2026-10-18 worklog](docs/06-worklog/2026-10-18-native-host-integration.md) records a 35-check focused CLI pass plus the preceding shared-pipeline/report defects. Broader native invocation encountered local camera input during an assumption of no input; that failure is retained. After implementing the explicit [viewer input policy](docs/03-architecture/native-view-input-policy.md), the original full native flag combination passed all 65 checks, including the quiet authority-camera assertions. Final local Rust tests passed 398 cases with 34 device cases ignored; Python discovery passed 72. PR20 passed all five checks and both packaged CPU62 suites; the checksum-verified downloaded Mac package then passed 72 native/offline-audio checks. Main and the verified CI tree match. Same-window pixels, performance, and D1 gates remain open.

## Physics/audio integration

The initial physics/audio adapters can be checked independently:

```sh
cargo test -p daedalus-physics -p daedalus-audio --locked -- --nocapture
cargo test -p daedalus-audio --features native inspect_native_default_output_device_without_playback --locked -- --ignored --nocapture
cargo test -p daedalus-audio --features native --locked service::tests::native_cpal_voice_progress_and_completion_diagnostic -- --ignored --exact --nocapture
cargo test -p daedalus-audio --features native --locked service::tests::direct_cpal_default_stream_callback_progress_diagnostic -- --ignored --exact --nocapture
cargo test -p daedalus-audio --features native --locked service::tests::direct_cpal_same_device_hal_callback_progress_diagnostic -- --ignored --exact --nocapture
```

The coordinator verified 7 physics and 3 audio tests, plus the separately requested hardware probe on this Mac (48 kHz stereo). The probe does not open a stream or play audio. These tests exercise actual Jolt/Rapier bodies and Kira PCM, not completed character, terrain-collider, audio streaming, or ECS integration. See the [E06 record](docs/06-worklog/2026-10-15-e06-physics-audio.md). Native dependencies and source-build requirements are in the [dependency baseline](docs/02-research/implementation-dependency-baseline.md); Linux CPAL builds need ALSA development headers.

The separate ignored Kira progress diagnostic opens the actual CPAL backend and plays the same finite 0.25-second tone used by the native project smoke. It records bounded position/state/error/CPU samples and requires real Stopped plus reap within the existing 10-second observation window. Its recorded attempt failed: position stayed zero, no Kira CPU samples were observed, and one underrun/overrun was reported. Two silent direct-CPAL diagnostics reuse one atomic callback monitor: default output, and macOS-only same-DeviceId enumerated HAL output. Each recorded attempt also failed with zero callbacks/frames. Default and native-feature CPU suites passed six tests each (native ignores four device checks). The OS `afplay` comparison independently failed AudioQueueStart(-66681) without interruption. Root cause remains unresolved. Later native project smoke recovered as described above; the standalone diagnostic commands were not rerun and must not be reported as passed. See the [diagnostic contract](docs/03-architecture/native-audio-progress-diagnostics.md).

## Evidence and remaining coverage

On 2026-10-15 the coordinator independently ran the initial all-feature suite (50 passed), expanded suite (72 passed), and post-review ten-crate suite (80 passed). Two hardware tests are ignored by default: GPU capture and output-device probe; both passed separately on this Mac. Formatting and all-target/all-feature Clippy with warnings denied passed. Documentation checker tests remain 8. These are recorded checkpoints, not evidence for every later change. See the [integration record](docs/06-worklog/2026-10-15-runtime-integration.md).

Fixtures stay inside the engine test suite and do not grow into sample games. No web conformance, advanced renderer quality/performance, 30-player server, complete gameplay SDK, full UI/IME, complete physics/audio behavior, or long-running stability result is implied by the foundation checks. Add executable commands and evidence here as those integrations become real.
