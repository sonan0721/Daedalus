# Testing

Daedalus is currently in the planning and design stage. The repository does not contain a runnable engine, so engine, renderer, platform, and game-runtime tests are not available yet. Do not treat a future test command as something contributors can run today.

## Current checks

Run these commands from the repository root:

```sh
python3 scripts/check_docs.py
python3 -m unittest discover -s scripts -p 'test_*.py'
```

The documentation checker verifies UTF-8 Markdown, local link targets, repository path boundaries, and use of an obsolete project label. Its unit tests cover link resolution, code masking, reference links, path traversal, and encoding/name errors. GitHub Actions runs these checks in the `docs` job on pull requests and pushes to `main`.

## Engine checks later

When engine code is added, document the exact build and test commands alongside the code they cover. A useful test strategy can grow from focused unit tests and small engine fixtures to integration checks and platform-specific validation as those systems exist.

Fixtures and test scenes should exercise a narrow engine behavior and stay within the engine's test suite. They are different from sample games, demos, or cloned game projects; this repository is not accepting those projects at this stage. Do not add `cargo test`, renderer checks, GPU requirements, or runtime smoke tests to contributor instructions until the corresponding code and environment are present.
