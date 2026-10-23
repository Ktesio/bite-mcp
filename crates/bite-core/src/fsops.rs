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

/// Per-call temp-name counter: monotonic within the process, so uniqueness
/// never depends on clock granularity (CI VM clocks hand identical nanos to
/// concurrent calls — a timestamp-only temp name collided there).
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

/// Write a file with 0600 from the start (write to a per-call temp +
/// rename keeps the window where content exists at looser perms as small
/// as possible, and unique temp names keep concurrent writers to the same
/// destination from interleaving into one shared inode).
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // One temp file per call (pid + monotonic counter): two concurrent
    // writers to the same destination (e.g. two `bite config set`
    // invocations) must never share a temp inode — interleaved write_alls
    // would publish a mix of both payloads under the surviving rename, and
    // the loser's rename can fail ENOENT after the winner already moved the
    // shared temp away. The counter (not the clock) provides uniqueness:
    // timestamp-only names collided on CI VMs, whose coarse clocks hand
    // identical nanos to back-to-back calls.
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_owned();
    let tmp = path.with_file_name(format!("{name}.tmp-private-{pid}-{seq}"));
    let write = || -> io::Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        // 0600 at creation: File::create would honor umask (0666 & ~umask)
        // and leave a world-readable window until a later chmod.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all().ok();
        Ok(())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Enforce privacy on everything bite owns under its data dir: the dir tree
/// is 0700 (root, `batches/`, its subdirs like `quarantine/`, and the
/// `index.lance` store); config, lock, batch and quarantined files 0600.
/// Called at CLI/MCP startup (dispatch), install, and setup so fixes apply
/// retroactively to older installs — including a crawl-first lifecycle where
/// the helper materialized the dirs before any Rust code ran.
pub fn enforce_private_data_dir(data_dir: &Path) -> io::Result<()> {
    ensure_private_dir(data_dir)?;
    tighten_file(&data_dir.join("config.toml")).ok();
    tighten_file(&data_dir.join(".writer.lock")).ok();
    tighten_dir(&data_dir.join("index.lance")).ok();
    // batches/ itself (the old code tightened only its files — a dir created
    // by the Swift crawler with FileManager defaults stayed 0755), every
    // subdir (quarantine/), and every file at both levels.
    let batches = data_dir.join("batches");
    if let Ok(entries) = std::fs::read_dir(&batches) {
        tighten_dir(&batches).ok();
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                tighten_dir(&path).ok();
                if let Ok(inner) = std::fs::read_dir(&path) {
                    for q in inner.flatten() {
                        tighten_file(&q.path()).ok();
                    }
                }
            } else {
                tighten_file(&path).ok();
            }
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
        let base = tempfile::Builder::new()
            .prefix("bite-privtest-")
            .tempdir()
            .unwrap();
        let dir = base.path().join("nested");
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
    }

    #[test]
    fn concurrent_write_private_never_corrupts_destination() {
        // Two writers to the same destination previously shared ONE fixed
        // temp name (`tmp-private`): interleaved `write_all`s into the same
        // inode could publish a mix of both payloads under the surviving
        // rename, and the rename race surfaced as spurious ENOENT errors.
        // Each write must use its own temp file so concurrent writers never
        // touch each other's bytes.
        let base = tempfile::Builder::new()
            .prefix("bite-wpriv-race-")
            .tempdir()
            .unwrap();
        let dest = base.path().join("config.toml");
        write_private(&dest, b"init").unwrap();
        let dest = std::sync::Arc::new(dest);
        let handles: Vec<_> = (0..8u8)
            .map(|t| {
                let dest = std::sync::Arc::clone(&dest);
                std::thread::spawn(move || {
                    // payload = one writer-identifying byte repeated: a
                    // legitimate outcome is exactly one distinct byte
                    // everywhere; ANY mix of two bytes (or a short file) is
                    // cross-writer corruption
                    let payload = [b'a' + t; 8192];
                    for _ in 0..200 {
                        write_private(&dest, &payload)
                            .unwrap_or_else(|e| panic!("write failed: {e}"));
                        let got = std::fs::read(&*dest).unwrap();
                        assert_eq!(got.len(), 8192, "destination truncated");
                        assert!(
                            got.iter().all(|&b| b == got[0]),
                            "destination holds a mix of writers' bytes"
                        );
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // quiescent: no temp litter left behind
        for e in std::fs::read_dir(base.path()).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            assert!(!name.contains("tmp-private"), "temp file leaked: {name}");
        }
    }

    #[test]
    fn enforce_tightens_batches_dirs_and_quarantine() {
        let base = tempfile::Builder::new()
            .prefix("bite-enforce-test-")
            .tempdir()
            .unwrap();
        let dir = base.path().join("bite");
        // simulate a crawl-first lifecycle: dirs and files exist at loose
        // modes exactly as FileManager.createDirectory defaults leave them
        let batches = dir.join("batches");
        let quarantine = batches.join("quarantine");
        std::fs::create_dir_all(&quarantine).unwrap();
        std::fs::write(batches.join("crawl-1-0.jsonl"), b"{}").unwrap();
        std::fs::write(quarantine.join("crawl-0-9.jsonl"), b"bad").unwrap();
        std::fs::write(dir.join("config.toml"), b"x=1").unwrap();
        std::fs::create_dir(dir.join("index.lance")).unwrap();
        // Permissions/PermissionsExt come from the parent module via super::*
        let loosen = |p: &std::path::Path| {
            std::fs::set_permissions(p, Permissions::from_mode(0o755)).unwrap();
        };
        loosen(&dir);
        loosen(&batches);
        loosen(&quarantine);
        loosen(&dir.join("index.lance"));

        enforce_private_data_dir(&dir).unwrap();

        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700, "data dir root");
        assert_eq!(mode(&batches), 0o700, "batches/ dir itself");
        assert_eq!(mode(&batches.join("crawl-1-0.jsonl")), 0o600, "batch file");
        assert_eq!(mode(&quarantine), 0o700, "quarantine/ dir");
        assert_eq!(
            mode(&quarantine.join("crawl-0-9.jsonl")),
            0o600,
            "quarantined file"
        );
        assert_eq!(mode(&dir.join("config.toml")), 0o600, "config.toml");
        assert_eq!(mode(&dir.join("index.lance")), 0o700, "index.lance dir");
    }

    #[test]
    fn enforce_on_missing_tree_is_ok() {
        // fresh install: nothing exists yet — enforcement must not error
        let base = tempfile::Builder::new()
            .prefix("bite-enforce-empty-")
            .tempdir()
            .unwrap();
        assert!(enforce_private_data_dir(&base.path().join("bite")).is_ok());
    }
}
