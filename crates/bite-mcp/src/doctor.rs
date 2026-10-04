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

    // ── Crawler Mail-automation identity ──
    // bite-crawl is a separate binary with its own TCC identity: Mail may be
    // granted to the helper but denied to the crawler (silent empty results).
    let crawl_bin = bite_core::config::data_dir().join("bin").join("bite-crawl");
    if crawl_bin.exists() {
        let out = std::process::Command::new(&crawl_bin)
            .arg("--probe-mail")
            .output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout);
                if text.contains("no_accounts") {
                    // Mail answered fine — it just has nothing configured.
                    // Not a TCC problem; don't send users permission-chasing.
                    status_line(true, "crawler Mail automation");
                    println!(
                        "        {DIM}Mail has no accounts configured — nothing to index{RESET}"
                    );
                } else {
                    let authorized = text.contains("authorized");
                    status_line(authorized, "crawler Mail automation");
                    if !authorized {
                        println!("        {YELLOW}{}{RESET}", text.trim());
                        println!("        {DIM}→ approve the Mail automation prompt once, from your terminal{RESET}");
                    }
                }
            }
            Err(_) => println!("        {DIM}probe failed to run{RESET}"),
        }
    }

    // ── Storage ──
    println!();
    storage_report();

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

/// Storage overview: data-dir size breakdown, batch backlog, and temp-dir
/// litter. Informational (never prompts, never counts as a problem) — only
/// du-style sizes of the known top-level paths; nothing descends into lance
/// file formats.
fn storage_report() {
    let data = bite_core::config::data_dir();
    let batches = data.join("batches");

    let total = tree_size(&data);
    let index = tree_size(&data.join("index.lance"));
    let bin_dir = tree_size(&data.join("bin"));
    let batches_sz = tree_size(&batches);

    status_line(true, "storage");
    println!(
        "        {DIM}data dir {} — {} (index.lance {}, bin {}, batches {}){RESET}",
        data.display(),
        human_size(total),
        human_size(index),
        human_size(bin_dir),
        human_size(batches_sz),
    );

    let pending = crate::index::staged_batch_count_in(&batches);
    let quarantined = crate::index::staged_batch_count_in(&batches.join("quarantine"));
    if pending > 0 || quarantined > 0 {
        println!("        {DIM}batches: {pending} pending, {quarantined} quarantined{RESET}");
    }

    // leftover on-demand build scratch (`swift-build*`). Once the stable
    // helper exists the scratch is pure garbage (binaries live in bin/) and
    // doctor removes STALE instances itself; fresh ones might belong to a
    // concurrent install and are only reported.
    let scratches = crate::helper::swift_build_scratches_in(&data);
    if !scratches.is_empty() {
        let helper_installed = bite_core::config::helper_install_path().exists();
        let now = std::time::SystemTime::now();
        let mut removed: (usize, u64) = (0, 0);
        let mut in_progress = 0usize;
        let mut pending: (usize, u64) = (0, 0);
        for s in &scratches {
            let size = tree_size(s);
            let stale = std::fs::symlink_metadata(s)
                .and_then(|m| m.modified())
                .ok()
                .is_some_and(|m| crate::helper::scratch_is_stale(m, now));
            match scratch_action(true, helper_installed, stale) {
                ScratchAction::Remove => {
                    if std::fs::remove_dir_all(s).is_ok() {
                        removed = (removed.0 + 1, removed.1 + size);
                    }
                }
                ScratchAction::ReportInProgress => in_progress += 1,
                ScratchAction::ReportPendingInstall => {
                    pending = (pending.0 + 1, pending.1 + size);
                }
                ScratchAction::None => {}
            }
        }
        if removed.0 > 0 {
            println!(
                "        {DIM}removed {} stale swift-build scratch ({}) — build garbage; binaries live in bin/{RESET}",
                removed.0,
                human_size(removed.1),
            );
        }
        if in_progress > 0 {
            println!(
                "        {YELLOW}{in_progress} swift-build scratch present — a build may be in progress; left alone{RESET}",
            );
        }
        if pending.0 > 0 {
            println!(
                "        {YELLOW}{} swift-build scratch present ({}) — the next `bite install-helper` run removes it{RESET}",
                pending.0,
                human_size(pending.1),
            );
        }
    }

    // leaked test scratch dirs (panicked/killed runs) accumulate in $TMPDIR
    let tmp_bite = count_bite_prefixed(&std::env::temp_dir());
    if tmp_bite > 50 {
        println!(
            "        {YELLOW}{tmp_bite} bite-* entries in $TMPDIR — likely leaked test scratch dirs, safe to delete{RESET}",
        );
    }
}

