use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use harness_core::{
    agent::DEFAULT_MAX_STEPS,
    compaction::{DEFAULT_KEEP_RECENT, DEFAULT_THRESHOLD},
    edit_format::EditFormat,
    gate::{Gates, MAX_TIMEOUT_S},
    permission::Mode,
    role::{HandoffMode, RoleConfig},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::trust::TrustStore;

/// Wire protocol spoken by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    /// `POST /chat/completions`: Ollama, LM Studio, llama.cpp, OpenRouter and most others.
    OpenaiChat,
    /// `POST /responses`: OpenAI API keys and ChatGPT sign-in.
    OpenaiResponses,
    /// `POST /messages`: Anthropic API keys and Anthropic-compatible servers.
    AnthropicMessages,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub protocol: Protocol,
    pub base_url: String,
    /// The environment variable holding its key: a name (`[A-Za-z_][A-Za-z0-9_]*`), which
    /// [`parse_file`] checks.
    pub api_key_env: Option<String>,
    /// The file that defined it, for messages; [`parse_file`] sets it.
    #[serde(skip)]
    pub file: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub confirm: Vec<String>,
    #[serde(default)]
    pub read_dirs: Vec<String>,
}

/// `sandbox.linux_git_protection`: what to do on Linux when user namespaces are unavailable, so
/// git metadata is protected only after each command (the basic tier).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinuxGitProtection {
    /// Run commands in the basic tier after a startup warning.
    #[default]
    BestEffort,
    /// Treat the basic tier as no sandbox: every shell command asks first.
    Required,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default)]
    pub writable_roots: Vec<String>,
    pub allow_localhost: Option<bool>,
    /// `sandbox.linux_git_protection`: see [`LinuxGitProtection`]; unset means `"best-effort"`.
    /// A project's `"required"` always applies; a project's `"best-effort"` over a global
    /// `"required"` widens it, so it applies only once the workspace is trusted.
    pub linux_git_protection: Option<LinuxGitProtection>,
}

/// The lowest compaction threshold, in percent, a project may set without workspace trust: below
/// it harness would summarize, a paid request that replaces verbatim context, every few turns.
pub const MIN_UNTRUSTED_THRESHOLD_PERCENT: u8 = 50;

/// `[compaction]`: when the conversation is summarized, in percent of the context window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionSettings {
    /// Compact when estimated usage reaches this share of the context window (1 to 100).
    pub threshold_percent: Option<u8>,
    /// Keep this share of the context window of recent messages as they are (below the
    /// threshold).
    pub keep_recent_percent: Option<u8>,
}

impl CompactionSettings {
    /// The threshold as a fraction of the context window.
    pub fn threshold(&self) -> f64 {
        self.threshold_percent
            .map_or(DEFAULT_THRESHOLD, |p| f64::from(p) / 100.0)
    }

    /// The share kept as recent messages, as a fraction of the context window.
    pub fn keep_recent(&self) -> f64 {
        self.keep_recent_percent
            .map_or(DEFAULT_KEEP_RECENT, |p| f64::from(p) / 100.0)
    }

    /// Which value is outside 1 to 100, if any.
    fn out_of_range(&self) -> Option<String> {
        let bad = |p: Option<u8>| p.is_some_and(|p| p == 0 || p > 100);
        if bad(self.threshold_percent) {
            return Some("compaction.threshold_percent must be between 1 and 100".into());
        }
        if bad(self.keep_recent_percent) {
            return Some("compaction.keep_recent_percent must be between 1 and 100".into());
        }
        None
    }

    /// These settings with `project`'s over them.
    fn overlaid(&self, project: &CompactionSettings) -> CompactionSettings {
        CompactionSettings {
            threshold_percent: project.threshold_percent.or(self.threshold_percent),
            keep_recent_percent: project.keep_recent_percent.or(self.keep_recent_percent),
        }
    }

    /// What is wrong with these settings, if anything.
    fn problem(&self) -> Option<String> {
        if let Some(problem) = self.out_of_range() {
            return Some(problem);
        }
        if self.keep_recent() >= self.threshold() {
            return Some(
                "compaction.keep_recent_percent must be below compaction.threshold_percent".into(),
            );
        }
        None
    }
}

/// `[profiles."<glob>"]`: settings for the models whose ids match the glob (resolved in
/// `harness_providers::profiles`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSettings {
    /// The model's context window, in tokens.
    pub context_window: Option<u64>,
    /// The smallest window worth running agentic turns in; below it harness warns.
    pub min_context: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub reasoning_effort: Option<String>,
    /// Whether tool calls the model writes as text are run.
    pub text_tool_calls: Option<bool>,
    /// Whether the model runs on a server of the user's own.
    pub local: Option<bool>,
    /// How the model edits files: `str_replace` (the default), `apply_patch`, `whole_file` or
    /// `hashline`.
    pub edit_format: Option<EditFormat>,
}

impl ProfileSettings {
    /// These settings with `other`'s over them.
    pub fn overlaid(&self, other: &ProfileSettings) -> ProfileSettings {
        ProfileSettings {
            context_window: other.context_window.or(self.context_window),
            min_context: other.min_context.or(self.min_context),
            max_output_tokens: other.max_output_tokens.or(self.max_output_tokens),
            temperature: other.temperature.or(self.temperature),
            reasoning_effort: other
                .reasoning_effort
                .clone()
                .or_else(|| self.reasoning_effort.clone()),
            text_tool_calls: other.text_tool_calls.or(self.text_tool_calls),
            local: other.local.or(self.local),
            edit_format: other.edit_format.or(self.edit_format),
        }
    }

