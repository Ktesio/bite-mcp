# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Version headings match release tags: `## [vX.Y.Z] - YYYY-MM-DD`. Pull-request
numbers (`#N`) link to the change on GitHub. The release workflow enforces that
a tag has a matching section here — with [Unreleased] folded into it — before
it can ship.

## [Unreleased]

### Added

- `bite doctor --json` key reference: docs/permissions.md now documents the
  report's full skeleton (every top-level and nested key, with meanings),
  pinned by a test that serializes the sample report and fails when the
  documented keys and the emitted JSON drift in either direction — a new
  check field can no longer ship without the documentation noticing.

- `scripts/check_changelog.py --fold vX.Y.Z [--date YYYY-MM-DD]`: release
  prep no longer hand-assembles the released section. The v0.5.0 prep
  commit prepended a non-conforming block (unbracketed heading, process
  artifacts) while the hand-written entries stayed under [Unreleased];
  the fold now builds the `## [vX.Y.Z] - YYYY-MM-DD` section by
  construction — [Unreleased]'s entries move into it verbatim,
  [Unreleased] resets to empty — and refuses to write when the current
  file does not conform, when [Unreleased] is empty, when the version
  already has a section or is not the newest, or when the rewritten file
  fails the conformance check (the original file is left untouched in
  every refusal case).

### Fixed

- `write_private` fsyncs the file's bytes but never the parent directory,
  so the rename itself was not crash-durable: on power loss an unsynced
  directory entry can lose the rename and revert `config.toml` (or the
  helper install meta) to its previous content — or drop the file — even
  though the new bytes were written and synced. The parent directory is
  now fsynced after the rename (best-effort, like the content sync; a
  no-op on Windows, where opening a directory as a file fails and NTFS
  journals rename metadata).

- `write_private` (the 0600 writer behind `config.toml` and the helper
  install meta) used one fixed temp name, so two concurrent writers to the
  same destination — e.g. two `bite config set` invocations — interleaved
  into the SAME temp file: the surviving rename could publish a mix of
  both payloads, and the loser's rename failed with ENOENT after the
  winner moved the shared temp away. Each write now creates its own
  pid+nanos temp file, opens it 0600 from creation (no umask window
  before the old post-hoc chmod), and cleans the temp up on a failed
  write or rename.

### Security

- CI now declares a workflow-level `permissions: contents: read` block in
  ci.yml. Every job there is read-only (checkout, builds, checks), so the
  workflow no longer inherits the repo-default token scope — the same
  least-privilege shape release.yml already carries for its write needs.
  Closes the `missing-workflow-permissions` code-scanning alerts.

## [v0.5.0] - 2026-10-22

This release adds scoped, durable index purges (index_forget MCP tool and bite index forget CLI), an opt-in skip of the exact-total count scan, and safer helper-death replay semantics. It also enforces data-dir privacy at startup, resolves kill from absolute paths, dispatches requests concurrently, and updates dependencies including rustls-webpki for RUSTSEC-2026-0382.

### Added

