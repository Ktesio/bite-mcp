//! `bite doctor` — prompt-free environment check with per-app permission states.

use crate::clients;
use crate::helper::BridgeHandle;

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";

/// Everything `bite doctor` learned about the environment, plus the sizes of
/// the cleanup it performed. Collected once; rendered either as the classic
/// human report or as one compact JSON object (`bite doctor --json`) for
/// scripts/CI. The exit-code contract is shared: 0 = clean, 2 = problems.
#[derive(serde::Serialize, Debug)]
pub struct DoctorReport {
    /// `sw_vers` detail, e.g. `macOS 15 (major 15)`; None = not macOS 13+.
    pub os: Option<String>,
    /// `xcrun -f swiftc` path; None = no Swift toolchain (informational —
    /// only needed for source-compile installs, never counts as a problem).
    pub toolchain: Option<String>,
    pub helper: HelperStatus,
    /// Per-app permission status. Empty when the helper is down (nothing was
    /// probed) — check `helper.ok` before reading meaning into it.
    pub apps: Vec<AppStatus>,
    /// Crawler Mail-automation probe; None when `bite-crawl` isn't installed.
    pub crawler_mail: Option<CrawlerMailStatus>,
    pub storage: StorageReport,
    pub clients: Vec<ClientStatus>,
    /// Count of failed checks — the human `N issue(s) found` and exit code 2.
    pub problems: usize,
}

#[derive(serde::Serialize, Debug)]
pub struct HelperStatus {
    pub ok: bool,
    /// Stable helper path when the bridge came up.
    pub path: Option<String>,
    pub capabilities: Vec<String>,
    /// Rendered error when the helper is missing/broken.
    pub error: Option<String>,
}

#[derive(serde::Serialize, Debug)]
pub struct AppStatus {
    pub app: String,
    /// Raw `sys.probe` state (or `error` when the probe itself failed).
    pub state: String,
    /// Whether the state is workable (a pending first-use prompt is fine).
    pub ok: bool,
    /// Human context for the state, when there is any.
    pub note: Option<String>,
}

#[derive(serde::Serialize, Debug)]
pub struct CrawlerMailStatus {
    /// `authorized` | `no_accounts` | `denied` | `failed`
    pub state: String,
    /// Raw probe output for the denied case.
    pub detail: Option<String>,
}

#[derive(serde::Serialize, Debug)]
pub struct StorageReport {
    pub data_dir: String,
    pub total_bytes: u64,
    pub index_bytes: u64,
    pub bin_bytes: u64,
    pub batches_bytes: u64,
    pub batches_pending: usize,
    pub batches_quarantined: usize,
    /// Stale swift-build scratch dirs doctor removed just now (it always
    /// cleans those itself; the rest is reported, never deleted).
    pub scratch_removed_count: usize,
    pub scratch_removed_bytes: u64,
    /// Fresh scratch present — a build may be in progress; left alone.
    pub scratch_in_progress: usize,
    /// Stale scratch but no installed helper; the next
    /// `bite install-helper` run removes it.
    pub scratch_pending_install_count: usize,
    pub scratch_pending_install_bytes: u64,
    /// `bite-*` entries in $TMPDIR (leaked test scratch; reported >50).
    pub tmpdir_bite_entries: usize,
}

#[derive(serde::Serialize, Debug)]
pub struct ClientStatus {
    pub key: String,
    pub display: String,
    /// Binary on PATH or config file found.
    pub detected: bool,
    /// MCP server entry written into the client's config.
    pub installed: bool,
}

pub fn run(fix: bool, probe: bool, json_out: bool) -> Result<i32, bite_core::BiteError> {
    let report = collect(fix, probe)?;

    if json_out {
        // One compact JSON object on stdout; trailing newline via println.
        println!("{}", serde_json::to_string(&report).unwrap_or_default());
    } else {
        print!("{}", render_human(&report, fix));
    }
    if report.problems == 0 {
        Ok(0)
    } else {
        Ok(2)
    }
}

