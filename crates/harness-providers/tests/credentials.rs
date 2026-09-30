//! The credential store: keychain entries (through keyring-core's mock store, never the user's
//! real keychain), the `0600` file fallback, and account profiles.

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use harness_providers::credentials::{
    CredentialError, Credentials, DEFAULT_PROFILE, FileStore, KeychainStore, SecretStore,
    StoreChoice, TimeLimited,
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
    assert_eq!(stored.place, "the recording keychain");
    assert!(!stored.stale_file_copy);
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
        assert_eq!(stored.place, file.display().to_string());
        assert!(!stored.stale_file_copy);
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

// `Credentials::open` reaches the platform keychain unless the test hook, which release builds
// ignore, leaves it out: storing would remove the real keychain's entry.
#[cfg(debug_assertions)]
#[test]
fn the_file_store_can_be_chosen_with_an_environment_variable() {
    let dir = tempfile::tempdir().unwrap();
    // The test hook leaves the real keychain out.
    let creds = Credentials::open(dir.path(), |var| match var {
        "HARNESS_CREDENTIAL_STORE" => Some("file".to_string()),
        "HARNESS_TEST_NO_KEYCHAIN" => Some("1".to_string()),
        _ => None,
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
    let stored = creds
        .set("openai", DEFAULT_PROFILE, "sk-NEW-rotated")
        .unwrap();
    assert_eq!(
        stored.place,
        dir.path().join("credentials.json").display().to_string()
    );
    assert!(!stored.stale_file_copy);
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
    // Re-review B+C, R1: set for one command only, the older key would win again after it.
    assert!(error.contains("keep it set"), "{error}");
    assert!(error.contains("reads the keychain first"), "{error}");
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
    let stored = stored.unwrap();
    assert_eq!(stored.place, "the recording keychain");
    // Final review, wave 5 re-review R1: the caller (`harness auth add`, `harness login`) is
    // told the credential is not yet replaced everywhere it is read from, and must fail loudly.
    assert!(stored.stale_file_copy);
    let warnings = creds.take_warnings();
    assert!(
        warnings.iter().any(|w| w.contains("credentials.json")),
        "{warnings:?}"
    );
}

// Final review, wave 5 re-review R1 (probe c): after a keychain store that could not remove an
// older file copy (a read-only data directory, say, or a full disk), the older key in the file
// is used until the file's entry is removed by hand. The per-run warning at the next read must
// not claim the file's copy is the newer one: here it is the keychain's that is.
#[test]
fn a_stale_file_copy_is_read_back_with_a_neutral_warning() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    Credentials::with_keychain(&data, None)
        .set("openai", DEFAULT_PROFILE, "sk-OLD-in-file-0123")
        .unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o500)).unwrap();
    let creds = Credentials::with_keychain(&data, Some(Box::new(Recording::default())));
    let stored = creds
        .set("openai", DEFAULT_PROFILE, "sk-NEW-rotated-4567")
        .unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(stored.stale_file_copy);
    creds.take_warnings();
    // Every later read uses the file's (older) copy until the entry is removed by hand: see
    // `Credentials::newer`.
    let key = creds.active("openai").unwrap().unwrap();
    assert_eq!(key, "sk-OLD-in-file-0123");
    let warnings = creds.take_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    for part in [
        "hold different copies",
        "the file's is used",
        "openai/default",
    ] {
        assert!(warnings[0].contains(part), "{part}: {warnings:?}");
    }
    assert!(!warnings[0].contains("newer one"), "{warnings:?}");
    assert!(!warnings[0].contains("sk-"), "{warnings:?}");
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

/// A keychain connector that counts how often it connects, to `store`.
fn counting(
    store: impl SecretStore + Clone + 'static,
) -> (
    Arc<AtomicUsize>,
    impl Fn() -> Result<Box<dyn SecretStore>, CredentialError> + Send + Sync + 'static,
) {
    let connects = Arc::new(AtomicUsize::new(0));
    let counter = connects.clone();
    let connect = move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(store.clone()) as Box<dyn SecretStore>)
    };
    (connects, connect)
}

