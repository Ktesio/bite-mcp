# Setup — from `cargo install` to a working agent in five minutes

This is the end-to-end first-time walkthrough: install, `bite setup`,
permission prompts, connecting your agent CLI, and the first real call.
It complements [permissions.md](permissions.md) (the TCC reference — every
prompt and how to recover) rather than repeating it.

## What you need

- macOS 13+ (Ventura or newer)
- Xcode Command Line Tools — `xcode-select --install`. bite ships as
  source: the install compiles two small Swift binaries (the native helper
  and the Mail crawl worker) next to the Rust control plane.
- An agent CLI (optional for the CLI verbs, required for MCP):
  Claude Code, ZCode, Codex, OpenCode, Cursor, VS Code or Gemini CLI.

## Step 1 — install

```bash
cargo install bite-mcp
```

If `cargo` is not installed yet, install Rust first
(`https://rustup.rs`), then re-run. bite lands on `$CARGO_HOME/bin` —
make sure that directory is on your `PATH` (`bite --version` should
print).

Claude Code / ZCode users can alternatively start from the plugin
marketplace (`/plugin marketplace add ktesio/bite-mcp`) — but that path
still needs `bite` on `PATH` first, so the install above happens either
way.

## Step 2 — `bite setup`

```bash
bite setup
```

One command, four stages, in order:

1. **Data dir hardening** (silent) — `~/Library/Application Support/bite`
   and its contents are enforced to 0700/0600, including on upgrades of
   older installs. Nothing is sent anywhere; bite is local-only.
2. **Native helper install** — compiles (first run) or refreshes
   `bite-helper` at a stable path under the data dir. The stable path is
   deliberate: macOS ties permission grants to the binary's path, so a
   helper that moves would re-prompt for everything. If this stage fails,
   the usual fix is `xcode-select --install` and retry.
3. **`bite doctor`** — a prompt-free environment check: macOS version,
   helper presence and capabilities, per-app permission status. This is
   read-only; it never triggers prompts (that's what `--probe` is for —
   see [permissions.md](permissions.md)). Add `--json` for the same report
   as one compact machine-readable object (exit codes unchanged:
   0 clean, 2 problems).
4. **Agent clients** — for every agent CLI it detects (by config file or
   binary on `PATH`), bite asks once:

   ```
   register MCP server in Claude Code? [Y/n]
   ```

   Answer Enter/Y to write the config, `n` to skip that client. Detection
   means "this CLI exists on the machine" — bite never guesses about
   clients you don't have. No agent CLIs at all? It says so and moves on;
   re-run `bite setup` after installing one.

Flags: `--all-clients` (or `--yes`) registers every detected client without
asking — for headless use. The only prompt in the whole run is the per-client
registration question; pressing Enter accepts.

The run ends with the marketplace commands for Claude Code / ZCode and
the next-steps list. Exit code 0 = all good; 2 = some client configs
were left untouched (see "When setup reports leftovers" below).

### What exactly gets written

A single MCP server entry named `bite`, per client's own documented
format — nothing else in the file is touched:

| Client | File | Shape |
|---|---|---|
| Claude Code | `~/.claude.json` (or `$CLAUDE_CONFIG_DIR/.claude.json`) | `"mcpServers": { "bite": … }` |
| ZCode | `~/.zcode/settings.json`, else `~/.zcode/config.json` | `"mcpServers"` |
| Codex CLI | `~/.codex/config.toml` (or `$CODEX_HOME/config.toml`) | `[mcp_servers.bite]` table |
| OpenCode | `~/.config/opencode/opencode.json[c]` | `"mcp": { "bite": … }` |
| Cursor | `~/.cursor/mcp.json` | `"mcpServers"` |
| VS Code | `~/Library/Application Support/Code/User/mcp.json` | `"servers"` with `"type": "stdio"` |
| Gemini CLI | `~/.gemini/settings.json` | `"mcpServers"` |

All entries are the same server: `bite mcp` over stdio.

The writing itself is defensive by design:

- **Atomic.** The new config is written to a temp file in the same
  directory, fsynced, then renamed over the target — an interrupted
  setup can never leave a truncated or zero-byte config behind.
