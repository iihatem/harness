//! Where API keys and sign-in tokens are kept: the OS keychain (the macOS Keychain, or the Secret
//! Service on Linux), or `credentials.json` in the data directory, mode `0600`, when no keychain
//! can be used. Each credential belongs to a provider and an account profile, and is stored under
//! the account name `<provider>/<profile>`. Which profile a provider uses is recorded in
//! `accounts.toml` next to it. Nothing here is ever written to the configuration directory.

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// The keychain service every harness credential is stored under.
pub const SERVICE: &str = "harness";
/// The profile used when none is named.
pub const DEFAULT_PROFILE: &str = "default";
/// `file` keeps credentials in the file only, never in the keychain.
pub const STORE_ENV: &str = "HARNESS_CREDENTIAL_STORE";
/// A test hook, honoured by debug builds only: any value leaves the platform keychain out
/// entirely, removal included, so that tests never touch the real one.
pub const NO_KEYCHAIN_ENV: &str = "HARNESS_TEST_NO_KEYCHAIN";
/// How long a keychain operation may take: long enough to answer an unlock prompt, but a
/// prompt nobody sees (on a desktop, over SSH) does not hold harness up for good.
pub const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// What the keychain said, or why it could not be used.
    #[error("{0}")]
    Keychain(String),
    #[error("cannot use {}: {source}", .path.display())]
    File {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} is damaged: {message}", .path.display())]
    Damaged { path: PathBuf, message: String },
    #[error(
        "{keychain} refused to store the new credential for {account} ({refused}) and to remove the older one it may still hold ({kept}), so harness would go on using that one; nothing was stored. Unlock the keychain and try again, or set {STORE_ENV}=file to keep credentials in credentials.json instead"
    )]
    Stale {
        keychain: String,
        account: String,
        refused: String,
        kept: String,
    },
    #[error("invalid {what} name `{name}`: use letters, digits, `.`, `_` and `-`")]
    BadName { what: &'static str, name: String },
}

/// Somewhere secrets can be kept, by account name.
pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>, CredentialError>;
    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError>;
    /// Whether there was something to delete.
    fn delete(&self, account: &str) -> Result<bool, CredentialError>;
    /// Where the secrets are kept, for messages.
    fn describe(&self) -> String;
}

/// The OS keychain, through a `keyring-core` credential store.
pub struct KeychainStore {
    store: Arc<keyring_core::CredentialStore>,
    name: String,
}

impl KeychainStore {
    /// The platform's keychain: the macOS Keychain, or the Secret Service over D-Bus on Linux.
    /// An error means there is none to use (no session bus, say).
    pub fn platform() -> Result<KeychainStore, CredentialError> {
        #[cfg(target_os = "macos")]
        let (store, name): (Arc<keyring_core::CredentialStore>, _) = (
            apple_native_keyring_store::keychain::Store::new().map_err(keychain_error)?,
            "the macOS keychain",
        );
        #[cfg(target_os = "linux")]
        let (store, name): (Arc<keyring_core::CredentialStore>, _) = (
            zbus_secret_service_keyring_store::Store::new().map_err(keychain_error)?,
            "the Secret Service keyring",
        );
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        return Err(CredentialError::Keychain(
            "no keychain is supported on this system".into(),
        ));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        Ok(KeychainStore::with_store(store, name))
    }

    /// A keychain backed by `store` (tests use keyring-core's mock store).
    pub fn with_store(store: Arc<keyring_core::CredentialStore>, name: &str) -> KeychainStore {
        KeychainStore {
            store,
            name: name.to_string(),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring_core::Entry, CredentialError> {
        self.store
            .build(SERVICE, account, None)
            .map_err(keychain_error)
    }
}

fn keychain_error(error: keyring_core::Error) -> CredentialError {
    CredentialError::Keychain(format!("the keychain refused: {error}"))
}

/// A keychain whose every operation gives up after a time limit, with an error that says how
/// to do without the keychain. What was cut short goes on, unwaited for, on its own thread.
pub struct TimeLimited {
    inner: Arc<dyn SecretStore>,
    limit: Duration,
}

impl TimeLimited {
    pub fn new(inner: Arc<dyn SecretStore>, limit: Duration) -> TimeLimited {
        TimeLimited { inner, limit }
    }

    fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&dyn SecretStore) -> Result<T, CredentialError> + Send + 'static,
    ) -> Result<T, CredentialError> {
        let inner = self.inner.clone();
        within(self.limit, move || work(&*inner))
    }
}

