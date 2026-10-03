//! The meter the runtime reports to: it turns each request into a ledger record.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use harness_core::{
    message::Buckets,
    meter::{
        AccountKind, Avoided, BudgetKind, BudgetNotice, BudgetStatus, Meter, RequestCost,
        RequestRecord, TurnRecord, WindowSnapshot,
    },
    time::civil_date,
    time::now_unix,
};
use sha2::{Digest, Sha256};

use crate::{
    budget::{BudgetLine, Budgets, KINDS, reached},
    ledger::{Ledger, LedgerRecord, VERSION},
    outcomes::{OutcomeLog, OutcomeRecord},
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
    /// The kinds of problem already said (ledger, outcome log, ...): each is said once.
    warned_kinds: Mutex<std::collections::HashSet<&'static str>>,
    /// What this process billed, as (time, session, USD): the floor the budgets are checked
    /// against, so they still apply when the ledger or its cache cannot be used.
    spent_here: Mutex<Vec<(u64, String, f64)>>,
    /// The budgets as configured, which `/new` and `/resume` return the session budget to.
    configured: Mutex<Budgets>,
    /// The snapshot the request in flight has seen, to name in its record, and the last one
    /// written, so a snapshot repeated by every response is written once.
    window: Mutex<(Option<String>, Option<String>)>,
    dirs: Dirs,
    outcomes: OutcomeLog,
    /// Whether turns are written to the outcome log (`[outcomes] enabled`).
    outcomes_enabled: bool,
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
            warned_kinds: Mutex::new(Default::default()),
            spent_here: Mutex::new(Vec::new()),
            configured: Mutex::new(Budgets::default()),
            window: Mutex::new((None, None)),
            dirs: Dirs::under(data),
            outcomes: OutcomeLog::new(&Dirs::under(data).outcomes),
            outcomes_enabled: true,
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

    /// Turns the outcome log on or off. The usage ledger does not depend on it.
    pub fn with_outcomes(mut self, enabled: bool) -> UsageMeter {
        self.outcomes_enabled = enabled;
        self
    }

    /// Checks requests against `budgets`.
    pub fn with_budgets(self, budgets: Budgets) -> UsageMeter {
        *self.configured.lock().unwrap_or_else(|e| e.into_inner()) = budgets.clone();
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

    /// Returns the session budget to what is configured: a `/budget <usd>` figure applies to the
    /// session it was given in, not to the next one (`/new`, `/resume`).
    pub fn reset_session_budget(&self) {
        let configured = self
            .configured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_usd;
        self.budgets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_usd = configured;
    }

    /// What each budget allows and what is spent against it, for `session` today and this month.
    pub fn budget_report(&self, session: &str) -> Vec<BudgetLine> {
        let budgets = self.budgets();
        KINDS
            .iter()
            .map(|&kind| BudgetLine {
                budget: kind,
                limit_usd: budgets.limit(kind),
                spent_usd: self.spent_or_here(kind, session).0,
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

    /// What this process has billed against `kind`.
    fn spent_by_this_process(&self, kind: BudgetKind, session: &str) -> f64 {
        let day = civil_date((self.clock)());
        self.spent_here
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(t, s, _)| match kind {
                BudgetKind::Session => s == session,
                BudgetKind::Daily => civil_date(*t) == day,
                BudgetKind::Monthly => civil_date(*t)[..7] == day[..7],
            })
            .map(|(_, _, usd)| usd)
            .sum()
    }

    /// What was billed against `kind`: the ledger's figure, but never less than this process's
    /// own spending. When the ledger cannot be read, this process's figure alone, and the error.
    fn spent_or_here(&self, kind: BudgetKind, session: &str) -> (f64, Option<crate::error::Error>) {
        let here = self.spent_by_this_process(kind, session);
        match self.spent(kind, session) {
            Ok(spent) => (spent.max(here), None),
            Err(e) => (here, Some(e)),
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

    /// Says `message` once for the `kind` of problem it is about.
    fn warn(&self, kind: &'static str, message: String) {
        let first = self
            .warned_kinds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(kind);
        if first {
            self.warn_now(message);
        }
    }

    /// Says `message` now, and again on every call: a problem that goes on mattering.
    fn warn_now(&self, message: String) {
        let mut warnings = self.warnings.lock().unwrap_or_else(|e| e.into_inner());
        if !warnings.contains(&message) {
            warnings.push(message);
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
        if let Some(usd) = cost.billed_usd.filter(|usd| *usd > 0.0) {
            self.spent_here
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((record.t, record.session.clone(), usd));
        }
        if let Err(e) = self.ledger.append(&record) {
            self.warn(
                "ledger",
                format!(
                    "cannot write the usage ledger in {}: {e}; requests are no longer recorded",
                    self.ledger.dir().display()
                ),
            );
        }
        cost
    }

    fn record_window(&self, snapshot: &WindowSnapshot) {
        let id = snapshot.id();
        let mut window = self.window.lock().unwrap_or_else(|e| e.into_inner());
        if window.1.as_deref() == Some(id.as_str()) {
            window.0 = Some(id);
            return;
        }
        match self.ledger.append_window(snapshot) {
            Ok(()) => {
                window.0 = Some(id.clone());
                window.1 = Some(id);
            }
            Err(e) => {
                // Not written, so no record names it, and it is tried again when seen again.
                drop(window);
                self.warn(
                    "ledger",
                    format!(
                        "cannot write the usage ledger in {}: {e}; requests are no longer recorded",
                        self.ledger.dir().display()
                    ),
                );
            }
        }
    }

    fn record_turn(&self, turn: &TurnRecord) {
        if !self.outcomes_enabled {
            return;
        }
        let record = OutcomeRecord::of(turn, &self.project);
        if let Err(e) = self.outcomes.append(&record) {
            self.warn(
                "outcomes",
                format!(
                    "cannot write the outcome log in {}: {e}",
                    self.dirs.outcomes.display()
                ),
            );
        }
    }

    fn turns_rewound(&self, session: &str, turns: &[String]) {
        if !self.outcomes_enabled || turns.is_empty() {
            return;
        }
        if let Err(e) = self.outcomes.mark_rewound(session, turns, (self.clock)()) {
            self.warn(
                "outcomes-update",
                format!(
                    "cannot update the outcome log in {}: {e}",
                    self.dirs.outcomes.display()
                ),
            );
        }
    }

    fn check_budget(&self, session: &str, account: AccountKind) -> BudgetStatus {
        let budgets = self.budgets();
        let mut status = BudgetStatus::default();
        for kind in KINDS {
            let Some(limit) = budgets.limit(kind) else {
                continue;
            };
            let (spent, failed) = self.spent_or_here(kind, session);
            if let Some(e) = failed {
                // Every turn, until the ledger can be read again; meanwhile only what this
                // process has spent counts.
                self.warn_now(format!(
                    "cannot check the {} budget: {e}; counting only what this run has spent",
                    kind.name()
                ));
            }
            let notice = BudgetNotice {
                budget: kind,
                spent_usd: spent,
                limit_usd: limit,
            };
            if reached(spent, limit, 100.0) {
                // Only a request that would add billed cost is refused.
                if account == AccountKind::ApiKey {
                    status.stop.get_or_insert(notice);
                } else {
                    status.paused.get_or_insert(notice);
                }
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
