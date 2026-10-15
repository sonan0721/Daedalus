# Daedalus

**An independent, code-first game engine being designed for demanding 2D and 3D worlds.**

Daedalus aims to combine a Rust core, an AI-friendly scripting interface, responsive loading and streaming, modern GPU rendering, and authoritative dedicated servers. C#, TypeScript, and Luau are under evaluation for scripting. Development is driven through code, CLI, MCP, and a visible runtime window.

**Status: research and architecture. There is no runnable engine, renderer, game, or published performance result yet.** This repository currently contains engine specifications, decision records, and contribution infrastructure. Sample game development is outside the current scope.

## Direction

- Treat 2D tile simulation, 3D voxel streaming, and general engine capabilities as first-class design concerns.
- Isolate simulation, presentation, background work, and asset processing through explicit ownership and bounded communication.
- Keep loading UI and interaction responsive while long-running work progresses.
- Design for macOS and WebGPU browsers first; plan Windows, Linux, mobile, and later console ports with explicit capability and validation gates.
- Plan multiplayer and persistence from the beginning, targeting dedicated multiplayer sessions of about 30 players; activity and network budgets remain to be specified.
- Support stylized and physically based art through an extensible material and rendering architecture. Art references express intended quality, not shipped feature parity.
- Target 144 fps or higher under a defined workload and quality profile; preserve game functionality, save semantics, and scripting API across native and web exports, while allowing device-specific graphics settings.

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
