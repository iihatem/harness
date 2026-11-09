//! The CLI's side of worktrees: where they live and how a session opens them.

use std::sync::Arc;

use harness_context::project::{project_key, project_root};
use harness_core::worktree::{Record, Worktrees};

use crate::{setup::Setup, term::terminal_safe};

/// The worktrees of the session's project, kept in `<data>/worktrees/<project>`; `None` when
/// the workspace is not in a git repository (agents then cannot be isolated).
pub fn open(setup: &Setup) -> Option<Arc<Worktrees>> {
    let home = setup
        .paths
        .data_dir
        .join("worktrees")
        .join(project_key(&project_root(&setup.workspace)));
    Worktrees::new(&setup.workspace, home).ok().map(Arc::new)
}

/// The worktrees of the current directory's project, or why there are none.
fn project() -> Result<(Arc<Worktrees>, Setup), String> {
    let setup = crate::setup::load()?;
    let worktrees = open(&setup).ok_or_else(|| {
        "this directory is not in a git repository, so harness made no worktrees here".to_string()
    })?;
    Ok((worktrees, setup))
}

fn state(record: &Record, unsaved: bool) -> String {
    use harness_core::agents::AgentStatus::*;
    match (record.status, &record.outcome) {
        (Done | Failed | Cancelled, None) if unsaved => "ended, its changes are not saved".into(),
        (Done, Some(_)) => "ready for review".into(),
        (Done, None) => "done, nothing changed".into(),
        (Failed, Some(_)) => "failed, its changes kept".into(),
        (Failed, None) => "failed".into(),
        (Cancelled, Some(_)) => "stopped, its changes kept".into(),
        (Cancelled, None) => "stopped".into(),
        _ => "running".into(),
    }
}

fn counts(record: &Record) -> String {
    match &record.outcome {
        Some(o) => format!(
            "{} file{}, +{} -{}",
            o.files.len(),
            if o.files.len() == 1 { "" } else { "s" },
            o.insertions,
            o.deletions
        ),
        None => "no changes".into(),
    }
}

/// `harness worktrees`: what harness made here, one line each.
pub fn list() -> u8 {
    let (worktrees, _setup) = match project() {
        Ok(found) => found,
        Err(why) => {
            eprintln!("error: {}", terminal_safe(&why));
            return 2;
        }
    };
    let records = worktrees.records();
    if records.is_empty() {
        println!("harness has made no worktrees for this project.");
        return 0;
    }
    for record in records {
        let unsaved = record.status.is_final()
            && record.outcome.is_none()
            && worktrees.unsaved(&record).is_some();
        println!(
            "{}  {}  {}  {}",
            terminal_safe(&record.branch),
            state(&record, unsaved),
            counts(&record),
            terminal_safe(&record.path.display().to_string())
        );
    }
    0
}

/// Asks the user to confirm losing `lost`; `false` when they do not, or cannot be asked.
fn confirm_loss(lost: &[(String, String)], yes: bool) -> bool {
    use std::io::{BufRead, IsTerminal, Write};
    println!("these hold work that exists nowhere else, and would be lost for good:");
    for (branch, what) in lost {
        println!("  {} ({})", terminal_safe(branch), terminal_safe(what));
    }
    if yes {
        return true;
    }
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "error: refusing to discard unsaved work without a terminal to ask on: nothing was removed (--yes says you are sure)"
        );
        return false;
    }
    print!("type \"discard\" to lose them: ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    answer.trim() == "discard"
}

/// `harness worktrees clean`: removes what has nothing left to review and nothing to lose. A
/// running agent's worktree, a result not applied (without `all`), anything harness did not
/// make and any worktree that holds changes or commits saved nowhere else stay (and the last are
/// listed); `discard_unsaved` (with `all`) removes those too, after asking.
pub fn clean(all: bool, discard_unsaved: bool, yes: bool) -> u8 {
    let (worktrees, _setup) = match project() {
        Ok(found) => found,
        Err(why) => {
            eprintln!("error: {}", terminal_safe(&why));
            return 2;
        }
    };
    for name in worktrees.release_stale_locks() {
        println!("released the lock of {name}: the harness that held it is gone");
    }
    for name in worktrees.reconcile() {
        println!("settled {name}: its agent was left running by a harness that ended");
    }
    let mut kept_results = 0;
    let mut removable = Vec::new();
    let mut unsaved = Vec::new();
    for record in worktrees.records() {
        if !record.status.is_final() {
            println!(
                "left {} alone: its agent is running",
                terminal_safe(&record.branch)
            );
        } else if record.outcome.is_some() && !all {
            kept_results += 1;
        } else {
            match worktrees.unsaved(&record) {
                Some(what) => unsaved.push((record, what)),
                None => removable.push(record),
            }
        }
    }
    if discard_unsaved && !unsaved.is_empty() {
        let lost: Vec<(String, String)> = unsaved
            .iter()
            .map(|(record, what)| (record.branch.clone(), what.clone()))
            .collect();
        if !confirm_loss(&lost, yes) {
            println!("nothing was removed.");
            return 2;
        }
    }
    for record in removable {
        match worktrees.remove(&record.name) {
            Ok(()) => println!("removed {} and its branch", terminal_safe(&record.branch)),
            Err(e) => println!(
                "left {} alone: {}",
                terminal_safe(&record.branch),
                terminal_safe(&e.to_string())
            ),
        }
    }
    for (record, what) in unsaved {
        if discard_unsaved {
            match worktrees.discard(&record.name) {
                Ok(()) => println!("discarded {} and its branch", terminal_safe(&record.branch)),
                Err(e) => println!(
                    "left {} alone: {}",
                    terminal_safe(&record.branch),
                    terminal_safe(&e.to_string())
                ),
            }
        } else {
            println!(
                "left {} alone: it {}",
                terminal_safe(&record.branch),
                terminal_safe(&what)
            );
        }
    }
    if kept_results > 0 {
        println!(
            "kept {kept_results} result{} that {} not applied or kept: /results in a session reviews {}; `harness worktrees clean --all` removes {} and the branches once their commits are elsewhere",
            if kept_results == 1 { "" } else { "s" },
            if kept_results == 1 { "is" } else { "are" },
            if kept_results == 1 { "it" } else { "them" },
            if kept_results == 1 { "it" } else { "them" },
        );
    }
    0
}