// Review B, M8: on Linux, reaching the keychain means a D-Bus connection, which a run that needs
// no stored credential (`ask` on ollama) must not make.
#[test]
fn the_keychain_is_reached_only_once_a_credential_is_needed() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = Recording::default();
    keychain.set("openai/default", "sk-1").unwrap();
    let (connects, connect) = counting(keychain);
    let creds = Credentials::with_connector(dir.path(), StoreChoice::Keychain, connect);
    assert_eq!(creds.active_profile("openai").unwrap(), "default");
    assert_eq!(connects.load(Ordering::SeqCst), 0);
    assert_eq!(creds.active("openai").unwrap().as_deref(), Some("sk-1"));
    assert_eq!(creds.active("openai").unwrap().as_deref(), Some("sk-1"));
    assert_eq!(connects.load(Ordering::SeqCst), 1);
}

// Review B, M7: with `HARNESS_CREDENTIAL_STORE=file` the keychain is neither read nor written, but
// logging out removes what an earlier run stored there too.
#[test]
fn with_the_file_store_chosen_logout_still_clears_the_keychain() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = Recording::default();
    keychain.set("openai/default", "sk-from-before").unwrap();
    let (connects, connect) = counting(keychain.clone());
    let creds = Credentials::with_connector(dir.path(), StoreChoice::File, connect);
    assert_eq!(creds.get("openai", DEFAULT_PROFILE).unwrap(), None);
    assert_eq!(connects.load(Ordering::SeqCst), 0);
    creds.set("openai", "work", "sk-work").unwrap();
    assert!(creds.take_warnings().is_empty());
    assert_eq!(keychain.get("openai/work").unwrap(), None);
    assert!(creds.remove("openai", DEFAULT_PROFILE).unwrap());
    assert_eq!(keychain.get("openai/default").unwrap(), None);
    // No keychain at all is nothing to remove.
    let none = Credentials::with_connector(dir.path(), StoreChoice::File, || {
        Err(CredentialError::Keychain("no session bus".into()))
    });
    assert!(none.remove("openai", "work").unwrap());
    assert!(!none.remove("openai", "work").unwrap());
    // Nor is it anything to warn about when storing.
    none.set("openai", "work", "sk-work").unwrap();
    assert!(none.take_warnings().is_empty());
}

// Re-review B+C, R1: storing with `HARNESS_CREDENTIAL_STORE=file`, as the `Stale` error advises,
// must not leave the older keychain copy to be used again once the variable is unset.
#[test]
fn a_key_stored_in_file_mode_is_not_shadowed_by_an_older_keychain_copy() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = Recording::default();
    keychain.set("openai/default", "sk-OLD-leaked").unwrap();
    let (_, connect) = counting(keychain.clone());
    let file_mode = Credentials::with_connector(dir.path(), StoreChoice::File, connect);
    let place = file_mode
        .set("openai", DEFAULT_PROFILE, "sk-NEW-rotated")
        .unwrap();
    assert_eq!(
        place.place,
        dir.path().join("credentials.json").display().to_string()
    );
    assert!(!place.stale_file_copy);
    assert!(file_mode.take_warnings().is_empty());
    assert_eq!(
        file_mode.active("openai").unwrap().as_deref(),
        Some("sk-NEW-rotated")
    );
    // The variable is unset again.
    let (_, connect) = counting(keychain.clone());
    let keychain_mode = Credentials::with_connector(dir.path(), StoreChoice::Keychain, connect);
    assert_eq!(
        keychain_mode.active("openai").unwrap().as_deref(),
        Some("sk-NEW-rotated")
    );
    assert_eq!(keychain.get("openai/default").unwrap(), None);
}

/// Counts the deletes that reach `inner`.
#[derive(Clone)]
struct CountingDeletes {
    inner: ReadOnly,
    deletes: Arc<AtomicUsize>,
}

impl SecretStore for CountingDeletes {
    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        self.inner.get(account)
    }
    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        self.inner.set(account, secret)
    }
    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        self.inner.delete(account)
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
}

