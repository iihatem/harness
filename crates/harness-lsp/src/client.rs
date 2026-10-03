use std::{
    collections::HashMap,
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
    DidOpenTextDocumentParams, InitializeParams, PublishDiagnosticsParams,
    TextDocumentContentChangeEvent, TextDocumentItem, VersionedTextDocumentIdentifier,
    WorkspaceFolder,
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
    /// Nothing was published in time.
    Pending,
    /// The server exited.
    Exited,
}

impl Check {
    /// The errors among the diagnostics, in the order the server gave them.
    pub fn errors(&self) -> Vec<&Diagnostic> {
        match self {
            Check::Published(all) => all
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

#[derive(Default)]
struct Published {
    /// How many times diagnostics were published for the file.
    count: u64,
    diagnostics: Vec<Diagnostic>,
}

struct Shared {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>,
    published: Mutex<HashMap<String, Published>>,
    changed: Notify,
    exited: AtomicBool,
}

impl Shared {
    fn count(&self, uri: &str) -> u64 {
        self.published
            .lock()
            .expect("published lock")
            .get(uri)
            .map_or(0, |p| p.count)
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
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let shared = Arc::new(Shared {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::default(),
            published: Mutex::default(),
            changed: Notify::new(),
            exited: AtomicBool::new(false),
        });
        tokio::spawn(read_loop(BufReader::new(stdout), shared.clone()));
        let client = Client {
            child: tokio::sync::Mutex::new(child),
            shared,
            next_id: AtomicI64::new(1),
            versions: Mutex::new(HashMap::new()),
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
            capabilities: ClientCapabilities::default(),
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
        let before = self.shared.count(&uri);
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
        Ok(self.wait_for(&uri, before, wait).await)
    }

    async fn wait_for(&self, uri: &str, before: u64, wait: Duration) -> Check {
        let deadline = Instant::now() + wait;
        let mut seen = before;
        let mut settled_at: Option<Instant> = None;
        loop {
            let changed = self.shared.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let now_count = self.shared.count(uri);
            if now_count > seen {
                seen = now_count;
                settled_at = Some(Instant::now() + SETTLE);
            }
            let until = match settled_at {
                Some(settled) => settled.min(deadline),
                None => deadline,
            };
            if self.has_exited() && settled_at.is_none() {
                return Check::Exited;
            }
            if Instant::now() >= until {
                break;
            }
            tokio::select! {
                () = &mut changed => {}
                () = tokio::time::sleep_until(until) => {}
            }
        }
        match self
            .shared
            .published
            .lock()
            .expect("published lock")
            .get(uri)
        {
            Some(published) if published.count > before => {
                Check::Published(published.diagnostics.clone())
            }
            _ if self.has_exited() => Check::Exited,
            _ => Check::Pending,
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
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
}

/// How long the last publish may be followed by another before the diagnostics are taken as
/// final: servers publish a syntax pass and a type pass a moment apart.
const SETTLE: Duration = Duration::from_millis(150);

async fn read_loop(mut reader: BufReader<tokio::process::ChildStdout>, shared: Arc<Shared>) {
    while let Ok(Some(message)) = frame::read(&mut reader).await {
        handle(&shared, message).await;
    }
    shared.exited.store(true, Ordering::SeqCst);
    // Nobody waits on an answer that cannot come.
    shared.pending.lock().expect("pending lock").clear();
    shared.changed.notify_waiters();
}

async fn handle(shared: &Shared, message: Value) {
    let method = message["method"].as_str();
    match (method, message.get("id")) {
        (Some(method), Some(id)) => {
            // A request from the server: answered at once, with nothing, so it never waits for us.
            let result = if method == "workspace/configuration" {
                let items = message["params"]["items"].as_array().map_or(0, Vec::len);
                Value::Array(vec![Value::Null; items])
            } else {
                Value::Null
            };
            let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
            let _ = shared.send(&reply).await;
        }
        (Some("textDocument/publishDiagnostics"), None) => {
            if let Ok(params) =
                serde_json::from_value::<PublishDiagnosticsParams>(message["params"].clone())
            {
                let mut published = shared.published.lock().expect("published lock");
                let entry = published
                    .entry(params.uri.as_str().to_string())
                    .or_default();
                entry.count += 1;
                entry.diagnostics = params.diagnostics;
                drop(published);
                shared.changed.notify_waiters();
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
