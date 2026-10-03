//! `[gates]`: the settings, their defaults, and the trust a project's gates need.

use harness_config::{config, trust::TrustStore};
use harness_core::gate::Gates;

/// A workspace with `project` as its config and `global` as the global one, and the trust store
/// in the same temporary directory.
struct Fixture {
    _dir: tempfile::TempDir,
    global: std::path::PathBuf,
    workspace: std::path::PathBuf,
    trust: TrustStore,
}

fn fixture(global: &str, project: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("config.toml");
    std::fs::write(&global_file, global).unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(workspace.join(".harness")).unwrap();
    std::fs::write(workspace.join(".harness/config.toml"), project).unwrap();
    let trust = TrustStore::load(&dir.path().join("data")).unwrap();
    Fixture {
        global: global_file,
        workspace,
        trust,
        _dir: dir,
    }
}

impl Fixture {
    fn load(&self) -> Result<config::Config, config::ConfigError> {
        config::load(&self.global, &self.workspace, &self.trust)
    }

    fn trust_now(&mut self) {
        let widening = config::project_widening(&self.global, &self.workspace).unwrap();
        self.trust
            .trust(&self.workspace, &widening.fingerprint)
            .unwrap();
    }
}

// Spec "Defaults": only `test` is set; 300 seconds, 3 retries, a 60-line tail, no after-edit
// command.
#[test]
fn only_the_test_command_set_leaves_the_other_settings_at_their_defaults() {
    let cfg = fixture("[gates]\ntest = \"cargo test\"\n", "")
        .load()
        .unwrap();
    assert_eq!(
        cfg.gates,
        Gates {
            after_edit: None,
            test: Some("cargo test".into()),
            timeout_s: 300,
            max_retries: 3,
            output_tail_lines: 60,
        }
    );
    assert!(cfg.gates.is_configured());
}

// Spec "No gates configured".
#[test]
fn without_a_gates_table_nothing_is_configured() {
    let cfg = fixture("", "").load().unwrap();
    assert_eq!(cfg.gates, Gates::default());
    assert!(!cfg.gates.is_configured());
    assert_eq!(cfg.gates.timeout_s, 300);
}

#[test]
fn every_setting_is_read() {
    let cfg = fixture(
        "[gates]\nafter_edit = \"ruff check .\"\ntest = \"pytest -q\"\ntimeout_s = 5\nmax_retries = 1\noutput_tail_lines = 10\n",
        "",
    )
    .load()
    .unwrap();
    assert_eq!(cfg.gates.after_edit.as_deref(), Some("ruff check ."));
    assert_eq!(cfg.gates.test.as_deref(), Some("pytest -q"));
    assert_eq!((cfg.gates.timeout_s, cfg.gates.max_retries), (5, 1));
    assert_eq!(cfg.gates.output_tail_lines, 10);
}

