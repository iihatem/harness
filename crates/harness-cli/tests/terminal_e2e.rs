//! The interactive session in a real pseudo-terminal, drawn on a small terminal emulator that
//! answers harness's queries as a terminal does (where its cursor is, from what harness drew),
//! against a scripted provider that streams its replies in timed pieces: keys typed through a
//! prompt, the trust question, resizes, a terminal that closes while harness starts, keys typed
//! while it starts, and a burst of keys.

mod common;
#[path = "../../harness-tui/tests/support/vt.rs"]
mod vt;

use common::Isolate;
use vt::Vt;

use std::{
    collections::VecDeque,
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::process::CommandExt,
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
};
use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_harness");

/// What the scripted provider answers a request with.
enum Reply {
    /// Text streamed in pieces, each followed by a pause.
    Text(Vec<(String, Duration)>),
    /// A call of the `bash` tool, after a delay.
    Bash {
        command: &'static str,
        delay: Duration,
    },
}

impl Reply {
    fn text(text: &str) -> Reply {
        Reply::Text(vec![(text.into(), Duration::ZERO)])
    }

    /// `NUM-001` to `NUM-<count>`, a paragraph each, in pieces of `piece` bytes every `every`.
    fn numbered(count: usize, piece: usize, every: Duration) -> Reply {
        let text: String = (1..=count)
            .map(|i| format!("NUM-{i:03}\n\n"))
            .collect::<String>();
        let pieces = text
            .as_bytes()
            .chunks(piece)
            .map(|chunk| (String::from_utf8(chunk.to_vec()).unwrap(), every))
            .collect();
        Reply::Text(pieces)
    }
}

/// An OpenAI-compatible chat endpoint on 127.0.0.1 that answers requests with its script, in
/// order, and then with "done".
struct Provider {
    uri: String,
    /// The requests' bodies.
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Provider {
    fn start(script: Vec<Reply>) -> Provider {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let taken = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let (taken, script) = (taken.clone(), script.clone());
                std::thread::spawn(move || {
                    let _ = serve(stream, &taken, &script);
                });
            }
        });
        Provider { uri, requests }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

fn serve(
    stream: TcpStream,
    requests: &Mutex<Vec<Value>>,
    script: &Mutex<VecDeque<Reply>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let post = line.starts_with("POST");
    let mut length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    if !post {
        return out.write_all(
            b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
    }
    requests
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
    let reply = script
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| Reply::text("done"));
    out.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
    )?;
    let mut event = |chunk: Value| -> std::io::Result<()> {
        out.write_all(format!("data: {chunk}\n\n").as_bytes())?;
        out.flush()
    };
    match reply {
        Reply::Text(pieces) => {
            for (text, pause) in pieces {
                event(
                    json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]}),
                )?;
                std::thread::sleep(pause);
            }
            event(json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}))?;
        }
        Reply::Bash { command, delay } => {
            std::thread::sleep(delay);
            let arguments = json!({ "command": command }).to_string();
            event(json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": 0, "id": "c1", "type": "function",
                "function": {"name": "bash", "arguments": arguments}}]},
                "finish_reason": "tool_calls"}]}))?;
        }
    }
    out.write_all(b"data: [DONE]\n\n")?;
    out.flush()
}

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    fn new(provider: &Provider) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!(
                "model = \"mock/test-model\"\n\n[notifications]\ndesktop = false\nbell = false\n\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n",
                provider.uri
            ),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env { home, ws }
    }
}

/// How harness is started.
struct Start {
    mode: &'static str,
    cols: u16,
    rows: u16,
    /// Rows the shell printed before harness.
    shell_lines: usize,
    /// The terminal answers harness's queries.
    answers: bool,
    ignore_hangups: bool,
    /// Typed before harness starts.
    typed: &'static [u8],
}

impl Default for Start {
    fn default() -> Start {
        Start {
            mode: "ask",
            cols: 100,
            rows: 30,
            shell_lines: 0,
            answers: true,
            ignore_hangups: false,
            typed: b"",
        }
    }
}

