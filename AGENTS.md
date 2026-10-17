# Daedalus repository instructions

## Project and current stage

- This is the **Daedalus engine repository**, not a sample game repository.
- The user has authorized engine implementation. Keep the relevant Markdown contract and [implementation checklist](docs/04-plan/implementation-checklist.md) aligned before implementing each subsystem. Unfinished design and quality gates remain open; authorization to start is not evidence that they passed.
- Follow [ADR-0005](docs/05-decisions/0005-engine-product-and-game-validation.md): the top-level delivery is D1 usable AI-driven engine, then D2 a separate Minecraft-quality 3D game, then D3 a separate Terraria-quality 2D game. Engine product acceptance and the two game acceptances are all required before the overall goal is complete.
- Keep game projects outside this engine repository. Do not start D2/D3 before their predecessor product gate passes. Minimal engine integration fixtures remain allowed but never substitute for engine or game product acceptance.
- The engine is independent; reusable libraries such as ECS, GPU abstraction, physics, and layout are candidates, not permission to fork an entire engine by default.
- Follow [ADR-0003](docs/05-decisions/0003-csharp-and-native-gameplay.md): Rust core, C++ via explicit adapters, C# 14/.NET 10 LTS/CoreCLR as the main gameplay/PC mod SDK, and build-time Rust gameplay. C# supports the planned reload path; native hot-reload and a second scripting language are outside the initial contract. These are design decisions, not implemented capabilities.
- Read [the documentation index](docs/README.md), [the charter](docs/00-vision/project-charter.md), and [the answer log](docs/01-discovery/answer-log.md) before planning changes.

## Coding delegation

- The user explicitly requires project-owned implementation, test code, scripts, and CI configuration to be written by **Luna with xhigh reasoning** (`gpt-6-luna`, `xhigh`). The coordinating agent owns research, specifications, task definition, integration, and review.
- Pass a bounded task with file ownership, dependencies, acceptance criteria, and verification commands. Review the returned changes and run relevant checks before committing.
- Do not silently replace the requested model or effort if it is unavailable; report the problem and continue independent documentation work.
- This is the maintainer's AI workflow, not a prohibition on human open-source contributions.

## Build and verification coordination

- Share the public API contract before dependent parallel edits; keep file ownership disjoint and assign one Cargo.lock owner.
- Reuse the workspace build cache. Coordinate Cargo checks against the shared target directory; do not create a second cold workspace only to bypass an in-progress lockfile update.
- Run focused CPU checks while implementing. After source integration is stable, run the required workspace/managed checks and then hardware/UI checks. Repeat broad checks when changes or failures justify them.
- Create evidence logs exclusively (shell `noclobber` or equivalent), with task-specific names. Fail on an existing filename; never reset an attempt counter over earlier raw output, including failed runs. Record any lost evidence explicitly rather than reconstructing historical output.
- For focused tests, verify that the intended fully qualified test actually ran. A successful command with zero matching tests is not evidence for that case; keep the output and correct the filter.
- CI caches accelerate dependencies and compilation; cache hits never replace locked builds, tests, or package smoke checks. Keep credentials and per-user configuration outside cached paths.
- Optimize recurring development work across project lifecycle, common control service, SDK/schema binding, asset cooking, AI edit/run/observe/test loops, and packaging. Record actual duplication, correctness/invalidation behavior, and iteration evidence in [the workflow plan](docs/04-plan/development-iteration-pipeline.md). Do not equate this request with runtime optimization or CI caching alone.

## Evidence and decisions

- Distinguish user requirements, verified source facts, design proposals, and measured results.
- Prefer official documentation, source manifests, specifications, and original engineering presentations. Record URLs, access date, limitations, and the decision a source informs.
- Do not call a feature implemented, a platform supported, a benchmark passed, or a dependency combination compatible without corresponding evidence.
- Keep unanswered questions explicit. Do not convert the coordinator's suggested default into a user decision.
- No unconditional frame-rate, zero-stall, zero-race, or instant-reload guarantees.
- Record architectural decisions as ADRs. Changes to accepted decisions need a replacement ADR or an explicit revision trail.

## Git and collaboration

- Follow [CONTRIBUTING.md](CONTRIBUTING.md) and [TESTING.md](TESTING.md).
- Before opening or merging a PR, read [the GitHub workflow](docs/04-plan/github-workflow.md). The repository currently allows squash merges only; use the verified head SHA when merging and distinguish required protection checks from the full product-gate checks.
- Make periodic, coherent commits as requested; do not commit unrelated changes, credentials, proprietary SDKs, or unlicensed assets.
- Use issue/goal/requirement IDs in substantial changes. PR descriptions state the concrete behavior, evidence, and limitations.
- Explain the purpose and key findings of each newly created specification to the user. Small related batches are acceptable if every file is accounted for.
- Never report a future test plan as a successful test run.
- Every status report distinguishes overall goal (open until D1/D2/D3 acceptance), current product gate, and subordinate work. Test counts, merged PRs, fixtures, and CLI binaries do not establish a usable engine. Keep [the product acceptance contract](docs/04-plan/product-delivery-and-acceptance.md) current.
