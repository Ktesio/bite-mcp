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
        std::fs::create_dir_all(parent).map_err(|e| BridgeError::spawn(e.to_string()))?;
    }
    std::fs::copy(src, stable).map_err(|e| BridgeError::spawn(format!("copy helper: {e}")))?;
    set_exec(stable);
    adhoc_sign(stable);
    Ok(stable.clone())
}

fn compile_on_demand(stable: &PathBuf) -> Result<PathBuf, BridgeError> {
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
    let scratch = bite_core::config::data_dir().join("swift-build");
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
