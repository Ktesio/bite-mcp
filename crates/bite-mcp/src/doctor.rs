//! `bite doctor` — prompt-free environment check with per-app permission states.

use crate::clients;
use crate::helper::BridgeHandle;

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";

pub fn run(fix: bool, probe: bool) -> Result<i32, bite_core::BiteError> {
    let mut problems = 0;

    println!("bite doctor");
    println!("{}", DIM.repeat(60));

    // ── OS ──
    let os_ok = check("macOS 13+", || {
        let out = std::process::Command::new("sw_vers")
            .args(["-productVersion"])
            .output()
            .ok()?;
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let major: u32 = v.split('.').next()?.parse().ok()?;
        Some(format!("macOS {v} (major {major})"))
    });
    if !os_ok {
        problems += 1;
    }

    // ── Swift toolchain (only needed for source-compile installs) ──
    let _toolchain = check("Xcode Command Line Tools", || {
        std::process::Command::new("xcrun")
            .args(["-f", "swiftc"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    });

    // ── Helper ──
    let mut handle = BridgeHandle::new();
    let helper_ok = match handle.get() {
        Ok(bridge) => {
            let caps = bridge.capabilities();
            status_line(true, &format!("native helper ({})", bridge.helper_path));
            println!("        {DIM}capabilities: {}{RESET}", caps.join(", "));
            true
        }
        Err(e) => {
            status_line(false, "native helper");
            println!("        {RED}{}{RESET}", bite_core::BiteError(e).render());
            problems += 1;
            if fix {
                println!("        {DIM}→ run: bite install-helper --force{RESET}");
            }
            false
        }
    };

    // ── Per-app permission probes (no prompts unless --probe) ──
    println!();
    if helper_ok {
        for app in [
            "calendar",
            "reminders",
            "contacts",
            "mail",
            "notes",
            "messages",
        ] {
            let params = serde_json::json!({ "app": app, "probe": probe });
            let result = handle.get().and_then(|b| {
                b.call_timeout(
                    "sys.probe",
                    &params,
                    std::time::Duration::from_secs(if probe { 60 } else { 10 }),
                )
            });
            match result {
                Ok(v) => {
                    let state = v["state"].as_str().unwrap_or("unknown");
                    let (ok, note) = match state {
                        "authorized" | "write_only" => (true, ""),
                        "not_determined" | "will_prompt_on_first_use" => {
                            (true, "a system prompt appears on first use")
                        }
                        "denied" | "restricted" => (false, "denied in System Settings"),
                        "denied_or_unavailable" => (false, "denied or app missing"),
                        "app_missing" => (false, "app not installed"),
                        _ => (true, state),
                    };
                    status_line(ok, app);
                    if !note.is_empty() {
                        let color = if ok { DIM } else { RED };
                        println!("        {color}{note}{RESET}");
                        if !ok {
                            problems += 1;
                        }
                        if fix && !ok {
                            let pane = match app {
                                "calendar" => "Privacy_Calendars",
                                "reminders" => "Privacy_Reminders",
                                "contacts" => "Privacy_Contacts",
                                _ => "Privacy_Automation",
                            };
                            let url = format!(
                                "x-apple.systempreferences:com.apple.preference.security?{pane}"
                            );
                            println!("        {DIM}→ opening System Settings…{RESET}");
                            let _ = std::process::Command::new("open").arg(url).status();
                        }
                    }
                }
                Err(e) => {
                    status_line(false, app);
                    println!("        {RED}{}{RESET}", bite_core::BiteError(e).render());
                    problems += 1;
                }
            }
        }
    }

    // ── Agent clients ──
    println!();
    for spec in clients::clients() {
        let detected = spec.detect();
        let installed = spec.installed();
        let (icon, color) = if installed {
            ("✓", GREEN)
        } else if detected {
            ("○", YELLOW)
        } else {
            (" ", DIM)
        };
        println!(
            "  {color}{icon}{RESET} {deg:<11} {DIM}{key}{RESET}",
            deg = spec.display,
            key = spec.key
        );
        if detected && !installed {
            println!("      {DIM}→ run `bite setup` to register the MCP server{RESET}");
        }
    }

    println!("{}", DIM.repeat(60));
    if problems == 0 {
        println!("{GREEN}all checks passed{RESET}");
        Ok(0)
    } else {
        println!("{YELLOW}{problems} issue(s) found{RESET}");
        Ok(2)
    }
}

fn check(label: &str, f: impl FnOnce() -> Option<String>) -> bool {
    match f() {
        Some(detail) => {
            status_line(true, label);
            println!("        {DIM}{detail}{RESET}");
            true
        }
        None => {
            status_line(false, label);
            false
        }
    }
}

fn status_line(ok: bool, label: &str) {
    let (icon, color) = if ok { ("✓", GREEN) } else { ("✗", RED) };
    println!("  {color}{icon}{RESET} {label}");
}