/// Gather the full report. Side effects are unchanged from the classic
/// behavior: `--fix` opens System Settings panes for denied permissions and
/// stale swift-build scratch dirs are swept (they're pure garbage once the
/// stable helper exists).
fn collect(fix: bool, probe: bool) -> Result<DoctorReport, bite_core::BiteError> {
    let mut problems = 0usize;

    // ── OS ──
    let os = (|| {
        let out = std::process::Command::new("sw_vers")
            .args(["-productVersion"])
            .output()
            .ok()?;
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let major: u32 = v.split('.').next()?.parse().ok()?;
        Some(format!("macOS {v} (major {major})"))
    })();
    if os.is_none() {
        problems += 1;
    }

    // ── Swift toolchain (only needed for source-compile installs) ──
    fn probe_toolchain() -> Option<String> {
        std::process::Command::new("xcrun")
            .args(["-f", "swiftc"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }
    let toolchain = probe_toolchain();

    // ── Helper ──
    let mut handle = BridgeHandle::new();
    let helper = match handle.get() {
        Ok(bridge) => {
            let caps = bridge.capabilities();
            HelperStatus {
                ok: true,
                path: Some(bridge.helper_path.clone()),
                capabilities: caps,
                error: None,
            }
        }
        Err(e) => {
            problems += 1;
            HelperStatus {
                ok: false,
                path: None,
                capabilities: Vec::new(),
                error: Some(bite_core::BiteError(e).render()),
            }
        }
    };

    // ── Per-app permission probes (no prompts unless --probe) ──
    let mut apps = Vec::new();
    if helper.ok {
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
                    let state = v["state"].as_str().unwrap_or("unknown").to_string();
                    let (ok, note) = app_state_note(&state);
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
                        // stderr on purpose: stdout may carry the JSON report
                        eprintln!("        {DIM}→ opening System Settings…{RESET}");
                        let _ = std::process::Command::new("open").arg(url).status();
                    }
                    apps.push(AppStatus {
                        app: app.to_string(),
                        state,
                        ok,
                        note,
                    });
                }
                Err(e) => {
                    problems += 1;
                    apps.push(AppStatus {
                        app: app.to_string(),
                        state: "error".to_string(),
                        ok: false,
                        note: Some(bite_core::BiteError(e).render()),
                    });
                }
            }
        }
    }

    // ── Crawler Mail-automation identity ──
    // bite-crawl is a separate binary with its own TCC identity: Mail may be
    // granted to the helper but denied to the crawler (silent empty results).
    let mut crawler_mail = None;
    let crawl_bin = bite_core::config::data_dir().join("bin").join("bite-crawl");
    if crawl_bin.exists() {
        let out = std::process::Command::new(&crawl_bin)
            .arg("--probe-mail")
            .output();
        crawler_mail = Some(match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout);
                if text.contains("no_accounts") {
                    // Mail answered fine — it just has nothing configured.
                    // Not a TCC problem; don't send users permission-chasing.
                    CrawlerMailStatus {
                        state: "no_accounts".to_string(),
                        detail: None,
                    }
                } else if text.contains("authorized") {
                    CrawlerMailStatus {
                        state: "authorized".to_string(),
                        detail: None,
                    }
                } else {
                    CrawlerMailStatus {
                        state: "denied".to_string(),
                        detail: Some(text.trim().to_string()),
                    }
                }
            }
            Err(_) => CrawlerMailStatus {
                state: "failed".to_string(),
                detail: None,
            },
        });
    }

    // ── Storage ──
    let storage = collect_storage();

    // ── Agent clients ──
    let mut client_status = Vec::new();
    for spec in clients::clients() {
        client_status.push(ClientStatus {
            key: spec.key.to_string(),
            display: spec.display.to_string(),
            detected: spec.detect(),
            installed: spec.installed(),
        });
    }

    Ok(DoctorReport {
        os,
        toolchain,
        helper,
        apps,
        crawler_mail,
        storage,
        clients: client_status,
        problems,
    })
}

/// Map a raw `sys.probe` state to (workable?, human note). A pending
/// first-use prompt is fine; denials and missing apps are not. Unknown
/// states stay workable and surface themselves as the note.
fn app_state_note(state: &str) -> (bool, Option<String>) {
    match state {
        "authorized" | "write_only" => (true, None),
        "not_determined" | "will_prompt_on_first_use" => (
            true,
            Some("a system prompt appears on first use".to_string()),
        ),
        "denied" | "restricted" => (false, Some("denied in System Settings".to_string())),
        "denied_or_unavailable" => (false, Some("denied or app missing".to_string())),
        "app_missing" => (false, Some("app not installed".to_string())),
        _ => (true, Some(state.to_string())),
    }
}