    /// The settings that are set, as `key = value`, for listings and fingerprints.
    fn describe(&self) -> String {
        let mut set = Vec::new();
        let mut number = |key: &str, value: Option<u64>| {
            if let Some(value) = value {
                set.push(format!("{key} = {value}"));
            }
        };
        number("context_window", self.context_window);
        number("min_context", self.min_context);
        number("max_output_tokens", self.max_output_tokens);
        if let Some(t) = self.temperature {
            set.push(format!("temperature = {t}"));
        }
        if let Some(effort) = &self.reasoning_effort {
            set.push(format!("reasoning_effort = {effort:?}"));
        }
        if let Some(on) = self.text_tool_calls {
            set.push(format!("text_tool_calls = {on}"));
        }
        if let Some(local) = self.local {
            set.push(format!("local = {local}"));
        }
        if let Some(format) = self.edit_format {
            set.push(format!("edit_format = \"{format}\""));
        }
        set.join(", ")
    }

    /// What is wrong with the profile under `key`, if anything.
    fn problem(&self, key: &str) -> Option<String> {
        if let Err(e) = globset::Glob::new(key) {
            return Some(format!("profiles.{key:?} is not a valid glob: {e}"));
        }
        for (name, value) in [
            ("context_window", self.context_window),
            ("min_context", self.min_context),
            ("max_output_tokens", self.max_output_tokens),
        ] {
            if value == Some(0) {
                return Some(format!("profiles.{key:?}: {name} must be at least 1"));
            }
        }
        if self.temperature.is_some_and(|t| !(0.0..=2.0).contains(&t)) {
            return Some(format!(
                "profiles.{key:?}: temperature must be between 0 and 2"
            ));
        }
        None
    }
}

/// The first problem with any of `profiles`.
fn profiles_problem(profiles: &BTreeMap<String, ProfileSettings>) -> Option<String> {
    profiles
        .iter()
        .find_map(|(key, profile)| profile.problem(key))
}

/// `[pricing."<glob>"]`: a model's prices in USD per million tokens, over the price tables', for
/// the models whose ids match the glob. Fields left out come from the table below.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceSettings {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub cache_write_1h: Option<f64>,
}

impl PriceSettings {
    /// What is wrong with the prices under `key`, if anything.
    fn problem(&self, key: &str) -> Option<String> {
        if let Err(e) = globset::Glob::new(key) {
            return Some(format!("pricing.{key:?} is not a valid glob: {e}"));
        }
        for (name, value) in [
            ("input", self.input),
            ("output", self.output),
            ("cache_read", self.cache_read),
            ("cache_write", self.cache_write),
            ("cache_write_1h", self.cache_write_1h),
        ] {
            if value.is_some_and(|v| !v.is_finite() || v < 0.0) {
                return Some(format!(
                    "pricing.{key:?}: {name} must be a price in USD per million tokens, not below 0"
                ));
            }
        }
        None
    }
}

/// The first problem with any of `pricing`.
fn pricing_problem(pricing: &BTreeMap<String, PriceSettings>) -> Option<String> {
    pricing.iter().find_map(|(key, price)| price.problem(key))
}

/// `usage.auto_resume`: whether the terminal session offers to wait out a subscription limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoResume {
    /// Offer once, when the limit is hit.
    #[default]
    Ask,
    /// Never offer. (There is no "always": an unattended resume can spend quota meant for
    /// something else.)
    Never,
}

/// `[usage]`: how usage is shown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSettings {
    /// `usage.auto_resume`: see [`AutoResume`].
    #[serde(default)]
    pub auto_resume: AutoResume,
    /// The `<provider>/<model>` that "avoided" cost is measured against; none means no avoided
    /// figure is shown.
    pub baseline: Option<String>,
}

/// `[outcomes]`: the per-turn outcome log, on unless `enabled = false`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeSettings {
    pub enabled: Option<bool>,
}

/// `[budgets]`: money limits on billed cost, in USD. A budget that is not set has no limit.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSettings {
    pub session_usd: Option<f64>,
    pub daily_usd: Option<f64>,
    pub monthly_usd: Option<f64>,
}

impl BudgetSettings {
    fn named(&self) -> [(&'static str, Option<f64>); 3] {
        [
            ("session_usd", self.session_usd),
            ("daily_usd", self.daily_usd),
            ("monthly_usd", self.monthly_usd),
        ]
    }

    /// What is wrong with these budgets, if anything: a limit must be above 0.
    fn problem(&self) -> Option<String> {
        self.named().into_iter().find_map(|(name, value)| {
            value
                .is_some_and(|v| !v.is_finite() || v <= 0.0)
                .then(|| format!("budgets.{name} must be an amount in USD above 0"))
        })
    }

    /// These budgets with `project`'s: it may lower a limit or set one where there was none, and
    /// never raise one. The keys it tried to raise are returned.
    fn tightened_by(&self, project: &BudgetSettings) -> (BudgetSettings, Vec<&'static str>) {
        let mut raised = Vec::new();
        let mut pick =
            |name: &'static str, global: Option<f64>, project: Option<f64>| match (global, project)
            {
                (Some(g), Some(p)) if p > g => {
                    raised.push(name);
                    Some(g)
                }
                (Some(g), Some(p)) => Some(g.min(p)),
                (None, Some(p)) => Some(p),
                (g, None) => g,
            };
        let merged = BudgetSettings {
            session_usd: pick("session_usd", self.session_usd, project.session_usd),
            daily_usd: pick("daily_usd", self.daily_usd, project.daily_usd),
            monthly_usd: pick("monthly_usd", self.monthly_usd, project.monthly_usd),
        };
        (merged, raised)
    }
}

/// `[notifications]`: what the interactive session does when a long turn ends or an approval
/// waits. A project may set it without trust: it changes nothing the agent may do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationSettings {
    /// A desktop notification through the terminal (OSC 9); on unless set to `false`.
    pub desktop: Option<bool>,
    /// The terminal bell; on unless set to `false`.
    pub bell: Option<bool>,
}

