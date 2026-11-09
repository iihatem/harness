//! What a child agent may do: the narrower of what its parent may and what its own spec asks,
//! never wider. The child gets a permission engine of its own, so what the user allows for the
//! session while it runs is remembered by that engine alone.

use std::sync::{Arc, Mutex, PoisonError};

use super::workspace::ChildWorkspace;
use crate::{
    engine::{EngineConfig, PermissionEngine, RuleSet},
    permission::{Action, Decision, Mode, PermissionPolicy},
};

/// The rules of a child: the parent's deny and confirm rules and the spec's together (a superset
/// of each), and, for allow rules, only those in both when the spec names any (an allow rule the
/// parent does not have would widen the child), else the parent's.
pub fn child_rules(parent: &RuleSet, own: &RuleSet) -> RuleSet {
    let union = |a: &[String], b: &[String]| {
        let mut out = a.to_vec();
        for rule in b {
            if !out.contains(rule) {
                out.push(rule.clone());
            }
        }
        out
    };
    let allow = if own.allow.is_empty() {
        parent.allow.clone()
    } else {
        own.allow
            .iter()
            .filter(|rule| parent.allow.contains(rule))
            .cloned()
            .collect()
    };
    RuleSet {
        allow,
        deny: union(&parent.deny, &own.deny),
        confirm: union(&parent.confirm, &own.confirm),
    }
}

/// The engine settings of a child, from its parent's `parent` (its rules and paths), the mode
/// the parent is in now (`parent_mode`), and the child's spec: its mode (`None`: the parent's),
/// its own rules and its workspace (the parent's, or one harness made for it).
pub fn child_engine_config(
    parent: &EngineConfig,
    parent_mode: Mode,
    own_mode: Option<Mode>,
    own_rules: &RuleSet,
    workspace: &ChildWorkspace,
) -> EngineConfig {
    EngineConfig {
        mode: own_mode.map_or(parent_mode, |own| parent_mode.narrower(own)),
        workspace: workspace.resolve(&parent.workspace),
        read_dirs: parent.read_dirs.clone(),
        rules: child_rules(&parent.rules, own_rules),
        sandbox_available: parent.sandbox_available,
        writes_need_approval: parent.writes_need_approval,
    }
}

/// A child's permissions while it runs: its own engine, whose mode follows its parent's. On every
/// check the mode is the narrower of the child's own (what its spec asked, never wider than the
/// parent's when it began) and the parent's now, so switching the parent to plan or read-only
/// restricts the children it already runs, and switching it back lifts that. What the user
/// allows for the session while the child runs stays in the child's engine alone. The rules are
/// the parent's as they were when the child began: they do not change while a session runs (a
/// command's turn rules only add allows, and a child never gets those).
pub struct ChildPolicy {
    engine: PermissionEngine,
    parent: Arc<dyn PermissionPolicy>,
    /// The child's own mode, before the parent's narrows it.
    own: Mutex<Mode>,
}

impl ChildPolicy {
    /// The policy of a child with `engine` (made by [`child_engine_config`]) under `parent`;
    /// `own` is the mode its spec asked for (`None`: whatever its parent's is).
    pub fn new(
        engine: PermissionEngine,
        parent: Arc<dyn PermissionPolicy>,
        own: Option<Mode>,
    ) -> Self {
        ChildPolicy {
            engine,
            parent,
            own: Mutex::new(own.unwrap_or(Mode::FullAccess)),
        }
    }

    /// Brings the engine's mode to the narrower of the child's own and the parent's now.
    fn follow_parent(&self) {
        let own = *self.own.lock().unwrap_or_else(PoisonError::into_inner);
        let wanted = self
            .parent
            .mode()
            .map_or(own, |parent| own.narrower(parent));
        if self.engine.mode() != wanted {
            self.engine.set_mode(wanted);
        }
    }
}

impl PermissionPolicy for ChildPolicy {
    fn check(&self, action: &Action) -> Decision {
        self.follow_parent();
        self.engine.check(action)
    }

    fn remember(&self, action: &Action) -> bool {
        self.follow_parent();
        self.engine.remember(action)
    }

    fn can_remember(&self, action: &Action) -> bool {
        self.follow_parent();
        self.engine.can_remember(action)
    }

    fn set_mode(&self, mode: Mode) {
        *self.own.lock().unwrap_or_else(PoisonError::into_inner) = mode;
        self.follow_parent();
    }

    fn set_sandbox_available(&self, available: bool) {
        self.engine.set_sandbox_available(available);
    }

    fn set_unsandboxed_reason(&self, why: &str) {
        PermissionPolicy::set_unsandboxed_reason(&self.engine, why);
    }

    fn set_turn_rules(&self, rules: Option<RuleSet>) {
        self.engine.set_turn_rules(rules);
    }

    fn mode(&self) -> Option<Mode> {
        self.follow_parent();
        Some(self.engine.mode())
    }
}
