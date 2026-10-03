//! `[fallback]`: a model glob to an ordered list of models, and the trust a project's chains need.

use harness_config::{config, trust::TrustStore};

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

#[test]
fn a_chain_maps_a_glob_to_an_ordered_list() {
    let cfg = fixture(
        "[fallback]\n\"chatgpt/gpt-5*\" = [\"openai/gpt-5\", \"openrouter/qwen/qwen3-coder\"]\n",
        "",
    )
    .load()
    .unwrap();
    assert_eq!(
        cfg.fallback["chatgpt/gpt-5*"],
        ["openai/gpt-5", "openrouter/qwen/qwen3-coder"]
    );
    assert!(fixture("", "").load().unwrap().fallback.is_empty());
}

#[test]
fn a_chain_is_validated() {
    for bad in [
        "[fallback]\n\"chatgpt/*\" = [\"gpt-5\"]\n",
        "[fallback]\n\"chatgpt/*\" = [\"\"]\n",
        "[fallback]\n\"chatgpt/[\" = [\"openai/gpt-5\"]\n",
        "[fallback]\n\"chatgpt/*\" = \"openai/gpt-5\"\n",
    ] {
        assert!(fixture(bad, "").load().is_err(), "{bad}");
    }
    let message = fixture("[fallback]\n\"a/*\" = [\"b\"]\n", "")
        .load()
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("fallback") && message.contains("provider/model"),
        "{message}"
    );
}

// A cloned repository's chain would send a conversation to another provider, billed: it needs
// trust, as its roles do.
#[test]
fn a_projects_chains_need_trust() {
    let mut f = fixture(
        "[fallback]\n\"a/*\" = [\"b/x\"]\n",
        "[fallback]\n\"a/*\" = [\"evil/y\"]\n\"c/*\" = [\"d/z\"]\n",
    );
    let cfg = f.load().unwrap();
    assert_eq!(cfg.fallback["a/*"], ["b/x"]);
    assert!(!cfg.fallback.contains_key("c/*"));
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    assert!(cfg.warnings[0].contains("fallback"), "{:?}", cfg.warnings);
    let widening = config::project_widening(&f.global, &f.workspace).unwrap();
    assert_eq!(
        widening.items,
        [
            "fallback.\"a/*\" = [\"evil/y\"]",
            "fallback.\"c/*\" = [\"d/z\"]"
        ]
    );
    f.trust_now();
    let cfg = f.load().unwrap();
    assert_eq!(cfg.fallback["a/*"], ["evil/y"]);
    assert_eq!(cfg.fallback["c/*"], ["d/z"]);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}