impl SecretStore for TimeLimited {
    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        let account = account.to_string();
        self.run(move |store| store.get(&account))
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        let (account, secret) = (account.to_string(), secret.to_string());
        self.run(move |store| store.set(&account, &secret))
    }

    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        let account = account.to_string();
        self.run(move |store| store.delete(&account))
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }
}

/// Runs `work` on a thread of its own, and gives up on it after `limit`.
fn within<T: Send + 'static>(
    limit: Duration,
    work: impl FnOnce() -> Result<T, CredentialError> + Send + 'static,
) -> Result<T, CredentialError> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("keychain".into())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .map_err(|e| {
            CredentialError::Keychain(format!("cannot start a thread for the keychain: {e}"))
        })?;
    match rx.recv_timeout(limit) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(CredentialError::Keychain(format!(
            "the keychain did not answer within {} (it may be waiting to be unlocked); set {STORE_ENV}=file to keep credentials in credentials.json instead",
            if limit >= Duration::from_secs(1) {
                format!("{} s", limit.as_secs())
            } else {
                format!("{} ms", limit.as_millis())
            }
        ))),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(CredentialError::Keychain(
            "the keychain failed without answering".into(),
        )),
    }
}

impl SecretStore for KeychainStore {
    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        match self.entry(account)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(keychain_error(e)),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        self.entry(account)?
            .set_password(secret)
            .map_err(keychain_error)
    }

    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(e) => Err(keychain_error(e)),
        }
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    credentials: BTreeMap<String, String>,
}

