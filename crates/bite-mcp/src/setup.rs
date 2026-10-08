//! `bite setup` — the frictionless onboarding path.

use crate::{clients, doctor, helper};

pub fn run(all_clients: bool, yes: bool) -> Result<i32, bite_core::BiteError> {
    println!("bite setup — native Apple apps for agents");
    println!("{}", "─".repeat(48));

    // harden data dir first (0700/0600), including on older installs
    let _ = bite_core::fsops::enforce_private_data_dir(&bite_core::config::data_dir());

    // 1. helper
    print!("installing native helper… ");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    match helper::install_helper(false) {
        Ok(path) => println!("ok ({})", path.display()),
        Err(e) => {
            println!();
            eprintln!("error: {}", bite_core::BiteError(e).render());
            eprintln!("hint: run `xcode-select --install` and retry.");
            return Ok(1);
        }
    }

    // 2. doctor (prompt-free)
    println!();
    doctor::run(false, false)?;

    // 3. agent clients
    println!();
    println!("agent clients");
    let specs = clients::clients();
    let mut any = false;
    let mut failures: Vec<(&str, String)> = Vec::new();
    for spec in &specs {
        let installed = spec.installed();
        if installed {
            println!("  ✓ {} already configured", spec.display);
            continue;
        }
        if !spec.detect() {
            continue;
        }
        any = true;
        let should =
            all_clients || yes || ask(&format!("  register MCP server in {}? [Y/n]", spec.display));
        if should {
            match spec.add() {
                Ok(path) => println!("    → added (config: {})", path.display()),
                Err(e) => {
                    println!("    ! failed: {e}");
                    failures.push((spec.display, e));
                }
            }
        }
    }
    if !any {
        println!("  (no agent CLIs detected — add them anytime with `bite setup`)");
    }
    // files left behind by pre-0.3.1 writers — reported, never deleted.
    // On Linux dirs::config_dir() == ~/.config == the live directory; the
    // equality guard inside legacy_scars keeps the "safe to delete" note
    // away from live configs there.
    let legacy_opencode_root = dirs::config_dir()
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_default().into())
        .join("opencode");
    let live_opencode_root = clients::opencode_config_root();
    for scar in clients::legacy_scars(&specs, &legacy_opencode_root, &live_opencode_root) {
        println!("  note: {} — {}", scar.path.display(), scar.note);
    }
    // One client's failure must not hide the others' — and the files that
    // could not be written safely (unparseable configs are left untouched)
    // get a summary so nothing fails silently. stdout stays clean for
    // scripts; the summary is a failure report and goes to stderr.
    let failed = !failures.is_empty();
    if failed {
        eprintln!();
        eprintln!("  ! {} client config(s) left untouched:", failures.len());
        for (name, e) in &failures {
            eprintln!("      {name}: {e}");
        }
        eprintln!("      fix the reported file(s) and re-run `bite setup`");
    }

    // 4. marketplace instructions
    println!();
    println!("{}", "─".repeat(48));
    println!("marketplace install (Claude Code / ZCode):");
    println!("  /plugin marketplace add ktesio/bite-mcp");
    println!("  /plugin install bite@bite-mcp");
    println!();
    println!("next steps:");
    println!("  • bite doctor --fix   repair permissions");
    println!("  • bite reminders lists  first real call (system prompt appears once)");
    println!("  • restart your agent CLI after config changes");
    // doctor's convention: 2 = problems found (here: configs bite could not
    // write safely). Everything that could be done was done.
    Ok(if failed { 2 } else { 0 })
}

fn ask(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt} ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    let l = line.trim().to_ascii_lowercase();
    l.is_empty() || l == "y" || l == "yes"
}
