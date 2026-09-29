//! `[notifications]`: on by default, each of them can be turned off, and a project may set them
//! without trust.

use harness_config::{
    config::{self, Notifications},
    trust::TrustStore,
};

fn load(global: &str, project: Option<&str>) -> Result<config::Config, config::ConfigError> {
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("config.toml");
    std::fs::write(&global_file, global).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    if let Some(project) = project {
        std::fs::write(ws.join(".harness/config.toml"), project).unwrap();
    }
    let trust = TrustStore::load(&dir.path().join("data")).unwrap();
    config::load(&global_file, &ws, &trust)
}

#[test]
fn notifications_are_on_by_default() {
    let cfg = load("", None).unwrap();
    assert_eq!(
        cfg.notifications,
        Notifications {
            desktop: true,
            bell: true
        }
    );
}

#[test]
fn each_notification_can_be_turned_off() {
    let cfg = load("[notifications]\ndesktop = false\n", None).unwrap();
    assert_eq!(
        cfg.notifications,
        Notifications {
            desktop: false,
            bell: true
        }
    );
    let cfg = load("[notifications]\nbell = false\n", None).unwrap();
    assert!(cfg.notifications.desktop && !cfg.notifications.bell);
}

#[test]
fn a_project_sets_them_without_trust_or_a_warning() {
    let cfg = load(
        "[notifications]\nbell = false\n",
        Some("[notifications]\nbell = true\ndesktop = false\n"),
    )
    .unwrap();
    assert_eq!(
        cfg.notifications,
        Notifications {
            desktop: false,
            bell: true
        }
    );
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
    // They are not among the settings `harness trust` shows.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".harness")).unwrap();
    std::fs::write(
        dir.path().join(".harness/config.toml"),
        "[notifications]\ndesktop = false\n",
    )
    .unwrap();
    let widening = config::project_widening(&dir.path().join("none.toml"), dir.path()).unwrap();
    assert!(widening.items.is_empty(), "{:?}", widening.items);
}

#[test]
fn an_unknown_notification_setting_is_an_error() {
    let error = load("[notifications]\nsound = true\n", None).unwrap_err();
    assert!(error.to_string().contains("sound"), "{error}");
}
