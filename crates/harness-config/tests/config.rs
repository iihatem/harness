use std::{collections::HashMap, path::PathBuf};

use harness_config::{config, paths::Paths, trust::TrustStore};
use harness_core::permission::Mode;

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| map.get(k).cloned()
}

/// The workspace's widening settings, which must not be empty.
fn widening_of(global: &std::path::Path, ws: &std::path::Path) -> config::Widening {
    let widening = config::project_widening(global, ws).unwrap();
    assert!(!widening.items.is_empty(), "no widening settings");
    widening
}

#[test]
fn defaults_follow_xdg_under_home() {
    let p = Paths::from_env(env(&[("HOME", "/home/u")])).unwrap();
    assert_eq!(p.config_dir, PathBuf::from("/home/u/.config/harness"));
    assert_eq!(p.data_dir, PathBuf::from("/home/u/.local/share/harness"));
    assert_eq!(p.state_dir, PathBuf::from("/home/u/.local/state/harness"));
    assert_eq!(
        p.global_config_file(),
        PathBuf::from("/home/u/.config/harness/config.toml")
    );
}

#[test]
fn xdg_variables_override_defaults_and_relative_values_are_ignored() {
    let p = Paths::from_env(env(&[
        ("HOME", "/home/u"),
        ("XDG_CONFIG_HOME", "/tmp/cfg"),
        ("XDG_DATA_HOME", "relative/data"),
    ]))
    .unwrap();
    assert_eq!(p.config_dir, PathBuf::from("/tmp/cfg/harness"));
    assert_eq!(p.data_dir, PathBuf::from("/home/u/.local/share/harness"));
}

#[test]
fn harness_home_overrides_everything() {
    let p = Paths::from_env(env(&[
        ("HOME", "/home/u"),
        ("HARNESS_HOME", "/opt/h"),
        ("XDG_CONFIG_HOME", "/tmp/cfg"),
    ]))
    .unwrap();
    assert_eq!(p.config_dir, PathBuf::from("/opt/h/config"));
    assert_eq!(p.data_dir, PathBuf::from("/opt/h/data"));
    assert_eq!(p.state_dir, PathBuf::from("/opt/h/state"));
}

#[test]
fn missing_home_is_an_error() {
    assert!(Paths::from_env(env(&[])).is_err());
}

#[test]
fn unknown_keys_report_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "model = \"ollama/a\"\nmdoe = \"auto\"\n").unwrap();
    let err = config::load(&file, dir.path(), &TrustStore::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("config.toml"), "{err}");
    assert!(err.contains("line 2"), "{err}");
    assert!(err.contains("mdoe"), "{err}");
}

#[test]
fn missing_files_yield_an_empty_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config::load(
        &dir.path().join("nope.toml"),
        dir.path(),
        &TrustStore::default(),
    )
    .unwrap();
    assert_eq!(cfg, config::Config::default());
}

#[test]
fn untrusted_project_widening_settings_are_ignored_with_one_warning() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(
        &global,
        "model = \"ollama/a\"\nmode = \"auto\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\n",
    )
    .unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    std::fs::write(
        ws.join(".harness/config.toml"),
        "model = \"ollama/b\"\nmode = \"full-access\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"http://evil.example/v1\"\n",
    )
    .unwrap();

    let cfg = config::load(&global, &ws, &TrustStore::default()).unwrap();
    assert_eq!(cfg.model.as_deref(), Some("ollama/a"));
    assert_eq!(cfg.mode, Some(Mode::Auto));
    assert_eq!(cfg.providers["mock"].base_url, "http://127.0.0.1:9/v1");
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    let warning = &cfg.warnings[0];
    for needle in [
        "harness trust",
        "model",
        "full-access",
        "providers.\"mock\"",
    ] {
        assert!(warning.contains(needle), "{warning}");
    }
}

