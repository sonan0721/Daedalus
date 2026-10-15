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

The initial E01 ECS schedule is single-threaded, with bounded Rayon work independently configurable. Budgeted parallel ECS execution and full runtime/presentation integration are tracked follow-up work. Snapshot publication and result polling use nonwaiting entry points, but that does not make the whole engine formally lock-free or wait-free.

## Current CLI smoke

```sh
cargo run -p daedalus-cli -- inspect --entities 2
cargo run -p daedalus-cli -- run --ticks 3 --entities 2
cargo run -p daedalus-cli -- step --ticks 2
```

These commands return real ECS state as JSON and do not initialize a GPU. They currently create controlled engine state for inspection; persistent live-session control and scene integration remain work in progress.

## Real GPU smoke

The GPU test is ignored in the default suite so CPU CI does not silently depend on a graphics adapter. Run it explicitly:

```sh
cargo test -p daedalus-renderer --features test-fixtures --test gpu_capture -- --ignored --nocapture
```

It renders a small material/depth fixture through a real wgpu adapter, reads back 513×289 pixels with padded rows, writes a PNG in the system temporary directory, and decodes the pixels for validation. `DAEDALUS_RENDERER_CAPTURE_PATH` selects another output path. No adapter is an explicit failure in this requested test; a placeholder image is not a fallback.

The native window path compiles and has bounded input/message queues, local camera controls, and an independent animated loading overlay. Interactive window behavior has not yet been manually verified. Mac Metal offscreen evidence does not establish browser, Windows, mobile, or console support.

## Evidence and remaining coverage

On 2026-10-15 the coordinator independently ran the initial all-feature Rust suite: 50 tests passed; the one GPU test was skipped by default and then passed separately on Apple M3 Pro / Metal. Formatting and Clippy passed. Documentation checker tests remain 8. This describes the recorded foundation checkpoint, not every later commit.

Fixtures stay inside the engine test suite and do not grow into sample games. No web conformance, advanced renderer quality/performance, 30-player server, C# host, full UI/IME, physics/audio integration, or long-running stability result is implied by the foundation checks. Add executable commands and evidence here as those integrations become real.
