//! Compile the embedded Swift helper at build time (fast path for dev builds).
//!
//! - Runs `swift build` on `swift/Package.swift` into `<target>/<profile>/swift-build/`
//!   (content-hash gated so untouched Swift sources don't re-trigger).
//! - Exposes the product as `BITE_HELPER_BUILT` baked into the binary.
//! - Falls back silently when no Swift toolchain is present — the binary still
//!   builds; `bite install-helper` compiles on demand later.
//! - `cargo install` cleans its build dir, so the installed binary relies on
//!   the on-demand path from the packaged `swift/` sources; this baked path is
//!   a convenience for `cargo run` / CI.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    println!("cargo:rerun-if-changed=swift");
    println!("cargo:rerun-if-env-changed=BITE_SKIP_SWIFT_BUILD");
    if std::env::var("BITE_SKIP_SWIFT_BUILD").is_ok() {
        return;
    }
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let swift_pkg = manifest_dir.join("swift");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    // OUT_DIR = <target>/<profile>/build/bite-mcp-<hash> → target/<profile>
    let target_profile_dir = out_dir
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| out_dir.clone());
    let scratch = target_profile_dir.join("swift-build");
    let fp_file = out_dir.join("bite-swift.fingerprint");

    let hash = hash_sources(&swift_pkg);
    let bin_path = scratch.join(format!(".binpath-{profile}"));
    if let Ok(saved) = std::fs::read_to_string(&fp_file) {
        if let Ok(baked) = std::fs::read_to_string(&bin_path) {
            let baked = baked.trim();
            if saved == format!("{hash}\n{}", bin_path.display()) && PathBuf::from(baked).exists() {
                println!("cargo:rustc-env=BITE_HELPER_BUILT={baked}");
                return;
            }
        }
    }

    // locate swiftc without failing the build
    let has_swiftc = which("swiftc").is_some() || xcrun_swiftc().is_some();
    if !has_swiftc {
        eprintln!(
            "bite build: no Swift toolchain found; helper will be compiled on demand by `bite install-helper` (requires Xcode Command Line Tools)"
        );
        return;
    }

    let status = Command::new("swift")
        .args([
            "build",
            "-c",
            &profile,
            "--package-path",
            swift_pkg.to_str().unwrap(),
            "--scratch-path",
            scratch.to_str().unwrap(),
        ])
        .status();
    match status {
        Ok(s) if s.success() => {
            let bin_dir = Command::new("swift")
                .args([
                    "build",
                    "--show-bin-path",
                    "-c",
                    &profile,
                    "--package-path",
                    swift_pkg.to_str().unwrap(),
                    "--scratch-path",
                    scratch.to_str().unwrap(),
                ])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
            if let Some(bin_dir) = bin_dir {
                let helper = PathBuf::from(&bin_dir).join("bite-helper");
                if helper.exists() {
                    let _ = std::fs::write(&bin_path, format!("{}\n", helper.display()));
                    let _ = std::fs::write(&fp_file, format!("{hash}\n{}", bin_path.display()));
                    println!("cargo:rustc-env=BITE_HELPER_BUILT={}", helper.display());
                }
            }
        }
        other => {
            eprintln!(
                "bite build: swift build failed ({other:?}); continuing without baked helper"
            );
        }
    }
}

fn hash_sources(root: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    fn walk(dir: &Path, hasher: &mut DefaultHasher) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, hasher);
            } else if path
                .extension()
                .map(|e| e == "swift" || e == "h" || e == "modulemap")
                .unwrap_or(false)
                || path
                    .file_name()
                    .map(|f| f == "Package.swift")
                    .unwrap_or(false)
            {
                hasher.write(path.to_string_lossy().as_bytes());
                if let Ok(bytes) = std::fs::read(&path) {
                    hasher.write(&bytes);
                }
            }
        }
    }
    walk(root, &mut hasher);
    hasher.finish()
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

fn xcrun_swiftc() -> Option<PathBuf> {
    Command::new("xcrun")
        .args(["-f", "swiftc"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string()))
}