/// The notifications in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notifications {
    pub desktop: bool,
    pub bell: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Notifications {
            desktop: true,
            bell: true,
        }
    }
}

impl Notifications {
    /// These, with what `settings` sets.
    fn overlaid(self, settings: &NotificationSettings) -> Notifications {
        Notifications {
            desktop: settings.desktop.unwrap_or(self.desktop),
            bell: settings.bell.unwrap_or(self.bell),
        }
    }
}

/// `[gates]`: checks run after edits and when a turn ends (`harness_core::gate`). A project's
/// gates need trust: their commands run project code.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateSettings {
    pub after_edit: Option<String>,
    pub test: Option<String>,
    pub timeout_s: Option<u64>,
    pub max_retries: Option<u32>,
    pub output_tail_lines: Option<usize>,
}

impl GateSettings {
    /// These settings with `other`'s over them.
    fn overlaid(&self, other: &GateSettings) -> GateSettings {
        GateSettings {
            after_edit: other.after_edit.clone().or_else(|| self.after_edit.clone()),
            test: other.test.clone().or_else(|| self.test.clone()),
            timeout_s: other.timeout_s.or(self.timeout_s),
            max_retries: other.max_retries.or(self.max_retries),
            output_tail_lines: other.output_tail_lines.or(self.output_tail_lines),
        }
    }

    /// The settings with defaults filled in.
    fn resolve(&self) -> Gates {
        let default = Gates::default();
        Gates {
            after_edit: self.after_edit.clone(),
            test: self.test.clone(),
            timeout_s: self.timeout_s.unwrap_or(default.timeout_s),
            max_retries: self.max_retries.unwrap_or(default.max_retries),
            output_tail_lines: self.output_tail_lines.unwrap_or(default.output_tail_lines),
        }
    }

    /// What is wrong with these settings, if anything.
    fn problem(&self) -> Option<String> {
        for (key, command) in [("after_edit", &self.after_edit), ("test", &self.test)] {
            if command.as_deref().is_some_and(|c| c.trim().is_empty()) {
                return Some(format!("gates.{key} must not be empty"));
            }
        }
        if self
            .timeout_s
            .is_some_and(|s| !(1..=MAX_TIMEOUT_S).contains(&s))
        {
            return Some(format!(
                "gates.timeout_s must be between 1 and {MAX_TIMEOUT_S}"
            ));
        }
        if self.output_tail_lines == Some(0) {
            return Some("gates.output_tail_lines must be at least 1".into());
        }
        None
    }

    /// The settings that are set, as `gates.<key> = <value>`, for listings and fingerprints.
    fn items(&self) -> Vec<String> {
        let mut items = Vec::new();
        if let Some(command) = &self.after_edit {
            items.push(format!("gates.after_edit = {command:?}"));
        }
        if let Some(command) = &self.test {
            items.push(format!("gates.test = {command:?}"));
        }
        if let Some(seconds) = self.timeout_s {
            items.push(format!("gates.timeout_s = {seconds}"));
        }
        if let Some(retries) = self.max_retries {
            items.push(format!("gates.max_retries = {retries}"));
        }
        if let Some(lines) = self.output_tail_lines {
            items.push(format!("gates.output_tail_lines = {lines}"));
        }
        items
    }
}

/// Whether `id` is `<provider>/<model>`.
fn is_model_id(id: &str) -> bool {
    id.split_once('/')
        .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
}

/// What is wrong with the `[fallback]` chains, if anything.
fn fallback_problem(chains: &BTreeMap<String, Vec<String>>) -> Option<String> {
    chains.iter().find_map(|(glob, ids)| {
        if let Err(e) = globset::Glob::new(glob) {
            return Some(format!("fallback.{glob:?} is not a valid glob: {e}"));
        }
        ids.iter().find(|id| !is_model_id(id)).map(|id| {
            format!(
                "fallback.{glob:?} must list provider/model ids, such as openai/gpt-5, not {id:?}"
            )
        })
    })
}

/// `[roles]`: the model of each role, and `[roles.handoff]`. A project's roles need trust: they
/// choose where a conversation is sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleSettings {
    pub main: Option<String>,
    pub plan: Option<String>,
    pub build: Option<String>,
    pub background: Option<String>,
    #[serde(default)]
    pub handoff: HandoffSettings,
}

/// `[roles.handoff]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffSettings {
    pub mode: Option<HandoffMode>,
}

