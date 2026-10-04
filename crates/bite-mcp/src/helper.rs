//! Helper binary lifecycle: locate → install → provide a spawned `Bridge`.

use std::path::PathBuf;
use std::process::Command;

use bite_bridge::{Bridge, BridgeError};

/// Resolution order:
/// 1. `BITE_HELPER_BIN` env (tests / power users)
/// 2. stable install path (TCC grants stick to this location)
/// 3. baked-from-build path (`cargo run`), copied to the stable path
/// 4. compile-on-demand from the packaged Swift sources
pub fn ensure_helper() -> Result<PathBuf, BridgeError> {
    if let Ok(p) = std::env::var("BITE_HELPER_BIN") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Ok(path);
        }
        return Err(BridgeError::spawn(format!(
            "BITE_HELPER_BIN points to missing file: {p}"
        )));
    }

    let stable = bite_core::config::helper_install_path();
    if stable.exists() {
        return Ok(stable);
    }

    if let Ok(baked) = std::env::var("BITE_HELPER_BUILT") {
        let path = PathBuf::from(&baked);
        if path.exists() {
            return install_from(&path, &stable);
        }
    }

    compile_on_demand(&stable)
}

/// `bite install-helper [--force]`
pub fn install_helper(force: bool) -> Result<PathBuf, BridgeError> {
    // First, garbage-collect scratch dirs abandoned by builds that didn't
    // exit cleanly (crash/kill mid-compile). Runs on EVERY invocation —
    // including the non-forced early return — so stale scratch can't
    // outlive the helper it was meant to build.
    sweep_stale_scratch();
    let stable = bite_core::config::helper_install_path();
    if stable.exists() && !force {
        if let Ok(baked) = std::env::var("BITE_HELPER_BUILT") {
            let baked = PathBuf::from(baked);
            if baked.exists() && newer(&baked, &stable) {
                return install_from(&baked, &stable);
            }
        }
        return Ok(stable);
    }
    if let Ok(baked) = std::env::var("BITE_HELPER_BUILT") {
        let baked = PathBuf::from(baked);
        if baked.exists() {
            return install_from(&baked, &stable);
        }
    }
    compile_on_demand(&stable)
}

fn newer(a: &std::path::Path, b: &std::path::Path) -> bool {
    let mtime = |p: &std::path::Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    };
    mtime(a) > mtime(b)
}

fn install_from(src: &PathBuf, stable: &PathBuf) -> Result<PathBuf, BridgeError> {
    if let Some(parent) = stable.parent() {
        bite_core::fsops::ensure_private_dir(parent)
            .map_err(|e| BridgeError::spawn(e.to_string()))?;
    }
    // preserve the binary (and its TCC identity) when content is unchanged
    if path_exists_with_same_content(src, stable) {
        return Ok(stable.clone());
    }
    std::fs::copy(src, stable).map_err(|e| BridgeError::spawn(format!("copy helper: {e}")))?;
    set_exec(stable);
    adhoc_sign(stable);
    // the crawler binary ships alongside the helper
    if let Some(crawl_src) = src.parent().map(|d| d.join("bite-crawl")) {
        if crawl_src.exists() {
            let crawl_dst = stable.with_file_name("bite-crawl");
            std::fs::copy(&crawl_src, &crawl_dst)
                .map_err(|e| BridgeError::spawn(format!("copy crawl binary: {e}")))?;
            set_exec(&crawl_dst);
            adhoc_sign(&crawl_dst);
        }
    }
    Ok(stable.clone())
}