/// Storage overview: data-dir size breakdown, batch backlog, and temp-dir
/// litter. Informational (never prompts, never counts as a problem) — only
/// du-style sizes of the known top-level paths; nothing descends into lance
/// file formats.
fn collect_storage() -> StorageReport {
    let data = bite_core::config::data_dir();
    let batches = data.join("batches");

    let total = tree_size(&data);
    let index = tree_size(&data.join("index.lance"));
    let bin_dir = tree_size(&data.join("bin"));
    let batches_sz = tree_size(&batches);

    let pending = crate::index::staged_batch_count_in(&batches);
    let quarantined = crate::index::staged_batch_count_in(&batches.join("quarantine"));

    // leftover on-demand build scratch (`swift-build*`). Once the stable
    // helper exists the scratch is pure garbage (binaries live in bin/) and
    // doctor removes STALE instances itself; fresh ones might belong to a
    // concurrent install and are only reported.
    let mut removed: (usize, u64) = (0, 0);
    let mut in_progress = 0usize;
    let mut scratch_pending: (usize, u64) = (0, 0);
    let scratches = crate::helper::swift_build_scratches_in(&data);
    if !scratches.is_empty() {
        let helper_installed = bite_core::config::helper_install_path().exists();
        let now = std::time::SystemTime::now();
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
                    scratch_pending = (scratch_pending.0 + 1, scratch_pending.1 + size);
                }
                ScratchAction::None => {}
            }
        }
    }

    // leaked test scratch dirs (panicked/killed runs) accumulate in $TMPDIR
    let tmp_bite = count_bite_prefixed(&std::env::temp_dir());

    StorageReport {
        data_dir: data.display().to_string(),
        total_bytes: total,
        index_bytes: index,
        bin_bytes: bin_dir,
        batches_bytes: batches_sz,
        batches_pending: pending,
        batches_quarantined: quarantined,
        scratch_removed_count: removed.0,
        scratch_removed_bytes: removed.1,
        scratch_in_progress: in_progress,
        scratch_pending_install_count: scratch_pending.0,
        scratch_pending_install_bytes: scratch_pending.1,
        tmpdir_bite_entries: tmp_bite,
    }
}

