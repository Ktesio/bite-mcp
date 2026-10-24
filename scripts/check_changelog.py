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

--fold vX.Y.Z [--date YYYY-MM-DD] (release prep helper): rewrite the file so
[Unreleased]'s entries become a new `## [vX.Y.Z] - YYYY-MM-DD` section and
[Unreleased] is left empty. Prep is conforming by construction — the fold is
refused (file untouched) when the current file does not conform, when
[Unreleased] carries no entries, when that version already has a section or
is not newer than every existing one, or when the rewritten file fails the
conformance check above.
"""
from __future__ import annotations

import datetime
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


def fold(path: Path, version: str, date: str) -> None:
    """Fold [Unreleased] entries into a new `## [vX.Y.Z] - date` section.

    The v0.5.0 miss came from hand-assembling the released section: the
    prep commit prepended a non-conforming block and left the hand-written
    entries behind under [Unreleased]. Building the section here makes it
    conforming by construction, and the rewritten file is re-checked before
    the write is kept — non-conforming output is refused, file untouched.
    """
    v = version.lstrip("v")
    if not re.fullmatch(r"\d+\.\d+\.\d+", v):
        fail(f"--fold needs a plain vX.Y.Z version, got {version!r}")
    try:
        datetime.date.fromisoformat(date)
    except ValueError:
        fail(f"--date needs an ISO YYYY-MM-DD date, got {date!r}")

    # Folding must operate on an already-conforming file: laundering a
    # broken one through --fold would hide the breakage the check exists
    # to surface.
    check(path, None)
    sections = parse(path)

    new_v = tuple(int(p) for p in v.split("."))
    for name, _ in sections[1:]:
        m = VERSION_HEADING.match(name)
        if not m:  # unreachable after the check() above — keeps readers honest
            fail(f"heading `## {name}` is not in the documented form")
        existing = tuple(int(g) for g in m.groups()[:3])
        if new_v == existing:
            fail(f"[v{v}] already has a section — a version ships once")
        if new_v < existing:
            fail(f"[v{v}] is not newer than existing `## {name}` — "
                 "version sections run newest-first")

    body = sections[0][1]
    if not body.strip():
        fail("[Unreleased] carries no entries — nothing to fold")

    preamble: list[str] = []
    for line in path.read_text().splitlines():
        if line.startswith("## "):
            break
        preamble.append(line)

    out = ["\n".join(preamble).rstrip(), ""]
    out.append("## [Unreleased]")
    out.append("")
    out.append(f"## [v{v}] - {date}")
    out.append("")
    out.append(body.strip())
    for name, sbody in sections[1:]:
        out.append("")
        out.append(f"## {name}")
        out.append("")
        out.append(sbody.strip())
    new_text = "\n".join(out) + "\n"

    original = path.read_text()
    path.write_text(new_text)
    try:
        check(path, None)
    except SystemExit:
        path.write_text(original)
        fail("fold refused: rewritten file would not conform (file unchanged)")
    print(f"folded [Unreleased] ({body.count('### ')} subsection(s), "
          f"{len(REF.findall(body))} PR ref(s)) into `## [v{v}] - {date}`; "
          "[Unreleased] reset to empty")


def main() -> None:
    args = sys.argv[1:]
    require_released = None
    fold_version = None
    fold_date = datetime.date.today().isoformat()
    paths: list[str] = []
    it = iter(args)
    for arg in it:
        if arg == "--require-released":
            require_released = next(it, None)
            if not require_released:
                fail("--require-released needs a vX.Y.Z argument")
        elif arg == "--fold":
            fold_version = next(it, None)
            if not fold_version:
                fail("--fold needs a vX.Y.Z argument")
        elif arg == "--date":
            fold_date = next(it, None)
            if not fold_date:
                fail("--date needs a YYYY-MM-DD argument")
        else:
            paths.append(arg)
    if fold_version and require_released:
        fail("--fold and --require-released are mutually exclusive")
    path = Path(paths[0]) if paths else Path("CHANGELOG.md")
    if fold_version:
        fold(path, fold_version, fold_date)
    else:
        check(path, require_released)


if __name__ == "__main__":
    main()
