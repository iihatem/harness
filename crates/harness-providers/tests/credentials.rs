//! The credential store: keychain entries (through keyring-core's mock store, never the user's
//! real keychain), the `0600` file fallback, and account profiles.

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use harness_providers::credentials::{
    CredentialError, Credentials, DEFAULT_PROFILE, FileStore, KeychainStore, SecretStore,
};

fn mock_keychain() -> KeychainStore {
    KeychainStore::with_store(
        keyring_core::mock::Store::new().unwrap(),
        "the test keychain",
    )
}

/// A keychain that is there but refuses every operation, as a Secret Service without an
/// unlocked collection does.
struct Refusing;

impl SecretStore for Refusing {
    fn get(&self, _account: &str) -> Result<Option<String>, CredentialError> {
        Err(CredentialError::Keychain("the collection is locked".into()))
    }
    fn set(&self, _account: &str, _secret: &str) -> Result<(), CredentialError> {
        Err(CredentialError::Keychain("the collection is locked".into()))
    }
    fn delete(&self, _account: &str) -> Result<bool, CredentialError> {
        Err(CredentialError::Keychain("the collection is locked".into()))
    }
    fn describe(&self) -> String {
        "a locked keychain".into()
    }
}

/// A keychain that can be read, and can remove what it holds, but refuses to store anything:
/// a macOS keychain whose access prompt was denied for the write, say.
#[derive(Clone)]
struct ReadOnly {
    entries: Arc<Mutex<Vec<(String, String)>>>,
    /// Whether it refuses to delete too.
    keeps: bool,
}

impl ReadOnly {
    fn holding(entries: &[(&str, &str)]) -> ReadOnly {
        ReadOnly {
            entries: Arc::new(Mutex::new(
                entries
                    .iter()
                    .map(|(a, s)| (a.to_string(), s.to_string()))
                    .collect(),
            )),
            keeps: false,
        }
    }
}

impl SecretStore for ReadOnly {
    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        let entries = self.entries.lock().unwrap();
        Ok(entries
            .iter()
            .find(|(a, _)| a == account)
            .map(|(_, s)| s.clone()))
    }
    fn set(&self, _account: &str, _secret: &str) -> Result<(), CredentialError> {
        Err(CredentialError::Keychain("the write was denied".into()))
    }
    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        if self.keeps {
            return Err(CredentialError::Keychain("the delete was denied".into()));
        }
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();
        entries.retain(|(a, _)| a != account);
        Ok(entries.len() != before)
    }
    fn describe(&self) -> String {
        "a read-only keychain".into()
    }
}

/// Records what reaches it, to check what goes to the keychain.
#[derive(Clone, Default)]
struct Recording(Arc<Mutex<Vec<(String, String)>>>);

impl SecretStore for Recording {
    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        let entries = self.0.lock().unwrap();
        Ok(entries
            .iter()
            .rev()
            .find(|(a, _)| a == account)
            .map(|(_, s)| s.clone()))
    }
    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        self.0
            .lock()
            .unwrap()
            .push((account.to_string(), secret.to_string()));
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        let mut entries = self.0.lock().unwrap();
        let before = entries.len();
        entries.retain(|(a, _)| a != account);
        Ok(entries.len() != before)
    }
    fn describe(&self) -> String {
        "the recording keychain".into()
    }
}

#[test]
fn keychain_entries_are_named_by_provider_and_profile() {
    let keychain = mock_keychain();
    assert_eq!(keychain.get("openai/default").unwrap(), None);
    keychain.set("openai/default", "sk-1").unwrap();
    assert_eq!(
        keychain.get("openai/default").unwrap().as_deref(),
        Some("sk-1")
    );
    assert_eq!(keychain.get("openai/work").unwrap(), None);
    assert!(keychain.delete("openai/default").unwrap());
    assert!(!keychain.delete("openai/default").unwrap());
    assert_eq!(keychain.get("openai/default").unwrap(), None);
}

#[test]
fn keys_go_to_the_keychain_and_never_to_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = Recording::default();
    let creds = Credentials::with_keychain(dir.path(), Some(Box::new(keychain.clone())));
    let stored = creds
        .set("openai", DEFAULT_PROFILE, "sk-in-keychain")
        .unwrap();
    assert_eq!(stored, "the recording keychain");
    assert!(creds.take_warnings().is_empty());
    assert_eq!(
        keychain.0.lock().unwrap().as_slice(),
        [("openai/default".to_string(), "sk-in-keychain".to_string())]
    );
    assert!(!dir.path().join("credentials.json").exists());
    assert_eq!(
        creds.active("openai").unwrap().as_deref(),
        Some("sk-in-keychain")
    );
}

