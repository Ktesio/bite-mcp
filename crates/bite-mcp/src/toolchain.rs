//! Secure resolution of the external toolchain binaries bite executes:
//! `swift` (on-demand helper builds), `codesign` (ad-hoc signing) and
//! `xcrun` (toolchain probing).
//!
//! `bite install-helper` compiles Swift sources and stages the result into
//! the stable, TCC-granted install dir, so whatever `swift` produces runs
//! with the helper's permission grants for the rest of the machine's life.
//! A `swift` planted in a writable `PATH` entry (a world-writable or
//! foreign-owned directory, a symlink) must never be picked up. Resolution
//! therefore uses secure-which semantics:
//!
//! * every directory from `/` down to the tool must be owned by root or by
//!   the invoking user, with no group/other write bits;
//! * the tool itself must be a regular executable file, not a symlink;
//! * `xcrun -f <tool>` results (toolchains under `/Applications`, where a
//!   group-writable top dir is stock macOS layout) are validated at file
//!   level instead: a regular executable owned by root, outside `$HOME`.
//!
//! Same-uid compromise — malware already running as this user rewriting
//! files in the user's own directories — is out of scope for this layer:
//! it owns the bite binary and its configs just as much. What this stops
//! is the cheaper attack: binaries planted in shared or other-owned
//! writable locations that a normal PATH lookup would happily execute.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Outcome of a secure `PATH` scan for one tool name.
#[derive(Debug)]
pub enum Found {
    /// First PATH hit that passed every trust check.
    Trusted(PathBuf),
    /// Candidates existed but each was rejected — the first offender and a
    /// human-readable reason.
    Rejected(PathBuf, String),
    /// No candidate on PATH at all.
    Absent,
}

/// Trust decision for one directory on the chain, on its (mode, owner).
fn trust_dir(mode: u32, owner: u32, euid: Option<u32>) -> Result<(), String> {
    if mode & 0o022 != 0 {
        return Err("its directory is group- or world-writable".into());
    }
    let trusted_owner = owner == 0 || euid == Some(owner);
    if !trusted_owner {
        return Err(format!(
            "its directory is owned by uid {owner} (neither root nor the invoking user)"
        ));
    }
    Ok(())
}

/// Trust decision for the tool file itself, on its stat facts.
fn trust_file(is_symlink: bool, is_file: bool, mode: u32) -> Result<(), String> {
    if is_symlink {
        return Err("it is a symlink".into());
    }
    if !is_file {
        return Err("it is not a regular file".into());
    }
    if mode & 0o111 == 0 {
        return Err("it is not executable".into());
    }
    Ok(())
}

/// Every ancestor directory of `dir` (inclusive) must pass `trust_dir`.
/// `dir` must already be canonicalized by the caller: resolution of
/// symlinked PATH entries is where the real layout lives.
fn trust_chain(dir: &Path, euid: Option<u32>) -> Result<(), String> {
    for ancestor in dir.ancestors() {
        let meta = std::fs::symlink_metadata(ancestor)
            .map_err(|e| format!("its directory {} is unreadable ({e})", ancestor.display()))?;
        trust_dir(meta.mode() & 0o777, meta.uid(), euid)
            .map_err(|reason| format!("{reason}: {}", ancestor.display()))?;
    }
    Ok(())
}

/// Effective uid of this process, without a libc dependency: a file this
/// process creates is owned by its effective uid. Cached; the probe file
/// is removed immediately. `None` when the data dir is unusable — callers
/// then trust only root-owned directories.
fn current_uid() -> Option<u32> {
    static CACHE: OnceLock<Option<u32>> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let dir = bite_core::config::data_dir();
        std::fs::create_dir_all(&dir).ok()?;
        let probe = dir.join(".uid-probe");
        let uid = std::fs::File::create(&probe)
            .ok()
            .and_then(|_| std::fs::metadata(&probe).ok())
            .map(|m| m.uid());
        let _ = std::fs::remove_file(&probe);
        uid
    })
}