impl RoleSettings {
    fn named(&self) -> [(&'static str, &Option<String>); 4] {
        [
            ("main", &self.main),
            ("plan", &self.plan),
            ("build", &self.build),
            ("background", &self.background),
        ]
    }

    /// What is wrong with these settings, if anything: a role is a `provider/model` id.
    fn problem(&self) -> Option<String> {
        self.named().into_iter().find_map(|(role, id)| {
            let id = id.as_deref()?;
            (!is_model_id(id)).then(|| {
                format!(
                    "roles.{role} must be a provider/model id, such as ollama/llama3, not {id:?}"
                )
            })
        })
    }

    /// These settings with `other`'s over them, role by role.
    fn overlaid(&self, other: &RoleSettings) -> RoleSettings {
        RoleSettings {
            main: other.main.clone().or_else(|| self.main.clone()),
            plan: other.plan.clone().or_else(|| self.plan.clone()),
            build: other.build.clone().or_else(|| self.build.clone()),
            background: other.background.clone().or_else(|| self.background.clone()),
            handoff: HandoffSettings {
                mode: other.handoff.mode.or(self.handoff.mode),
            },
        }
    }

    fn resolve(&self) -> RoleConfig {
        RoleConfig {
            main: self.main.clone(),
            plan: self.plan.clone(),
            build: self.build.clone(),
            background: self.background.clone(),
            handoff: self.handoff.mode,
        }
    }

    /// The settings that are set, as `roles.<key> = <value>`, for listings and fingerprints.
    fn items(&self) -> Vec<String> {
        let mut items: Vec<String> = self
            .named()
            .into_iter()
            .filter_map(|(role, id)| Some(format!("roles.{role} = {:?}", id.as_deref()?)))
            .collect();
        if let Some(mode) = self.handoff.mode {
            items.push(format!("roles.handoff.mode = {:?}", mode.as_str()));
        }
        items
    }
}

/// The languages `[lsp.servers]` names.
pub const LSP_LANGUAGES: [&str; 4] = ["rust", "typescript", "python", "go"];

/// The shortest and longest wait for a language server's diagnostics, in milliseconds.
pub const LSP_WAIT_MS: std::ops::RangeInclusive<u64> = 100..=60_000;

/// `[lsp]`: language servers that report the errors in edited files. Turning them off, or
/// shortening the wait, narrows what harness does and applies from a project without trust; a
/// command to run, or turning them on again, does not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspSettings {
    pub enabled: Option<bool>,
    pub wait_ms: Option<u64>,
    #[serde(default)]
    pub servers: BTreeMap<String, LspServerSettings>,
}

/// `[lsp.servers.<language>]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspServerSettings {
    /// The command to run for the language, instead of the one looked up on `PATH`.
    pub command: Option<String>,
    pub enabled: Option<bool>,
}

impl LspSettings {
    /// What is wrong with these settings, if anything.
    fn problem(&self) -> Option<String> {
        if self.wait_ms.is_some_and(|ms| !LSP_WAIT_MS.contains(&ms)) {
            return Some(format!(
                "lsp.wait_ms must be between {} and {}",
                LSP_WAIT_MS.start(),
                LSP_WAIT_MS.end()
            ));
        }
        for (language, server) in &self.servers {
            if !LSP_LANGUAGES.contains(&language.as_str()) {
                return Some(format!(
                    "lsp.servers.{language}: unknown language (expected {})",
                    LSP_LANGUAGES.join(", ")
                ));
            }
            if server
                .command
                .as_deref()
                .is_some_and(|c| c.trim().is_empty())
            {
                return Some(format!("lsp.servers.{language}.command must not be empty"));
            }
        }
        None
    }

    /// What a project sets that needs trust: `lsp.servers.<language>.command = <command>` (it runs
    /// project code), and turning servers on (`enabled = true`, which the user may have turned off).
    fn items(&self) -> Vec<String> {
        let mut items = Vec::new();
        if self.enabled == Some(true) {
            items.push("lsp.enabled = true".to_string());
        }
        for (language, server) in &self.servers {
            if server.enabled == Some(true) {
                items.push(format!("lsp.servers.{language}.enabled = true"));
            }
            if let Some(command) = &server.command {
                items.push(format!("lsp.servers.{language}.command = {command:?}"));
            }
        }
        items
    }
}

/// The language-server settings in effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspConfig {
    pub enabled: bool,
    pub wait_ms: u64,
    pub servers: BTreeMap<String, LspServer>,
}

impl Default for LspConfig {
    fn default() -> Self {
        LspConfig {
            enabled: true,
            wait_ms: 2_000,
            servers: BTreeMap::new(),
        }
    }
}

/// One language's server setting in effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspServer {
    pub command: Option<String>,
    pub enabled: bool,
}

impl LspConfig {
    /// These with `settings` over them. `trusted` lets commands in, and turning servers on again;
    /// without it a setting can only turn a server off.
    fn overlaid(&mut self, settings: &LspSettings, commands: bool) {
        if let Some(enabled) = settings.enabled {
            self.enabled = if commands {
                enabled
            } else {
                self.enabled & enabled
            };
        }
        if let Some(ms) = settings.wait_ms {
            self.wait_ms = ms;
        }
        for (language, server) in &settings.servers {
            let entry = self.servers.entry(language.clone()).or_insert(LspServer {
                command: None,
                enabled: true,
            });
            if let Some(enabled) = server.enabled {
                entry.enabled = if commands {
                    enabled
                } else {
                    entry.enabled & enabled
                };
            }
            if commands && let Some(command) = &server.command {
                entry.command = Some(command.clone());
            }
        }
    }
}

/// One `config.toml` file as written by the user.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub model: Option<String>,
    pub mode: Option<Mode>,
    pub max_steps: Option<u32>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub compaction: CompactionSettings,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileSettings>,
    #[serde(default)]
    pub notifications: NotificationSettings,
    #[serde(default)]
    pub pricing: BTreeMap<String, PriceSettings>,
    #[serde(default)]
    pub usage: UsageSettings,
    #[serde(default)]
    pub budgets: BudgetSettings,
    #[serde(default)]
    pub outcomes: OutcomeSettings,
    #[serde(default)]
    pub gates: GateSettings,
    #[serde(default)]
    pub lsp: LspSettings,
    #[serde(default)]
    pub roles: RoleSettings,
    /// `[fallback]`: a model glob to the models a failed request on it is sent to, in order.
    #[serde(default)]
    pub fallback: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {path}: {message}")]
    Parse { path: PathBuf, message: String },
}

