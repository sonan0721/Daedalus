"""Focused tests for scripts/check_docs.py."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from check_docs import check_repository


class CheckDocsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def write(self, name: str, contents: str) -> None:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")

    def problems(self) -> list[str]:
        return [str(problem) for problem in check_repository(self.root)]

    def test_valid_absolute_relative_percent_encoded_and_external_links(self) -> None:
        self.write("README.md", """# Root

[guide](docs/guide%20one.md#intro)
[absolute](/CONTRIBUTING.md)
[web](https://example.com/missing.md)
[anchor](#top)
""")
        self.write("docs/guide one.md", "# Guide")
        self.write("CONTRIBUTING.md", "# Contributions")

        self.assertEqual(self.problems(), [])

    def test_broken_links_include_source_line(self) -> None:
        self.write("README.md", "# Title\n\n[missing](docs/nope.md)\n")

        problems = self.problems()

        self.assertEqual(len(problems), 1)
        self.assertIn("README.md:3: broken local link", problems[0])

    def test_links_inside_fenced_and_inline_code_are_ignored(self) -> None:
        self.write("README.md", """# Docs

`[inline](missing-inline.md)`

```md
[fenced](missing-fenced.md)
```
""")

        self.assertEqual(self.problems(), [])

    def test_reference_links_are_checked(self) -> None:
        self.write("README.md", """[guide][docs]

[docs]: <missing page.md> "Guide"
""")

        problems = self.problems()

        self.assertEqual(len(problems), 1)
        self.assertIn("README.md:3: broken local link", problems[0])

    def test_links_cannot_escape_the_repository(self) -> None:
        self.write("README.md", "[outside](%2e%2e/secret.md)\n")

        problems = self.problems()

        self.assertEqual(len(problems), 1)
        self.assertIn("out-of-repository", problems[0])

    def test_links_cannot_escape_through_a_symlink(self) -> None:
        self.write("README.md", "[outside](linked.md)\n")
        with tempfile.TemporaryDirectory() as external_dir:
            external_file = Path(external_dir) / "secret.md"
            external_file.write_text("private", encoding="utf-8")
            try:
                (self.root / "linked.md").symlink_to(external_file)
            except OSError as error:
                self.skipTest(f"symlinks unavailable: {error}")

            problems = self.problems()

        self.assertEqual(len(problems), 1)
        self.assertIn("out-of-repository", problems[0])

    def test_malformed_url_is_reported_without_crashing(self) -> None:
        self.write("README.md", "[invalid](http://[bad)\n")

        problems = self.problems()

        self.assertEqual(len(problems), 1)
        self.assertIn("invalid or out-of-repository link", problems[0])

    def test_invalid_utf8_and_obsolete_name_are_reported(self) -> None:
        (self.root / "bad.md").write_bytes(b"# valid\n\xff\n")
        self.write("old.md", "# Former name\nThe old project was called Aegis.\n")

        problems = self.problems()

        self.assertTrue(any("bad.md:2: file is not valid UTF-8" in problem for problem in problems))
        self.assertTrue(any("old.md:2: obsolete project label" in problem for problem in problems))


if __name__ == "__main__":
    unittest.main()
