//! The interactive session in a real pseudo-terminal, against a mock provider: how it ends when
//! harness is asked to stop (SIGTERM), when the terminal hangs up (SIGHUP), and when the terminal
//! closes while hangups are ignored (as under `nohup`).

mod common;
use common::Isolate;

use std::{
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use nix::{
    libc,
    pty::{Winsize, openpty},
    sys::{
        signal::{Signal, kill, killpg},
        termios::{LocalFlags, tcgetattr},
    },
    unistd::Pid,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn stream(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// A model that runs `command` with `bash`, and would then answer.
async fn runs(server: &MockServer, command: &str) {
    let arguments = json!({ "command": command }).to_string();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(stream(&[
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 0, "id": "c1", "type": "function",
            "function": {"name": "bash", "arguments": arguments}}]},
            "finish_reason": "tool_calls"}]}),
        ]))
        .mount(server)
        .await;
}

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    fn new(server_uri: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!(
                "model = \"mock/test-model\"\n\n[notifications]\ndesktop = false\nbell = false\n\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"
            ),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env { home, ws }
    }
}

/// harness in a pseudo-terminal of its own, as a session leader whose controlling terminal it
/// is; what it writes, with the terminal's answers to its queries.
struct Session {
    child: Child,
    /// The terminal's side harness has: kept to read its modes after harness leaves.
    slave: OwnedFd,
    master: Arc<File>,
    output: Arc<Mutex<Vec<u8>>>,
    reading: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// With `controlling`, the terminal is harness's controlling terminal, as in a terminal
    /// window; without, it is only its standard input and output, so the terminal's modes can
    /// still be read once harness, the session's leader, has left.
    fn start(env: &Env, controlling: bool, ignore_hangups: bool) -> Session {
        let size = Winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // Until harness is started, no other test starts a process: one started meanwhile would
        // hold this terminal's other side, which is not close-on-exec until set so below.
        static STARTING: Mutex<()> = Mutex::new(());
        let _starting = STARTING.lock().unwrap_or_else(|e| e.into_inner());
        let pty = openpty(Some(&size), None).unwrap();
        // harness must not inherit the terminal's other side, or it never closes.
        for fd in [pty.master.as_raw_fd(), pty.slave.as_raw_fd()] {
            // SAFETY: sets a flag on a descriptor this process owns.
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let master = Arc::new(File::from(pty.master));
        let slave = pty.slave;
        let mut cmd = Command::new(BIN);
        cmd.args(["--mode", "full-access"])
            .current_dir(env.ws.path())
            .env("HARNESS_HOME", env.home.path())
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR")
            .isolate()
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        // SAFETY: between fork and exec, only async-signal-safe calls: setsid, ioctl, signal.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0
                    || (controlling && libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0)
                {
                    return Err(std::io::Error::last_os_error());
                }
                if ignore_hangups {
                    libc::signal(libc::SIGHUP, libc::SIG_IGN);
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let reading = Arc::new(AtomicBool::new(true));
        let reader = {
            let (master, output, reading) = (master.clone(), output.clone(), reading.clone());
            std::thread::spawn(move || answer(&master, &output, &reading))
        };
        Session {
            child,
            slave,
            master,
            output,
            reading,
            reader: Some(reader),
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    fn wait_for(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.output().contains(text) {
            assert!(
                Instant::now() < deadline,
                "{text:?} never showed:\n{}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn type_keys(&self, keys: &str) {
        (&*self.master).write_all(keys.as_bytes()).unwrap();
    }

    fn signal(&self, signal: Signal) {
        kill(Pid::from_raw(self.child.id() as i32), signal).unwrap();
    }

    /// harness's exit, within `limit`; killed if it has not exited by then.
    fn exit_within(&mut self, limit: Duration) -> ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("harness did not exit:\n{}", self.output());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Closes the terminal: nothing holds its side opposite harness's any more.
    fn close_terminal(&mut self) {
        self.reading.store(false, Ordering::SeqCst);
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
        // The reader has dropped its handle; this is the last one.
        let master =
            std::mem::replace(&mut self.master, Arc::new(File::open("/dev/null").unwrap()));
        drop(Arc::into_inner(master).expect("the last handle on the terminal"));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reading.store(false, Ordering::SeqCst);
    }
}

/// Reads what harness writes into `output`, answering the cursor-position and device-attribute
/// queries as a terminal does, until `reading` is cleared.
fn answer(master: &File, output: &Mutex<Vec<u8>>, reading: &AtomicBool) {
    let mut answered_position = 0;
    let mut answered_attributes = 0;
    let mut buf = [0u8; 4096];
    while reading.load(Ordering::SeqCst) {
        let mut poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut poll, 1, 50) } <= 0 {
            continue;
        }
        let n = match (&*master).read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let mut output = output.lock().unwrap();
        output.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&output).into_owned();
        drop(output);
        let positions = text.matches("\x1b[6n").count();
        let attributes = text.matches("\x1b[c").count();
        while answered_position < positions {
            let _ = (&*master).write_all(b"\x1b[1;1R");
            answered_position += 1;
        }
        while answered_attributes < attributes {
            let _ = (&*master).write_all(b"\x1b[?62c");
            answered_attributes += 1;
        }
    }
}

/// Starts a session whose model runs a long command, and waits until the command runs; its
/// process group.
fn start_running_a_command(env: &Env, controlling: bool, ignore_hangups: bool) -> (Session, i32) {
    let session = Session::start(env, controlling, ignore_hangups);
    session.wait_for("mock/test-model · full-access");
    session.type_keys("go\r");
    let group_file = env.ws.path().join("group");
    let deadline = Instant::now() + Duration::from_secs(20);
    let group = loop {
        if let Ok(text) = std::fs::read_to_string(&group_file)
            && text.ends_with('\n')
        {
            break text.trim().parse::<i32>().unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "the command never started:\n{}",
            session.output()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    (session, group)
}

const LONG_COMMAND: &str = "echo $$ > group; sleep 30; touch survived";

fn group_gone(group: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while killpg(Pid::from_raw(group), None).is_ok() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

fn cooked(slave: &OwnedFd) -> bool {
    let modes = tcgetattr(slave).unwrap();
    modes
        .local_flags
        .contains(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG)
}

fn exits_cleanly_on(signal: Signal, code: i32) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(MockServer::start());
    runtime.block_on(runs(&server, LONG_COMMAND));
    let env = Env::new(&server.uri());
    let (mut session, group) = start_running_a_command(&env, false, false);
    session.signal(signal);
    let status = session.exit_within(Duration::from_secs(15));
    assert_eq!(
        status.code(),
        Some(code),
        "{status:?} (signal {:?}):\n{}",
        status.signal(),
        session.output()
    );
    assert!(group_gone(group), "the command's processes still run");
    assert!(!env.ws.path().join("survived").exists());
    assert!(cooked(&session.slave), "the terminal was left in raw mode");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        session.output().contains("\x1b[?2004l"),
        "bracketed paste was left on"
    );
}

// Review C I1: asked to stop, harness stops the command it runs, gives the terminal back as it
// was, ends the sandbox's session and exits 143.
#[test]
fn sigterm_ends_the_session_cleanly() {
    exits_cleanly_on(Signal::SIGTERM, 143);
}

// Review C I1: so does a hangup, with 129.
#[test]
fn sighup_ends_the_session_cleanly() {
    exits_cleanly_on(Signal::SIGHUP, 129);
}

// Review C I1: a terminal that closes while hangups are ignored ends the session the same way,
// rather than leaving harness reading its end forever.
#[test]
fn a_closed_terminal_ends_the_session() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(MockServer::start());
    runtime.block_on(runs(&server, LONG_COMMAND));
    let env = Env::new(&server.uri());
    let (mut session, group) = start_running_a_command(&env, true, true);
    session.close_terminal();
    let status = session.exit_within(Duration::from_secs(15));
    assert_eq!(status.code(), Some(129), "{status:?}");
    assert!(group_gone(group), "the command's processes still run");
    assert!(!env.ws.path().join("survived").exists());
}