/// The merged, effective configuration plus warnings about settings that were ignored.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub model: Option<String>,
    pub mode: Option<Mode>,
    pub max_steps: Option<u32>,
    pub compaction: CompactionSettings,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub confirm: Vec<String>,
    pub read_dirs: Vec<PathBuf>,
    pub writable_roots: Vec<PathBuf>,
    pub allow_localhost: bool,
    pub linux_git_protection: LinuxGitProtection,
    /// Model profiles by model-id glob: the global config's, with a trusted project's over them.
    pub profiles: BTreeMap<String, ProfileSettings>,
    pub notifications: Notifications,
    /// The user's prices by model-id glob (global config only: a project cannot make a model
    /// look free).
    pub pricing: BTreeMap<String, PriceSettings>,
    /// `[usage]` (global config only).
    pub usage: UsageSettings,
    /// `[budgets]`: the global config's, which a project may only lower.
    pub budgets: BudgetSettings,
    /// Whether the outcome log is turned off (`[outcomes] enabled = false`; it is on by default;
    /// global config only).
    pub outcomes_disabled: bool,
    /// The verification gates: the global config's, with a trusted project's over them.
    pub gates: Gates,
    /// The language servers' settings: the global config's, with a project's over them.
    pub lsp: LspConfig,
    /// The model roles: the global config's, with a trusted project's over them.
    pub roles: RoleConfig,
    /// The fallback chains by model-id glob: the global config's, with a trusted project's over
    /// them.
    pub fallback: BTreeMap<String, Vec<String>>,
    /// Whether the user trusted this workspace with its project settings as they are now
    /// (`harness trust`), so that their widening settings apply. A workspace with no such
    /// settings can be trusted too. A project command file's `model` applies only then.
    pub trusted: bool,
    /// What the user answered to "start language servers here?" for this workspace, if they did.
    /// A trusted workspace starts them whatever this says.
    pub lsp_servers_allowed: Option<bool>,
    pub warnings: Vec<String>,
}

/// Parses one config file. A missing file is `Ok(None)`; an invalid one is an error naming file and line.
pub fn parse_file(path: &Path) -> Result<Option<ConfigFile>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut file: ConfigFile = toml::from_str(&text).map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        message: toml_error(&text, &e),
    })?;
    for provider in file.providers.values_mut() {
        provider.file = Some(path.to_path_buf());
    }
    if let Some(name) = file
        .providers
        .keys()
        .find(|name| RESERVED_PROVIDERS.contains(&name.as_str()))
    {
        return Err(ConfigError::Parse {
            path: path.to_path_buf(),
            message: format!(
                "[providers.{name}]: the name `{name}` is reserved for ChatGPT sign-in (`harness login {name}`); give this provider another name"
            ),
        });
    }
    // A key pasted here would be printed wherever the variable is named; the error never echoes it.
    if let Some(name) = file.providers.iter().find_map(|(name, provider)| {
        provider
            .api_key_env
            .as_deref()
            .is_some_and(|var| !is_variable_name(var))
            .then_some(name)
    }) {
        return Err(ConfigError::Parse {
            path: path.to_path_buf(),
            message: format!(
                "[providers.{name}]: `api_key_env` names an environment variable, not a key: give the variable's name (letters, digits and `_`), and keep the key in that variable, or store it with `harness auth add {name}`"
            ),
        });
    }
    Ok(Some(file))
}

/// Whether `name` can name an environment variable: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_variable_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// What is wrong with `text`, which TOML could not read: where (line and column) and why, never
/// the line itself, which TOML's own message quotes and which can hold a key (`api_key = "sk-…"`
/// is a setting of other tools).
pub fn toml_error(text: &str, error: &toml::de::Error) -> String {
    let why = error.message().trim_end();
    match error.span() {
        Some(span) => {
            let before = &text[..floor_char_boundary(text, span.start)];
            let line = before.matches('\n').count() + 1;
            let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
            format!("line {line}, column {column}: {}", why.replace('\n', "; "))
        }
        None => why.replace('\n', "; "),
    }
}

/// The char boundary at or before `i` in `s`, or its end.
fn floor_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Provider names no config may define: `chatgpt` is the account signed in with `harness login
/// chatgpt`, whose stored tokens a provider defined under that name would be handed as its key.
pub const RESERVED_PROVIDERS: [&str; 1] = ["chatgpt"];

/// Project settings that widen what the agent may do, and a fingerprint of them. Trust is granted to a
/// fingerprint, that of the empty set included, so any change to these settings needs trust again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Widening {
    /// Empty when the project has no widening settings.
    pub items: Vec<String>,
    pub fingerprint: String,
}

/// The mode, step limit and Linux git protection in effect without the project config: the global
/// config's, or the defaults. A project setting that does not go beyond them narrows and needs no
/// trust.
#[derive(Debug, Clone, Copy)]
struct Baseline {
    mode: Mode,
    max_steps: u32,
    linux_git_protection: LinuxGitProtection,
}

impl Baseline {
    fn new(global: Option<&ConfigFile>, workspace: &Path) -> Baseline {
        Baseline {
            mode: global
                .and_then(|g| g.mode)
                .unwrap_or_else(|| default_mode(workspace)),
            max_steps: global
                .and_then(|g| g.max_steps)
                .unwrap_or(DEFAULT_MAX_STEPS),
            linux_git_protection: global
                .and_then(|g| g.sandbox.linux_git_protection)
                .unwrap_or_default(),
        }
    }
}

