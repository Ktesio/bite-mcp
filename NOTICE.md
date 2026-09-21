# NOTICE — bite-mcp

Copyright © 2026 the bite-mcp authors. Licensed under the MIT License
(see [LICENSE](LICENSE)).

## Trademarks

bite is an independent open-source project. It is **not affiliated with,
sponsored, or endorsed by Apple Inc.**

"Apple", "macOS", "iMessage", "iCloud", "Mail", "Notes", "Messages",
"Calendar", "Reminders", "Contacts", "Safari", "Finder", "Music",
"Shortcuts" and the Apple logo are trademarks of Apple Inc., registered in
the U.S. and other countries. They are used here solely to describe
compatibility and the applications this software interoperates with, on and
for Apple's own operating system. No endorsement is implied.

"Model Context Protocol" and "MCP" are used descriptively to refer to the
open protocol this server implements.

## Third-party data

This software interoperates with Apple applications and the personal data
they hold on the user's own device, using Apple's public frameworks
(EventKit, Contacts, ScriptingBridge/Apple Events) under macOS's standard
permission system. All such data remains the property of the user of the
device; the project's authors do not collect, transmit, or retain any of it.

## Dependencies

Rust dependencies are declared in `crates/*/Cargo.toml` and distributed under
their own MIT/Apache-2.0 (or equivalent permissive) licenses. The Swift
helper uses only Apple system frameworks.
