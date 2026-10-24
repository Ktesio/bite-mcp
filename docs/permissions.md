# Permissions (TCC) — what prompts, when, and how to recover

bite uses Apple's own frameworks, so macOS permission prompts are part of
first use. There is no way (and no legitimate way) around them; bite's job is
to make them obvious, one-time, and recoverable.

## The four permission families

| Family | Apps | Prompt | Stable after grant? |
|--------|------|--------|--------------------|
| Calendars | Calendar | "bite-helper would like access to your calendars" | yes (helper path is stable) |
| Reminders | Reminders | same, for reminders | yes |
| Contacts | Contacts | "…full access to contacts" | yes |
| Apple Events (per app) | Mail, Notes, Messages | "'Terminal' wants to control Mail" — attributed to the *parent* app of the process chain (your terminal or the agent CLI) | yes once granted to that parent |

Messages **history** additionally reads `~/Library/Messages/chat.db`, which
needs **Full Disk Access** for the terminal/agent app. Without FDA,
`messages_history` degrades to active-chats data; send/list always work.

## Doctor: status checks are prompt-free

`bite doctor`'s status checks read TCC *status* only (no prompts):

```
✓ calendar     a system prompt appears on first use
✗ reminders    denied in System Settings
○ mail         will prompt on first use
```

- `bite doctor --probe` additionally live-tests the Apple Events grants for
  Mail/Notes/Messages — this *does* trigger the prompts if pending.
- Once the crawl worker is installed, plain `bite doctor` also runs its
  one-shot Mail probe (`bite-crawl --probe-mail`) — a real Apple Event that
  can raise the Mail automation prompt once. It distinguishes `authorized` /
  `no_accounts` / `denied_or_empty`: a zero-account Mail is a configuration
  state, not a TCC denial, and `denied_or_empty` also covers Mail simply not
  running — launch Mail before permission-chasing.
- `bite doctor --fix` opens the matching System Settings pane for anything
  denied.
- `bite doctor --json` prints the same report as one compact JSON object
  (helper, per-app raw states, storage, clients) for scripts and CI; the
  exit code (0 clean / 2 problems) is identical to the human form. The full
  key-by-key shape is pinned in [`bite doctor --json` shape](#bite-doctor---json-shape)
  below.

## `bite doctor --json` shape

Every key is always present — sections that don't apply serialize as `null`
or `[]` (`apps` is empty when the helper is down, because nothing was
probed; `dev_target` is `null` for installed copies). The skeleton, exactly
as emitted:

```json
{
  "os": "macOS 15 (major 15)",
  "toolchain": "/usr/bin/swiftc",
  "helper": {
    "ok": true,
    "path": "/x/bite/bin/bite-helper",
    "capabilities": ["calendar", "mail"],
    "error": null
  },
  "apps": [
    { "app": "calendar", "state": "authorized", "ok": true, "note": null }
  ],
  "crawler_mail": { "state": "denied", "detail": "not authorized" },
  "storage": {
    "data_dir": "/x/bite",
    "total_bytes": 4096,
    "index_bytes": 2048,
    "bin_bytes": 1024,
    "batches_bytes": 512,
    "batches_pending": 1,
    "batches_quarantined": 2,
    "scratch_removed_count": 1,
    "scratch_removed_bytes": 700,
    "scratch_in_progress": 0,
    "scratch_pending_install_count": 0,
    "scratch_pending_install_bytes": 0,
    "tmpdir_bite_entries": 3,
    "tmpdir_swept_count": 2,
    "tmpdir_swept_bytes": 900
  },
  "dev_target": { "root": "/ws", "target_bytes": 3221225472 },
  "clients": [
    { "key": "claude", "display": "Claude Code", "detected": true, "installed": true }
  ],
  "problems": 1
}
```

| Key | Meaning |
|-----|---------|
| `os` | `sw_vers` detail, e.g. `macOS 15 (major 15)`; `null` when not macOS 13+ |
| `toolchain` | `swiftc` path; `null` = no Swift toolchain (informational — only source-compile installs need it) |
| `helper` | bridge state: `ok`, installed `path`, `capabilities[]`, rendered `error` when the helper is missing/broken |
| `apps[]` | per-app TCC probe: `app`, raw `state`, workable `ok`, optional human `note`; empty when the helper is down — check `helper.ok` before reading meaning into it |
| `crawler_mail` | crawl-worker Mail probe: `state` (`authorized` \| `no_accounts` \| `denied` \| `failed`) + raw `detail`; `null` when `bite-crawl` isn't installed |
| `storage` | data-dir usage in raw bytes (scripts do their own units): `index`/`bin`/`batches_*` sizes and counts, scratch-sweep results, `$TMPDIR` litter counts |
| `dev_target` | `null` for installed copies; build-tree `root` + `target_bytes` when the exe runs out of a cargo `target/` |
| `clients[]` | per-client integration state: `key`, `display`, `detected` (binary or config found), `installed` (MCP server entry written) |
| `problems` | failed-check count — the human `N issue(s) found` line and the exit code (0 clean / 2 problems) |

The skeleton above is pinned by a test: a report field added, renamed, or
dropped without updating this page (or the other way around) fails CI.

## Recovery flows

- **Prompt pending** (`not_determined`): the prompt appears on the first real
  call in an interactive session. Approve once; done forever.
- **Denied** (`denied`): the tool error contains a `fix` — e.g.
  `open 'x-apple.systempreferences:com.apple.preference.security?Privacy_Reminders'`.
  Toggle the app in the list; the next call just works. Or run
  `bite doctor --fix`.
- **Silent denial in non-interactive shells** (CI, agents without a GUI
  session): ScriptingBridge calls fail quietly or return empty results. bite
  maps this to `permission_denied` / `app_missing` — run the same call from a
  real terminal once to trigger the prompt.

## Why prompts may repeat after upgrades

TCC attributes Apple Events permission to the *requesting app chain* (your
terminal or agent CLI), not to bite itself, and attributes helper-level
grants to the helper binary at its install path. Two rules keep grants
stable:

1. The helper lives at a fixed path
   (`~/Library/Application Support/bite/bin/bite-helper`) — refreshes via
   `bite install-helper` update it in place, preserving grants.
2. If you switch terminals/agent CLIs, the first Apple Events call from the
   new parent prompts once.

`codesign` ad-hoc signature is applied on every install. bite ships
source-only (compilation happens on the user's machine), so Gatekeeper never
stands between users and the helper; a notarized prebuilt channel is a
considered Phase 2 addition (ROADMAP).

## macOS 27+: Apple Events go through OSA

From macOS 27, raw (non-OSA) Apple Event sends return silent empty replies.
bite sends through OSA everywhere — the helper via ScriptingBridge, the Mail
crawl worker via NSAppleScript — so prompts and grants behave exactly as
documented above.

One caveat: `bite-crawl` is a separate binary with its own TCC identity.
Mail may be granted to the helper yet denied for the crawler — and that
denial is silent: the crawl sits in `waiting_mail` (60 s heartbeats, 24 h
cap) and then reports `failed`; it never returns a `permission_denied` tool
error. If indexing keeps failing while live Mail tools work, run
`bite doctor` — the crawler Mail automation line distinguishes
`denied_or_empty` from `no_accounts` — and approve the Mail automation
prompt once from a real terminal.
