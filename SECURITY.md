# Security Policy

## Reporting a vulnerability

Please do **not** open a public issue for a security problem.

Use GitHub's **private vulnerability reporting**
([Security → Report a vulnerability](https://github.com/ktesio/bite-mcp/security/advisories/new)),
or contact a maintainer directly if that is unavailable. You will get an
acknowledgement within a few days and a fix timeline for confirmed issues.

## Scope

bite is a local-only tool: two processes on your Mac, no servers. The
security-relevant surface is:

- **The local bridge protocol** (`docs/protocol.md`) between `bite` and its
  Swift helper over stdio — e.g. anything that would let another local
  process impersonate or hijack the helper.
- **Permission boundaries** — anything that lets bite access data *without*
  the corresponding macOS TCC grant, or act destructively without the
  `confirm: true` handshake.
- **Injection through agent input** — bite deliberately runs no shell, no
  AppleScript strings, and no dynamic code; report anything that changes that
  (e.g. a path where app-controlled strings reach an interpreter).
- **The toolchain invocation path** — `bite install-helper` compiles the
  Swift helper with `swift` resolved through secure-path rules (every
  directory in the chain root- or user-owned and not group/world-writable,
  the tool a real executable file, never a symlink) and stages the result
  into the location macOS permissions are granted to. Report anything that
  lets an untrusted `swift`/`codesign`/`xcrun` produce or sign the
  installed helper.
- **The config writers** (`bite setup`) — they edit other applications' MCP
  config files; report anything that could corrupt or clobber unrelated
  settings.

Out of scope: vulnerabilities in the macOS permission system itself, in the
AI agents bite connects to, or social-engineering of the user into granting
permissions.

## Supported versions

Security fixes land on `main`; users should update via
`cargo install bite-mcp` (or their install channel). Given the pre-1.0 stage,
only the latest release receives fixes.
