# Daedalus repository instructions

## Project and current stage

- This is the **Daedalus engine repository**, not a sample game repository.
- The current stage is research and architecture. Write or update the relevant Markdown specification before implementation. Do not start engine runtime implementation until the user requests that stage.
- Do not add sample games, game clones, or demo projects. Minimal engine test fixtures may be proposed with an explicit validation purpose.
- The engine is independent; reusable libraries such as ECS, GPU abstraction, physics, and layout are candidates, not permission to fork an entire engine by default.
- Read [the documentation index](docs/README.md), [the charter](docs/00-vision/project-charter.md), and [the answer log](docs/01-discovery/answer-log.md) before planning changes.

## Coding delegation

- The user explicitly requires project-owned implementation, test code, scripts, and CI configuration to be written by **Luna with xhigh reasoning** (`gpt-6-luna`, `xhigh`). The coordinating agent owns research, specifications, task definition, integration, and review.
- Pass a bounded task with file ownership, dependencies, acceptance criteria, and verification commands. Review the returned changes and run relevant checks before committing.
- Do not silently replace the requested model or effort if it is unavailable; report the problem and continue independent documentation work.
- This is the maintainer's AI workflow, not a prohibition on human open-source contributions.

## Evidence and decisions

- Distinguish user requirements, verified source facts, design proposals, and measured results.
- Prefer official documentation, source manifests, specifications, and original engineering presentations. Record URLs, access date, limitations, and the decision a source informs.
- Do not call a feature implemented, a platform supported, a benchmark passed, or a dependency combination compatible without corresponding evidence.
- Keep unanswered questions explicit. Do not convert the coordinator's suggested default into a user decision.
- No unconditional frame-rate, zero-stall, zero-race, or instant-reload guarantees.
- Record architectural decisions as ADRs. Changes to accepted decisions need a replacement ADR or an explicit revision trail.

## Git and collaboration

- Follow [CONTRIBUTING.md](CONTRIBUTING.md) and [TESTING.md](TESTING.md).
- Make periodic, coherent commits as requested; do not commit unrelated changes, credentials, proprietary SDKs, or unlicensed assets.
- Use issue/goal/requirement IDs in substantial changes. PR descriptions state the concrete behavior, evidence, and limitations.
- Explain the purpose and key findings of each newly created specification to the user. Small related batches are acceptable if every file is accounted for.
- Never report a future test plan as a successful test run.
