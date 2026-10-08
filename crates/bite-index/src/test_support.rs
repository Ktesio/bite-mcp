//! Test-only scratch directories that clean up after themselves.
//!
//! `tempfile::TempDir` implements only `AsRef<Path>`, which forces `.path()`
//! noise at every call site. `ScratchDir` wraps it with `Deref<Target =
//! Path>` so tests read exactly like the old hand-rolled `PathBuf` helpers
//! (`.join(...)`, `&dir` into `&Path` params) — while drop, including during
//! panic unwind, removes the directory. (A SIGKILL can still leak it; that
//! race is accepted.)

use std::path::Path;

pub struct ScratchDir(tempfile::TempDir);

impl std::ops::Deref for ScratchDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        self.0.path()
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

/// Create a uniquely-named scratch dir under $TMPDIR with the given prefix.
pub fn scratch(prefix: &str) -> ScratchDir {
    ScratchDir(
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("create temp dir"),
    )
}
