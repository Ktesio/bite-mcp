# Configuration reference

Every knob `bite` reads at runtime: the TOML config file, the `bite config`
CLI verb, and the environment hooks. Location, accepted keys, defaults, and
precedence are all defined by code — this page mirrors `crates/bite-core/src/config.rs`,
`crates/bite-mcp/src/dispatch.rs` (the `config` verb) and `crates/bite-mcp/src/helper.rs`.

## The config file

- **macOS path:** `~/Library/Application Support/bite/config.toml`
  (`bite config path` prints it for the current machine).
- The file is optional. Missing file, unparseable TOML, or a file with
  unknown keys all fall back to defaults — unknown keys are ignored
  silently, and a malformed file is ignored **whole** (not per-key).
- It is written mode `0600` by `bite config set`; hand-editing is fine too.
- Config is read **once per process** and cached for its lifetime. A
  long-lived `bite mcp` server keeps the config it started with; every
  short-lived CLI invocation re-reads the file.

## Keys

| Key | Type | Default | Effect |
|---|---|---|---|
| `default_calendar` | string | none | Used as the `calendar` argument for calendar event creation and event searches when the request itself doesn't specify a calendar. When unset, no default is injected — the request goes through as-is. |
| `default_mail_account` | string | none | Used as the `account` argument for **every** `mail.*` call (send, reply, search, …) when the request doesn't name an account. |
| `timeout_secs` | number | `300` | Per-request bridge timeout: how long the Rust side waits for the Swift helper to answer one call. Slow Apple Event queries on large mailboxes can legitimately take a while. |
| `release_repo` | string | none | `owner/repo` reserved for prebuilt-helper downloads. Accepted and persisted, but currently **no code consumes it** — helper installation resolves locally (see below). Kept for the download path. |
| `auto_refresh_hours` | number | `24` | How old the entity index may get before a background refresh crawl is spawned when you read mail through bite. File-only: `bite config set` does not accept this key. |

## The `bite config` verb

```
bite config path          # print the config file path
bite config show          # print the current config as JSON
bite config set KEY VALUE # set one key and save (0600)
```

`config set` accepts kebab-case or snake_case for these keys (and only
these — anything else fails with an unknown-key error):

- `default-calendar`
- `default-mail-account`
- `timeout-secs` (value must parse as a number)
- `release-repo`

`auto-refresh-hours` is **not** settable through the verb — edit the file
directly.

## Environment hooks

| Variable | Effect |
|---|---|
| `BITE_TIMEOUT_SECS` | Overrides `timeout_secs` from the config file (a one-off ops lever that needs no file edit). Must parse as a number — a non-numeric value is silently ignored, and the file value (or the 300 s default) applies. |
| `BITE_HELPER_BIN` | Use this exact path as the helper binary (tests / power users). If set but the file is missing, bite fails the call with an explicit spawn error instead of falling through. |
| `BITE_HELPER_BUILT` | Path to a just-built helper artifact; bite copies it to the stable install path so macOS TCC grants keep sticking to one location. |

## Helper binary resolution

`BITE_HELPER_BIN` aside, bite locates the helper in this order:

1. the stable install path (`…/bite/bin/bite-helper`) if present,
2. `BITE_HELPER_BUILT`, copied to the stable path,
3. compile-on-demand from the Swift sources packaged with the crate.

## Precedence, end to end

For the one value with three sources — `BITE_TIMEOUT_SECS` env >
`timeout_secs` in the file > built-in `300`. Every other setting lives in
the file only (or, for the helper paths, in the environment only).

## Auto-refresh behavior

When a mail read finds the index older than `auto_refresh_hours`, bite
spawns a background refresh crawl — unless one is already running. Checks
are rate-limited to one every 15 minutes regardless of how many reads
happen in between.