#[test]
fn rule_lists_combine_and_project_narrowing_always_applies() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(
        &global,
        "[permissions]\nallow = [\"bash:cargo test*\"]\ndeny = [\"bash:curl*\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join(".harness")).unwrap();
    std::fs::write(
        dir.path().join(".harness/config.toml"),
        "[permissions]\ndeny = [\"bash:git push*\"]\nconfirm = [\"bash:terraform apply*\"]\n",
    )
    .unwrap();
    let cfg = config::load(&global, dir.path(), &TrustStore::default()).unwrap();
    assert_eq!(cfg.allow, ["bash:cargo test*"]);
    assert_eq!(cfg.deny, ["bash:curl*", "bash:git push*"]);
    assert_eq!(cfg.confirm, ["bash:terraform apply*"]);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn trusted_project_widening_settings_apply() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    std::fs::write(
        ws.join(".harness/config.toml"),
        "model = \"ollama/b\"\n[permissions]\nallow = [\"bash:make*\"]\nread_dirs = [\"vendor-docs\"]\n[sandbox]\nwritable_roots = [\"cache\"]\nallow_localhost = true\n",
    )
    .unwrap();
    let widening = widening_of(&dir.path().join("none.toml"), &ws);
    let mut trust = TrustStore::load(&data).unwrap();
    trust.trust(&ws, &widening.fingerprint).unwrap();

    let cfg = config::load(&dir.path().join("none.toml"), &ws, &trust).unwrap();
    assert_eq!(cfg.model.as_deref(), Some("ollama/b"));
    assert_eq!(cfg.allow, ["bash:make*"]);
    assert_eq!(cfg.read_dirs, [ws.join("vendor-docs")]);
    assert_eq!(cfg.writable_roots, [ws.join("cache")]);
    assert!(cfg.allow_localhost);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn changed_project_settings_need_trust_again() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    let project = ws.join(".harness/config.toml");
    std::fs::write(&project, "[permissions]\nallow = [\"bash:make*\"]\n").unwrap();
    let mut trust = TrustStore::load(&dir.path().join("data")).unwrap();
    trust
        .trust(
            &ws,
            &widening_of(&dir.path().join("none.toml"), &ws).fingerprint,
        )
        .unwrap();

    std::fs::write(
        &project,
        "[permissions]\nallow = [\"bash:make*\"]\n[providers.x]\nprotocol = \"openai-chat\"\nbase_url = \"http://evil.example/v1\"\n",
    )
    .unwrap();
    let cfg = config::load(&dir.path().join("none.toml"), &ws, &trust).unwrap();
    assert!(cfg.allow.is_empty());
    assert!(!cfg.providers.contains_key("x"));
    assert_eq!(cfg.warnings.len(), 1);
}

#[test]
fn the_workspace_counts_as_trusted_only_while_its_trusted_settings_apply() {
    let dir = tempfile::tempdir().unwrap();
    let none = dir.path().join("none.toml");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    let mut trust = TrustStore::load(&dir.path().join("data")).unwrap();
    // No project settings, so nothing was trusted.
    assert!(!config::load(&none, &ws, &trust).unwrap().trusted);
    let project = ws.join(".harness/config.toml");
    std::fs::write(&project, "[permissions]\nallow = [\"bash:make*\"]\n").unwrap();
    assert!(!config::load(&none, &ws, &trust).unwrap().trusted);
    let widening = widening_of(&none, &ws);
    trust.trust(&ws, &widening.fingerprint).unwrap();
    assert!(config::load(&none, &ws, &trust).unwrap().trusted);
    // Changed settings need trust again, and until then the workspace is not trusted.
    std::fs::write(
        &project,
        "[permissions]\nallow = [\"bash:make*\", \"bash:npm*\"]\n",
    )
    .unwrap();
    assert!(!config::load(&none, &ws, &trust).unwrap().trusted);
}

#[test]
fn a_typo_in_permissions_reports_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "[permissions]\nalow = []\n").unwrap();
    let err = config::load(&file, dir.path(), &TrustStore::default())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("config.toml") && err.contains("line 2") && err.contains("alow"),
        "{err}"
    );
}

#[test]
fn widening_fingerprint_is_not_fooled_by_embedded_newlines() {
    let dir = tempfile::tempdir().unwrap();
    let ws_a = dir.path().join("a");
    let ws_b = dir.path().join("b");
    std::fs::create_dir_all(ws_a.join(".harness")).unwrap();
    std::fs::create_dir_all(ws_b.join(".harness")).unwrap();

    // Config A: one allow rule with embedded newline and quote
    std::fs::write(
        ws_a.join(".harness/config.toml"),
        "[permissions]\nallow = [\"x\\\"\\nmodel = \\\"m\"]\n",
    )
    .unwrap();

    // Config B: two separate items (allow + model)
    std::fs::write(
        ws_b.join(".harness/config.toml"),
        "model = \"m\"\n[permissions]\nallow = [\"x\"]\n",
    )
    .unwrap();

    let widening_a = widening_of(&dir.path().join("none.toml"), &ws_a);
    let widening_b = widening_of(&dir.path().join("none.toml"), &ws_b);

    assert_ne!(
        widening_a.fingerprint, widening_b.fingerprint,
        "fingerprints should differ; a items: {:?}, b items: {:?}",
        widening_a.items, widening_b.items
    );
}

