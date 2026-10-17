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
            // the rendered error already names its real fix — a hardcoded
            // `xcode-select --install` hint here was wrong for every other
            // failure mode (missing dir, sign failure, …)
            eprintln!("error: {}", bite_core::BiteError(e).render());
            return Ok(1);
        }
    }

    // 2. doctor (prompt-free)
    println!();
    doctor::run(false, false, false)?;

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

/// The `[Y/n]` decision for one answer line: an EMPTY line is the default
/// Yes; y/yes accept; everything else declines.
fn yes_answer(line: &str) -> bool {
    let l = line.trim().to_ascii_lowercase();
    l.is_empty() || l == "y" || l == "yes"
}

/// One prompt answered from `input`. Returns false on EOF or a read error:
/// a closed stdin (scripted/headless runs without `--yes`) must NOT read as
/// "Yes" — silently registering every detected client is the opposite of
/// consent. An explicit empty line (plain Enter) is still the default Yes.
fn ask_reader<R: std::io::BufRead>(prompt: &str, input: &mut R) -> bool {
    use std::io::Write;
    print!("{prompt} ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => false, // EOF (closed stdin) / read error = No
        Ok(_) => yes_answer(&line),
    }
}

fn ask(prompt: &str) -> bool {
    let mut stdin = std::io::stdin().lock();
    ask_reader(prompt, &mut stdin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_eof_is_no_but_empty_line_is_yes() {
        let answer = |input: &'static [u8]| {
            let mut r = std::io::BufReader::new(input);
            ask_reader("  register? [Y/n]", &mut r)
        };
        // closed stdin (EOF, zero bytes) is "No" — scripted runs must not
        // silently register every detected client
        assert!(!answer(b""), "EOF must read as No");
        // an explicit empty line (plain Enter) is still the [Y/n] default Yes
        assert!(answer(b"\n"));
        assert!(answer(b"  \n"));
        // explicit y/yes/n/no
        assert!(answer(b"y\n"));
        assert!(answer(b"Y\n"));
        assert!(answer(b" yes \n"));
        assert!(!answer(b"n\n"));
        assert!(!answer(b"No\n"));
        // garbage declines rather than guessing
        assert!(!answer(b"maybe\n"));
    }
}
