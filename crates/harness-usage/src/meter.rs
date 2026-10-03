//! The meter the runtime reports to: it turns each request into a ledger record.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use harness_core::{
    message::Buckets,
    meter::{AccountKind, Avoided, Meter, RequestCost, RequestRecord},
    time::now_unix,
};
use sha2::{Digest, Sha256};

use crate::{
    ledger::{Ledger, LedgerRecord, VERSION},
    paths::Dirs,
    pricing::Pricing,
};

/// Where the current time comes from, in seconds since the epoch: the clock, or a test's.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A hash of the workspace root, which names a project in the ledger without giving its path.
pub fn project_id(workspace: &Path) -> String {
    let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
    hex::encode(&digest[..8])
}

/// What `buckets` would have cost on `baseline`, the figure "avoided" is: unknown when the
/// baseline has no price, never 0.
pub fn avoided_for(pricing: &Pricing, baseline: &str, buckets: &Buckets) -> Avoided {
    if buckets.priced_total() == 0 {
        return Avoided::Usd(0.0);
    }
    match pricing
        .price_of(baseline)
        .and_then(|(price, _)| price.cost(buckets))
    {
        Some(usd) => Avoided::Usd(usd),
        None => Avoided::Unknown,
    }
}

/// Keeps the ledger.
pub struct UsageMeter {
    ledger: Ledger,
    project: String,
    clock: Clock,
    pricing: Pricing,
    /// The model `usage.baseline` names, which the avoided figure is measured against.
    baseline: Option<String>,
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
            pricing: Pricing::load(&Dirs::under(data).pricing, Vec::new()),
            baseline: None,
            warnings: Mutex::new(Vec::new()),
            failed: Mutex::new(false),
        }
    }

    /// Reads the time from `clock`.
    pub fn with_clock(mut self, clock: Clock) -> UsageMeter {
        self.clock = clock;
        self
    }

    /// Prices requests with `pricing`.
    pub fn with_pricing(mut self, pricing: Pricing) -> UsageMeter {
        self.pricing = pricing;
        self
    }

    /// Measures the avoided figure against `baseline` (`<provider>/<model>`), when there is one.
    pub fn with_baseline(mut self, baseline: Option<String>) -> UsageMeter {
        self.baseline = baseline;
        self
    }

    /// The prices requests are costed with.
    pub fn pricing(&self) -> &Pricing {
        &self.pricing
    }

    /// The three figures for `buckets` on `model` through `account`, and which table priced it.
    fn cost_of(
        &self,
        model: &str,
        account: AccountKind,
        buckets: &Buckets,
    ) -> (RequestCost, Option<String>) {
        let none_sent = buckets.priced_total() == 0;
        let priced = self.pricing.price_of(model);
        let list = match account {
            AccountKind::Local => Some(0.0),
            _ if none_sent => Some(0.0),
            _ => priced.as_ref().and_then(|(price, _)| price.cost(buckets)),
        };
        let billed = match account {
            AccountKind::ApiKey => list,
            AccountKind::Subscription | AccountKind::Local => Some(0.0),
        };
        let avoided = match (&self.baseline, account) {
            (Some(_), AccountKind::ApiKey) | (None, _) => Avoided::NotApplicable,
            (Some(baseline), _) => avoided_for(&self.pricing, baseline, buckets),
        };
        let source = (account != AccountKind::Local && !none_sent)
            .then(|| priced.map(|(_, source)| source.label()))
            .flatten();
        (
            RequestCost {
                account,
                billed_usd: billed,
                list_usd: list,
                avoided,
            },
            source,
        )
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
    fn record_request(&self, request: &RequestRecord) -> RequestCost {
        let account = AccountKind::of(&request.model, request.local);
        let buckets = request.usage.buckets();
        let (cost, price) = self.cost_of(&request.model, account, &buckets);
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
            billed_usd: cost.billed_usd,
            list_usd: cost.list_usd,
            price,
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
        cost
    }

    fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.lock().unwrap_or_else(|e| e.into_inner()))
    }
}
