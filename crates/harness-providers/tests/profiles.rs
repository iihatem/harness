//! Model profiles: the user's settings, then the built-in profiles, then protocol defaults, each
//! setting on its own, the most specific key winning within a layer.

use std::collections::BTreeMap;

use harness_config::config::ProfileSettings;
use harness_core::message::RequestOptions;
use harness_providers::profiles::{
    DEFAULT_MIN_CONTEXT, ModelProfile, builtin_profiles, is_local, resolve,
};

fn user(entries: &[(&str, ProfileSettings)]) -> BTreeMap<String, ProfileSettings> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn temperature(t: f64) -> ProfileSettings {
    ProfileSettings {
        temperature: Some(t),
        ..ProfileSettings::default()
    }
}

fn window(tokens: u64) -> ProfileSettings {
    ProfileSettings {
        context_window: Some(tokens),
        ..ProfileSettings::default()
    }
}

// Spec: "User profile overrides built-in".
#[test]
fn the_users_profile_overrides_the_builtin_one() {
    let builtin = resolve("ollama/qwen3-coder:30b", true, &BTreeMap::new());
    assert_eq!(builtin.temperature, Some(0.7));
    let mine = user(&[("ollama/qwen3-coder*", temperature(0.2))]);
    let profile = resolve("ollama/qwen3-coder:30b", true, &mine);
    assert_eq!(profile.temperature, Some(0.2));
    // Settings the user left alone still come from the built-in profile.
    assert_eq!(profile.context_window, builtin.context_window);
}

// Spec: "The most specific key wins".
#[test]
fn the_most_specific_key_wins() {
    let mine = user(&[
        ("ollama/*", window(65_536)),
        ("ollama/qwen3-coder*", window(16_384)),
    ]);
    assert_eq!(
        resolve("ollama/qwen3-coder:30b", true, &mine).context_window,
        Some(16_384)
    );
    assert_eq!(
        resolve("ollama/llama3.1", true, &mine).context_window,
        Some(65_536)
    );
    // The same rule holds among the built-in profiles: qwen3-coder over qwen3.
    let none = BTreeMap::new();
    assert_eq!(
        resolve("ollama/qwen3:14b", true, &none).context_window,
        Some(32_768)
    );
    assert_eq!(
        resolve("ollama/qwen3-coder:30b", true, &none).context_window,
        Some(262_144)
    );
}

#[test]
fn keys_match_without_regard_to_case_and_across_slashes() {
    let none = BTreeMap::new();
    assert_eq!(
        resolve("lmstudio/Qwen/Qwen3-Coder-30B-A3B-Instruct", true, &none).context_window,
        Some(262_144)
    );
    assert_eq!(
        resolve("openrouter/anthropic/claude-sonnet-4.5", false, &none).context_window,
        Some(200_000)
    );
}

#[test]
fn defaults_depend_on_whether_the_model_is_local() {
    let none = BTreeMap::new();
    let local = resolve("ollama/some-new-model", true, &none);
    assert_eq!(
        local,
        ModelProfile {
            context_window: None,
            min_context: DEFAULT_MIN_CONTEXT,
            max_output_tokens: None,
            temperature: None,
            reasoning_effort: None,
            text_tool_calls: true,
            local: true,
            edit_format: Default::default(),
        }
    );
    let hosted = resolve("openrouter/some-new-model", false, &none);
    assert!(!hosted.local);
    assert!(!hosted.text_tool_calls);
    // A profile can say otherwise: a hosted open-weight model that writes tool calls as text.
    let mine = user(&[(
        "openrouter/*",
        ProfileSettings {
            text_tool_calls: Some(true),
            ..ProfileSettings::default()
        },
    )]);
    assert!(resolve("openrouter/some-new-model", false, &mine).text_tool_calls);
}

#[test]
fn hosted_model_families_know_their_windows() {
    let none = BTreeMap::new();
    for (id, tokens) in [
        ("anthropic/claude-sonnet-4-5", 200_000),
        ("chatgpt/gpt-5.5", 272_000),
        ("openai/gpt-5", 272_000),
        ("openai/gpt-4.1-mini", 1_047_576),
    ] {
        assert_eq!(
            resolve(id, false, &none).context_window,
            Some(tokens),
            "{id}"
        );
    }
}

