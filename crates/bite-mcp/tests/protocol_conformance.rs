//! Rust↔Swift protocol conformance: every bridge method in the tool registry
//! must be registered in the Swift dispatcher (or implemented by the
//! standalone crawler process for index.*), and vice versa.

use std::path::PathBuf;

fn swift_sources() -> Vec<PathBuf> {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("swift/Sources");
    let mut out = Vec::new();
    let mut stack = vec![base];
    while let Some(d) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().map(|x| x == "swift").unwrap_or(false) {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

#[test]
fn registry_methods_exist_in_swift_and_no_strays() {
    let sources: Vec<String> = swift_sources()
        .iter()
        .map(|p| std::fs::read_to_string(p).expect("read swift source"))
        .collect();
    let combined = sources.join("\n");

    // Every Rust registry method is implemented in Swift: helper methods via
    // d.register(...), index.* methods by the standalone bite-crawl process
    // (manifest const in BiteCrawlCore).
    let mut missing = Vec::new();
    for tool in bite_core::tools() {
        if tool.method.starts_with("index.") {
            if !combined.contains(tool.method) {
                missing.push(tool.method.to_string());
            }
            continue;
        }
        let needle = format!("d.register(\"{}\")", tool.method);
        if !combined.contains(&needle) {
            missing.push(needle);
        }
    }
    assert!(
        missing.is_empty(),
        "registry methods missing in Swift: {missing:?}"
    );

    // No stray registered helper methods that the registry doesn't know
    // about. (sys.* is infrastructure; index.* lives in the crawler process.)
    let mut strays = Vec::new();
    for src in &sources {
        for line in src.lines() {
            if let Some(idx) = line.find("d.register(\"") {
                let rest = &line[idx + "d.register(\"".len()..];
                if let Some(end) = rest.find('"') {
                    let method = &rest[..end];
                    if method.starts_with("sys.") || method.starts_with("index.") {
                        continue;
                    }
                    if bite_core::tools().iter().all(|t| t.method != method) {
                        strays.push(method.to_string());
                    }
                }
            }
        }
    }
    assert!(
        strays.is_empty(),
        "Swift handlers not in the registry: {strays:?}"
    );
}
