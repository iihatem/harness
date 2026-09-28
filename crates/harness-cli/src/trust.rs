use std::io::{BufRead, IsTerminal, Write};

use harness_config::{config, paths::Paths, trust::TrustStore};

use crate::term::terminal_safe;

pub fn run(yes: bool, revoke: bool) -> u8 {
    let workspace = match std::env::current_dir().and_then(|d| d.canonicalize()) {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!(
                "error: cannot determine the working directory: {}",
                terminal_safe(&e.to_string())
            );
            return 2;
        }
    };
    let paths = match Paths::from_process_env() {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 2;
        }
    };
    let mut store = match TrustStore::load(&paths.data_dir) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 1;
        }
    };
    if revoke {
        return match store.revoke(&workspace) {
            Ok(true) => {
                println!("Revoked trust for {}.", workspace.display());
                0
            }
            Ok(false) => {
                println!("{} was not trusted.", workspace.display());
                0
            }
            Err(e) => {
                eprintln!("error: {}", terminal_safe(&e.to_string()));
                1
            }
        };
    }
    let widening = match config::project_widening(&paths.global_config_file(), &workspace) {
        Ok(widening) => widening,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 2;
        }
    };
    if widening.items.is_empty() {
        // Trust still matters: a trusted workspace's command files may choose their model.
        println!(
            "No project settings in {} widen what the agent may do. Trusting it lets its command files choose their model, until such settings appear.",
            workspace.display()
        );
    } else {
        println!(
            "{} contains settings that need trust:",
            config::project_file(&workspace).display()
        );
        for item in &widening.items {
            println!("  - {}", terminal_safe(item));
        }
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            eprintln!("error: stdin is not a terminal; re-run with --yes to trust this workspace");
            return 2;
        }
        print!("Trust this workspace? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().lock().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Not trusted.");
            return 0;
        }
    }
    match store.trust(&workspace, &widening.fingerprint) {
        Ok(()) => {
            println!("Trusted {}.", workspace.display());
            0
        }
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            1
        }
    }
}
