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

#[test]
fn malformed_trust_store_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("trust.toml"), "invalid toml ][}{").unwrap();
    let err = TrustStore::load(&data).unwrap_err().to_string();
    assert!(err.contains("trust.toml"), "{err}");
}

// Final review, important 2: the data directory holds sessions, checkpoints and the trust list,
// so when harness makes it, only the user may read it, whichever part of harness makes it first.
#[test]
fn a_data_directory_made_for_the_trust_list_is_private() {
    unsafe { libc::umask(0o022) };
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("share/harness");
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    TrustStore::load(&data).unwrap().trust(&ws, "fp").unwrap();
    let mode = std::fs::metadata(&data).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
}
