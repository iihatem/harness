//! What a child agent may do: the narrower of what its parent may and what its own spec asks,
//! never wider. The child gets a permission engine of its own, so what the user allows for the
//! session while it runs is remembered by that engine alone.

use std::path::PathBuf;

use crate::{
    engine::{EngineConfig, RuleSet},
    permission::Mode,
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
/// its own rules and its workspace (`None`: the parent's).
pub fn child_engine_config(
    parent: &EngineConfig,
    parent_mode: Mode,
    own_mode: Option<Mode>,
    own_rules: &RuleSet,
    workspace: Option<PathBuf>,
) -> EngineConfig {
    EngineConfig {
        mode: own_mode.map_or(parent_mode, |own| parent_mode.narrower(own)),
        workspace: workspace.unwrap_or_else(|| parent.workspace.clone()),
        read_dirs: parent.read_dirs.clone(),
        rules: child_rules(&parent.rules, own_rules),
        sandbox_available: parent.sandbox_available,
        writes_need_approval: parent.writes_need_approval,
    }
}
