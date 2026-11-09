//! M3a (C3): a child's permissions are the narrower of its parent's and its own, never wider; deny
//! rules are a superset; and what the user allows for the session while a child runs stays with
//! that child.

use std::path::Path;

use harness_core::{
    agents::perm::{child_engine_config, child_rules},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    permission::{Action, Decision, Mode, PermissionPolicy},
};

const MODES: [Mode; 5] = [
    Mode::Plan,
    Mode::ReadOnly,
    Mode::Ask,
    Mode::Auto,
    Mode::FullAccess,
];

fn rules(allow: &[&str], deny: &[&str], confirm: &[&str]) -> RuleSet {
    let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
    RuleSet {
        allow: own(allow),
        deny: own(deny),
        confirm: own(confirm),
    }
}

fn parent_config(mode: Mode, ws: &Path, rules: RuleSet) -> EngineConfig {
    EngineConfig {
        mode,
        workspace: ws.to_path_buf(),
        read_dirs: vec![],
        rules,
        sandbox_available: true,
        writes_need_approval: false,
    }
}

fn write(ws: &Path) -> Action {
    Action::Write(ws.join("a.txt"))
}

#[test]
fn the_narrower_mode_is_never_wider_than_either() {
    for a in MODES {
        for b in MODES {
            let n = a.narrower(b);
            assert!(n.grants_at_most(a) && n.grants_at_most(b), "{a} {b} -> {n}");
        }
    }
}

#[test]
fn a_child_in_a_wider_mode_than_its_parent_gets_the_parents() {
    let ws = tempfile::tempdir().unwrap();
    // Parent asks about writes; a spec that says auto cannot lift that.
    let config = child_engine_config(
        &parent_config(Mode::Ask, ws.path(), RuleSet::default()),
        Mode::Ask,
        Some(Mode::Auto),
        &RuleSet::default(),
        None,
    );
    assert_eq!(config.mode, Mode::Ask);
    let child = PermissionEngine::new(config);
    assert!(matches!(child.check(&write(ws.path())), Decision::Ask(_)));
}

#[test]
fn a_read_only_parent_makes_a_read_only_child_whatever_the_spec_says() {
    let ws = tempfile::tempdir().unwrap();
    for own in MODES {
        let config = child_engine_config(
            &parent_config(Mode::ReadOnly, ws.path(), RuleSet::default()),
            Mode::ReadOnly,
            Some(own),
            &RuleSet::default(),
            None,
        );
        let child = PermissionEngine::new(config);
        assert!(
            matches!(child.check(&write(ws.path())), Decision::Deny(_)),
            "spec mode {own}"
        );
    }
}

#[test]
fn a_narrower_spec_narrows_the_child() {
    let ws = tempfile::tempdir().unwrap();
    let config = child_engine_config(
        &parent_config(Mode::Auto, ws.path(), RuleSet::default()),
        Mode::Auto,
        Some(Mode::ReadOnly),
        &RuleSet::default(),
        None,
    );
    assert_eq!(config.mode, Mode::ReadOnly);
}

#[test]
fn the_parents_live_mode_counts_not_the_one_it_started_in() {
    let ws = tempfile::tempdir().unwrap();
    let config = child_engine_config(
        &parent_config(Mode::Auto, ws.path(), RuleSet::default()),
        Mode::Plan,
        None,
        &RuleSet::default(),
        None,
    );
    assert_eq!(config.mode, Mode::Plan);
}

#[test]
fn deny_and_confirm_rules_are_a_superset_and_allow_rules_an_intersection() {
    let merged = child_rules(
        &rules(
            &["bash:cargo *", "bash:ls *"],
            &["bash:rm *"],
            &["bash:git *"],
        ),
        &rules(
            &["bash:cargo *", "bash:curl *"],
            &["bash:sudo *"],
            &["bash:ssh *"],
        ),
    );
    assert_eq!(merged.deny, vec!["bash:rm *", "bash:sudo *"]);
    assert_eq!(merged.confirm, vec!["bash:git *", "bash:ssh *"]);
    // `curl` is the spec's alone: the parent never allowed it, so the child may not either.
    assert_eq!(merged.allow, vec!["bash:cargo *"]);
}

#[test]
fn a_spec_with_no_allow_rules_keeps_the_parents() {
    let merged = child_rules(&rules(&["bash:ls *"], &[], &[]), &RuleSet::default());
    assert_eq!(merged.allow, vec!["bash:ls *"]);
}

#[test]
fn the_parents_deny_rules_bind_the_child() {
    let ws = tempfile::tempdir().unwrap();
    let config = child_engine_config(
        &parent_config(Mode::Auto, ws.path(), rules(&[], &["bash:rm *"], &[])),
        Mode::Auto,
        None,
        &RuleSet::default(),
        None,
    );
    let child = PermissionEngine::new(config);
    assert!(matches!(
        child.check(&Action::Bash("rm -r build".into())),
        Decision::Deny(_)
    ));
}

#[test]
fn allow_for_session_given_to_a_child_never_reaches_the_parent_or_a_sibling() {
    let ws = tempfile::tempdir().unwrap();
    let parent_cfg = parent_config(Mode::Ask, ws.path(), RuleSet::default());
    let parent = PermissionEngine::new(parent_cfg.clone());
    let make = || {
        PermissionEngine::new(child_engine_config(
            &parent_cfg,
            Mode::Ask,
            None,
            &RuleSet::default(),
            None,
        ))
    };
    let (first, second) = (make(), make());
    let action = Action::Bash("cargo test".into());
    assert!(matches!(first.check(&action), Decision::Ask(_)));
    assert!(first.remember(&action));
    assert_eq!(first.check(&action), Decision::Allow);
    assert!(matches!(parent.check(&action), Decision::Ask(_)));
    assert!(matches!(second.check(&action), Decision::Ask(_)));
}

#[test]
fn a_spec_can_move_the_child_to_another_workspace() {
    let ws = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let config = child_engine_config(
        &parent_config(Mode::Auto, ws.path(), RuleSet::default()),
        Mode::Auto,
        None,
        &RuleSet::default(),
        Some(other.path().to_path_buf()),
    );
    assert_eq!(config.workspace, other.path());
}