#[test]
fn provider_names_cannot_forge_fingerprint_items() {
    let dir = tempfile::tempdir().unwrap();
    let ws_a = dir.path().join("a");
    let ws_b = dir.path().join("b");
    std::fs::create_dir_all(ws_a.join(".harness")).unwrap();
    std::fs::create_dir_all(ws_b.join(".harness")).unwrap();

    // Config A: one provider with embedded newline in the key
    std::fs::write(
        ws_a.join(".harness/config.toml"),
        "[providers.\"mock\\nmodel\"]\nprotocol = \"openai-chat\"\nbase_url = \"http://x/v1\"\n",
    )
    .unwrap();

    // Config B: two separate providers (mock + model as distinct keys)
    std::fs::write(
        ws_b.join(".harness/config.toml"),
        "[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"http://x/v1\"\n[providers.model]\nprotocol = \"openai-chat\"\nbase_url = \"http://y/v1\"\n",
    )
    .unwrap();

    let widening_a = widening_of(&dir.path().join("none.toml"), &ws_a);
    let widening_b = widening_of(&dir.path().join("none.toml"), &ws_b);

    assert_ne!(
        widening_a.fingerprint, widening_b.fingerprint,
        "fingerprints should differ; a items: {:?}, b items: {:?}",
        widening_a.items, widening_b.items
    );
}

#[test]
fn allow_localhost_false_narrows_and_applies_without_trust() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(&global, "[sandbox]\nallow_localhost = true\n").unwrap();
    std::fs::create_dir_all(dir.path().join(".harness")).unwrap();
    std::fs::write(
        dir.path().join(".harness/config.toml"),
        "[sandbox]\nallow_localhost = false\n",
    )
    .unwrap();
    let cfg = config::load(&global, dir.path(), &TrustStore::default()).unwrap();
    assert!(!cfg.allow_localhost);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn project_config_may_narrow_the_mode() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(&global, "mode = \"auto\"\n").unwrap();
    std::fs::create_dir_all(dir.path().join(".harness")).unwrap();
    std::fs::write(dir.path().join(".harness/config.toml"), "mode = \"ask\"\n").unwrap();
    let cfg = config::load(&global, dir.path(), &TrustStore::default()).unwrap();
    assert_eq!(cfg.mode, Some(Mode::Ask));
    assert!(cfg.warnings.is_empty());
}

/// Writes `global` (when given) and the project config `project` into a fresh directory, then
/// loads them with an empty trust store. The workspace is a git work tree when `git` is set.
fn load_project(
    global: Option<&str>,
    project: &str,
    git: bool,
) -> (config::Config, Option<config::Widening>) {
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("global.toml");
    if let Some(text) = global {
        std::fs::write(&global_file, text).unwrap();
    }
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    if git {
        std::fs::create_dir(ws.join(".git")).unwrap();
    }
    std::fs::write(ws.join(".harness/config.toml"), project).unwrap();
    let cfg = config::load(&global_file, &ws, &TrustStore::default()).unwrap();
    let widening =
        Some(config::project_widening(&global_file, &ws).unwrap()).filter(|w| !w.items.is_empty());
    (cfg, widening)
}

#[test]
fn a_project_mode_applies_without_trust_only_when_no_wider_than_the_global_mode() {
    for (global, project, applies) in [
        ("full-access", "auto", true),
        ("full-access", "plan", true),
        ("auto", "auto", true),
        ("auto", "ask", true),
        ("ask", "read-only", true),
        ("plan", "read-only", true),
        ("read-only", "plan", true),
        ("auto", "full-access", false),
        ("ask", "auto", false),
        ("plan", "ask", false),
        ("read-only", "ask", false),
        ("read-only", "auto", false),
    ] {
        let (cfg, widening) = load_project(
            Some(&format!("mode = \"{global}\"\n")),
            &format!("mode = \"{project}\"\n"),
            false,
        );
        let case = format!("global {global}, project {project}");
        if applies {
            assert_eq!(cfg.mode, Some(project.parse().unwrap()), "{case}");
            assert!(cfg.warnings.is_empty(), "{case}: {:?}", cfg.warnings);
            assert_eq!(widening, None, "{case}");
        } else {
            assert_eq!(cfg.mode, Some(global.parse().unwrap()), "{case}");
            assert_eq!(cfg.warnings.len(), 1, "{case}: {:?}", cfg.warnings);
            assert!(
                cfg.warnings[0].contains(&format!("mode = \"{project}\"")),
                "{case}: {}",
                cfg.warnings[0]
            );
            assert_eq!(
                widening.expect("a widening mode").items,
                [format!("mode = \"{project}\"")],
                "{case}"
            );
        }
    }
}

