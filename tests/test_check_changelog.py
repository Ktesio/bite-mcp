#!/usr/bin/env python3
"""Self-test battery for scripts/check_changelog.py.

The fence itself is fenced: the changelog CI lane runs only the default
check mode against the real CHANGELOG.md, so nothing in the repo exercised
a single refusal — a future edit that loosens a regex or breaks a refusal
kept CI green and the fence silently stopped fencing. This battery calls
the script in the true CLI shape (subprocess, exit codes, stderr text and
file bytes asserted) against synthetic valid files and tampered variants,
asserting BOTH directions: green on valid input, red on each malformed
shape the script documents.

The `--fold` release-prep cases probe for fold support first and self-skip
(with a visible note) when the deployed script predates the mode, so this
battery stays green regardless of merge order; on a tree that carries
`--fold` the full battery runs.

Run: python3 tests/test_check_changelog.py   (exit 0 = all cases green)
"""
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
SCRIPT = REPO / "scripts" / "check_changelog.py"

PREAMBLE = (
    "# Changelog\n"
    "\n"
    "All notable changes to this project are documented in this file.\n"
    "\n"
    "Version headings match release tags: `## [vX.Y.Z] - YYYY-MM-DD`.\n"
)

# Valid base: one entry under [Unreleased], two released sections
# newest-first, one PR ref each.
VALID = (
    PREAMBLE
    + "\n"
    "## [Unreleased]\n"
    "\n"
    "### Fixed\n"
    "\n"
    "- synthetic unreleased entry (#42)\n"
    "\n"
    "## [v1.1.0] - 2026-10-01\n"
    "\n"
    "### Added\n"
    "\n"
    "- synthetic released entry (#41)\n"
    "\n"
    "## [v1.0.0] - 2026-09-01\n"
    "\n"
    "### Fixed\n"
    "\n"
    "- older released entry (#40)\n"
)

# The v0.5.0 mis-fold shape: unbracketed prepended heading + process
# artifacts, entries still under [Unreleased].
MISFOLD = (
    "# Changelog\n"
    "\n"
    "## v1.2.0\n"
    "\n"
    "Bump: 1.2.0\n"
    "Range: v1.1.0..HEAD\n"
    "\n"
    "### Commits\n"
    "\n"
    "- abc123 fix\n"
    "\n"
    "## [Unreleased]\n"
    "\n"
    "### Fixed\n"
    "\n"
    "- an entry\n"
    "\n"
    "## [v1.1.0] - 2026-10-01\n"
    "\n"
    "body (#41)\n"
)

results = []


def run(args, path):
    return subprocess.run(
        [sys.executable, str(SCRIPT), *args, str(path)],
        capture_output=True, text=True,
    )