fn compile_on_demand(stable: &PathBuf) -> Result<PathBuf, BridgeError> {
    // PER-INVOCATION scratch (`swift-build-<pid>`): two MCP clients
    // cold-starting, or doctor racing install-helper, each build isolated —
    // nobody's cleanup can delete another process's in-flight build state.
    let scratch = bite_core::config::data_dir().join(scratch_name(std::process::id()));
    let result = compile_into(&scratch, stable);
    // Wipe our OWN scratch on every clean exit — success or failure: the
    // artifacts worth keeping are the installed binaries (bin/) and the
    // build diagnostics printed above, not the object-file intermediates.
    // A crash/kill mid-build skips this drop-point; that dir is caught by
    // the 24 h stale sweep (install-helper start) and `bite doctor`.
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn compile_into(scratch: &std::path::Path, stable: &PathBuf) -> Result<PathBuf, BridgeError> {
    // packaged sources live next to the binary's crate manifest (baked path)
    let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("swift");
    if !src_dir.join("Package.swift").exists() {
        return Err(BridgeError::spawn(format!(
            "no compiled helper and no Swift sources at {}; reinstall bite-mcp",
            src_dir.display()
        )));
    }
    if !which("swift") && !xcrun_available() {
        return Err(BridgeError::spawn(
            "no Swift toolchain found — run `xcode-select --install`, then retry",
        ));
    }
    let status = Command::new("swift")
        .args([
            "build",
            "-c",
            "release",
            "--package-path",
            src_dir.to_str().unwrap(),
            "--scratch-path",
            scratch.to_str().unwrap(),
        ])
        .status()
        .map_err(|e| BridgeError::spawn(format!("cannot run swift build: {e}")))?;
    if !status.success() {
        return Err(BridgeError::spawn("swift build failed — see output above"));
    }
    let bin_dir = Command::new("swift")
        .args([
            "build",
            "--show-bin-path",
            "-c",
            "release",
            "--package-path",
            src_dir.to_str().unwrap(),
            "--scratch-path",
            scratch.to_str().unwrap(),
        ])
        .output()
        .map_err(|e| BridgeError::spawn(e.to_string()))?;
    let bin_dir = String::from_utf8_lossy(&bin_dir.stdout).trim().to_string();
    let built = PathBuf::from(&bin_dir).join("bite-helper");
    if !built.exists() {
        return Err(BridgeError::spawn("swift build produced no bite-helper"));
    }
    install_from(&built, stable)
}

/// Scratch dir name for this process's on-demand build.
pub(crate) fn scratch_name(pid: u32) -> String {
    format!("swift-build-{pid}")
}

/// Matches the legacy FIXED scratch (`swift-build`, pre per-pid naming) and
/// every per-invocation `swift-build-<pid>`. The dash terminator keeps
/// sibling names (`swift-builds`, `swift-builder`) from matching.
pub(crate) fn is_swift_build_scratch(name: &str) -> bool {
    name == "swift-build" || name.starts_with("swift-build-")
}

/// A scratch dir abandoned this long is from a dead build, not a live one.
pub(crate) const SCRATCH_STALE_AFTER_SECS: u64 = 24 * 3600;

/// Age test for one scratch dir. A future mtime (clock skew) is never stale.
pub(crate) fn scratch_is_stale(
    modified: std::time::SystemTime,
    now: std::time::SystemTime,
) -> bool {
    now.duration_since(modified)
        .map(|d| d.as_secs() > SCRATCH_STALE_AFTER_SECS)
        .unwrap_or(false)
}

/// Delete decision for one scratch entry; unknown mtime → keep (can't prove
/// it's stale).
fn should_sweep_scratch(
    modified: Option<std::time::SystemTime>,
    now: std::time::SystemTime,
) -> bool {
    modified.is_some_and(|m| scratch_is_stale(m, now))
}

/// All swift-build scratch dirs currently present under `root`.
pub(crate) fn swift_build_scratches_in(root: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .map(|it| {
            it.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(is_swift_build_scratch)
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Remove `swift-build*` dirs under `root` older than the stale threshold —
/// leftovers from builds that died without a clean exit. Age-based, so an
/// in-flight concurrent build (seconds old) is never touched.
pub(crate) fn sweep_stale_scratch_in(root: &std::path::Path, now: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !is_swift_build_scratch(&name) {
            continue;
        }
        let path = entry.path();
        let modified = std::fs::symlink_metadata(&path)
            .and_then(|m| m.modified())
            .ok();
        if should_sweep_scratch(modified, now) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

fn sweep_stale_scratch() {
    sweep_stale_scratch_in(&bite_core::config::data_dir(), std::time::SystemTime::now());
}

fn path_exists_with_same_content(src: &PathBuf, dst: &PathBuf) -> bool {
    dst.exists()
        && std::fs::read(src)
            .map(|a| a == std::fs::read(dst).unwrap_or_default())
            .unwrap_or(false)
}

fn set_exec(path: &PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
}

fn adhoc_sign(path: &PathBuf) {
    // ad-hoc signature keeps Gatekeeper happy for locally built helpers
    let _ = Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(path)
        .status();
}

fn which(bin: &str) -> bool {
    std::env::var("PATH")
        .ok()
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(bin))
                .any(|p| p.is_file())
        })
        .unwrap_or(false)
}

fn xcrun_available() -> bool {
    Command::new("xcrun")
        .args(["-f", "swiftc"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Process-wide bridge (spawned lazily, reused).
pub struct BridgeHandle {
    bridge: Option<Bridge>,
}

impl BridgeHandle {
    pub fn new() -> Self {
        Self { bridge: None }
    }

    pub fn get(&mut self) -> Result<&Bridge, BridgeError> {
        if self.bridge.is_none() {
            let path = ensure_helper()?;
            let bridge = Bridge::spawn(path.to_string_lossy().to_string())?;
            bridge.set_notification_handler(std::sync::Arc::new(|method, params| {
                crate::jobs::handle_notification(method, params);
                if method == "job_progress" {
                    // opportunistically ingest staged batches while a crawl runs
                    let _ = crate::index::ingest_pending();
                }
            }));
            self.bridge = Some(bridge);
        }
        Ok(self.bridge.as_ref().unwrap())
    }

    /// Reset after a helper exit so the next call respawns.
    pub fn reset(&mut self) {
        self.bridge = None;
    }
}

impl Default for BridgeHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_names_are_pid_suffixed() {
        assert_eq!(scratch_name(1234), "swift-build-1234");
    }

    #[test]
    fn scratch_match_includes_legacy_excludes_siblings() {
        assert!(is_swift_build_scratch("swift-build")); // legacy fixed dir
        assert!(is_swift_build_scratch("swift-build-1"));
        assert!(is_swift_build_scratch("swift-build-999999"));
        assert!(!is_swift_build_scratch("swift-builds"));
        assert!(!is_swift_build_scratch("swift-builder"));
        assert!(!is_swift_build_scratch("bin"));
        assert!(!is_swift_build_scratch("index.lance"));
        assert!(!is_swift_build_scratch(""));
    }

    #[test]
    fn stale_means_older_than_24h() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let h24 = std::time::Duration::from_secs(SCRATCH_STALE_AFTER_SECS);
        assert!(!scratch_is_stale(
            now - (h24 - std::time::Duration::from_secs(1)),
            now
        ));
        assert!(scratch_is_stale(
            now - (h24 + std::time::Duration::from_secs(1)),
            now
        ));
        // future mtime (clock skew) → never stale
        assert!(!scratch_is_stale(now + h24, now));
        // unknown mtime → keep (can't prove staleness)
        assert!(!should_sweep_scratch(None, now));
    }

    #[test]
    fn scratch_listing_globs_the_prefix() {
        let root = tempfile::Builder::new()
            .prefix("bite-helper-tests-")
            .tempdir()
            .unwrap();
        std::fs::create_dir_all(root.path().join("swift-build-7")).unwrap();
        std::fs::create_dir_all(root.path().join("swift-build")).unwrap();
        std::fs::create_dir_all(root.path().join("bin")).unwrap();
        let mut got: Vec<_> = swift_build_scratches_in(root.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, vec!["swift-build", "swift-build-7"]);
        assert!(swift_build_scratches_in(&root.path().join("missing")).is_empty());
    }

    #[test]
    fn sweep_removes_only_stale_scratch_dirs() {
        let root = tempfile::Builder::new()
            .prefix("bite-helper-tests-")
            .tempdir()
            .unwrap();
        let stale_pid = root.path().join(scratch_name(999));
        let fresh_pid = root.path().join(scratch_name(998));
        let stale_legacy = root.path().join("swift-build");
        let unrelated = root.path().join("keep-me");
        for d in [&stale_pid, &fresh_pid, &stale_legacy, &unrelated] {
            std::fs::create_dir_all(d).unwrap();
        }
        backdate(&stale_pid);
        backdate(&stale_legacy);
        backdate(&unrelated); // old, but not a scratch name → untouched
        sweep_stale_scratch_in(root.path(), std::time::SystemTime::now());
        assert!(!stale_pid.exists(), "stale per-pid scratch swept");
        assert!(!stale_legacy.exists(), "stale legacy scratch swept");
        assert!(fresh_pid.exists(), "fresh scratch kept (live build risk)");
        assert!(unrelated.exists(), "non-scratch dirs untouched");
    }

    #[test]
    fn sweep_on_missing_root_is_a_noop() {
        sweep_stale_scratch_in(
            std::path::Path::new("/nonexistent-bite-test"),
            std::time::SystemTime::now(),
        );
    }

    /// Set mtime to Jan 1 2020 (`-t` works on both BSD and GNU touch).
    fn backdate(path: &std::path::Path) {
        let ok = Command::new("touch")
            .args(["-t", "202001010000"])
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "touch -t failed for {}", path.display());
    }
}