#[test]
fn every_builtin_key_is_a_valid_glob() {
    for (key, _) in builtin_profiles() {
        assert!(globset::Glob::new(key).is_ok(), "{key} is not a valid glob");
    }
}

#[test]
fn local_means_a_local_server_or_the_loopback_interface() {
    assert!(is_local("ollama/m", "http://gpu-box:11434/v1"));
    assert!(is_local("lmstudio/m", "http://127.0.0.1:1234/v1"));
    assert!(is_local("mine/m", "http://localhost:8000/v1"));
    assert!(is_local("mine/m", "http://127.0.0.2:8000/v1"));
    assert!(is_local("mine/m", "http://[::1]:8000/v1"));
    assert!(!is_local("mine/m", "https://llm.example/v1"));
    assert!(!is_local("openai/gpt-5", "https://api.openai.com/v1"));
}

#[test]
fn a_profile_gives_the_request_options() {
    let mine = user(&[(
        "openai/*",
        ProfileSettings {
            max_output_tokens: Some(8_000),
            temperature: Some(0.3),
            reasoning_effort: Some("high".into()),
            ..ProfileSettings::default()
        },
    )]);
    assert_eq!(
        resolve("openai/gpt-5", false, &mine).request_options(),
        RequestOptions {
            max_output_tokens: Some(8_000),
            temperature: Some(0.3),
            reasoning_effort: Some("high".into()),
            local: false,
        }
    );
    // Ruling on review A M7: a local model's requests say so, and wait longer for a reply.
    assert!(
        resolve("ollama/qwen3-coder:30b", true, &mine)
            .request_options()
            .local
    );
}

mod edit_format {
    use super::*;
    use harness_core::edit_format::EditFormat;

    fn format(format: EditFormat) -> ProfileSettings {
        ProfileSettings {
            edit_format: Some(format),
            ..ProfileSettings::default()
        }
    }

    // Spec "Edit format unset": the edit tool (`str_replace`).
    #[test]
    fn no_layer_setting_it_gives_str_replace() {
        assert_eq!(
            resolve("ollama/llama3.1", true, &BTreeMap::new()).edit_format,
            EditFormat::StrReplace
        );
    }

    // Spec "Edit format from a profile": the user's config sets it, a built-in profile for the
    // same glob sets none.
    #[test]
    fn the_users_profile_sets_the_format_over_a_builtin_one_that_sets_none() {
        let mine = user(&[("ollama/qwen3-coder*", format(EditFormat::ApplyPatch))]);
        let profile = resolve("ollama/qwen3-coder:30b", true, &mine);
        assert_eq!(profile.edit_format, EditFormat::ApplyPatch);
        // The built-in window is still there: each setting is resolved on its own.
        assert_eq!(profile.context_window, Some(262_144));
    }

    #[test]
    fn the_most_specific_key_wins() {
        let mine = user(&[
            ("openai/*", format(EditFormat::ApplyPatch)),
            ("openai/gpt-5*", format(EditFormat::WholeFile)),
        ]);
        assert_eq!(
            resolve("openai/gpt-5", false, &mine).edit_format,
            EditFormat::WholeFile
        );
        assert_eq!(
            resolve("openai/gpt-4o", false, &mine).edit_format,
            EditFormat::ApplyPatch
        );
    }

    #[test]
    fn the_format_does_not_change_the_other_settings() {
        let profile = resolve(
            "openai/gpt-5",
            false,
            &user(&[("openai/gpt-5", format(EditFormat::Hashline))]),
        );
        assert_eq!(profile.context_window, Some(272_000));
    }

    // Spec: a built-in profile sets `edit_format` to something other than `str_replace` only where
    // a checked-in eval result supports it; none does yet.
    #[test]
    fn no_builtin_profile_changes_the_format_yet() {
        for (key, settings) in builtin_profiles() {
            assert_eq!(settings.edit_format, None, "{key}");
        }
    }
}
