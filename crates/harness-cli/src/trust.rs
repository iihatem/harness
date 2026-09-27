use std::io::{BufRead, IsTerminal, Write};

use harness_config::{config, paths::Paths, trust::TrustStore};

pub fn run(yes: bool, revoke: bool) -> u8 {
    let workspace = match std::env::current_dir().and_then(|d| d.canonicalize()) {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("error: cannot determine the working directory: {e}");
            return 2;
        }
    };
    let paths = match Paths::from_process_env() {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let mut store = match TrustStore::load(&paths.data_dir) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("error: {e}");
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
                eprintln!("error: {e}");
                1
            }
        };
    }
    let widening = match config::project_widening(&paths.global_config_file(), &workspace) {
        Ok(Some(widening)) => widening,
        Ok(None) => {
            println!("No project settings in {} need trust.", workspace.display());
            return 0;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    println!(
        "{} contains settings that widen what the agent may do:",
        config::project_file(&workspace).display()
    );
    for item in &widening.items {
        println!("  - {item}");
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
            eprintln!("error: {e}");
            1
        }
    }
}