// Spec "Gate command in a cloned repository": shown for trust, and until trusted no gate runs
// from that command.
#[test]
fn a_projects_gates_need_trust() {
    let mut f = fixture(
        "[gates]\ntest = \"cargo test\"\n",
        "[gates]\ntest = \"make evil\"\nafter_edit = \"make lint\"\nmax_retries = 9\n",
    );
    let cfg = f.load().unwrap();
    assert_eq!(cfg.gates.test.as_deref(), Some("cargo test"));
    assert_eq!(cfg.gates.after_edit, None);
    assert_eq!(cfg.gates.max_retries, 3);
    assert!(!cfg.trusted);
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    assert!(cfg.warnings[0].contains("gates.test"), "{:?}", cfg.warnings);

    let widening = config::project_widening(&f.global, &f.workspace).unwrap();
    assert_eq!(
        widening.items,
        [
            "gates.after_edit = \"make lint\"",
            "gates.test = \"make evil\"",
            "gates.max_retries = 9",
        ]
    );

    f.trust_now();
    let cfg = f.load().unwrap();
    assert_eq!(cfg.gates.test.as_deref(), Some("make evil"));
    assert_eq!(cfg.gates.after_edit.as_deref(), Some("make lint"));
    assert_eq!(cfg.gates.max_retries, 9);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn a_gate_changed_after_trust_is_untrusted_again() {
    let mut f = fixture("", "[gates]\ntest = \"make test\"\n");
    f.trust_now();
    assert_eq!(f.load().unwrap().gates.test.as_deref(), Some("make test"));
    std::fs::write(
        f.workspace.join(".harness/config.toml"),
        "[gates]\ntest = \"make evil\"\n",
    )
    .unwrap();
    let cfg = f.load().unwrap();
    assert_eq!(cfg.gates.test, None);
    assert_eq!(cfg.warnings.len(), 1);
}

#[test]
fn a_trusted_projects_gates_go_over_the_global_ones_field_by_field() {
    let mut f = fixture(
        "[gates]\nafter_edit = \"ruff check .\"\ntest = \"pytest\"\ntimeout_s = 30\n",
        "[gates]\ntest = \"pytest -x\"\n",
    );
    f.trust_now();
    let gates = f.load().unwrap().gates;
    assert_eq!(gates.after_edit.as_deref(), Some("ruff check ."));
    assert_eq!(gates.test.as_deref(), Some("pytest -x"));
    assert_eq!(gates.timeout_s, 30);
}

#[test]
fn invalid_gates_are_errors_naming_the_file() {
    for (text, problem) in [
        ("[gates]\ntimeout_s = 0\n", "timeout_s"),
        ("[gates]\ntimeout_s = 601\n", "timeout_s"),
        ("[gates]\noutput_tail_lines = 0\n", "output_tail_lines"),
        ("[gates]\ntest = \"  \"\n", "gates.test"),
        ("[gates]\nafter_edit = \"\"\n", "gates.after_edit"),
        ("[gates]\nmax_retry = 1\n", "max_retry"),
    ] {
        let f = fixture(text, "");
        let error = f.load().unwrap_err().to_string();
        assert!(error.contains("config.toml"), "{text}: {error}");
        assert!(error.contains(problem), "{text}: {error}");
        // A project's invalid gates are errors untrusted too, and cannot be trusted.
        let f = fixture("", text);
        assert!(f.load().is_err(), "{text}");
        assert!(
            config::project_widening(&f.global, &f.workspace).is_err(),
            "{text}"
        );
    }
}

// A confirmed detection is configuration: it supplies the commands when none is configured.
mod answers {
    use super::*;
    use harness_config::trust::GateAnswer;

    fn confirmed(test: &str) -> GateAnswer {
        GateAnswer {
            confirmed: true,
            after_edit: None,
            test: Some(test.into()),
        }
    }

    #[test]
    fn a_confirmed_detection_supplies_the_commands() {
        let mut f = fixture("", "");
        f.trust
            .set_gate_answer(&f.workspace, confirmed("cargo test"))
            .unwrap();
        let gates = f.load().unwrap().gates;
        assert_eq!(gates.test.as_deref(), Some("cargo test"));
        assert_eq!(gates.after_edit, None);
        assert_eq!(gates.timeout_s, 300);
    }

    #[test]
    fn configured_gates_win_over_a_confirmed_detection() {
        let mut f = fixture("[gates]\nafter_edit = \"make lint\"\n", "");
        f.trust
            .set_gate_answer(&f.workspace, confirmed("cargo test"))
            .unwrap();
        let gates = f.load().unwrap().gates;
        assert_eq!(gates.after_edit.as_deref(), Some("make lint"));
        assert_eq!(gates.test, None);
    }

    #[test]
    fn settings_without_commands_keep_their_values_beside_a_confirmed_detection() {
        let mut f = fixture("[gates]\ntimeout_s = 20\n", "");
        f.trust
            .set_gate_answer(&f.workspace, confirmed("cargo test"))
            .unwrap();
        let gates = f.load().unwrap().gates;
        assert_eq!(
            (gates.test.as_deref(), gates.timeout_s),
            (Some("cargo test"), 20)
        );
    }

    // Spec "Declined once": no gate runs.
    #[test]
    fn a_declined_detection_supplies_nothing() {
        let mut f = fixture("", "");
        f.trust
            .set_gate_answer(&f.workspace, GateAnswer::declined())
            .unwrap();
        assert!(!f.load().unwrap().gates.is_configured());
    }
}