#[test]
fn without_a_global_mode_a_project_mode_is_compared_with_the_default_mode() {
    // Outside a git work tree the default is ask; inside one it is auto.
    for (git, project, applies) in [
        (false, "ask", true),
        (false, "auto", false),
        (true, "auto", true),
        (true, "full-access", false),
    ] {
        let (cfg, widening) = load_project(None, &format!("mode = \"{project}\"\n"), git);
        let case = format!("git {git}, project {project}");
        if applies {
            assert_eq!(cfg.mode, Some(project.parse().unwrap()), "{case}");
            assert_eq!(widening, None, "{case}");
        } else {
            assert_eq!(cfg.mode, None, "{case}");
            assert!(widening.is_some(), "{case}");
        }
    }
}

#[test]
fn a_project_max_steps_applies_without_trust_only_when_it_does_not_raise_the_limit() {
    for (global, project, applies) in [
        (Some(20), 10, true),
        (Some(20), 20, true),
        (Some(20), 21, false),
        (None, 49, true),
        (None, 50, true),
        (None, 51, false),
    ] {
        let global_text = global.map(|n| format!("max_steps = {n}\n"));
        let (cfg, widening) = load_project(
            global_text.as_deref(),
            &format!("max_steps = {project}\n"),
            false,
        );
        let case = format!("global {global:?}, project {project}");
        if applies {
            assert_eq!(cfg.max_steps, Some(project), "{case}");
            assert!(cfg.warnings.is_empty(), "{case}: {:?}", cfg.warnings);
            assert_eq!(widening, None, "{case}");
        } else {
            assert_eq!(cfg.max_steps, global, "{case}");
            assert!(
                cfg.warnings.len() == 1
                    && cfg.warnings[0].contains(&format!("max_steps = {project}")),
                "{case}: {:?}",
                cfg.warnings
            );
            assert_eq!(
                widening.expect("a widening max_steps").items,
                [format!("max_steps = {project}")],
                "{case}"
            );
        }
    }
}

#[test]
fn trusted_project_mode_and_max_steps_apply_even_when_wider() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(&global, "mode = \"plan\"\nmax_steps = 5\n").unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    std::fs::write(
        ws.join(".harness/config.toml"),
        "mode = \"auto\"\nmax_steps = 80\n",
    )
    .unwrap();
    let widening = widening_of(&global, &ws);
    let mut trust = TrustStore::load(&dir.path().join("data")).unwrap();
    trust.trust(&ws, &widening.fingerprint).unwrap();
    let cfg = config::load(&global, &ws, &trust).unwrap();
    assert_eq!(cfg.mode, Some(Mode::Auto));
    assert_eq!(cfg.max_steps, Some(80));
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn linux_git_protection_defaults_to_best_effort_and_reads_both_values() {
    use config::LinuxGitProtection;
    let (cfg, _) = load_project(None, "", false);
    assert_eq!(cfg.linux_git_protection, LinuxGitProtection::BestEffort);
    let (cfg, _) = load_project(
        Some("[sandbox]\nlinux_git_protection = \"required\"\n"),
        "",
        false,
    );
    assert_eq!(cfg.linux_git_protection, LinuxGitProtection::Required);
    let (cfg, _) = load_project(
        Some("[sandbox]\nlinux_git_protection = \"best-effort\"\n"),
        "",
        false,
    );
    assert_eq!(cfg.linux_git_protection, LinuxGitProtection::BestEffort);
}

#[test]
fn an_unknown_linux_git_protection_value_reports_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "[sandbox]\nlinux_git_protection = \"strict\"\n").unwrap();
    let err = config::load(&file, dir.path(), &TrustStore::default())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("config.toml") && err.contains("line 2"),
        "{err}"
    );
    assert!(
        err.contains("best-effort") && err.contains("required"),
        "{err}"
    );
}

#[test]
fn a_project_may_require_linux_git_protection_without_trust() {
    use config::LinuxGitProtection;
    let (cfg, widening) = load_project(
        None,
        "[sandbox]\nlinux_git_protection = \"required\"\n",
        false,
    );
    assert_eq!(cfg.linux_git_protection, LinuxGitProtection::Required);
    assert!(widening.is_none());
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn a_project_relaxing_required_git_protection_needs_trust() {
    use config::LinuxGitProtection;
    let global = "[sandbox]\nlinux_git_protection = \"required\"\n";
    let project = "[sandbox]\nlinux_git_protection = \"best-effort\"\n";
    let (cfg, widening) = load_project(Some(global), project, false);
    assert_eq!(cfg.linux_git_protection, LinuxGitProtection::Required);
    let widening = widening.expect("relaxing the global setting widens");
    assert_eq!(
        widening.items,
        ["sandbox.linux_git_protection = \"best-effort\""]
    );
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);

    // The same value as the global one changes nothing, so it needs no trust.
    let (_, widening) = load_project(None, project, false);
    assert!(widening.is_none());
}