// Re-review B+C, R1: when the keychain keeps its older copy, the new one is stored all the same (the
// file is what was chosen), and the user hears that the variable has to stay set.
#[test]
fn an_older_keychain_copy_file_mode_cannot_remove_is_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let mut read_only = ReadOnly::holding(&[("chatgpt/default", "old-sign-in-json")]);
    read_only.keeps = true;
    let keychain = CountingDeletes {
        inner: read_only,
        deletes: Arc::new(AtomicUsize::new(0)),
    };
    let (_, connect) = counting(keychain.clone());
    let creds = Credentials::with_connector(dir.path(), StoreChoice::File, connect);
    creds
        .set("chatgpt", DEFAULT_PROFILE, "new-sign-in-json")
        .unwrap();
    assert_eq!(
        creds.get("chatgpt", DEFAULT_PROFILE).unwrap().as_deref(),
        Some("new-sign-in-json")
    );
    let warnings = creds.take_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    for part in [
        "a read-only keychain",
        "older",
        "chatgpt/default",
        "HARNESS_CREDENTIAL_STORE",
        "delete was denied",
    ] {
        assert!(warnings[0].contains(part), "{part}: {warnings:?}");
    }
    assert!(!warnings[0].contains("sign-in-json"), "{warnings:?}");
    // A keychain that refused once, and may be waiting for an unlock prompt nobody answers, is
    // not asked again by every renewal of the same run.
    creds
        .set("chatgpt", DEFAULT_PROFILE, "newer-sign-in-json")
        .unwrap();
    assert!(creds.take_warnings().is_empty());
    assert_eq!(keychain.deletes.load(Ordering::SeqCst), 1);
}

/// A keychain waiting for an unlock prompt nobody answers.
#[derive(Clone)]
struct Stuck;

impl SecretStore for Stuck {
    fn get(&self, _account: &str) -> Result<Option<String>, CredentialError> {
        std::thread::sleep(Duration::from_secs(5));
        Ok(None)
    }
    fn set(&self, _account: &str, _secret: &str) -> Result<(), CredentialError> {
        std::thread::sleep(Duration::from_secs(5));
        Ok(())
    }
    fn delete(&self, _account: &str) -> Result<bool, CredentialError> {
        std::thread::sleep(Duration::from_secs(5));
        Ok(false)
    }
    fn describe(&self) -> String {
        "a stuck keychain".into()
    }
}

// Review B, M8: a locked Secret Service collection waits for its unlock prompt without a limit.
#[test]
fn keychain_operations_give_up_after_their_time_limit() {
    let limited = TimeLimited::new(Arc::new(Stuck), Duration::from_millis(100));
    let started = Instant::now();
    let error = limited.get("openai/default").unwrap_err();
    // Final review, I-2: a keychain that did not answer is told apart from one there is not.
    assert!(matches!(error, CredentialError::TimedOut(_)), "{error:?}");
    let error = error.to_string();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(error.contains("HARNESS_CREDENTIAL_STORE=file"), "{error}");
    assert!(error.contains("keep it set"), "{error}");
    assert!(limited.set("openai/default", "sk-1").is_err());
    assert!(limited.delete("openai/default").is_err());
    assert_eq!(limited.describe(), "a stuck keychain");
}

#[cfg(debug_assertions)]
#[test]
fn an_unknown_store_choice_is_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let creds = Credentials::open(dir.path(), |var| match var {
        "HARNESS_CREDENTIAL_STORE" => Some("files".to_string()),
        "HARNESS_TEST_NO_KEYCHAIN" => Some("1".to_string()),
        _ => None,
    });
    let warnings = creds.take_warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("HARNESS_CREDENTIAL_STORE")),
        "{warnings:?}"
    );
}

