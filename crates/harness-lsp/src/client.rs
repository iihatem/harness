use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::Duration,
};

use lsp_types::{
    ClientCapabilities, Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, InitializeParams,
    PublishDiagnosticsClientCapabilities, PublishDiagnosticsParams, TextDocumentClientCapabilities,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    VersionedTextDocumentIdentifier, WindowClientCapabilities, WorkspaceFolder,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Notify, oneshot},
    time::Instant,
};

use crate::frame;

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("cannot start the language server: {0}")]
    Spawn(std::io::Error),
    #[error("the language server did not answer `initialize` in time")]
    InitializeTimeout,
    #[error("the language server exited")]
    Exited,
    #[error("the language server answered with an error: {0}")]
    Server(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// What waiting for a file's diagnostics came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    /// The server published diagnostics for the file after it was told of its text: all of them,
    /// whatever their severity (an empty list is a clean file).
    Published(Vec<Diagnostic>),
    /// Nothing new was published for this text in time, but an earlier set is known: it is given
    /// back, to be marked as not confirmed for this edit.
    Unchanged(Vec<Diagnostic>),
    /// Nothing was published in time.
    Pending,
    /// The server exited.
    Exited,
}

impl Check {
    /// The errors among the diagnostics, in the order the server gave them.
    pub fn errors(&self) -> Vec<&Diagnostic> {
        match self {
            Check::Published(all) | Check::Unchanged(all) => all
                .iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// The `file://` URI of `path`, percent-encoded.
pub fn uri_of(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// A URI as a key: percent-encoding is undone, so that a server that writes `%c3%a9` where the
/// client wrote `%C3%A9` (or leaves `@` or `+` unescaped) still names the same file.
fn key_of(uri: &str) -> String {
    let bytes = uri.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Default)]
struct Published {
    /// How many times diagnostics were published for the file.
    count: u64,
    diagnostics: Vec<Diagnostic>,
    /// The version of the text the last publish says it is for, if it says.
    version: Option<i32>,
    /// When the last one came.
    at: Option<Instant>,
}

/// What the server says it is busy with: work-done progress that has begun and not ended, and (for
/// rust-analyzer) `experimental/serverStatus`.
#[derive(Default)]
struct Busy {
    tokens: HashSet<String>,
    not_quiescent: bool,
    /// When that last changed.
    changed_at: Option<Instant>,
    /// Whether the server has ever said it was busy.
    ever: bool,
}

struct Shared {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>,
    published: Mutex<HashMap<String, Published>>,
    busy: Mutex<Busy>,
    changed: Notify,
    exited: AtomicBool,
}

/// Where a wait for one file's diagnostics stands.
struct Standing {
    /// A publish came for this text: after it was sent, and not under an older version.
    fresh: bool,
    /// The server is in the middle of work whose end may change the answer.
    busy: bool,
    /// The server has reported being busy at some time.
    ever_busy: bool,
    /// The published set for this text is empty.
    empty: bool,
    /// When the last thing that can change the answer happened.
    last: Option<Instant>,
}

impl Shared {
    fn count(&self, key: &str) -> u64 {
        self.published
            .lock()
            .expect("published lock")
            .get(key)
            .map_or(0, |p| p.count)
    }

    fn standing(&self, key: &str, before: u64, version: i32) -> Standing {
        let published = self.published.lock().expect("published lock");
        let busy = self.busy.lock().expect("busy lock");
        let found = published.get(key);
        let fresh =
            found.is_some_and(|p| p.count > before && p.version.is_none_or(|v| v >= version));
        let last = [busy.changed_at, found.filter(|_| fresh).and_then(|p| p.at)]
            .into_iter()
            .flatten()
            .max();
        Standing {
            fresh,
            busy: !busy.tokens.is_empty() || busy.not_quiescent,
            ever_busy: busy.ever,
            empty: found.is_none_or(|p| p.diagnostics.is_empty()),
            last,
        }
    }

    async fn send(&self, message: &Value) -> std::io::Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&frame::encode(message)).await?;
        stdin.flush().await
    }
}

/// A running language server.
pub struct Client {
    child: tokio::sync::Mutex<Child>,
    shared: Arc<Shared>,
    next_id: AtomicI64,
    /// The version last sent for each open file.
    versions: Mutex<HashMap<String, i32>>,
    /// The server's process group, which it leads when it was started in one of its own.
    group: Option<i32>,
    /// Whether the server ended by itself after the goodbye: nothing is left to signal.
    parted: AtomicBool,
    /// Whether a check has been answered: the first is more patient.
    warmed: AtomicBool,
}

impl Drop for Client {
    /// A server that is ended without a goodbye (the initialize timed out, it hung on shutdown, the
    /// session ended) takes what it started with it: cargo check, tsserver, proc-macro servers.
    fn drop(&mut self) {
        if !self.parted.load(Ordering::SeqCst)
            && let Some(group) = self.group
            && group > 1
        {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(group),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

impl Client {
    /// Starts the server `command` runs, and initializes it for the project at `root`, giving it
    /// `timeout` to answer.
    pub async fn start(
        mut command: Command,
        root: &Path,
        timeout: Duration,
    ) -> Result<Client, LspError> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(LspError::Spawn)?;
        let group = child.id().and_then(|pid| i32::try_from(pid).ok());
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let shared = Arc::new(Shared {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::default(),
            published: Mutex::default(),
            busy: Mutex::default(),
            changed: Notify::new(),
            exited: AtomicBool::new(false),
        });
        tokio::spawn(read_loop(BufReader::new(stdout), shared.clone()));
        let client = Client {
            child: tokio::sync::Mutex::new(child),
            shared,
            next_id: AtomicI64::new(1),
            versions: Mutex::new(HashMap::new()),
            group,
            parted: AtomicBool::new(false),
            warmed: AtomicBool::new(false),
        };
        let root_uri = uri_of(root);
        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: root_uri.parse().ok(),
            workspace_folders: root_uri.parse().ok().map(|uri| {
                vec![WorkspaceFolder {
                    uri,
                    name: root
                        .file_name()
                        .map_or_else(|| "workspace".into(), |n| n.to_string_lossy().into_owned()),
                }]
            }),
            // Some servers (typescript-language-server) publish diagnostics only to a client that
            // says it takes them.
            // A server that works in the background (rust-analyzer's indexing and cargo check,
            // gopls's package loading) reports it as progress only to a client that takes it, and
            // rust-analyzer says when it is idle only to one that asks for its status.
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    publish_diagnostics: Some(PublishDiagnosticsClientCapabilities::default()),
                    ..TextDocumentClientCapabilities::default()
                }),
                window: Some(WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..WindowClientCapabilities::default()
                }),
                experimental: Some(json!({"serverStatusNotification": true})),
                ..ClientCapabilities::default()
            },
            ..InitializeParams::default()
        };
        let initialized = client.request("initialize", serde_json::to_value(params).unwrap());
        match tokio::time::timeout(timeout, initialized).await {
            Err(_) => return Err(LspError::InitializeTimeout),
            Ok(result) => {
                result?;
            }
        }
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    /// Whether the server has exited.
    pub fn has_exited(&self) -> bool {
        self.shared.exited.load(Ordering::SeqCst)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .expect("pending lock")
            .insert(id, tx);
        if self.has_exited() {
            return Err(LspError::Exited);
        }
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write(&message).await?;
        match rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(message)) => Err(LspError::Server(message)),
            Err(_) => Err(LspError::Exited),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.write(&message).await
    }

    /// Writes `message`; a server that is gone is `Exited`.
    async fn write(&self, message: &Value) -> Result<(), LspError> {
        match self.shared.send(message).await {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Err(LspError::Exited),
            other => Ok(other?),
        }
    }

    /// Tells the server `path` now has `text` (opening it the first time, replacing its text
    /// after), and waits up to `wait` for the diagnostics it publishes for the file. Once some
    /// came, it waits a moment more for others that follow at once, and gives the last.
    pub async fn check(
        &self,
        path: &Path,
        language_id: &str,
        text: &str,
        wait: Duration,
    ) -> Result<Check, LspError> {
        let uri = uri_of(path);
        let key = key_of(&uri);
        let before = self.shared.count(&key);
        let version = {
            let mut versions = self.versions.lock().expect("versions lock");
            let version = versions.entry(uri.clone()).or_insert(0);
            *version += 1;
            *version
        };
        let parsed = uri
            .parse()
            .map_err(|e| LspError::Server(format!("not a URI: {e}")))?;
        if version == 1 {
            let params = DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: parsed,
                    language_id: language_id.into(),
                    version,
                    text: text.into(),
                },
            };
            self.notify(
                "textDocument/didOpen",
                serde_json::to_value(params).unwrap(),
            )
            .await?;
        } else {
            let params = DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: parsed,
                    version,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.into(),
                }],
            };
            self.notify(
                "textDocument/didChange",
                serde_json::to_value(params).unwrap(),
            )
            .await?;
        }
        // rust-analyzer runs `cargo check` on a save, and only then (or at its start).
        let saved = DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: uri
                    .parse()
                    .map_err(|e| LspError::Server(format!("not a URI: {e}")))?,
            },
            text: None,
        };
        self.notify("textDocument/didSave", serde_json::to_value(saved).unwrap())
            .await?;
        Ok(self.wait_for(&key, before, version, wait).await)
    }

    /// Waits for the server's answer on the text of `version`: a publish for it that comes when
    /// the server is not in the middle of background work (indexing, `cargo check`), and is not
    /// followed at once by another. At the end of `wait` it gives the latest publish for this text
    /// if there is one, else the set last known for the file, else nothing.
    async fn wait_for(&self, key: &str, before: u64, version: i32, wait: Duration) -> Check {
        let deadline = Instant::now() + wait;
        // A server that works in the background when it starts (rust-analyzer) is idle for a
        // moment before it starts `cargo check`, whose errors come last. The first check waits that
        // moment out when all it has is an empty set; later ones do not (the work starts at once).
        let patient = !self.warmed.swap(true, Ordering::SeqCst);
        loop {
            let changed = self.shared.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let standing = self.shared.standing(key, before, version);
            if self.has_exited() && !standing.fresh {
                return Check::Exited;
            }
            let settle = if patient && standing.ever_busy && standing.empty {
                IDLE_SETTLE
            } else {
                SETTLE
            };
            let until = match standing.last {
                Some(last) if standing.fresh && !standing.busy => (last + settle).min(deadline),
                _ => deadline,
            };
            if Instant::now() >= until {
                break;
            }
            tokio::select! {
                () = &mut changed => {}
                () = tokio::time::sleep_until(until) => {}
            }
        }
        let published = self.shared.published.lock().expect("published lock");
        match published.get(key) {
            Some(p) if p.count > before && p.version.is_none_or(|v| v >= version) => {
                Check::Published(p.diagnostics.clone())
            }
            _ if self.has_exited() => Check::Exited,
            Some(p) => Check::Unchanged(p.diagnostics.clone()),
            None => Check::Pending,
        }
    }

    /// Asks the server to shut down and exit, and ends it if it has not within a few seconds.
    pub async fn shutdown(self) {
        if !self.has_exited() {
            let asked = tokio::time::timeout(
                Duration::from_secs(2),
                self.request("shutdown", Value::Null),
            )
            .await;
            if matches!(asked, Ok(Ok(_))) {
                let _ = self.notify("exit", Value::Null).await;
            }
        }
        let mut child = self.child.lock().await;
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_ok()
        {
            self.parted.store(true, Ordering::SeqCst);
        } else {
            // Dropping `self` ends the rest of its group.
            let _ = child.kill().await;
        }
    }
}