// Spec: "No keychain on a headless Linux host".
#[test]
fn without_a_keychain_keys_go_to_a_private_file_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let empty_read_only = ReadOnly::holding(&[]);
    for keychain in [
        None,
        Some(Box::new(empty_read_only) as Box<dyn SecretStore>),
    ] {
        let creds = Credentials::with_keychain(&data, keychain);
        let stored = creds.set("openrouter", DEFAULT_PROFILE, "sk-or-1").unwrap();
        let file = data.join("credentials.json");
        assert_eq!(stored, file.display().to_string());
        let warnings = creds.take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("no keychain"), "{warnings:?}");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            creds.active("openrouter").unwrap().as_deref(),
            Some("sk-or-1")
        );
        std::fs::remove_file(file).unwrap();
    }
}

#[test]
fn a_key_stored_in_the_file_is_found_once_a_keychain_exists() {
    let dir = tempfile::tempdir().unwrap();
    Credentials::with_keychain(dir.path(), None)
        .set("openai", "work", "sk-file")
        .unwrap();
    let creds = Credentials::with_keychain(dir.path(), Some(Box::new(mock_keychain())));
    assert_eq!(
        creds.get("openai", "work").unwrap().as_deref(),
        Some("sk-file")
    );
    // Removing it removes it everywhere.
    assert!(creds.remove("openai", "work").unwrap());
    assert_eq!(creds.get("openai", "work").unwrap(), None);
    assert!(!creds.remove("openai", "work").unwrap());
}

#[test]
fn the_file_store_keeps_several_entries_and_rewrites_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path());
    store.set("a/default", "1").unwrap();
    store.set("b/default", "2").unwrap();
    store.set("a/default", "3").unwrap();
    assert_eq!(store.get("a/default").unwrap().as_deref(), Some("3"));
    assert_eq!(store.get("b/default").unwrap().as_deref(), Some("2"));
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.contains("tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

// Review Focus: a credentials file someone made readable by others is made private again.
#[test]
fn a_readable_credentials_file_is_made_private_again() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("credentials.json");
    std::fs::write(&file, r#"{"credentials":{"openai/default":"sk-1"}}"#).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let store = FileStore::new(dir.path());
    assert_eq!(
        store.get("openai/default").unwrap().as_deref(),
        Some("sk-1")
    );
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_damaged_credentials_file_is_an_error_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("credentials.json"), "{not json").unwrap();
    let error = FileStore::new(dir.path())
        .get("openai/default")
        .unwrap_err();
    assert!(error.to_string().contains("credentials.json"), "{error}");
}

// Spec: "Switching ChatGPT accounts".
#[test]
fn the_active_profile_is_remembered_per_provider() {
    let dir = tempfile::tempdir().unwrap();
    let creds = Credentials::with_keychain(dir.path(), Some(Box::new(mock_keychain())));
    creds.set("openai", DEFAULT_PROFILE, "sk-personal").unwrap();
    creds.set("openai", "work", "sk-work").unwrap();
    assert_eq!(creds.active_profile("openai").unwrap(), "default");
    creds.use_profile("openai", "work").unwrap();
    assert_eq!(creds.active_profile("anthropic").unwrap(), "default");
    // A new process reads the choice back.
    let reopened = Credentials::with_keychain(dir.path(), None);
    assert_eq!(reopened.active_profile("openai").unwrap(), "work");
    assert_eq!(creds.active("openai").unwrap().as_deref(), Some("sk-work"));
    let accounts = std::fs::read_to_string(dir.path().join("accounts.toml")).unwrap();
    assert!(!accounts.contains("sk-"), "{accounts}");
}

#[test]
fn provider_and_profile_names_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let creds = Credentials::with_keychain(dir.path(), None);
    for (provider, profile) in [("open/ai", "default"), ("openai", "../x"), ("openai", "")] {
        assert!(
            matches!(
                creds.set(provider, profile, "k"),
                Err(CredentialError::BadName { .. })
            ),
            "{provider} {profile}"
        );
    }
    assert!(creds.use_profile("openai", "a b").is_err());
}

