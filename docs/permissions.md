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

## Doctor: prompt-free by default

`bite doctor` reads TCC *status* only (no prompts):

```
✓ calendar     a system prompt appears on first use
✗ reminders    denied in System Settings
○ mail         will prompt on first use
```

- `bite doctor --probe` additionally live-tests the Apple Events grants for
  Mail/Notes/Messages — this *does* trigger the prompts if pending.
- `bite doctor --fix` opens the matching System Settings pane for anything
  denied.

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

`codesign` ad-hoc signature is applied on every install; notarized release
binaries carry a real Developer ID signature.