/// `auto` inside a git work tree (changes are recoverable), `ask` elsewhere.
pub fn default_mode(workspace: &Path) -> Mode {
    if workspace.ancestors().any(|dir| dir.join(".git").exists()) {
        Mode::Auto
    } else {
        Mode::Ask
    }
}

fn widening(project: &ConfigFile, baseline: Baseline) -> Widening {
    let mut items = Vec::new();
    if let Some(mode) = project.mode.filter(|m| !m.grants_at_most(baseline.mode)) {
        items.push(format!("mode = \"{mode}\""));
    }
    if let Some(steps) = project.max_steps.filter(|&n| n > baseline.max_steps) {
        items.push(format!("max_steps = {steps}"));
    }
    if let Some(model) = &project.model {
        items.push(format!("model = {model:?}"));
    }
    for rule in &project.permissions.allow {
        items.push(format!("permissions.allow: {rule:?}"));
    }
    for dir in &project.permissions.read_dirs {
        items.push(format!("permissions.read_dirs: {dir:?}"));
    }
    for (name, provider) in &project.providers {
        items.push(format!(
            "providers.{name:?}: protocol = {:?}, base_url = {:?}, api_key_env = {:?}",
            provider.protocol, provider.base_url, provider.api_key_env
        ));
    }
    for root in &project.sandbox.writable_roots {
        items.push(format!("sandbox.writable_roots: {root:?}"));
    }
    if let Some(true) = project.sandbox.allow_localhost {
        items.push("sandbox.allow_localhost = true".to_string());
    }
    // They choose output limits, reasoning effort and context budgets (paid requests), and
    // whether text is run as tool calls.
    for (key, profile) in &project.profiles {
        items.push(format!("profiles.{key:?}: {}", profile.describe()));
    }
    // Gate commands run project code, and the retry limit spends paid requests.
    items.extend(project.gates.items());
    // Language-server commands run project code.
    items.extend(project.lsp.items());
    // Roles choose which provider a conversation goes to.
    items.extend(project.roles.items());
    // So do chains: a failed request goes to another provider, possibly billed.
    for (glob, ids) in &project.fallback {
        items.push(format!("fallback.{glob:?} = {ids:?}"));
    }
    if project.sandbox.linux_git_protection == Some(LinuxGitProtection::BestEffort)
        && baseline.linux_git_protection == LinuxGitProtection::Required
    {
        items.push("sandbox.linux_git_protection = \"best-effort\"".to_string());
    }
    // It does not widen what the agent may do, but it needs trust all the same (ruling P3-R4).
    if let Some(p) = low_threshold(project) {
        items.push(format!("{THRESHOLD_ITEM}{p}"));
    }
    // No item is empty, so only the empty set joins to "".
    let fingerprint = hex::encode(Sha256::digest(items.join("\n").as_bytes()));
    Widening { items, fingerprint }
}

/// How a project's too-low compaction threshold is listed among the settings that need trust.
const THRESHOLD_ITEM: &str = "compaction.threshold_percent = ";

/// The project's compaction threshold when it is below [`MIN_UNTRUSTED_THRESHOLD_PERCENT`].
fn low_threshold(project: &ConfigFile) -> Option<u8> {
    project
        .compaction
        .threshold_percent
        .filter(|&p| p < MIN_UNTRUSTED_THRESHOLD_PERCENT)
}

pub fn project_file(workspace: &Path) -> PathBuf {
    workspace.join(".harness").join("config.toml")
}

/// The widening settings in the workspace's project config, possibly none (shown and trusted by
/// `harness trust`). Whether a mode or step limit widens depends on the global config, so it is
/// read too.
pub fn project_widening(global_file: &Path, workspace: &Path) -> Result<Widening, ConfigError> {
    let global = parse_file(global_file)?;
    let baseline = Baseline::new(global.as_ref(), workspace);
    let path = project_file(workspace);
    let project = parse_file(&path)?.unwrap_or_default();
    // Settings that would be invalid once trusted cannot be trusted.
    let global_compaction = global.map(|g| g.compaction).unwrap_or_default();
    if let Some(message) = project
        .compaction
        .out_of_range()
        .or_else(|| global_compaction.overlaid(&project.compaction).problem())
        .or_else(|| profiles_problem(&project.profiles))
        .or_else(|| project.gates.problem())
        .or_else(|| project.lsp.problem())
        .or_else(|| project.roles.problem())
        .or_else(|| fallback_problem(&project.fallback))
    {
        return Err(ConfigError::Parse { path, message });
    }
    Ok(widening(&project, baseline))
}

/// Whether the user trusts `dir`, as `harness trust` run there records it: trust for `dir` holds
/// the fingerprint of `dir`'s own project settings. Settings that cannot be read are not trusted.
pub fn is_trusted(global_file: &Path, dir: &Path, trust: &TrustStore) -> bool {
    project_widening(global_file, dir).is_ok_and(|w| trust.is_trusted(dir, &w.fingerprint))
}