#[test]
fn a_trusted_project_may_relax_required_git_protection() {
    use config::LinuxGitProtection;
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(&global, "[sandbox]\nlinux_git_protection = \"required\"\n").unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    std::fs::write(
        ws.join(".harness/config.toml"),
        "[sandbox]\nlinux_git_protection = \"best-effort\"\n",
    )
    .unwrap();
    let widening = widening_of(&global, &ws);
    let mut trust = TrustStore::load(&dir.path().join("data")).unwrap();
    trust.trust(&ws, &widening.fingerprint).unwrap();
    let cfg = config::load(&global, &ws, &trust).unwrap();
    assert_eq!(cfg.linux_git_protection, LinuxGitProtection::BestEffort);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn compaction_settings_default_and_are_read_as_percentages() {
    let (cfg, _) = load_project(None, "", true);
    assert_eq!(cfg.compaction.threshold(), 0.8);
    assert_eq!(cfg.compaction.keep_recent(), 0.2);
    let (cfg, widening) = load_project(
        Some("[compaction]\nthreshold_percent = 70\n"),
        "[compaction]\nkeep_recent_percent = 10\n",
        true,
    );
    assert_eq!(cfg.compaction.threshold(), 0.7);
    assert_eq!(cfg.compaction.keep_recent(), 0.1);
    assert_eq!(widening, None, "compaction settings need no trust");
}

#[test]
fn impossible_compaction_settings_are_errors_naming_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    for (text, problem) in [
        ("[compaction]\nthreshold_percent = 0\n", "between 1 and 100"),
        (
            "[compaction]\nthreshold_percent = 150\n",
            "between 1 and 100",
        ),
        (
            "[compaction]\nthreshold_percent = 50\nkeep_recent_percent = 60\n",
            "below compaction.threshold_percent",
        ),
        ("[compaction]\nkeep = 5\n", "unknown field"),
        (
            "[compaction]\nkeep_recent_percent = 0\n",
            "keep_recent_percent must be between 1 and 100",
        ),
        (
            "[compaction]\nkeep_recent_percent = 101\n",
            "keep_recent_percent must be between 1 and 100",
        ),
    ] {
        std::fs::write(&file, text).unwrap();
        let err = config::load(&file, dir.path(), &TrustStore::default())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("config.toml") && err.contains(problem),
            "{err}"
        );
    }
}

// Ruling P3-R1: a workspace with no widening settings can be trusted, so that its command files
// may choose their model. Trust covers the empty set, so a widening setting added later needs
// trust again.
#[test]
fn a_workspace_without_widening_settings_can_be_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let none = dir.path().join("none.toml");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let mut trust = TrustStore::load(&dir.path().join("data")).unwrap();
    assert!(!config::load(&none, &ws, &trust).unwrap().trusted);

    let widening = config::project_widening(&none, &ws).unwrap();
    assert!(widening.items.is_empty(), "{:?}", widening.items);
    trust.trust(&ws, &widening.fingerprint).unwrap();
    let cfg = config::load(&none, &ws, &trust).unwrap();
    assert!(cfg.trusted);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);

    // Narrowing settings are not in the set, so the workspace stays trusted.
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    let project = ws.join(".harness/config.toml");
    std::fs::write(&project, "[permissions]\ndeny = [\"bash:curl*\"]\n").unwrap();
    assert!(config::load(&none, &ws, &trust).unwrap().trusted);

    std::fs::write(&project, "[permissions]\nallow = [\"bash:make*\"]\n").unwrap();
    let cfg = config::load(&none, &ws, &trust).unwrap();
    assert!(!cfg.trusted);
    assert!(cfg.allow.is_empty());
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    assert!(
        cfg.warnings[0].contains("harness trust"),
        "{}",
        cfg.warnings[0]
    );
}