- **Never clobbers what it can't parse.** A config that fails to parse
  (strict JSON with comments in it, trailing commas, hand edits) is left
  untouched and reported — bite would rather do nothing than destroy
  your file.
- **Backups.** Before first modifying a non-empty config, bite saves
  `<name>.bite-bak` from the original bytes. The first backup is never
  overwritten.
- **Symlinks are refused.** Dotfile-manager-managed configs (stow,
  chezmoi) are skipped with a note to merge the entry manually — rename
  would silently sever the link.
- **Permissions never widen.** Replacements keep the file's existing
  mode; new files are 0600.
- **Comments survive.** OpenCode's JSONC is edited with a
  comment-preserving parser; formatting and comments outside the
  inserted key come out unchanged.
- **Idempotent.** Running setup twice yields identical bytes.

### When setup reports leftovers

If a client config could not be written safely, setup prints a summary
on stderr and exits 2 — every other client was still handled. Fix the
reported file (usually: resolve the syntax error bite names) and re-run
`bite setup`. Two other notes you might see:

- **legacy scar note** — files left behind by pre-0.3.1 writers are
  reported with a hint but never deleted by bite.
- **"no agent CLIs detected"** — nothing to configure yet; the CLI verbs
  (`bite reminders lists`, …) work without any client config.

## Step 3 — permission prompts (once)

The first real call per app family raises macOS's own prompt once:

- **Calendar / Reminders / Contacts** — "bite-helper would like access…",
  attributed to the helper's stable path (granted once, stays granted).
- **Mail / Notes / Messages** — "'Terminal' wants to control Mail" (or
  your agent CLI's name), attributed to the parent app of the process
  chain.
- **Mail indexing** — the crawl worker is a separate binary with its own
  TCC identity, so it raises one more "wants to control Mail" prompt the
  first time indexing starts.

Approve them from a real terminal when they appear (prompts raised from
deep inside an agent session can be awkward to attribute — running the
first call yourself avoids the confusion). The full prompt matrix,
including Messages history's Full Disk Access requirement, is in
[permissions.md](permissions.md).

Denied something later or never saw a prompt? `bite doctor --fix` opens
the matching System Settings pane for anything denied.

## Step 4 — restart the agent CLI

Agent CLIs read their MCP config at startup. After setup writes an
entry, **restart the CLI** (or reload the window in VS Code) before
expecting `bite` tools to appear. `bite doctor` shows a per-client line
(`✓` configured, `○` detected but not configured, blank = not found) to
confirm what each CLI will pick up.

## Step 5 — first call

Verify the whole chain from your terminal first — this raises the
permission prompts in your own context, not the agent's:

```bash
bite reminders lists
bite calendar availability --from "today 9am" --to "today 6pm"
bite mail search --mailbox INBOX --unread --limit 10
```

Then ask your agent. In Claude Code / ZCode the tools are namespaced
under the `bite` MCP server; elsewhere the 45 tools (`calendar_search`,
`mail_send`, `notes_create`, `messages_send`, `contacts_search`, …)
appear in the client's normal tool list. Mail searches stay fast because
the index builds in the background after first use — see the README's
Performance section.

## Uninstall / clean slate

- Remove bite from an agent CLI: delete the `bite` entry from the file
  listed in the table above (or your dotfiles repo if you manage configs
  that way — bite refuses to write through symlinks, so it never edited
  the link itself).
- Remove the binary: `cargo uninstall bite-mcp`.
- Remove all local data (index, helper, config): delete
  `~/Library/Application Support/bite`. Nothing else on the system
  references it; macOS permission grants for the helper's old path
  become inert when the binary is gone.

## Quick troubleshooting

| Symptom | First move |
|---|---|
| `bite: command not found` after install | `$CARGO_HOME/bin` not on `PATH` |
| helper install fails in setup | `xcode-select --install`, retry `bite setup` |
| agent doesn't list bite tools | restart the CLI; then `bite doctor` (client lines) |
| setup exit 2 | read the stderr summary — fix the named config, re-run |
| permission denied on a call | `bite doctor --fix`; details in [permissions.md](permissions.md) |
| Mail tools work but indexing fails | crawler TCC identity — see the macOS 27+ section of [permissions.md](permissions.md) |