/// Loads the global config, then the workspace's `.harness/config.toml`. Project settings that narrow
/// what the agent may do always apply; widening ones apply only when `trust` holds their fingerprint,
/// which also makes the workspace trusted (`Config::trusted`) when it has none.
pub fn load(
    global_file: &Path,
    workspace: &Path,
    trust: &TrustStore,
) -> Result<Config, ConfigError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let home = home.as_deref();
    let mut cfg = Config::default();
    let mut gates = GateSettings::default();
    let mut roles = RoleSettings::default();
    let global = parse_file(global_file)?;
    let baseline = Baseline::new(global.as_ref(), workspace);
    if let Some(message) = global.as_ref().and_then(|g| {
        g.compaction
            .problem()
            .or_else(|| profiles_problem(&g.profiles))
            .or_else(|| pricing_problem(&g.pricing))
            .or_else(|| g.budgets.problem())
            .or_else(|| g.gates.problem())
            .or_else(|| g.lsp.problem())
            .or_else(|| g.roles.problem())
            .or_else(|| fallback_problem(&g.fallback))
    }) {
        return Err(ConfigError::Parse {
            path: global_file.to_path_buf(),
            message,
        });
    }
    if let Some(global) = global {
        let base = global_file.parent().unwrap_or(Path::new("/"));
        cfg.model = global.model;
        cfg.mode = global.mode;
        cfg.max_steps = global.max_steps;
        cfg.providers = global.providers;
        cfg.allow = global.permissions.allow;
        cfg.deny = global.permissions.deny;
        cfg.confirm = global.permissions.confirm;
        cfg.compaction = global.compaction;
        cfg.read_dirs = expand_all(&global.permissions.read_dirs, base, home);
        cfg.writable_roots = expand_all(&global.sandbox.writable_roots, base, home);
        cfg.allow_localhost = global.sandbox.allow_localhost.unwrap_or(false);
        cfg.linux_git_protection = global.sandbox.linux_git_protection.unwrap_or_default();
        cfg.profiles = global.profiles;
        cfg.pricing = global.pricing;
        cfg.usage = global.usage;
        cfg.budgets = global.budgets;
        cfg.outcomes_disabled = global.outcomes.enabled == Some(false);
        cfg.notifications = cfg.notifications.overlaid(&global.notifications);
        gates = global.gates;
        roles = global.roles;
        cfg.fallback = global.fallback;
        cfg.lsp.overlaid(&global.lsp, true);
    }
    let path = project_file(workspace);
    let project = parse_file(&path)?;
    let widening = widening(project.as_ref().unwrap_or(&ConfigFile::default()), baseline);
    cfg.trusted = trust.is_trusted(workspace, &widening.fingerprint);
    cfg.lsp_servers_allowed = trust.servers_answer(workspace);
    if let Some(project) = project {
        if let Some(message) = project
            .compaction
            .out_of_range()
            .or_else(|| profiles_problem(&project.profiles))
            .or_else(|| project.gates.problem())
            .or_else(|| project.lsp.problem())
            .or_else(|| project.roles.problem())
            .or_else(|| fallback_problem(&project.fallback))
        {
            return Err(ConfigError::Parse { path, message });
        }
        // Turning servers off and the wait narrow what harness does; commands wait for trust.
        cfg.lsp.overlaid(&project.lsp, cfg.trusted);
        cfg.notifications = cfg.notifications.overlaid(&project.notifications);
        if !project.pricing.is_empty() {
            cfg.warnings.push(format!(
                "{}: ignoring [pricing]: prices are read from the global config only, so a cloned repository cannot make a model look free",
                path.display()
            ));
        }
        if let Some(message) = project.budgets.problem() {
            return Err(ConfigError::Parse { path, message });
        }
        let (budgets, raised) = cfg.budgets.tightened_by(&project.budgets);
        cfg.budgets = budgets;
        for name in raised {
            cfg.warnings.push(format!(
                "{}: ignoring budgets.{name}: a project may lower a budget but not raise it",
                project_file(workspace).display()
            ));
        }
        if project.outcomes != OutcomeSettings::default() {
            cfg.warnings.push(format!(
                "{}: ignoring [outcomes]: it is read from the global config only",
                path.display()
            ));
        }
        if project.usage != UsageSettings::default() {
            cfg.warnings.push(format!(
                "{}: ignoring [usage]: it is read from the global config only",
                path.display()
            ));
        }
        cfg.deny.extend(project.permissions.deny.iter().cloned());
        cfg.confirm
            .extend(project.permissions.confirm.iter().cloned());
        // Compaction settings change when the conversation is summarized, not what the agent
        // may do, so they apply without trust, except a threshold low enough to summarize (a paid
        // request that drops verbatim context) every few turns (ruling P3-R4).
        // The project's settings are checked as they would apply once trusted, so a config that
        // is invalid then is invalid now too.
        let as_trusted = cfg.compaction.overlaid(&project.compaction);
        if let Some(message) = as_trusted.problem() {
            return Err(ConfigError::Parse { path, message });
        }
        match low_threshold(&project).filter(|_| !cfg.trusted) {
            None => cfg.compaction = as_trusted,
            Some(p) => {
                // The global threshold or the default applies, and the project's keep share
                // with it only while it is below that threshold.
                let with_keep = CompactionSettings {
                    threshold_percent: cfg.compaction.threshold_percent,
                    ..as_trusted
                };
                let mut ignored = format!("{THRESHOLD_ITEM}{p}");
                if with_keep.problem().is_none() {
                    cfg.compaction = with_keep;
                } else if let Some(keep) = project.compaction.keep_recent_percent {
                    ignored.push_str(&format!(" and keep_recent_percent = {keep}"));
                }
                cfg.warnings.push(format!(
                    "{}: ignoring {ignored}: a project may compact below {MIN_UNTRUSTED_THRESHOLD_PERCENT}% of the context window only in a trusted workspace, so {}% applies, keeping {}%; run `harness trust` to review and apply it",
                    path.display(),
                    (cfg.compaction.threshold() * 100.0).round(),
                    (cfg.compaction.keep_recent() * 100.0).round()
                ));
            }
        }
        if let Some(steps) = project.max_steps.filter(|&n| n <= baseline.max_steps) {
            cfg.max_steps = Some(steps);
        }
        if let Some(mode) = project.mode.filter(|m| m.grants_at_most(baseline.mode)) {
            cfg.mode = Some(mode);
        }
        if let Some(false) = project.sandbox.allow_localhost {
            cfg.allow_localhost = false;
        }
        if let Some(LinuxGitProtection::Required) = project.sandbox.linux_git_protection {
            cfg.linux_git_protection = LinuxGitProtection::Required;
        }
        // The threshold has its own warning above.
        let widening_items: Vec<&String> = widening
            .items
            .iter()
            .filter(|item| !item.starts_with(THRESHOLD_ITEM))
            .collect();
        if !widening_items.is_empty() {
            if cfg.trusted {
                if project.mode.is_some() {
                    cfg.mode = project.mode;
                }
                if project.max_steps.is_some() {
                    cfg.max_steps = project.max_steps;
                }
                if project.model.is_some() {
                    cfg.model = project.model.clone();
                }
                cfg.allow.extend(project.permissions.allow.iter().cloned());
                cfg.read_dirs
                    .extend(expand_all(&project.permissions.read_dirs, workspace, home));
                cfg.providers.extend(project.providers.clone());
                cfg.writable_roots.extend(expand_all(
                    &project.sandbox.writable_roots,
                    workspace,
                    home,
                ));
                if let Some(allow) = project.sandbox.allow_localhost {
                    cfg.allow_localhost = allow;
                }
                if let Some(protection) = project.sandbox.linux_git_protection {
                    cfg.linux_git_protection = protection;
                }
                for (key, profile) in &project.profiles {
                    let merged = cfg.profiles.entry(key.clone()).or_default();
                    *merged = merged.overlaid(profile);
                }
                gates = gates.overlaid(&project.gates);
                roles = roles.overlaid(&project.roles);
                cfg.fallback.extend(project.fallback.clone());
            } else {
                let items: Vec<&str> = widening_items.iter().map(|item| item.as_str()).collect();
                cfg.warnings.push(format!(
                    "{}: ignoring {} setting(s) that widen what the agent may do ({}); run `harness trust` to review and apply them",
                    path.display(),
                    items.len(),
                    items.join("; ")
                ));
            }
        }
    }
    // A detected gate the user confirmed is configuration, when none is configured.
    if gates.after_edit.is_none()
        && gates.test.is_none()
        && let Some(answer) = trust.gate_answer(workspace).filter(|a| a.confirmed)
    {
        gates.after_edit = answer.after_edit.clone();
        gates.test = answer.test.clone();
    }
    cfg.gates = gates.resolve();
    cfg.roles = roles.resolve();
    Ok(cfg)
}

