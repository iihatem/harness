//! Money budgets: limits on billed cost for a session, a day and a month (UTC), checked before
//! each model request.

use harness_core::meter::BudgetKind;

/// `[budgets]`, in USD; a budget that is not set has no limit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Budgets {
    pub session_usd: Option<f64>,
    pub daily_usd: Option<f64>,
    pub monthly_usd: Option<f64>,
}

impl Budgets {
    /// The limit of `kind`.
    pub fn limit(&self, kind: BudgetKind) -> Option<f64> {
        match kind {
            BudgetKind::Session => self.session_usd,
            BudgetKind::Daily => self.daily_usd,
            BudgetKind::Monthly => self.monthly_usd,
        }
    }

    /// Whether any budget is set.
    pub fn any(&self) -> bool {
        self.session_usd.is_some() || self.daily_usd.is_some() || self.monthly_usd.is_some()
    }
}

/// The three budgets, in the order they are checked and reported.
pub const KINDS: [BudgetKind; 3] = [BudgetKind::Session, BudgetKind::Daily, BudgetKind::Monthly];

/// One line of `/budget`: a budget, its limit if it has one, and what is spent against it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetLine {
    pub budget: BudgetKind,
    pub limit_usd: Option<f64>,
    pub spent_usd: f64,
}

/// Whether `spent` is at least `percent` of `limit`, allowing for the rounding of sums of money.
pub fn reached(spent: f64, limit: f64, percent: f64) -> bool {
    spent * 100.0 + 1e-9 >= limit * percent
}
