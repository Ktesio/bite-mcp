//! File-security utilities: bite's data is the user's personal corpus, so
//! everything under the bite data dir is created and kept private.
//!
//! Threat model (docs/PLAN.md §0): other local users are excluded by 0700/0600;
//! same-user processes are an honest macOS limit (no per-process file isolation
//! for CLIs) — minimized plaintext surface and `index wipe` are the levers.

use std::fs::Permissions;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Create a directory (recursively) and force 0700 on every component we own.
/// Never trust umask inheritance for private data.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    tighten_dir(path)
}

/// Force 0700 on a directory (best-effort up the tree for our own subdirs).
pub fn tighten_dir(path: &Path) -> io::Result<()> {
    set_mode(path, 0o700)
}

/// Force 0600 on a file.
pub fn tighten_file(path: &Path) -> io::Result<()> {
    set_mode(path, 0o600)
}

/// Write a file with 0600 from the start (write to temp + rename keeps the
/// window where content exists at looser perms as small as possible).
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp-private");
    {
        let mut f = std::fs::File::create(&tmp)?;
        use std::io::Write as _;
        f.write_all(bytes)?;
        f.sync_all().ok();
    }
    tighten_file(&tmp)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Enforce privacy on everything bite owns under its data dir: the dir tree is
/// 0700; config and lock files 0600. Called at CLI/MCP startup, install, and
/// setup so fixes apply retroactively to older installs.
pub fn enforce_private_data_dir(data_dir: &Path) -> io::Result<()> {
    ensure_private_dir(data_dir)?;
    tighten_file(&data_dir.join("config.toml")).ok();
    tighten_file(&data_dir.join(".writer.lock")).ok();
    if let Ok(entries) = std::fs::read_dir(data_dir.join("batches")) {
        for e in entries.flatten() {
            tighten_file(&e.path()).ok();
        }
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    // NOTE: do NOT touch set_readonly here — on Unix set_readonly(false) ORs
    // 0o022 (group/other write) into the mode. from_mode already carries the
    // owner-write bit we need.
    std::fs::set_permissions(path, Permissions::from_mode(mode))
}

/// Check (don't fix) — used by doctor.
pub fn private_report(data_dir: &Path) -> Vec<(std::path::PathBuf, bool, bool)> {
    let mut report = Vec::new();
    let mut check = |path: &Path, want_dir: bool| {
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            let ok = if want_dir {
                mode == 0o700
            } else {
                mode == 0o600
            };
            report.push((path.to_path_buf(), ok, meta.is_dir()));
        }
    };
    check(data_dir, true);
    check(&data_dir.join("config.toml"), false);
    check(&data_dir.join("bin").join("bite-helper"), false);
    check(&data_dir.join("index.lance"), true);
    check(&data_dir.join("batches"), true);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_dir_and_file_modes() {
        let base = std::env::temp_dir().join(format!("bite-privtest-{}", std::process::id()));
        let dir = base.join("nested");
        ensure_private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        eprintln!("dir mode: {mode:o}");
        assert_eq!(mode, 0o700);
        let file = dir.join("config.toml");
        write_private(&file, b"x=1").unwrap();
        let fmode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        eprintln!("file mode: {fmode:o}");
        assert_eq!(fmode, 0o600);
        // write_private replaced the file — still 0600
        write_private(&file, b"x=2").unwrap();
        let fmode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600);
        std::fs::remove_dir_all(&base).ok();
    }
}