/// du-style byte size of a path: metadata-only walk, symlinks not followed,
/// absent paths are 0. Fast enough for the known data-dir paths.
fn tree_size(path: &std::path::Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if !ft.is_symlink() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// `1.2 MB`-style human-readable size (base-1024).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Count entries in `dir` whose names start with `bite-` (leaked scratch).
fn count_bite_prefixed(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .map(|n| n.starts_with("bite-"))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

/// What doctor should do with a leftover swift-build scratch dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScratchAction {
    /// nothing present
    None,
    /// helper installed + stale: pure build garbage → doctor deletes it
    Remove,
    /// fresh scratch (helper or not): could be a concurrent install's
    /// in-flight build → report only, never delete
    ReportInProgress,
    /// no installed helper + stale: report; install-helper's start sweep
    /// will remove it on the next run
    ReportPendingInstall,
}

/// Pure decision for one scratch dir. Deletion fires ONLY when the stable
/// helper is installed (the scratch's whole purpose is producing that
/// binary — with it present the scratch is garbage) AND the dir is stale
/// (a fresh one may belong to a live concurrent build).
fn scratch_action(present: bool, helper_installed: bool, stale: bool) -> ScratchAction {
    if !present {
        ScratchAction::None
    } else if helper_installed && stale {
        ScratchAction::Remove
    } else if helper_installed {
        ScratchAction::ReportInProgress
    } else if stale {
        ScratchAction::ReportPendingInstall
    } else {
        ScratchAction::ReportInProgress
    }
}

fn status_line(ok: bool, label: &str) {
    let (icon, color) = if ok { ("✓", GREEN) } else { ("✗", RED) };
    println!("  {color}{icon}{RESET} {label}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024 * 37), "37.0 MB");
        assert_eq!(human_size(1024u64.pow(3) * 2), "2.0 GB");
    }

    #[test]
    fn tree_size_sums_files_and_skips_symlinks() {
        let dir = tempfile::Builder::new()
            .prefix("bite-doctor-tests-")
            .tempdir()
            .unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("b"), vec![0u8; 50]).unwrap();
        // a big regular file that must be counted only ONE — the symlink to
        // it must not be followed into a second count
        let target = dir.path().join("target");
        std::fs::write(&target, vec![0u8; 4096]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, dir.path().join("link")).unwrap();
        assert_eq!(tree_size(dir.path()), 100 + 50 + 4096);
        assert_eq!(tree_size(&dir.path().join("missing")), 0);
    }

    #[test]
    fn bite_prefix_count_is_name_based() {
        let dir = tempfile::Builder::new()
            .prefix("bite-doctor-tests-")
            .tempdir()
            .unwrap();
        std::fs::create_dir(dir.path().join("bite-index-1")).unwrap();
        std::fs::write(dir.path().join("bite-clients-tests-x"), b"").unwrap();
        std::fs::write(dir.path().join("bite-fifo-42"), b"").unwrap();
        std::fs::write(dir.path().join("unrelated"), b"").unwrap();
        std::fs::create_dir(dir.path().join("biteBUTnot")).unwrap();
        assert_eq!(count_bite_prefixed(dir.path()), 3);
        assert_eq!(count_bite_prefixed(&dir.path().join("missing")), 0);
    }

    #[test]
    fn scratch_delete_only_when_installed_and_stale() {
        use ScratchAction as A;
        // nothing present → nothing to do, regardless of state
        assert_eq!(scratch_action(false, false, false), A::None);
        assert_eq!(scratch_action(false, true, true), A::None);
        // installed + stale → doctor deletes the garbage
        assert_eq!(scratch_action(true, true, true), A::Remove);
        // installed + FRESH → possibly a concurrent install's live build:
        // report only (deleting would reintroduce the concurrent-install race)
        assert_eq!(scratch_action(true, true, false), A::ReportInProgress);
        // not installed + stale → report; install-helper's start sweep removes it
        assert_eq!(scratch_action(true, false, true), A::ReportPendingInstall);
        // not installed + fresh → recent failed/in-progress compile: report only
        assert_eq!(scratch_action(true, false, false), A::ReportInProgress);
    }
}
