//! The meter a child agent reports to: the shared meter, told who is asking.

use std::sync::Arc;

use crate::meter::{
    AccountKind, AgentScope, BudgetStatus, Meter, RequestCost, RequestRecord, TurnRecord,
    WindowSnapshot,
};

/// Wraps the session's meter for one child: its records name the child and its parent, and its
/// budget checks are made against the parent's session, so what a child spends counts towards
/// the session budget that paid for it.
pub struct ScopedMeter {
    inner: Arc<dyn Meter>,
    scope: AgentScope,
}

impl ScopedMeter {
    pub fn new(inner: Arc<dyn Meter>, scope: AgentScope) -> Self {
        ScopedMeter { inner, scope }
    }
}

impl Meter for ScopedMeter {
    fn record_request(&self, request: &RequestRecord) -> RequestCost {
        self.inner.record_request_as(request, Some(&self.scope))
    }

    fn check_budget(&self, _session: &str, account: AccountKind) -> BudgetStatus {
        self.inner.check_budget(&self.scope.parent, account)
    }

    fn record_window(&self, snapshot: &WindowSnapshot) {
        self.inner.record_window(snapshot)
    }

    fn record_turn(&self, turn: &TurnRecord) {
        self.inner.record_turn_as(turn, Some(&self.scope))
    }

    fn turns_rewound(&self, session: &str, turns: &[String]) {
        self.inner.turns_rewound(session, turns)
    }

    /// Left to the main agent's meter calls: a child's events are not where they are shown.
    fn take_warnings(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Usage;
    use std::{sync::Mutex, time::Duration};

    #[derive(Default)]
    struct Recording {
        requests: Mutex<Vec<(String, Option<AgentScope>)>>,
        checked: Mutex<Vec<String>>,
    }

    impl Meter for Recording {
        fn record_request(&self, r: &RequestRecord) -> RequestCost {
            self.record_request_as(r, None)
        }
        fn record_request_as(&self, r: &RequestRecord, scope: Option<&AgentScope>) -> RequestCost {
            self.requests
                .lock()
                .unwrap()
                .push((r.session.clone(), scope.cloned()));
            RequestCost {
                account: AccountKind::Local,
                billed_usd: Some(0.0),
                list_usd: Some(0.0),
                avoided: crate::meter::Avoided::NotApplicable,
            }
        }
        fn check_budget(&self, session: &str, _a: AccountKind) -> BudgetStatus {
            self.checked.lock().unwrap().push(session.into());
            BudgetStatus::default()
        }
    }

    fn request() -> RequestRecord {
        RequestRecord {
            session: "child-session".into(),
            role: "main".into(),
            model: "m/x".into(),
            local: true,
            usage: Usage::default(),
            duration: Duration::from_millis(1),
            outcome: "ok".into(),
        }
    }

    #[test]
    fn a_childs_requests_name_it_and_its_budget_is_the_parents() {
        let inner = Arc::new(Recording::default());
        let scoped = ScopedMeter::new(
            inner.clone(),
            AgentScope {
                agent: "a1".into(),
                parent: "parent-session".into(),
            },
        );
        scoped.record_request(&request());
        scoped.check_budget("child-session", AccountKind::ApiKey);
        let requests = inner.requests.lock().unwrap();
        assert_eq!(requests[0].0, "child-session");
        assert_eq!(requests[0].1.as_ref().unwrap().agent, "a1");
        assert_eq!(requests[0].1.as_ref().unwrap().parent, "parent-session");
        assert_eq!(*inner.checked.lock().unwrap(), vec!["parent-session"]);
    }

    #[test]
    fn the_main_meter_calls_are_unchanged() {
        let inner = Recording::default();
        inner.record_request(&request());
        assert_eq!(inner.requests.lock().unwrap()[0].1, None);
    }
}