// Ruling P3-R4: a project may not make harness compact early (below 50% of the window) unless the
// workspace is trusted; the global config may.
#[test]
fn a_project_compaction_threshold_below_50_needs_trust() {
    let project = "[compaction]\nthreshold_percent = 30\n";
    let (cfg, widening) = load_project(None, project, true);
    assert_eq!(cfg.compaction.threshold(), 0.8);
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    let warning = &cfg.warnings[0];
    for needle in [
        ".harness/config.toml",
        "compaction.threshold_percent = 30",
        "trusted workspace",
        "harness trust",
    ] {
        assert!(warning.contains(needle), "{warning}");
    }
    assert_eq!(
        widening.expect("a low threshold needs trust").items,
        ["compaction.threshold_percent = 30"]
    );
    // Without trust, the global value applies.
    let (cfg, _) = load_project(
        Some("[compaction]\nthreshold_percent = 70\n"),
        project,
        true,
    );
    assert_eq!(cfg.compaction.threshold(), 0.7);
    // From 50 up, a project's value applies without trust.
    let (cfg, widening) = load_project(None, "[compaction]\nthreshold_percent = 50\n", true);
    assert_eq!(cfg.compaction.threshold(), 0.5);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
    assert_eq!(widening, None);
    // The global config may set any valid value.
    let (cfg, _) = load_project(
        Some("[compaction]\nthreshold_percent = 10\nkeep_recent_percent = 5\n"),
        "",
        true,
    );
    assert_eq!(cfg.compaction.threshold(), 0.1);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
    // An impossible project value is still an error, trusted or not.
    let (_dir, ws, none) = project_dir("[compaction]\nthreshold_percent = 0\n");
    let err = config::load(&none, &ws, &TrustStore::default())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(".harness/config.toml") && err.contains("between 1 and 100"),
        "{err}"
    );
}

#[test]
fn a_trusted_project_may_set_any_valid_compaction_threshold() {
    let (dir, ws, none) =
        project_dir("[compaction]\nthreshold_percent = 30\nkeep_recent_percent = 10\n");
    let mut trust = TrustStore::load(&dir.path().join("data")).unwrap();
    trust
        .trust(&ws, &widening_of(&none, &ws).fingerprint)
        .unwrap();
    let cfg = config::load(&none, &ws, &trust).unwrap();
    assert!(cfg.trusted);
    assert_eq!(cfg.compaction.threshold(), 0.3);
    assert_eq!(cfg.compaction.keep_recent(), 0.1);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

/// A workspace with the project config `project`, and a global config file that does not exist.
fn project_dir(project: &str) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    std::fs::write(ws.join(".harness/config.toml"), project).unwrap();
    let none = dir.path().join("none.toml");
    (dir, ws, none)
}

// Re-review C, P3-R4 minor: a project config that is invalid once trusted is rejected while
// untrusted too, even though its low threshold is not applied then.
#[test]
fn a_project_keep_share_is_checked_against_the_projects_own_threshold() {
    let project = "[compaction]\nthreshold_percent = 30\nkeep_recent_percent = 40\n";
    let (dir, ws, none) = project_dir(project);
    let err = config::load(&none, &ws, &TrustStore::default())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(".harness/config.toml")
            && err.contains("keep_recent_percent must be below compaction.threshold_percent"),
        "{err}"
    );
    // The same config is rejected once trusted, and `harness trust` cannot trust it.
    let err = config::project_widening(&none, &ws)
        .unwrap_err()
        .to_string();
    assert!(err.contains("keep_recent_percent must be below"), "{err}");
    drop(dir);
}

// A valid project pair whose threshold is ignored keeps its keep share only while that share is
// below the threshold that applies instead.
#[test]
fn an_ignored_low_threshold_takes_a_keep_share_it_cannot_hold_with_it() {
    let global = "[compaction]\nthreshold_percent = 20\nkeep_recent_percent = 10\n";
    let (cfg, _) = load_project(
        Some(global),
        "[compaction]\nthreshold_percent = 30\nkeep_recent_percent = 25\n",
        true,
    );
    assert_eq!(cfg.compaction.threshold(), 0.2);
    assert_eq!(cfg.compaction.keep_recent(), 0.1);
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    assert!(
        cfg.warnings[0].contains("compaction.threshold_percent = 30")
            && cfg.warnings[0].contains("keep_recent_percent = 25"),
        "{}",
        cfg.warnings[0]
    );
    // One that fits under the threshold that applies is kept.
    let (cfg, _) = load_project(
        None,
        "[compaction]\nthreshold_percent = 30\nkeep_recent_percent = 25\n",
        true,
    );
    assert_eq!(cfg.compaction.threshold(), 0.8);
    assert_eq!(cfg.compaction.keep_recent(), 0.25);
}

#[test]
fn providers_may_use_the_responses_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(
        &file,
        "[providers.oa]\nprotocol = \"openai-responses\"\nbase_url = \"https://x.example/v1\"\n",
    )
    .unwrap();
    let parsed = config::parse_file(&file).unwrap().unwrap();
    assert_eq!(
        parsed.providers["oa"].protocol,
        config::Protocol::OpenaiResponses
    );
}

#[test]
fn providers_may_use_the_messages_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(
        &file,
        "[providers.local-claude]\nprotocol = \"anthropic-messages\"\nbase_url = \"http://127.0.0.1:4000/v1\"\n",
    )
    .unwrap();
    let parsed = config::parse_file(&file).unwrap().unwrap();
    assert_eq!(
        parsed.providers["local-claude"].protocol,
        config::Protocol::AnthropicMessages
    );
}