fn expand_all(paths: &[String], base: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    paths.iter().map(|p| expand(p, base, home)).collect()
}

/// `~` and `~/x` expand to the home directory; other relative paths are relative to `base`.
fn expand(path: &str, base: &Path, home: Option<&Path>) -> PathBuf {
    if let (Some(rest), Some(home)) = (path.strip_prefix('~'), home)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return home.join(rest.trim_start_matches('/'));
    }
    if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        base.join(path)
    }
}

/// Saves `id` as the default model in the global config file `global`, as the first-run model
/// choice does: `model = "<id>"` goes first in the file, where a top-level key must be, and the
/// rest stays as written, comments included. The file is created when missing, and replaced
/// through a temporary file, keeping its permissions; a new file is private (0600), and so is a
/// directory made for it (0700). A file that is a link is written through: the file it names is
/// the one replaced, beside it. A file that is not valid TOML, or that sets a model already, is
/// left alone.
pub fn save_default_model(global: &Path, id: &str) -> std::io::Result<()> {
    use std::{
        io::Write,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    };
    let global = &through_links(global)?;
    let existing = match std::fs::read_to_string(global) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let text = existing.clone().unwrap_or_default();
    let table: toml::Table = text.parse().map_err(|e| {
        std::io::Error::other(format!("{} is not valid TOML: {e}", global.display()))
    })?;
    if table.contains_key("model") {
        return Err(std::io::Error::other(format!(
            "{} already sets model",
            global.display()
        )));
    }
    let line = format!("model = {}\n", toml::Value::String(id.to_string()));
    if let Some(dir) = global.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let tmp = global.with_file_name(format!(
        "{}.tmp-{}",
        global
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        if existing.is_some() {
            let mode = std::fs::metadata(global)?.permissions().mode();
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(line.as_bytes())?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, global)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// `path`, or, when it is a link, where the links lead (which need not exist yet).
fn through_links(path: &Path) -> std::io::Result<PathBuf> {
    let mut path = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&path)?;
                path = match path.parent() {
                    Some(dir) => dir.join(target),
                    None => target,
                };
            }
            _ => return Ok(path),
        }
    }
    Err(std::io::Error::other(format!(
        "too many links to follow from {}",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_expand_home_and_relative_to_the_base() {
        let home = Path::new("/home/u");
        let base = Path::new("/work/proj");
        assert_eq!(expand("~", base, Some(home)), PathBuf::from("/home/u"));
        assert_eq!(
            expand("~/.cargo", base, Some(home)),
            PathBuf::from("/home/u/.cargo")
        );
        assert_eq!(
            expand("~other/x", base, Some(home)),
            PathBuf::from("/work/proj/~other/x")
        );
        assert_eq!(expand("/opt/x", base, Some(home)), PathBuf::from("/opt/x"));
        assert_eq!(
            expand("cache", base, Some(home)),
            PathBuf::from("/work/proj/cache")
        );
    }
}