#[test]
fn the_file_store_can_be_chosen_with_an_environment_variable() {
    let dir = tempfile::tempdir().unwrap();
    let creds = Credentials::open(dir.path(), |var| {
        (var == "HARNESS_CREDENTIAL_STORE").then(|| "file".to_string())
    });
    creds.set("openai", DEFAULT_PROFILE, "sk-1").unwrap();
    assert!(dir.path().join("credentials.json").exists());
    // Chosen, so no warning.
    assert!(creds.take_warnings().is_empty());
}

// Review B, I1 (and C, I4): a keychain that refuses the new key must not go on serving the old
// one, which it would shadow the file copy with.
#[test]
fn a_key_the_keychain_refuses_replaces_the_one_it_held() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = ReadOnly::holding(&[("openai/default", "sk-OLD-leaked")]);
    let creds = Credentials::with_keychain(dir.path(), Some(Box::new(keychain.clone())));
    let place = creds
        .set("openai", DEFAULT_PROFILE, "sk-NEW-rotated")
        .unwrap();
    assert_eq!(
        place,
        dir.path().join("credentials.json").display().to_string()
    );
    assert_eq!(
        creds.active("openai").unwrap().as_deref(),
        Some("sk-NEW-rotated")
    );
    let warnings = creds.take_warnings();
    assert!(
        warnings.iter().any(|w| w.contains("credentials.json")),
        "{warnings:?}"
    );
    assert_eq!(keychain.get("openai/default").unwrap(), None);
}

// When the old key cannot be removed either, storing the new one would change nothing: that is
// an error, and nothing is written.
#[test]
fn a_keychain_that_keeps_an_old_key_it_cannot_replace_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut keychain = ReadOnly::holding(&[("openai/default", "sk-OLD-leaked")]);
    keychain.keeps = true;
    let creds = Credentials::with_keychain(dir.path(), Some(Box::new(keychain)));
    let error = creds
        .set("openai", DEFAULT_PROFILE, "sk-NEW-rotated")
        .unwrap_err()
        .to_string();
    assert!(error.contains("older"), "{error}");
    assert!(error.contains("HARNESS_CREDENTIAL_STORE=file"), "{error}");
    assert!(!error.contains("sk-"), "{error}");
    assert!(!dir.path().join("credentials.json").exists());
}

// Review B, I1: once the key is in the keychain, a copy left in the file that cannot be removed
// is worth a warning.
#[test]
fn an_older_file_copy_that_cannot_be_removed_is_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    Credentials::with_keychain(&data, None)
        .set("openai", DEFAULT_PROFILE, "sk-in-file")
        .unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o500)).unwrap();
    let creds = Credentials::with_keychain(&data, Some(Box::new(Recording::default())));
    let stored = creds.set("openai", DEFAULT_PROFILE, "sk-in-keychain");
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(stored.unwrap(), "the recording keychain");
    let warnings = creds.take_warnings();
    assert!(
        warnings.iter().any(|w| w.contains("credentials.json")),
        "{warnings:?}"
    );
}

// Review B, I2: `logout` must not report success while the keychain keeps the key.
#[test]
fn a_keychain_that_refuses_to_remove_a_key_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    Credentials::with_keychain(dir.path(), None)
        .set("openai", DEFAULT_PROFILE, "sk-file")
        .unwrap();
    let creds = Credentials::with_keychain(dir.path(), Some(Box::new(Refusing)));
    let error = creds.remove("openai", DEFAULT_PROFILE).unwrap_err();
    assert!(error.to_string().contains("locked"), "{error}");
    // The copy in the file is gone all the same.
    let file_only = Credentials::with_keychain(dir.path(), None);
    assert_eq!(file_only.get("openai", DEFAULT_PROFILE).unwrap(), None);
}

// Review B, M9: a keychain that cannot be read is reported, not taken for an empty one.
#[test]
fn a_keychain_that_cannot_be_read_is_an_error_unless_the_file_has_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let creds = Credentials::with_keychain(dir.path(), Some(Box::new(Refusing)));
    let error = creds.get("openai", DEFAULT_PROFILE).unwrap_err();
    assert!(error.to_string().contains("locked"), "{error}");
    Credentials::with_keychain(dir.path(), None)
        .set("openai", DEFAULT_PROFILE, "sk-file")
        .unwrap();
    assert_eq!(
        creds.get("openai", DEFAULT_PROFILE).unwrap().as_deref(),
        Some("sk-file")
    );
    let warnings = creds.take_warnings();
    assert!(
        warnings.iter().any(|w| w.contains("locked")),
        "{warnings:?}"
    );
}