fn mode(path: &std::path::Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

// Review B, M1 (probe P6): a `credentials.json` that is a symbolic link is neither read through
// nor chmodded, nor replaced by a file (leaving the secrets in the link's target).
#[test]
fn a_symlinked_credentials_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = dir.path().join("elsewhere.json");
    std::fs::write(&elsewhere, r#"{"credentials":{"openai/default":"sk-1"}}"#).unwrap();
    std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o644)).unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::os::unix::fs::symlink(&elsewhere, data.join("credentials.json")).unwrap();
    let creds = Credentials::with_keychain(&data, None);
    let error = creds
        .get("openai", DEFAULT_PROFILE)
        .unwrap_err()
        .to_string();
    assert!(error.contains("symbolic link"), "{error}");
    assert!(creds.set("openai", DEFAULT_PROFILE, "sk-2").is_err());
    assert!(creds.remove("openai", DEFAULT_PROFILE).is_err());
    assert_eq!(mode(&elsewhere), 0o644);
    assert!(
        std::fs::symlink_metadata(data.join("credentials.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

// Review B, M1 (probes P4 and P5): the temporary file a rewrite goes through has a name nobody
// can plant a link or a readable file at in advance.
#[test]
fn a_rewrite_never_goes_through_a_planted_file() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let old_name = |ext: &str| data.join(format!("{ext}.tmp-{}", std::process::id()));
    let trap = dir.path().join("trap");
    std::os::unix::fs::symlink(&trap, old_name("credentials.json")).unwrap();
    let creds = Credentials::with_keychain(&data, None);
    creds.set("openai", DEFAULT_PROFILE, "sk-secret").unwrap();
    assert!(!trap.exists());
    let file = data.join("credentials.json");
    assert!(std::fs::symlink_metadata(&file).unwrap().is_file());
    // A stale readable file at the old name does not make the store readable.
    std::fs::remove_file(old_name("credentials.json")).unwrap();
    std::fs::write(old_name("credentials.json"), "").unwrap();
    std::fs::set_permissions(
        old_name("credentials.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    creds.set("openai", "work", "sk-work").unwrap();
    assert_eq!(mode(&file), 0o600);
    // The same holds for accounts.toml.
    std::os::unix::fs::symlink(&trap, old_name("accounts.toml")).unwrap();
    creds.use_profile("openai", "work").unwrap();
    assert!(!trap.exists());
    assert!(
        std::fs::symlink_metadata(data.join("accounts.toml"))
            .unwrap()
            .is_file()
    );
}

// Review B, M1 and M10: `accounts.toml` and the lock files are not followed through links
// either, and a lock file's error names the lock file.
#[test]
fn symlinked_accounts_and_lock_files_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::write(&elsewhere, "[active]\nopenai = \"work\"\n").unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::os::unix::fs::symlink(&elsewhere, data.join("accounts.toml")).unwrap();
    let creds = Credentials::with_keychain(&data, None);
    let error = creds.active_profile("openai").unwrap_err().to_string();
    assert!(error.contains("accounts.toml"), "{error}");
    assert!(error.contains("symbolic link"), "{error}");
    assert!(creds.use_profile("openai", "home").is_err());
    std::fs::remove_file(data.join("accounts.toml")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, data.join("credentials.json.lock")).unwrap();
    let error = creds
        .set("openai", DEFAULT_PROFILE, "sk-1")
        .unwrap_err()
        .to_string();
    assert!(error.contains("credentials.json.lock"), "{error}");
    let _ = std::fs::remove_file(data.join("accounts.toml.lock"));
    std::os::unix::fs::symlink(&elsewhere, data.join("accounts.toml.lock")).unwrap();
    let error = creds.use_profile("openai", "home").unwrap_err().to_string();
    assert!(error.contains("accounts.toml.lock"), "{error}");
}

// Review B, M1: a data directory the store makes is private.
#[test]
fn the_data_directory_the_store_makes_is_private() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("share/harness");
    Credentials::with_keychain(&data, None)
        .set("openai", DEFAULT_PROFILE, "sk-1")
        .unwrap();
    assert_eq!(mode(&data), 0o700);
}

// Review B, M10: `harness auth use` runs at the same time do not lose each other's choice.
#[test]
fn profile_choices_made_at_once_are_all_kept() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let rounds = 40;
    let threads: Vec<_> = ["a", "b", "c", "d"]
        .into_iter()
        .map(|provider| {
            let data = data.clone();
            std::thread::spawn(move || {
                let creds = Credentials::with_keychain(&data, None);
                for round in 0..rounds {
                    creds.use_profile(provider, &format!("p{round}")).unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let creds = Credentials::with_keychain(&data, None);
    for provider in ["a", "b", "c", "d"] {
        assert_eq!(
            creds.active_profile(provider).unwrap(),
            format!("p{}", rounds - 1),
            "{provider}"
        );
    }
}

// Review B, M2: serde's message quotes the value it could not read, which can be a secret.
#[test]
fn a_damaged_credentials_file_never_quotes_what_it_holds() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("credentials.json"),
        r#"{"credentials": "sk-proj-SECRETVALUE123"}"#,
    )
    .unwrap();
    let error = FileStore::new(dir.path())
        .get("openai/default")
        .unwrap_err()
        .to_string();
    assert!(!error.contains("SECRETVALUE"), "{error}");
    assert!(error.contains("line 1, column"), "{error}");
    // Review B, M3: it says how to recover, and what that loses.
    assert!(error.contains("delete"), "{error}");
    assert!(error.contains("API keys"), "{error}");
}

// Review B, M3: one line, with the way out.
#[test]
fn a_damaged_accounts_file_says_how_to_recover() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("accounts.toml"),
        "[active]\nchatgpt = work\n",
    )
    .unwrap();
    let creds = Credentials::with_keychain(dir.path(), None);
    let error = creds.active_profile("chatgpt").unwrap_err().to_string();
    assert!(error.contains("accounts.toml"), "{error}");
    assert!(error.contains("line 2"), "{error}");
    assert!(!error.contains('\n'), "{error}");
    assert!(error.contains("default profile"), "{error}");
}

// Review B, M3: an empty file holds nothing; it is not damaged.
#[test]
fn empty_files_hold_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("credentials.json"), "").unwrap();
    std::fs::write(dir.path().join("accounts.toml"), "\n").unwrap();
    let creds = Credentials::with_keychain(dir.path(), None);
    assert_eq!(creds.active("openai").unwrap(), None);
    assert_eq!(creds.active_profile("openai").unwrap(), "default");
}