/// How long the last publish may be followed by another before the diagnostics are taken as
/// final: servers publish a syntax pass and a type pass a moment apart.
const SETTLE: Duration = Duration::from_millis(150);

/// The same, on a server's first check, when the server has been busy and the set is empty.
const IDLE_SETTLE: Duration = Duration::from_millis(1000);

async fn read_loop(mut reader: BufReader<tokio::process::ChildStdout>, shared: Arc<Shared>) {
    while let Ok(Some(message)) = frame::read(&mut reader).await {
        handle(&shared, message);
    }
    shared.exited.store(true, Ordering::SeqCst);
    // Nobody waits on an answer that cannot come.
    shared.pending.lock().expect("pending lock").clear();
    shared.changed.notify_waiters();
}

fn handle(shared: &Arc<Shared>, message: Value) {
    let method = message["method"].as_str();
    match (method, message.get("id")) {
        (Some(method), Some(id)) => {
            // A request from the server: answered at once, with nothing, so it never waits for us.
            // The reply is written by a task of its own, so that the reader never waits for the
            // pipe while the server is itself waiting for the reader.
            let result = if method == "workspace/configuration" {
                let items = message["params"]["items"].as_array().map_or(0, Vec::len);
                Value::Array(vec![Value::Null; items])
            } else {
                Value::Null
            };
            let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
            let shared = shared.clone();
            tokio::spawn(async move {
                let _ = shared.send(&reply).await;
            });
        }
        (Some("textDocument/publishDiagnostics"), None) => {
            if let Ok(params) =
                serde_json::from_value::<PublishDiagnosticsParams>(message["params"].clone())
            {
                let mut published = shared.published.lock().expect("published lock");
                let entry = published.entry(key_of(params.uri.as_str())).or_default();
                entry.count += 1;
                entry.diagnostics = params.diagnostics;
                entry.version = params.version;
                entry.at = Some(Instant::now());
                drop(published);
                shared.changed.notify_waiters();
            }
        }
        (Some("$/progress"), None) => {
            let token = message["params"]["token"].to_string();
            let mut busy = shared.busy.lock().expect("busy lock");
            let changed = match message["params"]["value"]["kind"].as_str() {
                Some("begin") => {
                    busy.ever = true;
                    busy.tokens.insert(token)
                }
                Some("end") => busy.tokens.remove(&token),
                _ => false,
            };
            if changed {
                busy.changed_at = Some(Instant::now());
                drop(busy);
                shared.changed.notify_waiters();
            }
        }
        (Some("experimental/serverStatus"), None) => {
            if let Some(quiescent) = message["params"]["quiescent"].as_bool() {
                let mut busy = shared.busy.lock().expect("busy lock");
                if busy.not_quiescent == quiescent {
                    busy.not_quiescent = !quiescent;
                    busy.ever = true;
                    busy.changed_at = Some(Instant::now());
                    drop(busy);
                    shared.changed.notify_waiters();
                }
            }
        }
        (Some(_), None) => {}
        (None, Some(id)) => {
            let Some(id) = id.as_i64() else { return };
            let sender = shared.pending.lock().expect("pending lock").remove(&id);
            if let Some(sender) = sender {
                let answer = match message.get("error") {
                    Some(error) => Err(error["message"].as_str().unwrap_or("an error").to_string()),
                    None => Ok(message["result"].clone()),
                };
                let _ = sender.send(answer);
            }
        }
        (None, None) => {}
    }
}