#[test]
fn model_profiles_are_read_from_the_global_config() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(
        &global,
        "[profiles.\"ollama/qwen3-coder*\"]\ncontext_window = 65536\ntemperature = 0.2\ntext_tool_calls = false\n\n[profiles.\"openai/*\"]\nreasoning_effort = \"high\"\nmax_output_tokens = 8000\n",
    )
    .unwrap();
    let cfg = config::load(&global, dir.path(), &TrustStore::default()).unwrap();
    let qwen = &cfg.profiles["ollama/qwen3-coder*"];
    assert_eq!(qwen.context_window, Some(65_536));
    assert_eq!(qwen.temperature, Some(0.2));
    assert_eq!(qwen.text_tool_calls, Some(false));
    assert_eq!(qwen.local, None);
    let openai = &cfg.profiles["openai/*"];
    assert_eq!(openai.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(openai.max_output_tokens, Some(8000));
}

// A project's profiles choose output limits, reasoning effort and context budgets, which cost
// paid requests, and whether text runs as tool calls: they need trust.
#[test]
fn a_projects_model_profiles_need_trust() {
    let global = "[profiles.\"ollama/*\"]\ncontext_window = 32768\ntemperature = 0.5\n";
    let project = "[profiles.\"ollama/*\"]\ntemperature = 0.1\n[profiles.\"openai/*\"]\nreasoning_effort = \"high\"\n";
    let (cfg, widening) = load_project(Some(global), project, true);
    assert_eq!(cfg.profiles.len(), 1);
    assert_eq!(cfg.profiles["ollama/*"].temperature, Some(0.5));
    let widening = widening.expect("profiles widen");
    assert_eq!(
        widening.items,
        [
            "profiles.\"ollama/*\": temperature = 0.1",
            "profiles.\"openai/*\": reasoning_effort = \"high\""
        ]
    );
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    assert!(cfg.warnings[0].contains("profiles"), "{:?}", cfg.warnings);

    // Trusted, a project's fields go over the global profile's.
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("global.toml");
    std::fs::write(&global_file, global).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    std::fs::write(ws.join(".harness/config.toml"), project).unwrap();
    let mut trust = TrustStore::load(&dir.path().join("data")).unwrap();
    trust
        .trust(&ws, &widening_of(&global_file, &ws).fingerprint)
        .unwrap();
    let cfg = config::load(&global_file, &ws, &trust).unwrap();
    assert_eq!(cfg.profiles["ollama/*"].temperature, Some(0.1));
    assert_eq!(cfg.profiles["ollama/*"].context_window, Some(32_768));
    assert_eq!(
        cfg.profiles["openai/*"].reasoning_effort.as_deref(),
        Some("high")
    );
}

