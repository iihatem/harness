use std::{
    io::{BufRead, IsTerminal, Write},
    path::Path,
    time::{Duration, Instant},
};

use harness_config::{config, paths::Paths, trust::TrustStore};
use harness_context::project::project_root;
use harness_providers::credentials::Credentials;
use harness_tui::approval::ARMING_DELAY;
use nix::{
    libc,
    sys::termios::{
        FlushArg, LocalFlags, SetArg, SpecialCharacterIndices, Termios, tcflush, tcgetattr,
        tcsetattr,
    },
};

use crate::term::terminal_safe;

/// The first-use question.
const QUESTION: &str = "Trust this workspace, so these settings apply? [y/N]";

/// Said when what the user typed as the question appeared was thrown away.
const DISCARDED: &str =
    "What you typed was not taken as the answer: the question takes one once you pause.";

/// The terminal the first-use question is asked on, as far as its answer goes: only what the
/// user types once they have seen the question and paused answers it, as for the session's
/// prompts.
pub trait Typing {
    /// Throws away what was typed and not read yet.
    fn discard(&mut self);
    /// Returns once the user has typed nothing for `quiet`, throwing away what they type
    /// meanwhile; whether they typed anything.
    fn pause(&mut self, quiet: Duration) -> bool;
}

/// This process's terminal, its standard input.
pub struct StdinTyping;

impl Typing for StdinTyping {
    fn discard(&mut self) {
        let _ = tcflush(std::io::stdin(), FlushArg::TCIFLUSH);
    }

    /// Keys are seen as they are typed, rather than a line at a time, and not echoed, until the
    /// user pauses; the terminal's modes are then put back. Ctrl+C, Ctrl+\ and Ctrl+Z still do
    /// what they do, with the modes put back first. So do SIGTERM, SIGHUP and SIGQUIT sent
    /// meanwhile: held until the modes are back, they take effect then.
    fn pause(&mut self, quiet: Duration) -> bool {
        let stdin = std::io::stdin();
        let Ok(cooked) = tcgetattr(&stdin) else {
            std::thread::sleep(quiet);
            return false;
        };
        let mut waiting = cooked.clone();
        waiting
            .local_flags
            .remove(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG);
        waiting.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        waiting.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        // Dropped after the modes are put back, whichever way this returns.
        let held = Held::new();
        if tcsetattr(&stdin, SetArg::TCSANOW, &waiting).is_err() {
            std::thread::sleep(quiet);
            return false;
        }
        let mut typed = false;
        let mut quiet_until = Instant::now() + quiet;
        loop {
            let left = quiet_until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let mut poll = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            let millis = libc::c_int::try_from(left.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
            // SAFETY: one valid `pollfd`, and its count.
            let ready = unsafe { libc::poll(&mut poll, 1, millis) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if ready == 0 {
                continue;
            }
            if poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                break;
            }
            // Read past std's buffer, which would keep what is thrown away for the answer.
            let mut buf = [0u8; 1024];
            // SAFETY: reads at most `buf.len()` bytes into `buf`.
            let read =
                unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
            if read <= 0 {
                break;
            }
            typed = true;
            quiet_until = Instant::now() + quiet;
            let read = &buf[..read as usize];
            if let Some(signal) = signal_key(&cooked, read) {
                let _ = tcsetattr(&stdin, SetArg::TCSANOW, &cooked);
                held.release();
                let _ = nix::sys::signal::raise(signal);
                held.hold();
                // Back from a stop (Ctrl+Z): wait for the pause again.
                if tcsetattr(&stdin, SetArg::TCSANOW, &waiting).is_err() {
                    return typed;
                }
                quiet_until = Instant::now() + quiet;
            }
        }
        let _ = tcsetattr(&stdin, SetArg::TCSANOW, &cooked);
        drop(held);
        typed
    }
}

/// SIGTERM, SIGHUP and SIGQUIT, blocked until dropped, while the terminal's modes are not its
/// own: one sent meanwhile stays pending, and is delivered once the modes are put back and the
/// signals are unblocked. The mask is this thread's, which is the process's: the question is
/// asked before harness starts any other thread.
struct Held {
    signals: nix::sys::signal::SigSet,
    /// The mask before, put back when dropped.
    before: Option<nix::sys::signal::SigSet>,
}

impl Held {
    fn new() -> Held {
        use nix::sys::signal::{SigSet, Signal};
        let mut signals = SigSet::empty();
        for signal in [Signal::SIGTERM, Signal::SIGHUP, Signal::SIGQUIT] {
            signals.add(signal);
        }
        let mut before = SigSet::empty();
        let blocked = nix::sys::signal::pthread_sigmask(
            nix::sys::signal::SigmaskHow::SIG_BLOCK,
            Some(&signals),
            Some(&mut before),
        );
        Held {
            signals,
            before: blocked.is_ok().then_some(before),
        }
    }

