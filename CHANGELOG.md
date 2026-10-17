# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Version headings match release tags: `## [vX.Y.Z] - YYYY-MM-DD`. Pull-request
numbers (`#N`) link to the change on GitHub. The release workflow enforces that
a tag has a matching section here before it can ship.

## [Unreleased]

### Added

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
- Dependency floors raised to `dirs` 7, `toml` 1.1 and `toml_edit` 0.25 (#27).
- Index-backed search folds newly crawled rows into the full-text index as soon
  as a crawl settles, so fresh mail is searchable without an explicit rebuild (#14).
- Mail crawl transport ported to OSA/NSAppleScript — the raw AppleEvents path is
  broken on macOS 27 (#3).
- Hardened Mail indexing: self-healing crawl, bounded mirrors, AppleEvents
  diagnostics (#2).

### Fixed

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