/// Secure `which` over an explicit PATH value, so tests never mutate the
/// environment. Scans in PATH order; the first candidate passing every
/// check wins; otherwise the first rejection is reported.
pub fn scan_path(path_var: &str, name: &str) -> Found {
    let euid = current_uid();
    let mut first_rejection: Option<(PathBuf, String)> = None;
    for entry in std::env::split_paths(path_var) {
        let candidate = entry.join(name);
        let Ok(meta) = std::fs::symlink_metadata(&candidate) else {
            continue; // absent here — keep scanning PATH
        };
        let file_verdict = trust_file(
            meta.file_type().is_symlink(),
            meta.is_file(),
            meta.mode() & 0o777,
        );
        let verdict = match file_verdict {
            Err(reason) => Err(reason),
            Ok(()) => {
                // Resolve relative PATH entries before walking the chain.
                let dir = match std::fs::canonicalize(&entry) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                trust_chain(&dir, euid)
            }
        };
        match verdict {
            Ok(()) => return Found::Trusted(candidate),
            Err(reason) => first_rejection.get_or_insert((candidate, reason)),
        };
    }
    match first_rejection {
        Some((path, reason)) => Found::Rejected(path, reason),
        None => Found::Absent,
    }
}

/// Secure `which` over the process PATH.
pub fn secure_which(name: &str) -> Found {
    match std::env::var("PATH") {
        Ok(p) => scan_path(&p, name),
        Err(_) => Found::Absent,
    }
}

/// File-level trust for tool paths surfaced by macOS's own toolchain
/// selector (`xcrun -f`): a regular executable owned by root that does not
/// live inside the user's home directory. Symlinks are followed — the
/// final file's ownership is what matters.
fn trusted_toolchain_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || (meta.mode() & 0o111 == 0) || meta.uid() != 0 {
        return false;
    }
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    let (Some(home), Ok(real)) = (home, std::fs::canonicalize(path)) else {
        return true;
    };
    let Some(home_real) = std::fs::canonicalize(&home).ok() else {
        return true;
    };
    !real.starts_with(&home_real)
}

/// Locate `tool` through `xcrun` (run only from its fixed `/usr/bin`
/// location) and validate the result at file level. `None` = no toolchain
/// or the reported path failed validation.
pub fn xcrun_find(tool: &str) -> Option<PathBuf> {
    let xcrun = Path::new("/usr/bin/xcrun");
    if !xcrun.is_file() {
        return None;
    }
    let out = std::process::Command::new(xcrun)
        .args(["-f", tool])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let reported = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if reported.is_empty() || reported.contains('\n') {
        return None;
    }
    let path = PathBuf::from(reported);
    trusted_toolchain_file(&path).then_some(path)
}

/// Resolve the `swift` driver for an on-demand helper build: a trusted
/// PATH hit first, then a validated `xcrun -f swift` lookup, then fail
/// with the most specific message the evidence allows.
pub fn resolve_swift() -> Result<PathBuf, String> {
    match secure_which("swift") {
        Found::Trusted(path) => Ok(path),
        Found::Rejected(path, reason) => {
            if let Some(found) = xcrun_find("swift") {
                return Ok(found);
            }
            Err(format!(
                "refusing to run swift at {} — {reason}; install Xcode or its Command Line Tools so the stock toolchain is used",
                path.display()
            ))
        }
        Found::Absent => xcrun_find("swift").ok_or_else(|| {
            "no Swift toolchain found — run `xcode-select --install`, then retry".to_string()
        }),
    }
}

