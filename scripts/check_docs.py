#!/usr/bin/env python3
"""Check Markdown files for invalid UTF-8, broken local links, and old naming."""

from __future__ import annotations

import argparse
import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import unquote, urlsplit


OBSOLETE_PROJECT_LABEL = "Aegis"
SKIP_DIRECTORIES = {
    ".git",
    ".hg",
    ".svn",
    ".venv",
    "venv",
    "node_modules",
    "target",
    "dist",
    "build",
    ".cache",
}


@dataclass(frozen=True, order=True)
class Problem:
    path: str
    line: int
    message: str

    def __str__(self) -> str:
        return f"{self.path}:{self.line}: {self.message}"


def _mask_range(chars: list[str], start: int, end: int) -> None:
    """Replace non-newline characters in a range while preserving line numbers."""
    for index in range(start, end):
        if chars[index] not in "\r\n":
            chars[index] = " "


def _without_fenced_code(text: str) -> str:
    """Mask CommonMark-style fenced blocks, retaining line and column offsets."""
    chars = list(text)
    offset = 0
    fence_char: str | None = None
    fence_length = 0

    for line in text.splitlines(keepends=True):
        content = line.rstrip("\r\n")
        marker = re.match(r"^ {0,3}(`{3,}|~{3,})(.*)$", content)

        if fence_char is None:
            if marker:
                run = marker.group(1)
                fence_char = run[0]
                fence_length = len(run)
                _mask_range(chars, offset, offset + len(line))
        else:
            _mask_range(chars, offset, offset + len(line))
            if marker:
                run = marker.group(1)
                rest = marker.group(2)
                if run[0] == fence_char and len(run) >= fence_length and not rest.strip():
                    fence_char = None
                    fence_length = 0

        offset += len(line)

    return "".join(chars)


def _without_inline_code(text: str) -> str:
    """Mask inline code spans delimited by matching backtick runs."""
    chars = list(text)
    index = 0
    while index < len(text):
        if text[index] != "`":
            index += 1
            continue

        run_end = index + 1
        while run_end < len(text) and text[run_end] == "`":
            run_end += 1
        run_length = run_end - index

        search = run_end
        closing = None
        while search < len(text):
            tick = text.find("`", search)
            if tick < 0:
                break
            tick_end = tick + 1
            while tick_end < len(text) and text[tick_end] == "`":
                tick_end += 1
            if tick_end - tick == run_length:
                closing = tick_end
                break
            search = tick_end

        if closing is None:
            index = run_end
            continue

        _mask_range(chars, index, closing)
        index = closing

    return "".join(chars)


def _unescape_destination(destination: str) -> str:
    return re.sub(r"\\([\\`*{}\[\]()<>#+.!_>\-])", r"\1", destination)


def _destination_at(text: str, start: int) -> tuple[str, int] | None:
    """Read a Markdown inline destination at start; return value and end offset."""
    index = start
    while index < len(text) and text[index] in " \t\r\n":
        index += 1
    if index >= len(text):
        return None

    if text[index] == "<":
        index += 1
        value: list[str] = []
        while index < len(text):
            char = text[index]
            if char == "\\" and index + 1 < len(text):
                value.append(char)
                value.append(text[index + 1])
                index += 2
            elif char == ">":
                return _unescape_destination("".join(value)), index + 1
            elif char in "\r\n<":
                return None
            else:
                value.append(char)
                index += 1
        return None

    value = []
    depth = 0
    while index < len(text):
        char = text[index]
        if char == "\\" and index + 1 < len(text):
            value.append(char)
            value.append(text[index + 1])
            index += 2
            continue
        if char in " \t\r\n" and depth == 0:
            break
        if char == "(" and depth == 0:
            depth = 1
            value.append(char)
            index += 1
            continue
        if char == "(" and depth > 0:
            depth += 1
        elif char == ")":
            if depth == 0:
                break
            depth -= 1
        value.append(char)
        index += 1

    if depth != 0:
        return None
    return _unescape_destination("".join(value)), index


def _is_escaped(text: str, index: int) -> bool:
    backslashes = 0
    index -= 1
    while index >= 0 and text[index] == "\\":
        backslashes += 1
        index -= 1
    return backslashes % 2 == 1


def _inline_destinations(text: str) -> list[tuple[str, int]]:
    """Return inline-link destinations and the source line of each destination."""
    links: list[tuple[str, int]] = []
    index = 0
    while index < len(text) - 1:
        close = text.find("](", index)
        if close < 0:
            break
        if _is_escaped(text, close):
            index = close + 2
            continue

        # A closing bracket needs a matching unescaped opening bracket. This
        # keeps prose such as "array](index)" from being treated as a link.
        opening = close - 1
        while opening >= 0:
            opening = text.rfind("[", 0, opening + 1)
            if opening < 0:
                break
            if not _is_escaped(text, opening):
                break
            opening -= 1
        if opening < 0:
            index = close + 2
            continue

        parsed = _destination_at(text, close + 2)
        if parsed is None:
            index = close + 2
            continue
        destination, end = parsed
        line = text.count("\n", 0, close + 2) + 1
        links.append((destination, line))
        index = max(end, close + 2)

    return links