/// The classic human report, byte-for-byte the pre-JSON output.
fn render_human(report: &DoctorReport, fix: bool) -> String {
    let mut out = String::new();
    macro_rules! line {
        ($($arg:tt)*) => {{
            out.push_str(&format!($($arg)*));
            out.push('\n');
        }};
    }

    line!("bite doctor");
    line!("{}", DIM.repeat(60));

    // ── OS / toolchain ──
    match &report.os {
        Some(detail) => {
            line!("{}", status_line(true, "macOS 13+"));
            line!("        {DIM}{detail}{RESET}");
        }
        None => line!("{}", status_line(false, "macOS 13+")),
    }
    match &report.toolchain {
        Some(detail) => {
            line!("{}", status_line(true, "Xcode Command Line Tools"));
            line!("        {DIM}{detail}{RESET}");
        }
        None => line!("{}", status_line(false, "Xcode Command Line Tools")),
    }

    // ── Helper ──
    if report.helper.ok {
        let path = report.helper.path.as_deref().unwrap_or("?");
        line!("{}", status_line(true, &format!("native helper ({path})")));
        line!(
            "        {DIM}capabilities: {}{RESET}",
            report.helper.capabilities.join(", ")
        );
    } else {
        line!("{}", status_line(false, "native helper"));
        line!(
            "        {RED}{}{RESET}",
            report.helper.error.as_deref().unwrap_or_default()
        );
        if fix {
            line!("        {DIM}→ run: bite install-helper --force{RESET}");
        }
    }

    // ── Per-app permission probes ──
    line!("");
    for app in &report.apps {
        line!("{}", status_line(app.ok, &app.app));
        if let Some(note) = &app.note {
            let color = if app.ok { DIM } else { RED };
            line!("        {color}{note}{RESET}");
        }
    }

    // ── Crawler Mail-automation identity ──
    if let Some(crawl) = &report.crawler_mail {
        match crawl.state.as_str() {
            "no_accounts" => {
                line!("{}", status_line(true, "crawler Mail automation"));
                line!("        {DIM}Mail has no accounts configured — nothing to index{RESET}");
            }
            "authorized" => {
                line!("{}", status_line(true, "crawler Mail automation"));
            }
            "denied" => {
                line!("{}", status_line(false, "crawler Mail automation"));
                line!(
                    "        {YELLOW}{}{RESET}",
                    crawl.detail.as_deref().unwrap_or_default()
                );
                line!("        {DIM}→ approve the Mail automation prompt once, from your terminal{RESET}");
            }
            _ => {
                line!("        {DIM}probe failed to run{RESET}");
            }
        }
    }

    // ── Storage ──
    line!("");
    let s = &report.storage;
    line!("{}", status_line(true, "storage"));
    line!(
        "        {DIM}data dir {} — {} (index.lance {}, bin {}, batches {}){RESET}",
        s.data_dir,
        human_size(s.total_bytes),
        human_size(s.index_bytes),
        human_size(s.bin_bytes),
        human_size(s.batches_bytes),
    );
    if s.batches_pending > 0 || s.batches_quarantined > 0 {
        line!(
            "        {DIM}batches: {} pending, {} quarantined{RESET}",
            s.batches_pending,
            s.batches_quarantined
        );
    }
    if s.scratch_removed_count > 0 {
        line!(
            "        {DIM}removed {} stale swift-build scratch ({}) — build garbage; binaries live in bin/{RESET}",
            s.scratch_removed_count,
            human_size(s.scratch_removed_bytes),
        );
    }
    if s.scratch_in_progress > 0 {
        line!(
            "        {YELLOW}{} swift-build scratch present — a build may be in progress; left alone{RESET}",
            s.scratch_in_progress,
        );
    }
    if s.scratch_pending_install_count > 0 {
        line!(
            "        {YELLOW}{} swift-build scratch present ({}) — the next `bite install-helper` run removes it{RESET}",
            s.scratch_pending_install_count,
            human_size(s.scratch_pending_install_bytes),
        );
    }
    if s.tmpdir_bite_entries > 50 {
        line!(
            "        {YELLOW}{} bite-* entries in $TMPDIR — likely leaked test scratch dirs, safe to delete{RESET}",
            s.tmpdir_bite_entries,
        );
    }

    // ── Agent clients ──
    line!("");
    for c in &report.clients {
        let (icon, color) = if c.installed {
            ("✓", GREEN)
        } else if c.detected {
            ("○", YELLOW)
        } else {
            (" ", DIM)
        };
        line!(
            "  {color}{icon}{RESET} {deg:<11} {DIM}{key}{RESET}",
            deg = c.display,
            key = c.key
        );
        if c.detected && !c.installed {
            line!("      {DIM}→ run `bite setup` to register the MCP server{RESET}");
        }
    }

    line!("{}", DIM.repeat(60));
    if report.problems == 0 {
        line!("{GREEN}all checks passed{RESET}");
    } else {
        line!("{YELLOW}{} issue(s) found{RESET}", report.problems);
    }
    out
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

fn status_line(ok: bool, label: &str) -> String {
    let (icon, color) = if ok { ("✓", GREEN) } else { ("✗", RED) };
    format!("  {color}{icon}{RESET} {label}")
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

    #[test]
    fn app_state_mapping_workable_vs_denied() {
        // granted (incl. write-only Mail) → clean, no note
        assert_eq!(app_state_note("authorized"), (true, None));
        assert_eq!(app_state_note("write_only"), (true, None));
        // pending first-use prompt is fine — that's the normal fresh install
        assert_eq!(
            app_state_note("not_determined"),
            (
                true,
                Some("a system prompt appears on first use".to_string())
            )
        );
        assert_eq!(
            app_state_note("will_prompt_on_first_use"),
            (
                true,
                Some("a system prompt appears on first use".to_string())
            )
        );
        // denials and missing apps are the real problems
        assert_eq!(
            app_state_note("denied"),
            (false, Some("denied in System Settings".to_string()))
        );
        assert_eq!(
            app_state_note("restricted"),
            (false, Some("denied in System Settings".to_string()))
        );
        assert_eq!(
            app_state_note("denied_or_unavailable"),
            (false, Some("denied or app missing".to_string()))
        );
        assert_eq!(
            app_state_note("app_missing"),
            (false, Some("app not installed".to_string()))
        );
        // unknown future states stay workable and surface themselves
        assert_eq!(
            app_state_note("someday_new_state"),
            (true, Some("someday_new_state".to_string()))
        );
    }

    fn sample_report() -> DoctorReport {
        DoctorReport {
            os: Some("macOS 15 (major 15)".to_string()),
            toolchain: Some("/usr/bin/swiftc".to_string()),
            helper: HelperStatus {
                ok: true,
                path: Some("/x/bite/bin/bite-helper".to_string()),
                capabilities: vec!["calendar".to_string(), "mail".to_string()],
                error: None,
            },
            apps: vec![
                AppStatus {
                    app: "calendar".to_string(),
                    state: "authorized".to_string(),
                    ok: true,
                    note: None,
                },
                AppStatus {
                    app: "reminders".to_string(),
                    state: "denied".to_string(),
                    ok: false,
                    note: Some("denied in System Settings".to_string()),
                },
            ],
            crawler_mail: Some(CrawlerMailStatus {
                state: "denied".to_string(),
                detail: Some("not authorized".to_string()),
            }),
            storage: StorageReport {
                data_dir: "/x/bite".to_string(),
                total_bytes: 4096,
                index_bytes: 2048,
                bin_bytes: 1024,
                batches_bytes: 512,
                batches_pending: 1,
                batches_quarantined: 2,
                scratch_removed_count: 1,
                scratch_removed_bytes: 700,
                scratch_in_progress: 0,
                scratch_pending_install_count: 0,
                scratch_pending_install_bytes: 0,
                tmpdir_bite_entries: 3,
            },
            clients: vec![
                ClientStatus {
                    key: "claude".to_string(),
                    display: "Claude Code".to_string(),
                    detected: true,
                    installed: true,
                },
                ClientStatus {
                    key: "codex".to_string(),
                    display: "Codex CLI".to_string(),
                    detected: true,
                    installed: false,
                },
            ],
            problems: 1,
        }
    }

    #[test]
    fn json_report_has_every_section() {
        let v = serde_json::to_value(sample_report()).unwrap();
        for key in [
            "os",
            "toolchain",
            "helper",
            "apps",
            "crawler_mail",
            "storage",
            "clients",
            "problems",
        ] {
            assert!(v.get(key).is_some(), "missing top-level key {key}");
        }
        // helper shape: path + capabilities when up
        assert_eq!(v["helper"]["path"], "/x/bite/bin/bite-helper");
        assert_eq!(v["helper"]["capabilities"][0], "calendar");
        // app rows carry the raw state next to the verdict
        assert_eq!(v["apps"][1]["state"], "denied");
        assert_eq!(v["apps"][1]["ok"], false);
        // storage sizes are raw bytes (scripts do their own units)
        assert_eq!(v["storage"]["total_bytes"], 4096);
        assert_eq!(v["storage"]["batches_quarantined"], 2);
        // clients expose the tri-state the human icons encode
        assert_eq!(v["clients"][0]["installed"], true);
        assert_eq!(v["clients"][1]["detected"], true);
        assert_eq!(v["clients"][1]["installed"], false);
        // problems mirrors the exit-code contract (0 clean / 2 problems)
        assert_eq!(v["problems"], 1);
    }

    #[test]
    fn json_report_helper_down_has_no_apps() {
        let mut r = sample_report();
        r.helper = HelperStatus {
            ok: false,
            path: None,
            capabilities: Vec::new(),
            error: Some("no Swift toolchain found".to_string()),
        };
        r.apps.clear();
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["helper"]["ok"], false);
        assert_eq!(v["helper"]["error"], "no Swift toolchain found");
        // empty apps = not probed (helper down), not "all granted"
        assert_eq!(v["apps"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn human_render_keeps_the_classic_shape() {
        let text = render_human(&sample_report(), false);
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines[0], "bite doctor");
        // verdict line is the last content line (before the trailing "")
        let last = lines[lines.len() - 2];
        assert!(last.contains("1 issue(s) found"), "got: {last}");
        // per-app note and fix-free helper failure text
        assert!(text.contains("denied in System Settings"));
        assert!(!text.contains("→ run: bite install-helper --force"));
        // storage detail keeps the human units
        assert!(text.contains("data dir /x/bite — 4.0 KB (index.lance 2.0 KB"));
        // client tri-state renders with the setup hint for detected-only
        assert!(text.contains("→ run `bite setup` to register the MCP server"));
        // every line ends with a newline
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn human_render_fix_hint_only_when_asked() {
        let mut r = sample_report();
        r.helper.ok = false;
        r.helper.path = None;
        r.helper.capabilities = Vec::new();
        r.helper.error = Some("boom".to_string());
        let with_fix = render_human(&r, true);
        assert!(with_fix.contains("→ run: bite install-helper --force"));
        let without_fix = render_human(&r, false);
        assert!(!without_fix.contains("→ run: bite install-helper --force"));
    }

    #[test]
    fn human_render_clean_report_passes() {
        let mut r = sample_report();
        r.problems = 0;
        let text = render_human(&r, false);
        assert!(text.contains("all checks passed"));
        assert!(!text.contains("issue(s) found"));
    }
}
