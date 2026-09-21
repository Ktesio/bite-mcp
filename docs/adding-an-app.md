# Adding an app (or extending one)

Three layers must stay in sync; tests catch drift.

## 1. Swift handler

Add a method to the right bridge module (or a new `FooBridge.swift` for a new
app) in `crates/bite-mcp/swift/Sources/BiteBridge/`:

```swift
enum FooBridge {
    static func register(_ d: Dispatcher) {
        d.register("foo.do_thing") { req in
            try withWatchdog(30) {
                // dynamic ScriptingBridge for scriptable apps:
                let app = try sbApp(bundleID: "com.apple.foo", name: "Foo")
                let items = sbElements(app, "items")
                // ... build a flat snake_case dict
                return ["things": []]
            }
        }
    }
}
```

Register it in `main.swift`. Rules:

- Flat snake_case dicts; dates through `D.format` (ISO 8601 + offset).
- Errors are `BridgeError` with a `code`, and a `fix` when user-actionable.
- Wrap every Apple-calling handler in `withWatchdog`.
- Access SB elements/properties via `sbElements` / `sbGet` / `sbAt` — never
  raw `object(at:)` indexing (convention is pinned in one place).
- If the app has a *native framework* (like Contacts or EventKit), prefer it
  over ScriptingBridge.

## 2. Rust registry

Add the tool to `crates/bite-core/src/registry.rs`:

```rust
t("foo_do_thing", App::Calendar /* or a new variant */, "foo.do_thing",
  "One-paragraph description for the agent.",
  ps![
      ParamSpec::req("id", ParamKind::Str, "what this is"),
      ParamSpec::opt("limit", ParamKind::Int, "max results"),
  ]),
```

The MCP schema and CLI dispatch both come from here. If you added a new `App`
variant, extend `as_str`.

Destructive verbs: use `td(...)` and take `confirm` — the helper returns a
`would_*` preview until it's true.

## 3. CLI verb

Add a clap variant in `crates/bite-mcp/src/cli.rs` and an arm in
`crates/bite-mcp/src/dispatch.rs` that builds the same param names and calls
`call(&mut h, "foo_do_thing", params)`. CLI date flags go through
`parse_date` (accepts relaxed forms).

## 4. Prove it

```bash
swift test --package-path crates/bite-mcp/swift   # logic tests
cargo test --workspace                             # includes
#   - tests/protocol_conformance.rs   ← fails if Swift/registry drift
#   - tests/mcp_smoke.rs              ← end-to-end MCP
./target/debug/bite foo do-thing                   # live smoke
```

Update `plugin/skills/bite/SKILL.md` (tool map + recipes) and `docs/protocol.md`
(method table) in the same PR.