/// Resolve `codesign` for ad-hoc signing: the stock `/usr/bin/codesign`
/// when it validates, else a trusted PATH hit, else `None` (the caller
/// skips signing, exactly as before when codesign was absent).
pub fn resolve_codesign() -> Option<PathBuf> {
    let stock = Path::new("/usr/bin/codesign");
    if trusted_toolchain_file(stock) {
        return Some(stock.to_path_buf());
    }
    match secure_which("codesign") {
        Found::Trusted(path) => Some(path),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn dirs_trust_rules() {
        // Root-owned, tight mode: always fine.
        assert!(trust_dir(0o755, 0, None).is_ok());
        // Any group/other write bit disqualifies, even for root ownership.
        assert!(trust_dir(0o775, 0, Some(0)).is_err());
        assert!(trust_dir(0o757, 0, Some(501)).is_err());
        // User-owned is trusted only when we know the uid is ours.
        assert!(trust_dir(0o700, 501, Some(501)).is_ok());
        assert!(trust_dir(0o700, 501, Some(502)).is_err());
        // Unknown uid → strictly root-owned.
        assert!(trust_dir(0o700, 501, None).is_err());
    }

    #[test]
    fn file_trust_rules() {
        assert!(trust_file(false, true, 0o755).is_ok());
        assert!(trust_file(true, true, 0o755).is_err()); // symlink
        assert!(trust_file(false, false, 0o755).is_err()); // not a regular file
        assert!(trust_file(false, true, 0o644).is_err()); // not executable
    }

    #[test]
    fn scan_missing_candidates_is_absent() {
        assert!(matches!(
            scan_path("/nonexistent-bite-dir-xyz", "swift"),
            Found::Absent
        ));
        // An empty PATH prefix means the CWD (POSIX which semantics); on this
        // crate that contains the `swift/` package DIRECTORY — present but
        // not a regular file, so it must be rejected, not trusted.
        match scan_path("", "swift") {
            Found::Rejected(_, reason) => assert!(reason.contains("regular file")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn scan_rejects_symlinked_tool() {
        let dir = tempfile::Builder::new()
            .prefix("bite-toolchain-sym-")
            .tempdir()
            .unwrap();
        std::os::unix::fs::symlink("/bin/sh", dir.path().join("swift")).unwrap();
        match scan_path(dir.path().to_str().unwrap(), "swift") {
            Found::Rejected(path, reason) => {
                assert_eq!(path, dir.path().join("swift"));
                assert!(reason.contains("symlink"), "reason was: {reason}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn scan_rejects_non_executable_tool() {
        // The old `which` accepted any regular file — even a non-executable
        // one — so a data file named `swift` shadowed the real toolchain.
        let dir = tempfile::Builder::new()
            .prefix("bite-toolchain-noexec-")
            .tempdir()
            .unwrap();
        let tool = dir.path().join("swift");
        std::fs::write(&tool, b"not really swift").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
        match scan_path(dir.path().to_str().unwrap(), "swift") {
            Found::Rejected(_, reason) => {
                assert!(reason.contains("executable"), "reason was: {reason}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn scan_trusts_stock_root_owned_tool() {
        // Find a real (non-symlink) stock executable: macOS keeps /bin/sh,
        // Arch-style layouts symlink /bin → /usr/bin. The rust test lane
        // runs on macOS; this also passes on merged-/usr Linux hosts.
        let candidates = ["/bin/sh", "/usr/bin/sh", "/usr/bin/env"];
        let real = candidates.iter().find(|p| {
            std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
        });
        let Some(real) = real else { return };
        let dir = Path::new(real).parent().unwrap();
        let name = Path::new(real).file_name().unwrap();
        match scan_path(
            &format!("/nonexistent-bite-dir-xyz:{}", dir.display()),
            name.to_str().unwrap(),
        ) {
            Found::Trusted(path) => assert_eq!(path, Path::new(real)),
            other => panic!("expected Trusted, got {other:?}"),
        }
    }

    #[test]
    fn uid_probe_reports_this_process() {
        let uid = current_uid().expect("uid probe needs a usable data dir");
        assert!(uid > 0, "CI must not run tests as root");
    }

    #[test]
    fn toolchain_file_trust() {
        // A root-owned executable outside $HOME validates.
        if Path::new("/bin/sh").exists() {
            let meta = std::fs::metadata("/bin/sh").unwrap();
            if meta.uid() == 0 && meta.mode() & 0o111 != 0 {
                assert!(trusted_toolchain_file(Path::new("/bin/sh")));
            }
        }
        // A file inside the user's own tree never does, whoever owns it.
        let home_file = tempfile::Builder::new()
            .prefix("bite-toolchain-home-")
            .tempfile()
            .unwrap();
        assert!(!trusted_toolchain_file(home_file.path()));
    }
}