def case(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(("PASS " if ok else "FAIL ") + name + (f" — {detail}" if detail and not ok else ""))


def red(name, r, *snippets, path=None, original=None):
    """Assert a refusal: exit 1, every snippet in stderr, file untouched."""
    ok = r.returncode == 1 and all(s in r.stderr for s in snippets)
    if path is not None and original is not None:
        ok = ok and path.read_text() == original
    case(name, ok, f"rc={r.returncode} stderr={r.stderr.strip()[:300]}")


tmp = tempfile.TemporaryDirectory(prefix="bite-changelog-selftest-")
work = Path(tmp.name)


def fixture(name, text):
    p = work / name
    p.write_text(text)
    return p


# --- default check mode: green --------------------------------------------
r = subprocess.run([sys.executable, "-m", "py_compile", str(SCRIPT)],
                   capture_output=True, text=True)
case("script compiles", r.returncode == 0, r.stderr)

valid = fixture("valid.md", VALID)
r = run([], valid)
case("valid file passes default check",
     r.returncode == 0 and "changelog OK" in r.stdout,
     f"rc={r.returncode} out={r.stdout} err={r.stderr}")

# Same PR cited twice inside ONE released section is legitimate (the #36
# precedent: one PR, several facets) — must stay green.
twice = VALID.replace(
    "- synthetic released entry (#41)\n",
    "- synthetic released entry (#41)\n- another facet (#41)\n",
)
p = fixture("twice.md", twice)
r = run([], p)
case("same ref twice inside one released section stays green",
     r.returncode == 0, f"rc={r.returncode} err={r.stderr}")

# Release guard green: [Unreleased] empty, section exists.
guard_ok = VALID.replace(
    "### Fixed\n\n- synthetic unreleased entry (#42)\n\n## [v1.1.0]",
    "## [v1.1.0]",
)
p = fixture("guard_ok.md", guard_ok)
r = run(["--require-released", "v1.1.0"], p)
case("empty [Unreleased] passes release guard",
     r.returncode == 0 and "release-guard clean" in r.stdout,
     f"rc={r.returncode} out={r.stdout} err={r.stderr}")

# The real repo changelog must pass the default check (same assertion the
# CI lane makes; pinned here so the battery cannot drift from the lane).
r = run([], REPO / "CHANGELOG.md")
case("repo CHANGELOG.md passes default check", r.returncode == 0, r.stderr)

# --- default check mode: red ----------------------------------------------
p = fixture("no_title.md", VALID.replace("# Changelog", "# History", 1))
red("missing `# Changelog` title refused", run([], p), "title line")

p = fixture("no_sections.md", PREAMBLE + "\njust body text, no sections\n")
red("no `## ` sections refused", run([], p), "no `## ` sections")

p = fixture("no_unreleased.md", VALID.replace("## [Unreleased]\n\n### Fixed\n\n- synthetic unreleased entry (#42)\n\n", "", 1))
red("[Unreleased] missing refused", run([], p), "exactly once, found 0")

p = fixture("dup_unreleased.md", VALID + "\n## [Unreleased]\n\n### Added\n\n- dup (#43)\n")
red("[Unreleased] duplicated refused", run([], p), "exactly once, found 2")

# Version section prepended ABOVE [Unreleased] (the v0.5.0 shape).
p = fixture("unreleased_not_first.md",
            PREAMBLE + "\n## [v1.2.0] - 2026-10-10\n\n- x (#44)\n\n" + VALID.split(PREAMBLE, 1)[1].lstrip("\n"))
red("[Unreleased] not first refused", run([], p), "must be the first")

p = fixture("unbracketed.md", VALID.replace("## [v1.0.0] - 2026-09-01", "## v1.0.0 - 2026-09-01"))
red("unbracketed heading refused", run([], p), "not in the documented")

# The actual v0.5.0 mis-fold shape: prepended unbracketed section above
# [Unreleased] — caught by the Unreleased-first rule (it runs first).
p = fixture("misfold.md", MISFOLD)
red("prepended version section refused", run([], p), "must be the first")

p = fixture("bad_date.md", VALID.replace("## [v1.1.0] - 2026-10-01", "## [v1.1.0] - 2026-1-1"))
red("non-ISO date in heading refused", run([], p), "not in the documented")

p = fixture("old_order.md", VALID.replace("## [v1.0.0] - 2026-09-01", "## [v1.2.0] - 2026-09-01"))
red("not newest-first refused", run([], p), "newest-first")

p = fixture("dup_ref.md", VALID.replace("- synthetic unreleased entry (#42)",
                                        "- unreleased repeat of a shipped ref (#41)"))
red("ref in [Unreleased] already released refused", run([], p), "already cited")

p = fixture("guard_missing_ver.md", guard_ok)
red("release guard without matching section refused",
    run(["--require-released", "v9.9.9"], p), "no `## [v9.9.9]` section")

p = fixture("guard_unrel_entries.md", VALID)
red("release guard with entries still in [Unreleased] refused",
    run(["--require-released", "v1.1.0"], p), "still carries entries")

r = subprocess.run([sys.executable, str(SCRIPT), "--require-released"],
                   capture_output=True, text=True)
red("--require-released without value refused", r, "needs a vX.Y.Z argument")

# --- --fold release-prep mode ----------------------------------------------
# Probe: a deployed script without fold support treats "--fold" as a path
# (FileNotFoundError traceback) instead of demanding its argument value.
probe = subprocess.run([sys.executable, str(SCRIPT), "--fold"],
                       capture_output=True, text=True)
fold_supported = probe.returncode == 1 and "needs a vX.Y.Z" in probe.stderr

if not fold_supported:
    print("SKIP --fold cases — deployed scripts/check_changelog.py predates "
          "the fold mode (merge + rebase to activate them)")
else:
    p = fixture("fold.md", VALID)
    original = p.read_text()
    r = run(["--fold", "v1.2.0", "--date", "2026-10-24"], p)
    folded = p.read_text()
    case("fold succeeds",
         r.returncode == 0 and "folded [Unreleased]" in r.stdout,
         f"rc={r.returncode} out={r.stdout} err={r.stderr}")
    case("[Unreleased] empty after fold",
         folded.split("## [Unreleased]", 1)[1].split("## [v1.2.0]", 1)[0].strip() == "")
    case("entries moved into the new section",
         "### Fixed" in folded.split("## [v1.2.0]", 1)[1].split("## [v1.1.0]", 1)[0]
         and "(#42)" in folded.split("## [v1.2.0]", 1)[1].split("## [v1.1.0]", 1)[0])
    case("preamble preserved", folded.startswith(PREAMBLE.rstrip("\n")))
    case("older sections retained",
         "## [v1.1.0] - 2026-10-01" in folded and "## [v1.0.0] - 2026-09-01" in folded)
    case("file single-newline-terminated",
         folded.endswith("\n") and not folded.endswith("\n\n"))
    case("folded file passes default check", run([], p).returncode == 0)
    case("folded file passes release guard",
         run(["--require-released", "v1.2.0"], p).returncode == 0)

    red("re-fold of an existing version refused",
        run(["--fold", "v1.2.0"], p), "already has a section", path=p, original=folded)
    red("older version refused",
        run(["--fold", "v1.0.0"], p), "not newer", path=p, original=folded)
    red("empty [Unreleased] refused",
        run(["--fold", "v9.9.9"], p), "nothing to fold", path=p, original=folded)
    red("bad date refused",
        run(["--fold", "v9.9.8", "--date", "2026-13-99"], p), "ISO", path=p, original=folded)
    red("bad version refused",
        run(["--fold", "1.2"], p), "plain vX.Y.Z", path=p, original=folded)

    p = fixture("fold_mis.md", MISFOLD)
    red("non-conforming input refused (no laundering)",
        run(["--fold", "v1.2.0"], p), path=p, original=MISFOLD)

    p = fixture("fold_mutex.md", VALID)
    red("--fold + --require-released refused",
        run(["--fold", "v1.2.0", "--require-released", "v1.1.0"], p), "mutually exclusive")

tmp.cleanup()

fails = [n for n, ok, _ in results if not ok]
print(f"\n{len(results) - len(fails)}/{len(results)} passed"
      + ("" if fold_supported else " (--fold cases skipped on this tree)"))
sys.exit(1 if fails else 0)