- New `index_forget` tool for scoped purges of the local search index:
  `index_forget {app?, container?, confirm?}` drops the LOCAL LanceDB rows
  for one app and/or container (exact names) and immediately compacts the
  FTS fragments so purged bodies stop answering queries — Apple data is
  never touched. Without `confirm` it returns a `would_delete` count; an
  unscoped call is refused (the complete reset stays on `index_wipe`, which
  also clears the crawl watermark). The purge is durable: the confirmed
  scope is recorded (0600 `purge-scopes.json` in the data dir) and later
  crawls skip it — `index_status` lists recorded scopes and ingest reports
  rows skipped for them (`skipped_purged`); `index_wipe` clears the record.
  Retention and purge paths are now covered in PRIVACY.md
  and docs/protocol.md. The purge takes the store's cross-process writer
  lock across its delete + compaction (Busy returns an explicit retryable
  error), so a concurrent ingest in any bite process can never interleave
  with the delete. (#44)
- The CLI grows `bite index forget --app <name> [--container <name>]`, the
  CLI surface for the scoped purge above (preview without `--yes`, delete
  with it — the same confirm gate as the MCP tool). Previously the purge
  was reachable only over MCP while `index_wipe` already had
  `bite index wipe`; scripted/terminal retention hygiene now matches the
  documented toolset. docs/protocol.md notes the CLI form. (#48)
- `index_rebuild` now declares its `no_body` param in the tool registry and
  the CLI grows `bite index rebuild --no-body`. Body-off indexing (mail
  metadata only, message bodies never stored — the data-minimization
  counterpart to `index_forget`'s scoped purge) was already honored by the
  crawl worker but reachable only through an undeclared MCP param, so
  clients could not discover it. (#47)
- New docs-conformance test fence (`docs_conformance`): every tool the
  registry exposes must be documented in docs/protocol.md — helper-facing
  methods in the Method table's group shorthand (both directions, plus a
  required-params check against the `{...}` shorthand), index-plane tools
  by MCP name in the index sections. The method table was previously
  hand-maintained with nothing machine-checking it; the 38-vs-45 as-built
  drift was caught by eye. A registry addition or removal that skips the
  docs now reds CI instead of drifting. (#52)

### Changed

- The MCP server now dispatches each request on its own thread: a slow
  helper round-trip (Apple Events can take the full per-call timeout) no
  longer stalls unrelated traffic — pings, tools/list and other pipelined
  requests are answered while a tool call is still in flight (JSON-RPC
  responses correlate by id). The process-wide bridge handle became
  interior-mutable for this: request threads hold `Arc` clones and the lock
  is touched only for the (rare) spawn or clone, never across a
  round-trip. Helper recycling after a crash/timeout is generation-safe
  (`reset_if_same`): only the failed bridge generation is killed, so a
  helper a sibling thread already respawned survives, and in-flight calls
  on a killed generation surface as the ambiguous `helper_exited`. (#50)

- `with_index` no longer holds the process-wide index mutex across
  opportunistic ingest and the post-crawl FTS refresh: the mutex now guards
  only the lazy open (the handle is shared as an `Arc`). Previously one
  caller's maintenance — a merge-insert plus `optimize`, which can take
  seconds — queued every other index tool call in the process behind it.
  Writes still serialize on the store's cross-process `.writer.lock` and
  opportunistic ingest still deduplicates on `.ingest.lock`, so behavior is
  unchanged for callers; only the queueing is gone. (#45)

### Security

- Data-dir privacy enforcement now runs at every CLI/MCP startup, not only
  `bite setup` — a crawl-first lifecycle (the helper materializes
  `~/Library/Application Support/bite` and `batches/` before setup ever
  ran) left those directories listable at 0755 until a manual setup. The
  enforcement pass itself now also tightens the `batches/` directory
  (previously only its files), the `quarantine/` subdirectory and the
  quarantined files inside it, and the `index.lance` store directory that
  `doctor` already expected at 0700. Creation-time privacy closes the
  mid-session gap for long-running servers: the Swift crawl worker creates
  its directories with 0700 attributes up front, and the Rust ingest path
  chmods a freshly created `quarantine/` at creation. (#49)
- The crawl liveness probe (`pid_alive`) and the crawl-cancel signal now
  resolve `kill` from absolute locations (`/bin/kill`, `/usr/bin/kill` —
  first that executes) instead of PATH, closing the same plantable-binary
  gap the `ps` probes were already hardened from; these were the last
  bare-name spawns in production code. When no kill binary can be executed
  at all the liveness probe now fails safe (crawler assumed alive) instead
  of treating it as dead — the old mapping deleted a live crawler's
  pidfile and double-spawned a second crawl against Mail. (#46)

- Helper-death retries no longer blind-replay mutating tools: when the native
  helper exits mid-call, read-only/idempotent tools are transparently retried
  against the respawned helper, while the send/create/update class (and all
  destructive tools) return an explicit not-replayed error with a re-check
  hint instead — the first attempt may already have committed, so a silent
  retry could double-send mail or duplicate a create. Applies to both the MCP
  server and the CLI dispatch path; classification lives in the tool registry
  and unknown tools default to never replay. (#39)
- New `helper_exited_unsent` transport error distinguishes a request that
  provably never reached the helper (the write failed or the transport was
  already torn down) from a mid-call death of unknown outcome. Unsent
  failures replay safely for every tool, mutating ones included — nothing
  ran the first time — while the ambiguous mid-call death keeps the
  not-replayed guard above. Applies to both the MCP server and the CLI
  dispatch path. (#41)
- `index_forget` purges are now durable across re-crawls: the confirmed
  scope is recorded as a tombstone (0600 `purge-scopes.json` in the data
  dir) and every later ingest skips rows inside it, so a full rebuild
  (`index_rebuild full:true`), the 365-day backfill and a wipe+re-crawl of
  a different index no longer silently re-import what was deliberately
  purged — the registry and docs previously documented exactly that
  undoing. Matching mirrors the purge's delete filter (exact app/container,
  `None` matches any), `index_status` lists the recorded scopes, ingest
  responses report `skipped_purged` when rows were dropped, and
  `index_wipe` remains the complete reset — it clears the tombstones too,
  so a wiped-and-rebuilt index starts clean. (#51)

## [v0.4.0] - 2026-10-18

### Added

- Opt-in `skip_count` on `mail_messages_search`: index-served filter-only
  searches normally pay a second full COUNT scan for the exact `count`;
  passing `skip_count: true` skips it — `count` comes back null (the same
  shape full-text queries return) and the page is unchanged.
- cargo-deny supply-chain checks in CI: license allow-list, wildcard-version
  bans with duplicate-major warnings, and a crates.io-only source policy
  (`deny.toml`), layered onto the cargo-audit lane (#26).
- Mail crawl indexes to-recipient addresses: `participants` is now
  `"sender, to1, to2, …"` (best-effort per-chunk read, capped at 20,
  de-duplicated) — the groundwork for serving `to` searches from the index (#18).
- Machine-readable diagnostics: `bite doctor --json` emits a structured report (#10).
- Incremental mail crawl: subsequent crawls process only the delta window since
  the last completed run instead of a full reindex (#12).
- `cargo-audit` supply-chain lane in CI (#11).

### Changed

- Index-backed `search` returns newest-first for filter-only queries
  (`updated_ms DESC NULLS LAST`) instead of an arbitrary scan-order page,
  matching the live `messages_search` walk; full-text queries keep BM25
  relevance ranking (#34).
- `bite index stats` computes the total, per-app counts and the newest
  `updated_ms` in one projected scan instead of eight sequential index
  queries (#33).
- Index search projects only the columns each hit consumes: `search_text`
  (the full text-search concat, which duplicates title + content) and
  `props` are no longer scanned and shipped per result row (#35).
- The job registry caps terminal-job history: `done`/`failed`/`cancelled` jobs
  are pruned oldest-first beyond 100 entries (running jobs are always kept), so
  `index_status` responses no longer grow without bound over a long-lived
  session (#38).
- Dependency floors raised to `dirs` 7, `toml` 1.1 and `toml_edit` 0.25 (#27).
- Agent-CLI registrations write the resolved absolute `bite` path (dev-checkout
  builds keep the bare `bite`), so GUI-launched clients without `~/.cargo/bin`
  on their PATH can start the server (#36).
- Config is read once per process instead of on every tool call, and
  crawler-liveness probes on read-only status paths are TTL-cached (5 s,
  keyed on the pidfile) — crawl spawn/cancel still probe fresh (#36).
- `bite doctor` honesty: the missing Swift toolchain renders as a warning
  (yellow ○) instead of a failing ✗ — it never counted toward the exit code —
  while a denied crawler Mail probe now DOES count (exit 2, matching its ✗),
  and the broken-helper fix hint prints without `--fix`. `--fix` opens each
  System Settings pane once (mail/notes/messages share one Automation pane)
  and skips apps that aren't installed; `--probe` warns on stderr before it
  can raise permission prompts (#36).
- Index-backed search folds newly crawled rows into the full-text index as soon
  as a crawl settles, so fresh mail is searchable without an explicit rebuild (#14).
- Mail crawl transport ported to OSA/NSAppleScript — the raw AppleEvents path is
  broken on macOS 27 (#3).
- Hardened Mail indexing: self-healing crawl, bounded mirrors, AppleEvents
  diagnostics (#2).

### Fixed

- A wedged helper (alive but stuck in a slow Apple Events call past the
  per-call timeout) is now recycled instead of staying behind the server-wide
  bridge lock — previously every subsequent MCP tool call queued behind the
  stuck process until its 300s budget expired, each while holding the lock.
  The abandoned call is never blindly replayed (it may already have committed
  inside the helper) and its error says so, with a verify-before-reissuing
  hint. The per-call budget gains a `BITE_TIMEOUT_SECS` env override (#37).
- Bulk-operation responses point agents at the real `index_status` tool instead
  of the nonexistent `job_status`.
- `bite setup` no longer suggests `xcode-select --install` for every helper
  failure (the rendered error names its real fix), and closed stdin (EOF) at
  the register prompt now counts as "No" — scripted runs without `--yes`
  register nothing instead of every detected client.
- Background index failures (auto-ingest, post-crawl FTS refresh) surface as
  `last_errors` in `index_status` instead of being visible only on stderr.
- `bite index stats` reports the true newest `updated_ms` — the limit-1
  lookup is now ordered (`updated_ms DESC NULLS LAST`) instead of returning
  an arbitrary row in scan order (#28).
- Index-backed search applies the `subject`, `body`, `from` and `account`
  filters it previously ignored; `unread` results are no longer inverted and
  a `to` filter no longer clobbers `from` — `to` now falls back to the live
  helper (#16).
- Count timeouts during a crawl fall back to blind pagination — one slow mailbox
  no longer fails the whole job (#4).
- Staged crawl batches are auto-ingested on any index touch, with actionable
  status hints (#5).
- Client config writing is safe and atomic, and verified against the documented
  format (#6).
- Storage hygiene: self-cleaning scratch directories, smaller debug profiles,
  doctor storage report (#7).
- Declared the dependency-enforced MSRV (1.91) and added an `msrv` CI lane (#8).

### Security

- Toolchain resolution hardened: `swift` and `codesign` are only accepted from
  securely-owned directories (or their stock system locations) and `xcrun` only
  from its fixed `/usr/bin` path, so a binary planted on a writable `PATH`
  entry can no longer build or sign the installed helper (#15).
- Transitive `rustls` lockfile bump to 0.23.46, clearing four advisories
  published 2026-10-14 (RUSTSEC-2026-0346 … -0349, incl. the medium-severity
  `Debug` exposure of TLS session secrets) (#27).
- CI workflows pin every third-party action to an immutable commit SHA instead
  of a movable tag (#19).
- Client-supplied search limits are capped at 200 at the MCP boundary (#13).

### Documentation

- End-to-end first-time setup walkthrough (#9).

## [v0.3.0] - 2026-09-23

### Added

- Entity index fast path backed by LanceDB (new `bite-index` crate): unified
  search across apps, bulk operations, and mirror management.
- Detached crawl process with staged-batch ingestion, wired into the global index.

### Changed

- Mail search walk is count-free and produces header-only summaries; CLI flag
  ergonomics improved.

### Fixed

- Bulk tools are served by the index layer under the `App::Index` namespace;
  the conformance crawler manifest lists the bulk + search index methods.
- crates.io release pipeline publishes `bite-index` and treats already-published
  crates as success on re-runs.

## [v0.1.1] - 2026-09-22

### Fixed

- `SBApplication.timeout` is in ticks, not seconds — Mail AppleEvents timeouts
  were effectively instantaneous.

## [v0.1.0] - 2026-09-21

Initial public release: native Apple app access over MCP — Rust control plane,
Swift helpers, Mail first.

- Source-only releases; prebuilt/notarized binaries deliberately deferred
  (see docs/ROADMAP.md).