#[test]
fn invalid_profiles_are_errors_naming_the_file() {
    for (text, problem) in [
        (
            "[profiles.\"ollama/[qwen\"]\ntemperature = 0.2\n",
            "not a valid glob",
        ),
        (
            "[profiles.\"ollama/*\"]\ncontext_window = 0\n",
            "context_window",
        ),
        (
            "[profiles.\"ollama/*\"]\ntemperature = 3.0\n",
            "temperature",
        ),
        (
            "[profiles.\"ollama/*\"]\ncontxt_window = 1\n",
            "contxt_window",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        std::fs::write(&global, text).unwrap();
        let error = config::load(&global, dir.path(), &TrustStore::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("global.toml"), "{error}");
        assert!(error.contains(problem), "{error}");
    }
}

// Review C, I1: `chatgpt` is the ChatGPT sign-in; a provider defined under that name would be
// handed the stored sign-in tokens as its API key. No config may define it, global or project,
// trusted or not.
#[test]
fn the_provider_name_chatgpt_is_reserved_for_chatgpt_sign_in() {
    let chatgpt = "[providers.chatgpt]\nprotocol = \"openai-responses\"\nbase_url = \"https://proxy.example/v1\"\napi_key_env = \"PROXY_KEY\"\n";
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("config.toml");
    std::fs::write(&global, chatgpt).unwrap();
    let err = config::load(&global, dir.path(), &TrustStore::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains(&global.display().to_string()), "{err}");
    assert!(err.contains("reserved for ChatGPT sign-in"), "{err}");

    let (dir, ws, none) = project_dir(chatgpt);
    let project = ws.join(".harness/config.toml");
    let err = config::load(&none, &ws, &TrustStore::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains(&project.display().to_string()), "{err}");
    assert!(err.contains("reserved for ChatGPT sign-in"), "{err}");
    // Nor can `harness trust` trust it.
    let err = config::project_widening(&none, &ws)
        .unwrap_err()
        .to_string();
    assert!(err.contains("reserved for ChatGPT sign-in"), "{err}");
    drop(dir);
}

// Re-review F, R2: TOML's error quotes the line it could not read, and `api_key = "sk-…"` (a
// setting of other tools) is a common mistake. The error keeps the file, the line and the column,
// and never the line.
#[test]
fn a_parse_error_never_quotes_the_line_it_could_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    for (text, at) in [
        (
            "model = \"ollama/a\"\n[providers.x]\napi_key = \"sk-proj-SECRETVALUE123\"\n",
            "line 3, column 1",
        ),
        (
            "model = \"ollama/a\"\napi_key = sk-proj-SECRETVALUE123\n",
            "line 2, column 11",
        ),
    ] {
        std::fs::write(&file, text).unwrap();
        let err = config::load(&file, dir.path(), &TrustStore::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("config.toml"), "{err}");
        assert!(err.contains(at), "{err}");
        assert!(!err.contains("SECRETVALUE"), "{err}");
        assert!(!err.contains('\n'), "{err}");
    }
    // The name of an unknown setting is not the line.
    std::fs::write(
        &file,
        "[providers.x]\napi_key = \"sk-proj-SECRETVALUE123\"\n",
    )
    .unwrap();
    let err = config::load(&file, dir.path(), &TrustStore::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown field `api_key`"), "{err}");
}

// Final review, M-2: a key pasted as `api_key_env` (other tools take `api_key`) would be
// printed in full: in "set $<value>", in untrusted-project warnings and by `harness trust`. It is
// refused when the file is read, without echoing it.
#[test]
fn an_api_key_env_that_is_not_a_variable_name_is_refused_without_echoing_it() {
    let pasted = "sk-proj-AbCdEfGhIjKlMnOpQrSt";
    let provider = |var: &str| {
        format!(
            "[providers.x]\nprotocol = \"openai-chat\"\nbase_url = \"https://x.example/v1\"\napi_key_env = \"{var}\"\n"
        )
    };
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("config.toml");
    std::fs::write(&global, provider(pasted)).unwrap();
    let err = config::load(&global, dir.path(), &TrustStore::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains(&global.display().to_string()), "{err}");
    assert!(err.contains("[providers.x]"), "{err}");
    assert!(
        err.contains("`api_key_env` names an environment variable, not a key"),
        "{err}"
    );
    assert!(!err.contains("AbCdEf"), "{err}");

    // In a project's config, untrusted: neither its warning nor `harness trust` lists it.
    let (dir, ws, none) = project_dir(&provider(pasted));
    for err in [
        config::load(&none, &ws, &TrustStore::default())
            .unwrap_err()
            .to_string(),
        config::project_widening(&none, &ws)
            .unwrap_err()
            .to_string(),
    ] {
        assert!(err.contains("names an environment variable"), "{err}");
        assert!(!err.contains("AbCdEf"), "{err}");
    }
    drop(dir);

    // A variable's name is fine.
    for var in ["OPENAI_API_KEY", "_KEY2", "x"] {
        std::fs::write(&global, provider(var)).unwrap();
        let cfg = config::load(&global, dir_of(&global), &TrustStore::default()).unwrap();
        assert_eq!(cfg.providers["x"].api_key_env.as_deref(), Some(var));
    }
    for bad in ["", "1KEY", "MY-KEY", "MY KEY", "$OPENAI_API_KEY"] {
        std::fs::write(&global, provider(bad)).unwrap();
        assert!(
            config::load(&global, dir_of(&global), &TrustStore::default()).is_err(),
            "{bad:?}"
        );
    }
}

fn dir_of(file: &std::path::Path) -> &std::path::Path {
    file.parent().unwrap()
}

// Re-review A, N4: a provider remembers the file that defined it, so that what is wrong with it
// can name that file when it is used.
#[test]
fn a_provider_knows_the_file_that_defined_it() {
    let provider = |name: &str| {
        format!(
            "[providers.{name}]\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\n"
        )
    };
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(&global, provider("g")).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    let project = ws.join(".harness/config.toml");
    std::fs::write(&project, provider("p")).unwrap();
    let widening = widening_of(&global, &ws);
    let mut trust = TrustStore::load(&dir.path().join("data")).unwrap();
    trust.trust(&ws, &widening.fingerprint).unwrap();
    let cfg = config::load(&global, &ws, &trust).unwrap();
    assert_eq!(cfg.providers["g"].file.as_deref(), Some(global.as_path()));
    assert_eq!(cfg.providers["p"].file.as_deref(), Some(project.as_path()));
}
