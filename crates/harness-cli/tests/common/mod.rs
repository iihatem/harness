//! What every end-to-end suite shares.

/// Keeps a `harness` run from what the developer's machine could lend it: the provider keys in
/// the environment (which runs that list models would send to the real providers) and the real
/// keychain (credentials go to the file store, and the keychain is left out entirely, removal
/// included).
pub trait Isolate {
    fn isolate(&mut self) -> &mut Self;
}

/// The environment variables of the built-in providers' keys.
fn provider_key_vars() -> impl Iterator<Item = &'static str> {
    harness_providers::registry::BUILTIN_PROVIDERS
        .iter()
        .filter_map(|builtin| builtin.key_env)
}

impl Isolate for assert_cmd::Command {
    fn isolate(&mut self) -> &mut Self {
        for var in provider_key_vars() {
            self.env_remove(var);
        }
        self.env("HARNESS_CREDENTIAL_STORE", "file")
            .env("HARNESS_TEST_NO_KEYCHAIN", "1")
    }
}

impl Isolate for std::process::Command {
    fn isolate(&mut self) -> &mut Self {
        for var in provider_key_vars() {
            self.env_remove(var);
        }
        self.env("HARNESS_CREDENTIAL_STORE", "file")
            .env("HARNESS_TEST_NO_KEYCHAIN", "1")
    }
}