/// `credentials.json` in the data directory: readable and writable by its owner only, rewritten
/// whole through a temporary file, and locked while it is read and rewritten.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(data_dir: &Path) -> FileStore {
        FileStore {
            path: data_dir.join("credentials.json"),
        }
    }

    fn io(&self, source: std::io::Error) -> CredentialError {
        CredentialError::File {
            path: self.path.clone(),
            source,
        }
    }

    /// Locks the file against other harness processes until the returned guard is dropped.
    fn lock(&self) -> Result<File, CredentialError> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| self.io(e))?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.path.with_extension("json.lock"))
            .map_err(|e| self.io(e))?;
        lock.lock().map_err(|e| self.io(e))?;
        Ok(lock)
    }

    fn read(&self) -> Result<CredentialsFile, CredentialError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CredentialsFile::default());
            }
            Err(e) => return Err(self.io(e)),
        };
        // A file someone made readable by others is made private again.
        if let Ok(metadata) = std::fs::metadata(&self.path)
            && metadata.permissions().mode() & 0o077 != 0
        {
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| self.io(e))?;
        }
        serde_json::from_str(&text).map_err(|e| CredentialError::Damaged {
            path: self.path.clone(),
            message: e.to_string(),
        })
    }

    fn write(&self, file: &CredentialsFile) -> Result<(), CredentialError> {
        let text = serde_json::to_string_pretty(file).expect("credentials serialize");
        let tmp = self
            .path
            .with_extension(format!("json.tmp-{}", std::process::id()));
        let written = (|| {
            let mut out = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            out.write_all(text.as_bytes())?;
            out.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        written.map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            self.io(e)
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SecretStore for FileStore {
    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        let _lock = self.lock()?;
        Ok(self.read()?.credentials.remove(account))
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        let _lock = self.lock()?;
        let mut file = self.read()?;
        file.credentials
            .insert(account.to_string(), secret.to_string());
        self.write(&file)
    }

    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        if !self.path.exists() {
            return Ok(false);
        }
        let _lock = self.lock()?;
        let mut file = self.read()?;
        let removed = file.credentials.remove(account).is_some();
        if removed {
            self.write(&file)?;
        }
        Ok(removed)
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AccountsFile {
    /// The profile each provider uses, when it is not `default`.
    #[serde(default)]
    active: BTreeMap<String, String>,
}

/// Where credentials are stored and looked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreChoice {
    /// The keychain, and the file when no keychain can be used.
    Keychain,
    /// Only the file (`HARNESS_CREDENTIAL_STORE=file`). Removing still clears the keychain.
    File,
}

/// Connects to a keychain.
type Connect = Box<dyn Fn() -> Result<Box<dyn SecretStore>, CredentialError> + Send + Sync>;

/// Stored credentials by provider and account profile.
pub struct Credentials {
    choice: StoreChoice,
    /// Connects to the keychain the first time a credential is needed: on Linux that is a D-Bus
    /// connection, which a run that needs no stored credential never makes.
    connect: Connect,
    /// The keychain, or why there is none.
    keychain: OnceLock<Result<Box<dyn SecretStore>, String>>,
    file: FileStore,
    accounts: PathBuf,
    warnings: Mutex<Vec<String>>,
}

impl Credentials {
    /// The credentials for `data_dir`: in the OS keychain when there is one, unless `STORE_ENV`
    /// (read through `env`) is `file`. The keychain is reached only once a credential is
    /// needed, and each of its operations gives up after [`KEYCHAIN_TIMEOUT`].
    pub fn open(data_dir: &Path, env: impl Fn(&str) -> Option<String>) -> Credentials {
        let chosen = env(STORE_ENV).filter(|value| !value.is_empty());
        let choice = match chosen.as_deref() {
            Some("file") => StoreChoice::File,
            _ => StoreChoice::Keychain,
        };
        let platform = !(cfg!(debug_assertions) && env(NO_KEYCHAIN_ENV).is_some());
        let credentials = if platform {
            Credentials::with_connector(data_dir, choice, || {
                within(KEYCHAIN_TIMEOUT, || {
                    let keychain = KeychainStore::platform()?;
                    Ok(
                        Box::new(TimeLimited::new(Arc::new(keychain), KEYCHAIN_TIMEOUT))
                            as Box<dyn SecretStore>,
                    )
                })
            })
        } else {
            Credentials::with_connector(data_dir, choice, || {
                Err(CredentialError::Keychain(format!(
                    "{NO_KEYCHAIN_ENV} leaves it out"
                )))
            })
        };
        if let Some(other) = chosen.filter(|value| value != "file") {
            credentials.warn(format!(
                "{STORE_ENV} is `{other}`, which harness does not know (only `file` is); credentials go to the keychain"
            ));
        }
        credentials
    }

    /// The credentials for `data_dir`, with `keychain` as the keychain. Without one, credentials
    /// go to the file with a warning.
    pub fn with_keychain(data_dir: &Path, keychain: Option<Box<dyn SecretStore>>) -> Credentials {
        let credentials = Credentials::with_connector(data_dir, StoreChoice::Keychain, || {
            Err(CredentialError::Keychain("none is available".into()))
        });
        if let Some(keychain) = keychain {
            let _ = credentials.keychain.set(Ok(keychain));
        }
        credentials
    }

    /// The credentials for `data_dir`, stored as `choice` says, with the keychain `connect`
    /// connects to, the first time one is needed. A connection that fails means there is no
    /// keychain.
    pub fn with_connector(
        data_dir: &Path,
        choice: StoreChoice,
        connect: impl Fn() -> Result<Box<dyn SecretStore>, CredentialError> + Send + Sync + 'static,
    ) -> Credentials {
        Credentials {
            choice,
            connect: Box::new(connect),
            keychain: OnceLock::new(),
            file: FileStore::new(data_dir),
            accounts: data_dir.join("accounts.toml"),
            warnings: Mutex::new(Vec::new()),
        }
    }

    /// The keychain, connected to now if it was not yet, or why there is none.
    fn keychain(&self) -> Result<&dyn SecretStore, &str> {
        match self
            .keychain
            .get_or_init(|| (self.connect)().map_err(|e| e.to_string()))
        {
            Ok(keychain) => Ok(keychain.as_ref()),
            Err(why) => Err(why),
        }
    }

    /// The keychain credentials are stored in and read from: none with the file chosen.
    fn storing_keychain(&self) -> Option<Result<&dyn SecretStore, &str>> {
        (self.choice == StoreChoice::Keychain).then(|| self.keychain())
    }

    /// Warnings gathered since the last call: that a credential went to the file, say, or that
    /// the keychain could not be read.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.lock().expect("warnings lock"))
    }

    /// Queues `warning` for [`take_warnings`](Self::take_warnings).
    pub fn warn(&self, warning: String) {
        self.warnings.lock().expect("warnings lock").push(warning);
    }

    fn read_accounts(&self) -> Result<AccountsFile, CredentialError> {
        match std::fs::read_to_string(&self.accounts) {
            Ok(text) => toml::from_str(&text).map_err(|e| CredentialError::Damaged {
                path: self.accounts.clone(),
                message: e.to_string(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AccountsFile::default()),
            Err(source) => Err(CredentialError::File {
                path: self.accounts.clone(),
                source,
            }),
        }
    }

    /// The profile `provider` uses: the one chosen with `harness auth use`, or `default`.
    pub fn active_profile(&self, provider: &str) -> Result<String, CredentialError> {
        Ok(self
            .read_accounts()?
            .active
            .remove(provider)
            .unwrap_or_else(|| DEFAULT_PROFILE.to_string()))
    }

    /// Makes `profile` the one `provider` uses from now on.
    pub fn use_profile(&self, provider: &str, profile: &str) -> Result<(), CredentialError> {
        check_name("provider", provider)?;
        check_name("profile", profile)?;
        let mut accounts = self.read_accounts()?;
        if profile == DEFAULT_PROFILE {
            accounts.active.remove(provider);
        } else {
            accounts
                .active
                .insert(provider.to_string(), profile.to_string());
        }
        let io = |source| CredentialError::File {
            path: self.accounts.clone(),
            source,
        };
        let text = toml::to_string(&accounts).expect("accounts serialize");
        if let Some(dir) = self.accounts.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let tmp = self
            .accounts
            .with_extension(format!("toml.tmp-{}", std::process::id()));
        let written = (|| {
            let mut out = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            out.write_all(text.as_bytes())?;
            std::fs::rename(&tmp, &self.accounts)
        })();
        written.map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io(e)
        })
    }

    /// The credential stored for `provider` under `profile`: from the keychain, or else the
    /// file (where it went when no keychain could be used). A keychain that cannot be read is an
    /// error, unless the file holds the credential, which is then used with a warning.
    pub fn get(&self, provider: &str, profile: &str) -> Result<Option<String>, CredentialError> {
        let account = account(provider, profile)?;
        if let Some(Ok(keychain)) = self.storing_keychain() {
            match keychain.get(&account) {
                Ok(Some(secret)) => return Ok(Some(secret)),
                Ok(None) => {}
                Err(unreadable) => {
                    let Some(secret) = self.file.get(&account)? else {
                        return Err(unreadable);
                    };
                    self.warn(format!(
                        "cannot read {} ({unreadable}); using the copy of {account} in {}",
                        keychain.describe(),
                        self.file.describe()
                    ));
                    return Ok(Some(secret));
                }
            }
        }
        self.file.get(&account)
    }

    /// The credential of the profile `provider` uses.
    pub fn active(&self, provider: &str) -> Result<Option<String>, CredentialError> {
        let profile = self.active_profile(provider)?;
        self.get(provider, &profile)
    }

    /// Stores `secret` for `provider` under `profile`, in the keychain when one works, else in
    /// the file with a warning. Returns where it went.
    pub fn set(
        &self,
        provider: &str,
        profile: &str,
        secret: &str,
    ) -> Result<String, CredentialError> {
        let account = account(provider, profile)?;
        // Why the file is used; nothing to say when it was chosen (`HARNESS_CREDENTIAL_STORE`).
        let why = match self.storing_keychain() {
            Some(Ok(keychain)) => match keychain.set(&account, secret) {
                Ok(()) => {
                    // An older copy in the file must not outlive this one.
                    if let Err(e) = self.file.delete(&account) {
                        self.warn(format!(
                            "the credential is in {}, but an older copy of it could not be removed from {} ({e}); remove that file's entry for {account} by hand",
                            keychain.describe(),
                            self.file.describe()
                        ));
                    }
                    return Ok(keychain.describe());
                }
                Err(refused) => {
                    // An older copy in the keychain would be read before the file's.
                    if let Err(kept) = keychain.delete(&account) {
                        return Err(CredentialError::Stale {
                            keychain: keychain.describe(),
                            account,
                            refused: refused.to_string(),
                            kept: kept.to_string(),
                        });
                    }
                    Some(refused.to_string())
                }
            },
            Some(Err(why)) => Some(why.to_string()),
            None => None,
        };
        self.file.set(&account, secret)?;
        if let Some(why) = why {
            self.warn(format!(
                "no keychain could store it ({why}); it is in {}, readable only by you",
                self.file.describe()
            ));
        }
        Ok(self.file.describe())
    }

    /// Removes what is stored for `provider` under `profile`, from the keychain and the file,
    /// whichever is chosen: an earlier run may have stored it in either, and removing is safe.
    /// A keychain that refuses is an error, once the file's copy is gone: the keychain's would
    /// still be used.
    pub fn remove(&self, provider: &str, profile: &str) -> Result<bool, CredentialError> {
        let account = account(provider, profile)?;
        let in_keychain = match self.keychain() {
            Ok(keychain) => keychain.delete(&account),
            // No keychain holds nothing.
            Err(_) => Ok(false),
        };
        let in_file = self.file.delete(&account)?;
        Ok(in_keychain? || in_file)
    }
}

/// The account name of a provider's profile.
fn account(provider: &str, profile: &str) -> Result<String, CredentialError> {
    check_name("provider", provider)?;
    check_name("profile", profile)?;
    Ok(format!("{provider}/{profile}"))
}

/// Checks a provider or profile name: letters, digits, `.`, `_` and `-`, at most 64.
pub fn check_name(what: &'static str, name: &str) -> Result<(), CredentialError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(CredentialError::BadName {
            what,
            name: name.to_string(),
        })
    }
}
