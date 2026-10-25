# Configuration — config.toml keys, environment hooks, precedence

bite reads all user-tunable settings from one optional TOML file. This page
is the complete reference: every key the config accepts, the environment
variables that override or extend it, and how the layers combine. A
conformance test (`crates/bite-core/tests/config_docs.rs`) pins this page
against the `Config` serializer, so a key that ships without a row here —
or a row that outlives its key — fails CI instead of drifting.

## The file

`bite config path` prints the exact location:

- macOS: `~/Library/Application Support/bite/config.toml`

The file is optional. Every key has a default, and a missing or unparsable
file is treated as empty (defaults apply). `bite config set` creates the
file on first write, mode `0600` — the config is not world-readable.

Config is read once per process and cached: a long-lived `bite mcp` server
serves the config it started with and must be restarted to pick up edits.
The only writer is `bite config set`, which runs in its own short-lived
process.

## Keys

| Key | Type | Default | Effect |
|-----|------|---------|--------|
| `default_calendar` | string | unset | Calendar name used to create events when a tool call passes no calendar. |
| `default_mail_account` | string | unset | Mail account used for sends when a tool call passes no account. |
| `timeout_secs` | integer | `300` | Per-request bridge timeout in seconds: how long a single Rust→helper round-trip may run before it is abandoned. A timeout routes into the same helper-reset path as a dead helper — the tool call fails, the server does not hang. |
| `release_repo` | string | unset | GitHub repository (`owner/repo`) prebuilt helper downloads resolve against. |
| `auto_refresh_hours` | integer | `24` | Age threshold for the entity index: once the last crawl is older than this many hours, the next tool call opportunistically starts a background refresh (the call itself is never blocked waiting on it). |

`bite config set` accepts `default-calendar`, `default-mail-account`,
`timeout-secs` and `release-repo` (snake_case spellings also accepted, e.g.
`bite config set default_calendar Home`). `auto_refresh_hours` is
file-only. `bite config get` prints the merged result as JSON.

## Environment hooks

| Variable | Effect |
|----------|--------|
| `BITE_TIMEOUT_SECS` | Overrides `timeout_secs` — wins over the file. A value that does not parse as an integer is silently ignored and the file value applies. |
| `BITE_HELPER_BIN` | Absolute path to the helper binary. When set, it takes precedence over every other resolution step (stable install path → build-time baked path → compile-on-demand). Set but pointing at a missing file is a hard error — it never silently falls back. |

`BITE_HELPER_BUILT` and `BITE_SKIP_SWIFT_BUILD` also exist, but they are
build-time hooks for cargo builds and CI, not user configuration.

## Precedence

For `timeout_secs` (the only key with an environment override):

1. `BITE_TIMEOUT_SECS` (if set and parseable)
2. `timeout_secs` from config.toml
3. built-in default (`300`)

Every other key follows: config.toml value, else built-in default. The
helper binary location resolves `BITE_HELPER_BIN`-first as described
above, independent of config.toml.
