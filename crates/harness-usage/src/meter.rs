//! The meter the runtime reports to: it turns each request into a ledger record.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use harness_core::{
    message::Buckets,
    meter::{
        AccountKind, Avoided, BudgetKind, BudgetNotice, BudgetStatus, Meter, RequestCost,
        RequestRecord, WindowSnapshot,
    },
    time::civil_date,
    time::now_unix,
};
use sha2::{Digest, Sha256};

use crate::{
    budget::{BudgetLine, Budgets, KINDS, reached},
    ledger::{Ledger, LedgerRecord, VERSION},
    paths::Dirs,
    pricing::Pricing,
    store::Store,
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
    /// The snapshot the request in flight has seen, to name in its record, and the last one
    /// written, so a snapshot repeated by every response is written once.
    window: Mutex<(Option<String>, Option<String>)>,
    dirs: Dirs,
    budgets: Mutex<Budgets>,
    /// The report cache the budgets are checked against, opened when first needed.
    store: Mutex<Option<Store>>,
    /// The 80% warnings given: budget, period and limit, so a raised limit warns again.
    warned: Mutex<std::collections::HashSet<String>>,
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
            window: Mutex::new((None, None)),
            dirs: Dirs::under(data),
            budgets: Mutex::new(Budgets::default()),
            store: Mutex::new(None),
            warned: Mutex::new(Default::default()),
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

    /// Checks requests against `budgets`.
    pub fn with_budgets(self, budgets: Budgets) -> UsageMeter {
        *self.budgets.lock().unwrap_or_else(|e| e.into_inner()) = budgets;
        self
    }

    /// The budgets now in force.
    pub fn budgets(&self) -> Budgets {
        self.budgets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Sets the session budget, for this session only (`/budget <usd>`).
    pub fn set_session_budget(&self, usd: f64) {
        self.budgets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_usd = Some(usd);
    }

    /// What each budget allows and what is spent against it, for `session` today and this month.
    pub fn budget_report(&self, session: &str) -> Vec<BudgetLine> {
        let budgets = self.budgets();
        KINDS
            .iter()
            .map(|&kind| BudgetLine {
                budget: kind,
                limit_usd: budgets.limit(kind),
                spent_usd: self.spent(kind, session).unwrap_or(0.0),
            })
            .collect()
    }

    /// What was billed against `kind`, from the ledger.
    fn spent(&self, kind: BudgetKind, session: &str) -> crate::error::Result<f64> {
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if store.is_none() {
            *store = Some(Store::open(&self.dirs)?);
        }
        let store = store.as_mut().expect("opened above");
        store.sync()?;
        let day = civil_date((self.clock)());
        match kind {
            BudgetKind::Session => store.session_spent(session),
            BudgetKind::Daily => store.day_spent(&day),
            BudgetKind::Monthly => store.month_spent(&day[..7]),
        }
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
            window: self
                .window
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .0
                .take(),
        };
        if let Err(e) = self.ledger.append(&record) {
            self.warn(format!(
                "cannot write the usage ledger in {}: {e}; requests are no longer recorded",
                self.ledger.dir().display()
            ));
        }
        cost
    }

    fn record_window(&self, snapshot: &WindowSnapshot) {
        let id = snapshot.id();
        let mut window = self.window.lock().unwrap_or_else(|e| e.into_inner());
        window.0 = Some(id.clone());
        if window.1.as_deref() == Some(id.as_str()) {
            return;
        }
        window.1 = Some(id);
        drop(window);
        if let Err(e) = self.ledger.append_window(snapshot) {
            self.warn(format!(
                "cannot write the usage ledger in {}: {e}; requests are no longer recorded",
                self.ledger.dir().display()
            ));
        }
    }

    fn check_budget(&self, session: &str) -> BudgetStatus {
        let budgets = self.budgets();
        let mut status = BudgetStatus::default();
        for kind in KINDS {
            let Some(limit) = budgets.limit(kind) else {
                continue;
            };
            let spent = match self.spent(kind, session) {
                Ok(spent) => spent,
                Err(e) => {
                    self.warn(format!("cannot check the budgets: {e}"));
                    return BudgetStatus::default();
                }
            };
            let notice = BudgetNotice {
                budget: kind,
                spent_usd: spent,
                limit_usd: limit,
            };
            if reached(spent, limit, 100.0) {
                status.stop.get_or_insert(notice);
            } else if reached(spent, limit, 80.0) {
                let period = match kind {
                    BudgetKind::Session => session.to_string(),
                    BudgetKind::Daily => civil_date((self.clock)()),
                    BudgetKind::Monthly => civil_date((self.clock)())[..7].to_string(),
                };
                let key = format!("{}:{period}:{limit}", kind.name());
                if self
                    .warned
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key)
                {
                    status.warnings.push(notice);
                }
            }
        }
        status
    }

    fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.lock().unwrap_or_else(|e| e.into_inner()))
    }
}
