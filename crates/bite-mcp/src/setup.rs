//! `bite setup` — the frictionless onboarding path.

use crate::{clients, doctor, helper};

pub fn run(all_clients: bool, yes: bool) -> Result<i32, bite_core::BiteError> {
    println!("bite setup — native Apple apps for agents");
    println!("{}", "─".repeat(48));

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
    let mut any = false;
    for spec in clients::clients() {
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
                Err(e) => println!("    ! failed: {e}"),
            }
        }
    }
    if !any {
        println!("  (no agent CLIs detected — add them anytime with `bite setup`)");
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
    Ok(0)
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
