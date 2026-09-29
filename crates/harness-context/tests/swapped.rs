//! Review B, important 2: a file swapped for a FIFO between harness's check and its open must be
//! skipped, never waited on. Each test swaps the file back and forth while harness reads it.

use std::{
    ffi::CString,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use harness_context::{commands, instructions};

const READS: usize = 3000;

/// Replaces `target` with a FIFO, then with a regular file, over and over until `stop` is set.
fn swap_with_a_fifo(target: PathBuf, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let dir = target.parent().unwrap().to_path_buf();
        let fifo = dir.join(".swap-fifo");
        let file = dir.join(".swap-file");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        while !stop.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&fifo);
            assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
            std::fs::rename(&fifo, &target).unwrap();
            std::fs::write(&file, "text\n").unwrap();
            std::fs::rename(&file, &target).unwrap();
        }
    })
}

/// Runs `read` [`READS`] times while `target` is swapped; fails if a read does not return.
fn never_blocks(target: &Path, read: impl Fn() + Send + 'static) {
    std::fs::write(target, "text\n").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let swapper = swap_with_a_fifo(target.to_path_buf(), stop.clone());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..READS {
            read();
        }
        let _ = tx.send(());
    });
    let finished = rx.recv_timeout(Duration::from_secs(30)).is_ok();
    stop.store(true, Ordering::Relaxed);
    swapper.join().unwrap();
    assert!(
        finished,
        "a read blocked on a FIFO swapped in for {}",
        target.display()
    );
}

fn base() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(base.join("config")).unwrap();
    std::fs::create_dir_all(base.join("home/repo/.git")).unwrap();
    (dir, base)
}

#[test]
fn an_instruction_file_swapped_for_a_fifo_is_never_waited_on() {
    let (_dir, base) = base();
    let target = base.join("home/repo/AGENTS.md");
    never_blocks(&target, move || {
        instructions::discover(
            &base.join("home/repo"),
            &base.join("config"),
            Some(&base.join("home")),
        );
    });
}

#[test]
fn a_command_file_swapped_for_a_fifo_is_never_waited_on() {
    let (_dir, base) = base();
    let commands_dir = base.join("home/repo/.claude/commands");
    std::fs::create_dir_all(&commands_dir).unwrap();
    never_blocks(&commands_dir.join("x.md"), move || {
        commands::discover(
            &base.join("home/repo"),
            &base.join("config"),
            Some(&base.join("home")),
        );
    });
}

#[test]
fn a_referenced_file_swapped_for_a_fifo_is_never_waited_on() {
    use harness_context::commands::{CustomCommand, Scope, expand::expand};
    use harness_core::{
        engine::{EngineConfig, PermissionEngine},
        permission::Mode,
    };
    let (_dir, base) = base();
    let ws = base.join("home/repo");
    let command = CustomCommand {
        name: "r".into(),
        path: ws.join(".claude/commands/r.md"),
        scope: Scope::Project,
        description: None,
        argument_hint: None,
        model: None,
        allowed_tools: vec![],
        body: "See @notes.md".into(),
    };
    let policy = PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: ws.clone(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: true,
        writes_need_approval: false,
    });
    never_blocks(&ws.join("notes.md"), move || {
        expand(
            &command,
            "",
            &ws,
            &policy,
            commands::expand::ProjectTrust {
                dir: &ws,
                trusted: false,
            },
        );
    });
}