/// The terminal harness runs in: what it shows, and every byte harness wrote.
struct Screen {
    vt: Vt,
    raw: Vec<u8>,
    /// When harness last wrote.
    written_at: Instant,
}

/// harness in a pseudo-terminal of its own, whose controlling terminal it is, as in a terminal
/// window.
struct Session {
    child: Mutex<Child>,
    master: Arc<File>,
    screen: Arc<Mutex<Screen>>,
    reading: Arc<AtomicBool>,
    pump: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    fn start(env: &Env, start: Start) -> Session {
        let size = Winsize {
            ws_row: start.rows,
            ws_col: start.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // Until harness is started, no other test starts a process: one started meanwhile would
        // hold this terminal's other side, which is not close-on-exec until set so below.
        static STARTING: Mutex<()> = Mutex::new(());
        let _starting = STARTING.lock().unwrap_or_else(|e| e.into_inner());
        let pty = openpty(Some(&size), None).unwrap();
        for fd in [pty.master.as_raw_fd(), pty.slave.as_raw_fd()] {
            // SAFETY: sets a flag on a descriptor this process owns.
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let master = Arc::new(File::from(pty.master));
        let slave: OwnedFd = pty.slave;
        let mut vt = Vt::new(start.cols, start.rows);
        for i in 0..start.shell_lines {
            vt.print(&format!("shell line {i}\n"));
        }
        if !start.typed.is_empty() {
            (&*master).write_all(start.typed).unwrap();
        }
        let mut cmd = Command::new(BIN);
        cmd.args(["--mode", start.mode])
            .current_dir(env.ws.path())
            .env("HARNESS_HOME", env.home.path())
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR")
            .env_remove("COLORTERM")
            .isolate()
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        let ignore_hangups = start.ignore_hangups;
        // SAFETY: between fork and exec, only async-signal-safe calls: setsid, ioctl, signal.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if ignore_hangups {
                    libc::signal(libc::SIGHUP, libc::SIG_IGN);
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        drop(slave);
        let screen = Arc::new(Mutex::new(Screen {
            vt,
            raw: Vec::new(),
            written_at: Instant::now(),
        }));
        let reading = Arc::new(AtomicBool::new(true));
        let pump = {
            let (master, screen, reading) = (master.clone(), screen.clone(), reading.clone());
            let answers = start.answers;
            std::thread::spawn(move || pump(&master, &screen, &reading, answers))
        };
        Session {
            child: Mutex::new(child),
            master,
            screen,
            reading,
            pump: Some(pump),
        }
    }

    /// Everything the terminal shows and has scrolled off, a row a line.
    fn shown(&self) -> String {
        self.screen.lock().unwrap().vt.everything().join("\n")
    }

    fn rows(&self) -> Vec<String> {
        self.screen.lock().unwrap().vt.everything()
    }

    fn raw(&self) -> String {
        String::from_utf8_lossy(&self.screen.lock().unwrap().raw).into_owned()
    }

    fn wait_for(&self, text: &str, limit: Duration) {
        let deadline = Instant::now() + limit;
        while !self.shown().contains(text) {
            assert!(
                Instant::now() < deadline,
                "{text:?} never showed:\n{}",
                self.shown()
            );
            assert!(
                self.alive(),
                "harness ended waiting for {text:?}:\n{}",
                self.shown()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits until harness has written nothing for `quiet`: it is not drawing.
    fn wait_quiet(&self, quiet: Duration) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.screen.lock().unwrap().written_at.elapsed() < quiet {
            assert!(
                Instant::now() < deadline,
                "harness never paused:\n{}",
                self.shown()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn type_keys(&self, keys: &[u8]) {
        (&*self.master).write_all(keys).unwrap();
    }

    /// Changes the window's size, as a user dragging its corner: the terminal moves its rows,
    /// and harness is told (SIGWINCH).
    fn resize(&self, cols: u16, rows: u16) {
        let mut screen = self.screen.lock().unwrap();
        screen.vt.resize(cols, rows);
        let size = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: sets the size of the terminal this test owns from a valid `winsize`.
        assert_eq!(
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
            0
        );
    }

    fn alive(&self) -> bool {
        self.child.lock().unwrap().try_wait().unwrap().is_none()
    }

    /// harness's exit, within `limit`; killed if it has not exited by then.
    fn exit_within(&mut self, limit: Duration) -> ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.lock().unwrap().try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let mut child = self.child.lock().unwrap();
                let _ = child.kill();
                let _ = child.wait();
                drop(child);
                panic!("harness did not exit:\n{}", self.shown());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Leaves harness, as the user does, and says it exited cleanly.
    fn quit(&mut self) {
        self.type_keys(b"\x03");
        std::thread::sleep(Duration::from_millis(100));
        self.type_keys(b"\x03");
        let status = self.exit_within(Duration::from_secs(10));
        assert_eq!(status.code(), Some(0), "{status:?}:\n{}", self.shown());
    }

    /// Closes the terminal: nothing holds its side opposite harness's any more.
    fn close_terminal(&mut self) {
        self.reading.store(false, Ordering::SeqCst);
        if let Some(pump) = self.pump.take() {
            pump.join().unwrap();
        }
        let master =
            std::mem::replace(&mut self.master, Arc::new(File::open("/dev/null").unwrap()));
        drop(Arc::into_inner(master).expect("the last handle on the terminal"));
    }

    /// The processor time harness has used.
    fn cpu(&self) -> Duration {
        cpu_time(self.child.lock().unwrap().id())
    }

    /// Sends harness `signal`.
    fn signal(&self, signal: libc::c_int) {
        let pid = self.child.lock().unwrap().id() as libc::pid_t;
        // SAFETY: sends a signal to the process this test started.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
    }

    /// Whether the terminal is in its usual modes: lines edited and echoed, and Ctrl+C, Ctrl+\
    /// and Ctrl+Z sending their signals.
    fn cooked(&self) -> bool {
        let modes = nix::sys::termios::tcgetattr(&*self.master).unwrap();
        use nix::sys::termios::LocalFlags;
        modes
            .local_flags
            .contains(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let child = self.child.get_mut().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
        let _ = child.wait();
        self.reading.store(false, Ordering::SeqCst);
    }
}

/// The queries harness asks the terminal, and the answer to each (the cursor's place is filled
/// in from the terminal).
const QUERIES: [&[u8]; 2] = [b"\x1b[6n", b"\x1b[c"];

/// Reads what harness writes onto the terminal's screen, answering its queries as a terminal
/// does when `answers`, until `reading` is cleared or harness is gone.
fn pump(master: &File, screen: &Mutex<Screen>, reading: &AtomicBool, answers: bool) {
    let mut carried = Vec::new();
    let mut buf = [0u8; 65536];
    while reading.load(Ordering::SeqCst) {
        let mut poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut poll, 1, 20) } <= 0 {
            continue;
        }
        let n = match (&*master).read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let mut screen = screen.lock().unwrap();
        screen.written_at = Instant::now();
        screen.raw.extend_from_slice(&buf[..n]);
        carried.extend_from_slice(&buf[..n]);
        let mut rest = std::mem::take(&mut carried);
        loop {
            let next = QUERIES
                .iter()
                .filter_map(|query| {
                    rest.windows(query.len())
                        .position(|w| w == *query)
                        .map(|at| (at, *query))
                })
                .min_by_key(|(at, _)| *at);
            let Some((at, query)) = next else {
                // The start of a query, cut off at the end of this read, waits for the rest.
                let keep = (1..4)
                    .rev()
                    .find(|len| {
                        rest.len() >= *len
                            && QUERIES
                                .iter()
                                .any(|query| query.starts_with(&rest[rest.len() - len..]))
                    })
                    .unwrap_or(0);
                screen.vt.feed(&rest[..rest.len() - keep]);
                carried = rest[rest.len() - keep..].to_vec();
                break;
            };
            screen.vt.feed(&rest[..at]);
            if answers {
                let answer = if query == b"\x1b[6n" {
                    let cursor = screen.vt.cursor();
                    format!("\x1b[{};{}R", cursor.y + 1, cursor.x + 1)
                } else {
                    "\x1b[?62c".into()
                };
                let _ = (&*master).write_all(answer.as_bytes());
            }
            rest.drain(..at + query.len());
        }
    }
}

/// The processor time process `pid` has used.
fn cpu_time(pid: u32) -> Duration {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // After the command's name, in parentheses: utime and stime are the 12th and 13th.
        let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split(' ').collect();
        let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
        // SAFETY: reads a configuration value.
        let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
        return Duration::from_millis(ticks * 1000 / per_second);
    }
    let out = Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let seconds = text
        .trim()
        .replace('-', ":")
        .split(':')
        .fold(0.0, |total, part| {
            total * 60.0 + part.parse::<f64>().unwrap_or(0.0)
        });
    Duration::from_secs_f64(seconds)
}

/// Said under a prompt once keys typed while it waited went to the input instead.
const TYPED_PAST: &str = "your typing went to your message; the prompt takes keys once you pause";

// Review D C1 residual (its `typing.py`): a user types a follow-up at 12 keys a second while
// the model works, and on through the approval it asks for, until a second after it shows. The
// command does not run, the prompt still waits and says where the keys went, and a key after a
// pause answers it.
#[test]
fn typing_through_an_approval_leaves_it_waiting_until_a_pause() {
    let provider = Provider::start(vec![
        Reply::Bash {
            command: "touch ran",
            delay: Duration::from_millis(300),
        },
        Reply::text("all finished"),
    ]);
    let env = Env::new(&provider);
    let mut session = Session::start(&env, Start::default());
    session.wait_for("mock/test-model · ask", Duration::from_secs(20));
    session.type_keys(b"go\r");
    let mut shown_at = None;
    for c in "then also add a test for the empty case and say what you ran; "
        .repeat(4)
        .bytes()
    {
        session.type_keys(&[c]);
        if shown_at.is_none() && session.shown().contains("approve?") {
            shown_at = Some(Instant::now());
        }
        if shown_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(1)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(83));
    }
    assert!(shown_at.is_some(), "no approval:\n{}", session.shown());
    std::thread::sleep(Duration::from_millis(200));
    let shown = session.shown();
    assert!(!env.ws.path().join("ran").exists(), "{shown}");
    // The answer lines, not the bare words: the basic tier's warning says "Permission denied".
    for answer in ["✓ approved: ", "✗ denied: ", "tell the model why"] {
        assert!(!shown.contains(answer), "{answer}:\n{shown}");
    }
    assert!(shown.contains(TYPED_PAST), "{shown}");
    // A pause, then the answer.
    std::thread::sleep(Duration::from_millis(600));
    session.type_keys(b"y");
    session.wait_for("all finished", Duration::from_secs(20));
    assert!(env.ws.path().join("ran").exists());
    assert!(
        session.shown().contains("✓ approved: "),
        "{}",
        session.shown()
    );
    session.quit();
}

// Review D C1 residual and N5, for the trust question: an answer typed as soon as the question
// shows is not taken, and harness says so; one typed after a pause is.
#[test]
fn the_trust_question_takes_an_answer_only_after_a_pause() {
    let provider = Provider::start(Vec::new());
    let env = Env::new(&provider);
    std::fs::create_dir(env.ws.path().join(".harness")).unwrap();
    std::fs::write(
        env.ws.path().join(".harness/config.toml"),
        "[permissions]\nallow = [\"bash:make *\"]\n",
    )
    .unwrap();
    let mut session = Session::start(&env, Start::default());
    session.wait_for("Trust this workspace", Duration::from_secs(20));
    session.type_keys(b"y\r");
    session.wait_for("not taken as the answer", Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(600));
    session.type_keys(b"n\r");
    session.wait_for("Not trusted", Duration::from_secs(5));
    session.wait_for("mock/test-model · ask", Duration::from_secs(20));
    assert!(!session.shown().contains("Trusted"), "{}", session.shown());
    session.quit();
}

// Final review M1 (its `seams.py` T4): SIGTERM or SIGHUP while the user types at the trust
// question, as it waits for a pause with the terminal's echo and line editing off, still ends
// harness with the terminal given back as it was.
#[test]
fn a_signal_during_the_trust_questions_pause_gives_the_terminal_back() {
    use std::os::unix::process::ExitStatusExt;
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        let provider = Provider::start(Vec::new());
        let env = Env::new(&provider);
        std::fs::create_dir(env.ws.path().join(".harness")).unwrap();
        std::fs::write(
            env.ws.path().join(".harness/config.toml"),
            "[permissions]\nallow = [\"bash:make *\"]\n",
        )
        .unwrap();
        let mut session = Session::start(&env, Start::default());
        session.wait_for("Trust this workspace", Duration::from_secs(20));
        session.type_keys(b"abc");
        std::thread::sleep(Duration::from_millis(100));
        assert!(!session.cooked(), "the pause had not begun ({signal})");
        session.signal(signal);
        let status = session.exit_within(Duration::from_secs(10));
        assert_eq!(status.signal(), Some(signal), "{status:?}");
        assert!(session.cooked(), "left without its modes ({signal})");
    }
}

/// Window sizes as a user drags a window's corner.
fn dragged(i: usize) -> (u16, u16) {
    (80 - (i % 15) as u16 * 2, 24 - (i % 10) as u16)
}

// Review A N1: resizing never ends the session, at an idle prompt or while a reply streams: 30
// resizes 50 ms apart, as a window's corner is dragged.
#[test]
fn resizes_never_end_the_session() {
    let provider = Provider::start(vec![Reply::numbered(80, 30, Duration::from_millis(40))]);
    let env = Env::new(&provider);
    let mut session = Session::start(
        &env,
        Start {
            cols: 80,
            rows: 24,
            shell_lines: 10,
            ..Start::default()
        },
    );
    session.wait_for("mock/test-model · ask", Duration::from_secs(20));
    for i in 0..30 {
        let (cols, rows) = dragged(i);
        session.resize(cols, rows);
        std::thread::sleep(Duration::from_millis(50));
        assert!(session.alive(), "ended at resize {i}:\n{}", session.shown());
    }
    session.resize(80, 24);
    session.type_keys(b"count\r");
    session.wait_for("NUM-005", Duration::from_secs(20));
    for i in 0..30 {
        let (cols, rows) = dragged(i + 3);
        session.resize(cols, rows);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            session.alive(),
            "ended at resize {i} while streaming:\n{}",
            session.shown()
        );
    }
    session.resize(80, 24);
    session.wait_for("NUM-080", Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(300));
    assert!(session.alive());
    session.quit();
}

// Review A N2 (its `s_resize_stream.py`): the window changes size while a reply streams, to
// its sizes (40x20, 60x10, 70x16) in turn, each time between two draws; every line of the reply
// is in the terminal once, in order. A resize reaches harness before its next draw, which then
// draws for the new size: a draw for the old one would scroll at the old bottom row, and lose
// lines.
#[test]
fn resizing_while_a_reply_streams_loses_no_line() {
    let provider = Provider::start(vec![Reply::numbered(110, 20, Duration::from_millis(25))]);
    let env = Env::new(&provider);
    let mut session = Session::start(
        &env,
        Start {
            cols: 60,
            rows: 20,
            shell_lines: 25,
            ..Start::default()
        },
    );
    session.wait_for("mock/test-model · ask", Duration::from_secs(20));
    session.type_keys(b"count\r");
    let sizes = [(40, 20), (60, 10), (70, 16)];
    for i in 0..12 {
        session.wait_for(&format!("NUM-{:03}", 8 * (i + 1)), Duration::from_secs(20));
        session.wait_quiet(Duration::from_millis(5));
        let (cols, rows) = sizes[i % sizes.len()];
        session.resize(cols, rows);
    }
    session.wait_for("NUM-110", Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(500));
    let numbered: Vec<String> = session
        .rows()
        .into_iter()
        .filter(|row| row.starts_with("NUM-"))
        .collect();
    let expected: Vec<String> = (1..=110).map(|i| format!("NUM-{i:03}")).collect();
    assert_eq!(numbered, expected, "\n{}", session.shown());
    session.quit();
}

// Review D N2: a terminal that closes while harness asks it about itself (which keys it reports,
// where its cursor is) ends harness at once, whether hangups reach it or are ignored.
#[test]
fn a_terminal_closed_during_the_startup_queries_ends_harness() {
    let provider = Provider::start(Vec::new());
    for ignore_hangups in [false, true] {
        let env = Env::new(&provider);
        let mut session = Session::start(
            &env,
            Start {
                answers: false,
                ignore_hangups,
                ..Start::default()
            },
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while !(session.raw().contains("\x1b[c") || session.raw().contains("\x1b[6n")) {
            assert!(Instant::now() < deadline, "no query:\n{:?}", session.raw());
            std::thread::sleep(Duration::from_millis(5));
        }
        session.close_terminal();
        let status = session.exit_within(Duration::from_secs(10));
        assert_eq!(status.code(), Some(129), "{status:?} ({ignore_hangups})");
    }
}

// Review D N3: keys typed while harness starts are drawn as it starts, without waiting for the
// next key, or for the reader's first wait on the terminal to end.
#[test]
fn keys_typed_while_harness_starts_are_drawn_at_once() {
    let provider = Provider::start(Vec::new());
    let env = Env::new(&provider);
    let mut session = Session::start(
        &env,
        Start {
            typed: b"hello",
            ..Start::default()
        },
    );
    session.wait_for("mock/test-model · ask", Duration::from_secs(20));
    session.wait_for("› hello", Duration::from_millis(50));
    session.quit();
}

// Review D N6: 3,000 plain keys at once (`tmux send-keys`, or a paste into a terminal without
// bracketed paste) all arrive, whole, and harness does not spin after them.
//
// On macOS they arrive within a second. On Linux (final review I1), crossterm is told of the
// terminal's bytes only as more arrive (epoll, edge-triggered) and reads 1 KiB at a time, so the
// rest of the burst can stall in the terminal until more keys come, each letting up to 1 KiB
// more through: a limitation M1 keeps, and documents. There harness must not spin while the rest
// waits, and nothing may be lost once more keys are sent (a harmless Enter, on an empty input).
#[test]
fn a_burst_of_keys_arrives_whole_and_nothing_spins() {
    let provider = Provider::start(Vec::new());
    let env = Env::new(&provider);
    let mut session = Session::start(&env, Start::default());
    session.wait_for("mock/test-model · ask", Duration::from_secs(20));
    std::thread::sleep(Duration::from_millis(300));
    let typed = "x".repeat(3000);
    let sent = Instant::now();
    session.type_keys(format!("{typed}\r").as_bytes());
    if cfg!(target_os = "linux") {
        std::thread::sleep(Duration::from_millis(300));
        let before = session.cpu();
        std::thread::sleep(Duration::from_secs(2));
        let spent = session.cpu().saturating_sub(before);
        assert!(
            spent < Duration::from_millis(500),
            "{spent:?} of CPU in 2 s while the rest of the burst waited"
        );
        let mut more = 0;
        while provider.requests().is_empty() {
            assert!(
                more < 10,
                "the keys were not all taken in after {more} more:\n{}",
                session.shown()
            );
            session.type_keys(b"\r");
            more += 1;
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    while provider.requests().is_empty() {
        assert!(
            sent.elapsed() < Duration::from_secs(1),
            "the keys were not all taken in within a second:\n{}",
            session.shown()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let request = &provider.requests()[0];
    let last = request["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .map(|message| message["content"].as_str().unwrap_or_default().to_string());
    assert_eq!(last.as_deref(), Some(typed.as_str()));
    session.wait_for("done", Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(300));
    let before = session.cpu();
    std::thread::sleep(Duration::from_secs(2));
    let spent = session.cpu().saturating_sub(before);
    assert!(
        spent < Duration::from_millis(500),
        "{spent:?} of CPU in 2 s idle"
    );
    session.quit();
}
