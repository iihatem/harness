//! Which session a run continues: a new one, the project's most recent (`-c`), or one by id
//! (`--resume <id>`). `harness --resume` without an id lists the project's sessions.

use std::{path::PathBuf, sync::Arc};

use harness_context::project::{project_key, project_root};
use harness_core::{
    checkpoint::Checkpoints,
    session::{self, Session},
};

use crate::{setup, setup::Setup, term::terminal_safe};

/// The session a run starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    New,
    /// The project's most recently used session (`-c`).
    Continue,
    /// A session by id (`--resume <id>`).
    Resume(String),
}

/// Where this project's sessions are saved.
pub fn dir(setup: &Setup) -> PathBuf {
    setup
        .paths
        .data_dir
        .join("sessions")
        .join(project_key(&project_root(&setup.workspace)))
}

/// The checkpoints of `session`, in the project's shadow repository. `None`, after a warning,
/// when they cannot work (no `git` on `PATH`, or the repository cannot be created).
pub fn checkpoints(setup: &Setup, session: &Session) -> Option<Arc<Checkpoints>> {
    let key = project_key(&project_root(&setup.workspace));
    let gitdir = setup
        .paths
        .data_dir
        .join("checkpoints")
        .join(format!("{key}.git"));
    match Checkpoints::open(&gitdir, &setup.workspace, session.id()) {
        Ok(checkpoints) => Some(Arc::new(checkpoints)),
        Err(e) => {
            eprintln!(
                "warning: checkpoints are disabled: {}; turns run normally but cannot be rewound",
                terminal_safe(&e.to_string())
            );
            None
        }
    }
}

/// Opens the session to run in, printing any warnings about its file. Errors are user-facing
/// messages (exit code 2).
pub fn open(setup: &Setup, choice: &Choice) -> Result<Session, String> {
    let dir = dir(setup);
    let path = match choice {
        Choice::New => return Ok(Session::create(&dir, &setup.workspace)),
        Choice::Continue => session::list(&dir)
            .into_iter()
            .next()
            .map(|s| s.path)
            .ok_or("there is no earlier session in this project to continue")?,
        Choice::Resume(id) => {
            // The id becomes a path: only a valid one is looked up.
            let path = dir.join(format!("{id}.jsonl"));
            if !session::is_valid_id(id) || !path.is_file() {
                return Err(format!(
                    "there is no session {id} in this project; run `harness --resume` to list them"
                ));
            }
            path
        }
    };
    let (session, warnings) = Session::open(&path).map_err(|e| e.to_string())?;
    for warning in warnings {
        eprintln!("warning: {}", terminal_safe(&warning));
    }
    Ok(session)
}

/// `harness --resume` without an id: prints each session of this project, most recent first,
/// with its id, start time and first message.
pub fn print_list() -> u8 {
    let setup = match setup::load() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    let listed = session::list(&dir(&setup));
    if listed.is_empty() {
        println!("No sessions in this project yet.");
        return 0;
    }
    for summary in listed {
        let first: String = summary
            .first_message
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(72)
            .collect();
        println!(
            "{}  {}  {}",
            terminal_safe(&summary.id),
            terminal_safe(&summary.started_at),
            terminal_safe(&first)
        );
    }
    println!("Continue one with: harness --resume <id> ask \"...\"");
    0
}