def _reference_definitions(text: str) -> list[tuple[str, str, int]]:
    """Return reference-label destinations and their source lines."""
    definitions = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        match = re.match(r"^ {0,3}\[([^\]]+)\]:[ \t]*(.*)$", line)
        if not match:
            continue
        parsed = _destination_at(match.group(2), 0)
        if parsed is None:
            continue
        destination, _ = parsed
        label = " ".join(match.group(1).split()).casefold()
        definitions.append((label, destination, line_number))
    return definitions


def _reference_uses(text: str, known_labels: set[str]) -> list[tuple[str, int]]:
    """Resolve reference-style uses against definitions in the same file."""
    uses: list[tuple[str, int]] = []
    for match in re.finditer(r"(?<!\\)!?\[([^\]]*)\](?:\[([^\]]*)\])?", text):
        label_text = match.group(2) if match.group(2) is not None else match.group(1)
        label = " ".join(label_text.split()).casefold()
        if not label and match.group(2) is not None:
            label = " ".join(match.group(1).split()).casefold()
        if label in known_labels:
            uses.append((label, text.count("\n", 0, match.start()) + 1))
    return uses


def _repo_relative(path: Path, root: Path) -> str:
    try:
        return path.relative_to(root).as_posix()
    except ValueError:
        return str(path)


def _local_path(destination: str, source: Path, root: Path) -> Path | None:
    """Resolve a local Markdown URL, or return None for external/anchor-only URLs."""
    if not destination or destination.startswith("#"):
        return None
    try:
        parts = urlsplit(destination)
    except ValueError:
        return Path("\x00")
    if parts.scheme or parts.netloc:
        return None
    raw_path = unquote(parts.path)
    if not raw_path:
        return None
    if "\x00" in raw_path:
        return Path("\x00")
    if raw_path.startswith("/"):
        target = root / raw_path.lstrip("/")
    else:
        target = source.parent / raw_path

    try:
        resolved_root = root.resolve()
        resolved_target = target.resolve(strict=False)
        resolved_target.relative_to(resolved_root)
    except (OSError, RuntimeError, ValueError):
        return Path("\x00")
    return resolved_target


def _markdown_files(root: Path) -> list[Path]:
    paths: list[Path] = []
    for current, directories, files in os.walk(root, followlinks=False):
        current_path = Path(current)
        directories[:] = [
            name
            for name in directories
            if name not in SKIP_DIRECTORIES
            and not (current_path / name).is_symlink()
        ]
        for name in files:
            candidate = current_path / name
            if candidate.suffix.lower() == ".md" and not candidate.is_symlink():
                paths.append(candidate)
    return sorted(paths)


def check_repository(root: Path) -> list[Problem]:
    root = root.resolve()
    problems: list[Problem] = []

    for path in _markdown_files(root):
        relative = _repo_relative(path, root)
        try:
            raw = path.read_bytes()
        except OSError as error:
            problems.append(Problem(relative, 1, f"cannot read file: {error}"))
            continue

        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError as error:
            line = raw[: error.start].count(b"\n") + 1
            problems.append(Problem(relative, line, "file is not valid UTF-8"))
            continue

        obsolete_name = re.search(
            rf"\b{re.escape(OBSOLETE_PROJECT_LABEL)}\b", text, re.IGNORECASE
        )
        if obsolete_name:
            line = text[: obsolete_name.start()].count("\n") + 1
            problems.append(
                Problem(relative, line, f"obsolete project label {OBSOLETE_PROJECT_LABEL!r} is not allowed")
            )

        visible = _without_inline_code(_without_fenced_code(text))
        destinations = _inline_destinations(visible)

        definitions = _reference_definitions(visible)
        definition_map: dict[str, tuple[str, int]] = {}
        for label, destination, line in definitions:
            definition_map.setdefault(label, (destination, line))
        for label, _use_line in _reference_uses(visible, set(definition_map)):
            destination, line = definition_map[label]
            destinations.append((destination, line))
        # Definitions are valid link destinations even if currently unused.
        destinations.extend((destination, line) for _, destination, line in definitions)

        for destination, line in destinations:
            target = _local_path(destination, path, root)
            if target is None:
                continue
            if str(target) == "\x00":
                problems.append(Problem(relative, line, f"invalid or out-of-repository link target: {destination!r}"))
            elif not target.exists():
                problems.append(Problem(relative, line, f"broken local link: {destination!r}"))

    return sorted(set(problems))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parents[1],
        help="repository root (defaults to the parent of scripts/)",
    )
    args = parser.parse_args(argv)

    if not args.root.is_dir():
        print(
            f"Documentation check failed: repository root does not exist: {args.root}",
            file=sys.stderr,
        )
        return 2

    problems = check_repository(args.root)
    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        print(f"Documentation check failed with {len(problems)} issue(s).", file=sys.stderr)
        return 1

    print("Documentation check passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