/// A connector to a keychain that cannot be connected to, for `why`.
fn unreachable(
    why: fn() -> CredentialError,
) -> impl Fn() -> Result<Box<dyn SecretStore>, CredentialError> + Send + Sync + 'static {
    move || Err(why())
}

fn no_session_bus() -> CredentialError {
    CredentialError::Keychain("no session bus".into())
}

fn connection_timed_out() -> CredentialError {
    CredentialError::TimedOut("the keychain did not answer within 30 s".into())
}

/// A keychain, as a later run that reaches it finds it: holding `openai`'s older key.
fn holding_the_older_key() -> Recording {
    let keychain = Recording::default();
    keychain
        .set("openai/default", "sk-OLD-leaked-0123")
        .unwrap();
    keychain
}

/// What a later run in keychain mode, which reaches `keychain`, uses for `openai`, and what it
/// warns about the first time and the second.
fn used_later(dir: &std::path::Path, keychain: &Recording) -> (String, Vec<String>, Vec<String>) {
    let (_, connect) = counting(keychain.clone());
    let later = Credentials::with_connector(dir, StoreChoice::Keychain, connect);
    let key = later.active("openai").unwrap().unwrap();
    let first = later.take_warnings();
    assert_eq!(
        later.active("openai").unwrap().as_deref(),
        Some(key.as_str())
    );
    (key, first, later.take_warnings())
}

