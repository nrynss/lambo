"""Unit tests for render_release_notes (fail-closed template rendering)."""

import unittest

from render_release_notes import (
    LIMITS_PLACEHOLDER,
    NotesError,
    WHATS_NEW_PLACEHOLDER,
    changelog_section,
    render,
)


TEMPLATE = """# Lambo v__LAMBO_VERSION__

## What's new

{whats_new}

## Known limits

- Bedrock embeddings are not shipped (blocked on account authorization).
{limits}
""".format(
    whats_new=WHATS_NEW_PLACEHOLDER, limits=LIMITS_PLACEHOLDER
)

CHANGELOG = """# Changelog

## Unreleased

## 0.10.1 (2026-11-10)

### Highlights

- **First thing (#1).**

### Upgrading from 0.10.0

No schema change.

## 0.10.0 (2026-11-09)

### Highlights

- **Older thing (#2).**
"""


class SectionExtraction(unittest.TestCase):
    def test_dated_heading_matches_and_stops_at_next_version(self):
        section = changelog_section(CHANGELOG, "0.10.1")
        self.assertIn("First thing", section)
        self.assertNotIn("Unreleased", section)
        self.assertNotIn("Older thing", section)

    def test_missing_version_fails(self):
        with self.assertRaises(NotesError):
            changelog_section(CHANGELOG, "9.9.9")

    def test_prefix_version_does_not_match(self):
        # "## 0.10.1" must not match a hypothetical "## 0.10.10".
        with self.assertRaises(NotesError):
            changelog_section("## 0.10.10\n\n- x.\n", "0.10.1")


class Rendering(unittest.TestCase):
    def test_happy_path(self):
        section = changelog_section(CHANGELOG, "0.10.1")
        out = render(TEMPLATE, "0.10.1", section)
        self.assertIn("v0.10.1", out)
        self.assertNotIn("__LAMBO_VERSION__", out)
        self.assertIn("First thing", out)
        self.assertNotIn(WHATS_NEW_PLACEHOLDER, out)
        self.assertNotIn(LIMITS_PLACEHOLDER, out)
        self.assertIn("Bedrock embeddings are not shipped", out)

    def test_maintainer_comment_is_stripped(self):
        section = changelog_section(CHANGELOG, "0.10.1")
        template = "<!-- Maintainer: do not ship me. -->\n" + TEMPLATE
        out = render(template, "0.10.1", section)
        self.assertNotIn("Maintainer", out)

    def test_missing_placeholders_fail_on_drift(self):
        section = changelog_section(CHANGELOG, "0.10.1")
        with self.assertRaises(NotesError):
            render(TEMPLATE.replace(WHATS_NEW_PLACEHOLDER, "- x."), "0.10.1", section)
        with self.assertRaises(NotesError):
            render(TEMPLATE.replace(LIMITS_PLACEHOLDER, "- y."), "0.10.1", section)


if __name__ == "__main__":
    unittest.main()