    /// Unblocks them, for a signal a key sends: one pending is delivered now.
    fn release(&self) {
        if let Some(before) = &self.before {
            let _ = nix::sys::signal::pthread_sigmask(
                nix::sys::signal::SigmaskHow::SIG_SETMASK,
                Some(before),
                None,
            );
        }
    }

    /// Blocks them again.
    fn hold(&self) {
        if self.before.is_some() {
            let _ = nix::sys::signal::pthread_sigmask(
                nix::sys::signal::SigmaskHow::SIG_BLOCK,
                Some(&self.signals),
                None,
            );
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.release();
    }
}

/// The signal a key among `read` would send in the terminal's `cooked` modes: interrupt, quit or
/// stop.
fn signal_key(cooked: &Termios, read: &[u8]) -> Option<nix::sys::signal::Signal> {
    use nix::sys::signal::Signal;
    if !cooked.local_flags.contains(LocalFlags::ISIG) {
        return None;
    }
    let key = |index: SpecialCharacterIndices| cooked.control_chars[index as usize];
    // A disabled key reads as `_POSIX_VDISABLE`, 0 on Linux and 0xff on macOS.
    let disabled = |byte: u8| byte == 0 || byte == 0xff;
    [
        (SpecialCharacterIndices::VINTR, Signal::SIGINT),
        (SpecialCharacterIndices::VQUIT, Signal::SIGQUIT),
        (SpecialCharacterIndices::VSUSP, Signal::SIGTSTP),
    ]
    .into_iter()
    .find(|(index, _)| {
        let byte = key(*index);
        !disabled(byte) && read.contains(&byte)
    })
    .map(|(_, signal)| signal)
}

/// The lines that list a workspace's widening settings, as `harness trust` and the interactive
/// session's first-use prompt show them.
pub fn describe_settings(workspace: &Path, items: &[String]) -> Vec<String> {
    let mut lines = vec![format!(
        "{} contains settings that need trust:",
        terminal_safe(&config::project_file(workspace).display().to_string())
    )];
    lines.extend(
        items
            .iter()
            .map(|item| format!("  - {}", terminal_safe(item))),
    );
    lines
}

/// How the first-use prompt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstUse {
    /// The workspace has no widening settings, or they are trusted already: nothing was asked.
    NothingToAsk,
    Trusted,
    Declined,
}

/// On interactive use of `workspace` while its project settings that widen what the agent may do
/// are not trusted, shows them on `out` and asks whether to trust the workspace, reading the
/// answer from `answer`. Only an answer typed for the question counts (`typing`): what was typed
/// before it showed is thrown away before it is written, and it takes an answer only once the
/// user has paused, as the session's prompts do. Trusting records it as `harness trust` does;
/// declining is asked again next time. Settings that cannot be read are left for loading the
/// configuration to report.
pub fn first_use(
    workspace: &Path,
    paths: &Paths,
    answer: &mut dyn BufRead,
    out: &mut dyn Write,
    typing: &mut dyn Typing,
) -> std::io::Result<FirstUse> {
    let Ok(widening) = config::project_widening(&paths.global_config_file(), workspace) else {
        return Ok(FirstUse::NothingToAsk);
    };
    let Ok(mut store) = TrustStore::load(&paths.data_dir) else {
        return Ok(FirstUse::NothingToAsk);
    };
    if widening.items.is_empty() || store.is_trusted(workspace, &widening.fingerprint) {
        return Ok(FirstUse::NothingToAsk);
    }
    typing.discard();
    for line in describe_settings(workspace, &widening.items) {
        writeln!(out, "{line}")?;
    }
    write!(out, "{QUESTION} ")?;
    out.flush()?;
    if typing.pause(ARMING_DELAY) {
        writeln!(out)?;
        writeln!(out, "{DISCARDED}")?;
        write!(out, "{QUESTION} ")?;
        out.flush()?;
    }
    let mut reply = String::new();
    answer.read_line(&mut reply)?;
    if !matches!(reply.trim(), "y" | "Y" | "yes") {
        writeln!(
            out,
            "Not trusted: harness runs without these settings. Run `harness trust` to review them again."
        )?;
        return Ok(FirstUse::Declined);
    }
    store
        .trust(workspace, &widening.fingerprint)
        .map_err(std::io::Error::other)?;
    // Trusting a workspace lets its language servers start, so they are not asked about.
    store
        .set_servers_answer(workspace, true)
        .map_err(std::io::Error::other)?;
    writeln!(
        out,
        "Trusted {}.",
        terminal_safe(&workspace.display().to_string())
    )?;
    Ok(FirstUse::Trusted)
}

/// What `harness trust --revoke` says it did.
fn revoked_message(workspace: &str, revoked: harness_config::trust::Revoked) -> String {
    match (revoked.trusted, revoked.servers_answer) {
        (true, _) => format!("Revoked trust for {workspace}."),
        (false, true) => format!(
            "{workspace} was not trusted; the answer stored about language servers was cleared."
        ),
        (false, false) => format!("{workspace} was not trusted."),
    }
}

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
            Ok(revoked) => {
                println!(
                    "{}",
                    revoked_message(&terminal_safe(&workspace.display().to_string()), revoked)
                );
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
            // What cannot be read can hold a key.
            let credentials = Credentials::open(&paths.data_dir, crate::setup::env);
            let redactor = crate::setup::known_secrets(&credentials);
            eprintln!("error: {}", terminal_safe(&redactor.redact(&e.to_string())));
            return 2;
        }
    };
    // Command files come from the project root: trust given here covers them only there.
    let root = project_root(&workspace);
    if widening.items.is_empty() && root == workspace {
        // Trust still matters: a trusted workspace's command files may choose their model.
        println!(
            "No project settings in {} widen what the agent may do. Trusting it lets its command files choose their model, until such settings appear.",
            terminal_safe(&workspace.display().to_string())
        );
    } else if widening.items.is_empty() {
        println!(
            "No project settings in {} widen what the agent may do.",
            terminal_safe(&workspace.display().to_string())
        );
    } else {
        for line in describe_settings(&workspace, &widening.items) {
            println!("{line}");
        }
    }
    if root != workspace {
        println!(
            "Trusting {} does not cover the command files used there: they come from {}; run `harness trust` there to let them choose their model.",
            terminal_safe(&workspace.display().to_string()),
            terminal_safe(&root.display().to_string())
        );
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
    match store
        .trust(&workspace, &widening.fingerprint)
        .and_then(|()| store.set_servers_answer(&workspace, true))
    {
        Ok(()) => {
            println!(
                "Trusted {}. Language servers may start in it (they run the project's build code).",
                terminal_safe(&workspace.display().to_string())
            );
            0
        }
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoking_says_what_was_forgotten() {
        use harness_config::trust::Revoked;
        let said = |trusted, servers_answer| {
            revoked_message(
                "/w",
                Revoked {
                    trusted,
                    servers_answer,
                },
            )
        };
        assert_eq!(said(true, true), "Revoked trust for /w.");
        assert_eq!(said(false, false), "/w was not trusted.");
        assert!(said(false, true).contains("answer stored about language servers was cleared"));
    }

    struct Workspace {
        _dir: tempfile::TempDir,
        ws: std::path::PathBuf,
        paths: Paths,
    }

    fn workspace(project: &str) -> Workspace {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join(".harness")).unwrap();
        if !project.is_empty() {
            std::fs::write(ws.join(".harness/config.toml"), project).unwrap();
        }
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
            state_dir: dir.path().join("state"),
        };
        Workspace {
            _dir: dir,
            ws,
            paths,
        }
    }

    /// A user who types nothing until asked.
    struct Waits;

    impl Typing for Waits {
        fn discard(&mut self) {}
        fn pause(&mut self, _quiet: Duration) -> bool {
            false
        }
    }

    fn ask(w: &Workspace, reply: &str) -> (FirstUse, String) {
        let mut out = Vec::new();
        let result =
            first_use(&w.ws, &w.paths, &mut reply.as_bytes(), &mut out, &mut Waits).unwrap();
        (result, String::from_utf8(out).unwrap())
    }

    type Log = std::rc::Rc<std::cell::RefCell<Vec<String>>>;

    /// A terminal: its input, which `discard` and `pause` empty as `tcflush` does, what is
    /// typed at the pause, and what harness writes, all in the order it happened.
    struct Terminal {
        typed: std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<u8>>>,
        /// Typed once the user paused.
        after_pause: &'static [u8],
        log: Log,
    }

    impl Typing for Terminal {
        fn discard(&mut self) {
            self.typed.borrow_mut().clear();
            self.log.borrow_mut().push("<discard>".into());
        }
        fn pause(&mut self, quiet: Duration) -> bool {
            assert_eq!(quiet, ARMING_DELAY);
            let mut typed = self.typed.borrow_mut();
            let any = !typed.is_empty();
            typed.clear();
            typed.extend(self.after_pause);
            self.log.borrow_mut().push("<pause>".into());
            any
        }
    }

    struct Input(std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<u8>>>);

    impl std::io::Read for Input {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut typed = self.0.borrow_mut();
            let n = buf.len().min(typed.len());
            for (slot, byte) in buf.iter_mut().zip(typed.drain(..n)) {
                *slot = byte;
            }
            Ok(n)
        }
    }

    struct Screen(Log);

    impl Write for Screen {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .borrow_mut()
                .push(String::from_utf8_lossy(buf).into_owned());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Asks with `typed` waiting before the question and `after_pause` typed once the user
    /// paused; the answer and what happened, in order.
    fn ask_on_terminal(
        w: &Workspace,
        typed: &[u8],
        after_pause: &'static [u8],
    ) -> (FirstUse, Vec<String>) {
        let typed = std::rc::Rc::new(std::cell::RefCell::new(typed.iter().copied().collect()));
        let log = Log::default();
        let mut answer = std::io::BufReader::new(Input(typed.clone()));
        let mut terminal = Terminal {
            typed,
            after_pause,
            log: log.clone(),
        };
        let result = first_use(
            &w.ws,
            &w.paths,
            &mut answer,
            &mut Screen(log.clone()),
            &mut terminal,
        )
        .unwrap();
        let log = log.borrow().clone();
        (result, log)
    }

    // Review D M1: what was typed before the question showed does not answer it, as sudo does.
    // Review D N5: it is thrown away before the question is written, not after, so an answer
    // typed as soon as the question shows is not thrown away with it.
    #[test]
    fn what_was_typed_before_the_question_does_not_answer_it() {
        let w = workspace(ALLOW);
        let (result, log) = ask_on_terminal(&w, b"y\n", b"");
        assert_eq!(result, FirstUse::Declined);
        assert!(allowed(&w).is_empty());
        let discarded = log.iter().position(|l| l == "<discard>").unwrap();
        let question = log
            .iter()
            .position(|l| l.contains("Trust this workspace"))
            .unwrap();
        assert!(discarded < question, "{log:#?}");
    }

    // Review D C1 residual, for the trust question: it takes an answer only once the user has
    // paused, and says so when their typing was thrown away meanwhile.
    #[test]
    fn the_question_takes_an_answer_only_after_a_pause() {
        let w = workspace(ALLOW);
        let (result, log) = ask_on_terminal(&w, b"", b"y\n");
        assert_eq!(result, FirstUse::Trusted, "{log:#?}");
        let question = log
            .iter()
            .position(|l| l.contains("Trust this workspace"))
            .unwrap();
        let pause = log.iter().position(|l| l == "<pause>").unwrap();
        assert!(question < pause, "{log:#?}");
        assert!(!log.iter().any(|l| l.contains(DISCARDED)), "{log:#?}");

        // Typing as the question shows is not the answer; the question says so and is asked
        // again.
        let w = workspace(ALLOW);
        let typed = std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::new()));
        let log = Log::default();
        let mut answer = std::io::BufReader::new(Input(typed.clone()));
        let mut terminal = Terminal {
            typed: typed.clone(),
            after_pause: b"n\n",
            log: log.clone(),
        };
        // Typed just after the question showed, before the pause.
        struct Racing<'a>(&'a mut Terminal);
        impl Typing for Racing<'_> {
            fn discard(&mut self) {
                self.0.discard();
                self.0.typed.borrow_mut().extend(b"y\n");
            }
            fn pause(&mut self, quiet: Duration) -> bool {
                self.0.pause(quiet)
            }
        }
        let result = first_use(
            &w.ws,
            &w.paths,
            &mut answer,
            &mut Screen(log.clone()),
            &mut Racing(&mut terminal),
        )
        .unwrap();
        let log = log.borrow().clone();
        assert_eq!(result, FirstUse::Declined, "{log:#?}");
        assert!(allowed(&w).is_empty());
        let note = log.iter().position(|l| l.contains(DISCARDED)).unwrap();
        let asked_again = log
            .iter()
            .rposition(|l| l.contains("Trust this workspace"))
            .unwrap();
        assert!(note < asked_again, "{log:#?}");
    }

    fn allowed(w: &Workspace) -> Vec<String> {
        let trust = TrustStore::load(&w.paths.data_dir).unwrap();
        config::load(&w.paths.global_config_file(), &w.ws, &trust)
            .unwrap()
            .allow
    }

    const ALLOW: &str = "[permissions]\nallow = [\"bash:make *\"]\n";

    #[test]
    fn trusting_on_first_use_applies_the_settings() {
        let w = workspace(ALLOW);
        assert!(allowed(&w).is_empty());
        let (result, shown) = ask(&w, "y\n");
        assert_eq!(result, FirstUse::Trusted);
        assert!(
            shown.contains("contains settings that need trust:"),
            "{shown}"
        );
        assert!(
            shown.contains("  - permissions.allow: \"bash:make *\""),
            "{shown}"
        );
        assert!(shown.contains("Trust this workspace, so these settings apply? [y/N]"));
        assert_eq!(allowed(&w), ["bash:make *"]);
        // Asked once: trusted now.
        assert_eq!(ask(&w, "").0, FirstUse::NothingToAsk);
    }

    // Ruling P3: trusting also enables language servers, which are then not asked about.
    #[test]
    fn trusting_on_first_use_also_enables_language_servers() {
        let w = workspace("[permissions]\nallow = [\"bash:make*\"]\n");
        assert_eq!(ask(&w, "y\n").0, FirstUse::Trusted);
        let store = TrustStore::load(&w.paths.data_dir).unwrap();
        assert_eq!(store.servers_answer(&w.ws), Some(true));
    }

    #[test]
    fn declining_leaves_language_servers_unanswered() {
        let w = workspace("[permissions]\nallow = [\"bash:make*\"]\n");
        assert_eq!(ask(&w, "n\n").0, FirstUse::Declined);
        let store = TrustStore::load(&w.paths.data_dir).unwrap();
        assert_eq!(store.servers_answer(&w.ws), None);
    }

    #[test]
    fn declining_leaves_them_off_and_asks_again_next_time() {
        let w = workspace(ALLOW);
        let (result, shown) = ask(&w, "\n");
        assert_eq!(result, FirstUse::Declined);
        assert!(shown.contains("Run `harness trust`"), "{shown}");
        assert!(allowed(&w).is_empty());
        assert_eq!(ask(&w, "no\n").0, FirstUse::Declined);
    }

    #[test]
    fn nothing_is_asked_without_widening_settings() {
        let w = workspace("[permissions]\ndeny = [\"bash:curl *\"]\n");
        assert_eq!(ask(&w, "y\n"), (FirstUse::NothingToAsk, String::new()));
        let w = workspace("");
        assert_eq!(ask(&w, "y\n"), (FirstUse::NothingToAsk, String::new()));
    }

    #[test]
    fn settings_from_the_repository_are_shown_escaped() {
        let w = workspace("[permissions]\nallow = [\"bash:\\u001b[2Jx\"]\n");
        let (_, shown) = ask(&w, "n\n");
        assert!(!shown.contains('\u{1b}'), "{shown:?}");
    }
}
