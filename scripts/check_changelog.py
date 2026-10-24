#!/usr/bin/env python3
"""Changelog conformance fence for CHANGELOG.md.

Enforces the section conventions documented in the file's own preamble so
release prep cannot leave shipped entries behind (the v0.5.0 miss: the
generated version section was prepended while the hand-written entries for
#44-#52 stayed under [Unreleased], ready to double-report at the next tag).

Default mode (CI, every push/PR):
  1. the file starts with the `# Changelog` title;
  2. `## [Unreleased]` appears exactly once and is the first `##` section;
  3. every other `## ` heading matches `## [vX.Y.Z] - YYYY-MM-DD` (bracketed
     version, ISO date) and version sections run newest-first;
  4. no `(#N)` pull-request reference appears in [Unreleased] and in a
     released version section at the same time — an entry left in
     [Unreleased] after its PR shipped in a version section double-reports
     at the next tag. (The same PR cited several times inside ONE released
     section is fine — #36 legitimately has three facets in v0.4.0.)

--require-released vX.Y.Z (release guard, tag checkout): all of the above,
plus the `[vX.Y.Z]` section exists and [Unreleased] carries no entries (no
`### ` subsection, no `(#N)` reference) — a tag may only ship once
[Unreleased] has been folded into the version section.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path
from typing import NoReturn

VERSION_HEADING = re.compile(r"^\[v(\d+)\.(\d+)\.(\d+)\] - (\d{4})-(\d{2})-(\d{2})$")
REF = re.compile(r"\(#(\d+)\)")


def fail(msg: str) -> NoReturn:
    print(f"check_changelog: {msg}", file=sys.stderr)
    sys.exit(1)


def parse(path: Path) -> list[tuple[str, str]]:
    """Return [(heading, body)] — top-level `## ` sections in file order."""
    lines = path.read_text().splitlines()
    if not lines or lines[0].strip() != "# Changelog":
        fail("file must start with the `# Changelog` title line")
    sections: list[tuple[str, str]] = []
    current: tuple[str, list[str]] | None = None
    for line in lines[1:]:
        if line.startswith("## "):
            if current is not None:
                sections.append((current[0], "\n".join(current[1])))
            current = (line[3:].strip(), [])
        elif current is not None:
            current[1].append(line)
    if current is not None:
        sections.append((current[0], "\n".join(current[1])))
    if not sections:
        fail("no `## ` sections found")
    return sections


def check(path: Path, require_released: str | None) -> None:
    sections = parse(path)

    unreleased = [name for name, _ in sections if name == "[Unreleased]"]
    if len(unreleased) != 1:
        fail(f"`## [Unreleased]` must appear exactly once, found {len(unreleased)}")
    if sections[0][0] != "[Unreleased]":
        fail("`## [Unreleased]` must be the first `## ` section "
             f"(found `## {sections[0][0]}`) — version sections are never prepended above it")

    versions: list[tuple[int, int, int, str]] = []
    for name, _ in sections[1:]:
        m = VERSION_HEADING.match(name)
        if not m:
            fail(f"heading `## {name}` is not in the documented "
                 "`## [vX.Y.Z] - YYYY-MM-DD` form")
        versions.append((int(m.group(1)), int(m.group(2)), int(m.group(3)), name))

    for newer, older in zip(versions, versions[1:]):
        if newer[:3] <= older[:3]:
            fail(f"version sections must run newest-first: `## {older[3]}` "
                 f"must not come after `## {newer[3]}`")

    released_refs: set[str] = set()
    for name, body in sections[1:]:
        released_refs.update(f"#{ref}" for ref in REF.findall(body))
    for ref in REF.findall(sections[0][1]):
        if f"#{ref}" in released_refs:
            fail(f"pull-request reference (#{ref}) sits in [Unreleased] but is "
                 "already cited in a released version section — it shipped; "
                 "fold the entry into that version section")

    if require_released is not None:
        want = require_released.lstrip("v")
        matching = [v for v in versions if v[3].startswith(f"[v{want}] - ")]
        if not matching:
            fail(f"no `## [{require_released}]` section for the tag being shipped")
        name, body = sections[0]
        if "### " in body or REF.search(body):
            fail("[Unreleased] still carries entries — fold them into the "
                 f"`## [{require_released}]` section before pushing the tag")

    all_refs = {f"#{ref}" for _, body in sections for ref in REF.findall(body)}
    print(f"changelog OK: {len(versions)} version sections, {len(all_refs)} distinct PR references"
          + (f", [{require_released}] release-guard clean" if require_released else ""))


def main() -> None:
    args = sys.argv[1:]
    require_released = None
    paths: list[str] = []
    it = iter(args)
    for arg in it:
        if arg == "--require-released":
            require_released = next(it, None)
            if not require_released:
                fail("--require-released needs a vX.Y.Z argument")
        else:
            paths.append(arg)
    path = Path(paths[0]) if paths else Path("CHANGELOG.md")
    check(path, require_released)


if __name__ == "__main__":
    main()
