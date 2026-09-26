use std::os::unix::fs::PermissionsExt;

use harness_config::trust::TrustStore;

#[test]
fn trust_round_trips_through_disk_and_can_be_revoked() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();

    let mut store = TrustStore::load(&data).unwrap();
    assert!(!store.is_trusted(&ws, "abc"));
    store.trust(&ws, "abc").unwrap();

    let reloaded = TrustStore::load(&data).unwrap();
    assert!(reloaded.is_trusted(&ws, "abc"));
    assert!(!reloaded.is_trusted(&ws, "other"));
    let mode = std::fs::metadata(data.join("trust.toml"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    let mut store = reloaded;
    assert!(store.revoke(&ws).unwrap());
    assert!(!store.revoke(&ws).unwrap());
    assert!(!TrustStore::load(&data).unwrap().is_trusted(&ws, "abc"));
}

#[test]
fn trust_is_keyed_by_the_canonical_path() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&ws, &alias).unwrap();
    let mut store = TrustStore::load(&dir.path().join("data")).unwrap();
    store.trust(&alias, "fp").unwrap();
    assert!(store.is_trusted(&ws, "fp"));
}
