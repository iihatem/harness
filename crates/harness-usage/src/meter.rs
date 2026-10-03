//! The meter the runtime reports to: it turns each request into a ledger record.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use harness_core::{
    meter::{AccountKind, Meter, RequestRecord},
    time::now_unix,
};
use sha2::{Digest, Sha256};

use crate::{
    ledger::{Ledger, LedgerRecord, VERSION},
    paths::Dirs,
};

/// Where the current time comes from, in seconds since the epoch: the clock, or a test's.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A hash of the workspace root, which names a project in the ledger without giving its path.
pub fn project_id(workspace: &Path) -> String {
    let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
    hex::encode(&digest[..8])
}

/// Keeps the ledger.
pub struct UsageMeter {
    ledger: Ledger,
    project: String,
    clock: Clock,
    warnings: Mutex<Vec<String>>,
    /// Whether the ledger failed to write already: it is said once.
    failed: Mutex<bool>,
}

impl UsageMeter {
    /// A meter writing the ledger under harness's data directory `data`, for a session in
    /// `workspace`.
    pub fn open(data: &Path, workspace: &Path) -> UsageMeter {
        UsageMeter {
            ledger: Ledger::new(&Dirs::under(data).usage),
            project: project_id(workspace),
            clock: Arc::new(now_unix),
            warnings: Mutex::new(Vec::new()),
            failed: Mutex::new(false),
        }
    }

    /// Reads the time from `clock`.
    pub fn with_clock(mut self, clock: Clock) -> UsageMeter {
        self.clock = clock;
        self
    }

    fn warn(&self, message: String) {
        let mut failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        if !*failed {
            *failed = true;
            self.warnings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(message);
        }
    }
}

impl Meter for UsageMeter {
    fn record_request(&self, request: &RequestRecord) {
        let account = AccountKind::of(&request.model, request.local);
        let buckets = request.usage.buckets();
        // A local model costs nothing; what a hosted one costs needs a price.
        let free = (account == AccountKind::Local).then_some(0.0);
        let record = LedgerRecord {
            v: VERSION,
            t: (self.clock)(),
            session: request.session.clone(),
            project: self.project.clone(),
            role: request.role.clone(),
            model: request.model.clone(),
            account,
            input: buckets.input,
            cache_read: buckets.cache_read,
            cache_write: buckets.cache_write,
            cache_write_1h: buckets.cache_write_1h,
            output: buckets.output,
            reasoning: buckets.reasoning,
            billed_usd: free,
            list_usd: free,
            price: None,
            ms: request.duration.as_millis() as u64,
            outcome: request.outcome.clone(),
            window: None,
        };
        if let Err(e) = self.ledger.append(&record) {
            self.warn(format!(
                "cannot write the usage ledger in {}: {e}; requests are no longer recorded",
                self.ledger.dir().display()
            ));
        }
    }

    fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.lock().unwrap_or_else(|e| e.into_inner()))
    }
}
