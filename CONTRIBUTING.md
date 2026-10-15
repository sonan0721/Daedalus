# Contributing

Thanks for helping shape Daedalus. The project is in its planning and design stage: it is intended to become a game engine, but there is no runnable engine yet. Read the [project overview](README.md) first, then open an issue or pull request with the question, requirement, or change you have in mind.

The repository is engine-only. Do not add sample game projects, game demos, or cloned games. Small fixtures and test scenes are welcome later when they exercise a specific engine behavior and remain part of the engine's test suite; they should not grow into playable sample projects.

## Issues and proposals

Use a bug report for a reproducible problem in the repository or its documented behavior. Use a design proposal to discuss a goal, requirement, architecture choice, or tradeoff before implementing a larger change. Small corrections can go straight to a pull request. Tie larger pull requests to an agreed issue, requirement, goal, or architecture decision record (ADR), and keep each change focused enough to review on its own.

Issues and pull requests are welcome in English or Korean. Community guidance is written in English; design notes may be Korean. You do not need to translate material before opening a discussion.

## Pull requests

Include the reason for the change, the relevant issue or decision when there is one, and the checks you ran. If a check does not apply, say why. Keep documentation claims aligned with what exists in the repository: do not describe planned engine features as implemented.

There is no required two-reviewer rule. The maintainer may merge a focused pull request after the required CI checks pass, including a pull request they authored. Human review and feedback are welcome, and an AI agent does not count as an independent human reviewer.

The maintainer currently coordinates project-owned coding tasks with Luna xhigh. This is an internal project workflow and does not restrict external human contributions; human pull requests are reviewed on their merits.

AI-assisted contributions should be easy to inspect: describe generated or substantially AI-assisted changes, keep the diff focused, and report how it was checked. The project maintainer makes project decisions; an AI agent cannot approve a requirement or architecture decision on the maintainer's behalf.

## Local checks

The current repository has documentation checks only. Run them from the repository root:

```sh
python3 scripts/check_docs.py
python3 -m unittest discover -s scripts -p 'test_*.py'
```

See [TESTING.md](TESTING.md) for what these commands cover and what remains planned. For security reports, follow [SECURITY.md](SECURITY.md). Please follow the expectations in [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

For examples of how established engine projects organize contributions, see the official [Bevy contribution guide](https://github.com/bevyengine/bevy/blob/main/CONTRIBUTING.md) and [Godot contribution guide](https://github.com/godotengine/godot/blob/master/CONTRIBUTING.md). They are references for process, not requirements copied into this project.

The documentation workflow uses read-only repository permissions and pins its checkout action to a full commit SHA. For the reasoning behind those choices, see GitHub's official [secure-use guidance for Actions](https://docs.github.com/en/actions/reference/security/secure-use).