// Final review, I-2, probe (a): with the file chosen, a keychain that cannot be connected to
// cannot have its older copy removed. A later run that reaches it must still use the new key:
// a keychain that stores a credential removes the file's copy, so a file copy next to a
// keychain copy is the newer one.
#[test]
fn a_key_stored_in_file_mode_without_a_keychain_is_not_shadowed_later() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = holding_the_older_key();
    let file_mode =
        Credentials::with_connector(dir.path(), StoreChoice::File, unreachable(no_session_bus));
    file_mode
        .set("openai", DEFAULT_PROFILE, "sk-NEW-rotated-4567")
        .unwrap();
    // No keychain at all, as in a container, is nothing to warn about.
    assert!(file_mode.take_warnings().is_empty());
    let (key, first, second) = used_later(dir.path(), &keychain);
    assert_eq!(key, "sk-NEW-rotated-4567");
    assert_eq!(first.len(), 1, "{first:?}");
    for part in [
        "credentials.json",
        "openai/default",
        "the recording keychain",
        "hold different copies",
        "the file's is used",
    ] {
        assert!(first[0].contains(part), "{part}: {first:?}");
    }
    // Final review, wave 5 re-review R1: the warning must not claim the file's copy is the
    // newer one, since it cannot always tell (see `a_stale_file_copy_is_read_back_with_a_neutral_warning`).
    assert!(!first[0].contains("newer one"), "{first:?}");
    assert!(!first[0].contains("sk-"), "{first:?}");
    // Once a run.
    assert!(second.is_empty(), "{second:?}");
}

// Final review, I-2: a keychain connection that timed out is not "no keychain": it may hold an
// older copy it was not asked to remove, which is worth a warning.
#[test]
fn a_keychain_connection_that_timed_out_in_file_mode_is_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let file_mode = Credentials::with_connector(
        dir.path(),
        StoreChoice::File,
        unreachable(connection_timed_out),
    );
    file_mode
        .set("openai", DEFAULT_PROFILE, "sk-NEW-rotated-4567")
        .unwrap();
    let warnings = file_mode.take_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    for part in [
        "did not answer",
        "older copy",
        "openai/default",
        "credentials.json",
    ] {
        assert!(warnings[0].contains(part), "{part}: {warnings:?}");
    }
    assert!(!warnings[0].contains("sk-"), "{warnings:?}");
    let (key, _, _) = used_later(dir.path(), &holding_the_older_key());
    assert_eq!(key, "sk-NEW-rotated-4567");
}

// Final review, I-2, probe (b): in keychain mode, a run that reaches no keychain stores in the
// file, and says a keychain reached later may hold an older copy; the later run uses the new key.
#[test]
fn a_key_stored_while_no_keychain_is_reachable_is_not_shadowed_later() {
    let dir = tempfile::tempdir().unwrap();
    let keychain_mode = Credentials::with_connector(
        dir.path(),
        StoreChoice::Keychain,
        unreachable(no_session_bus),
    );
    keychain_mode
        .set("openai", DEFAULT_PROFILE, "sk-NEW-rotated-4567")
        .unwrap();
    let warnings = keychain_mode.take_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    for part in ["no keychain could store it", "no session bus", "older copy"] {
        assert!(warnings[0].contains(part), "{part}: {warnings:?}");
    }
    let (key, first, _) = used_later(dir.path(), &holding_the_older_key());
    assert_eq!(key, "sk-NEW-rotated-4567");
    assert_eq!(first.len(), 1, "{first:?}");
}

// The same copy in both places is nothing to choose between, and a file that cannot be read
// leaves the keychain's copy in use, with a warning: it may hold a newer one.
#[test]
fn a_keychain_copy_is_used_when_the_file_has_no_other() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = holding_the_older_key();
    FileStore::new(dir.path())
        .set("openai/default", "sk-OLD-leaked-0123")
        .unwrap();
    let (key, first, _) = used_later(dir.path(), &keychain);
    assert_eq!(key, "sk-OLD-leaked-0123");
    assert!(first.is_empty(), "{first:?}");
    std::fs::write(dir.path().join("credentials.json"), "{not json").unwrap();
    let (key, first, second) = used_later(dir.path(), &keychain);
    assert_eq!(key, "sk-OLD-leaked-0123");
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(first[0].contains("credentials.json"), "{first:?}");
    assert!(second.is_empty(), "{second:?}");
    // Without a file, nothing is made: no data directory, no lock file.
    let empty = tempfile::tempdir().unwrap();
    let data = empty.path().join("data");
    let (key, first, _) = used_later(&data, &keychain);
    assert_eq!(key, "sk-OLD-leaked-0123");
    assert!(first.is_empty(), "{first:?}");
    assert!(!data.exists());
}
