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
    for keychain in [None, Some(Box::new(Refusing) as Box<dyn SecretStore>)] {
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
