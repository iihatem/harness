//! Built-in profiles change an edit format only where a checked-in eval result backs it.

use harness_core::edit_format::EditFormat;
use xtask::results::{Stored, backing, load};

fn stored(model: &str, format: EditFormat, pass_rate: f64) -> Stored {
    Stored {
        model: model.into(),
        format,
        pass_rate,
    }
}

// Spec "A default without a result": no file under eval/results/ covers the family.
#[test]
fn a_format_with_no_result_for_the_family_is_not_backed() {
    let error = backing(&[], "*/qwen3-coder*", EditFormat::ApplyPatch).unwrap_err();
    assert!(
        error.contains("eval/results") && error.contains("qwen3-coder"),
        "{error}"
    );
    let other_family = [stored("openai/gpt-5", EditFormat::ApplyPatch, 0.9)];
    assert!(backing(&other_family, "*/qwen3-coder*", EditFormat::ApplyPatch).is_err());
}

#[test]
fn str_replace_needs_no_result() {
    assert!(backing(&[], "*/qwen3-coder*", EditFormat::StrReplace).is_ok());
}

#[test]
fn a_result_for_the_format_alone_is_not_enough_without_a_baseline() {
    let results = [stored(
        "ollama/qwen3-coder:30b",
        EditFormat::ApplyPatch,
        0.9,
    )];
    let error = backing(&results, "*/qwen3-coder*", EditFormat::ApplyPatch).unwrap_err();
    assert!(error.contains("str_replace"), "{error}");
}

#[test]
fn the_format_must_beat_str_replace_on_the_same_family() {
    let key = "*/qwen3-coder*";
    let worse = [
        stored("ollama/qwen3-coder:30b", EditFormat::StrReplace, 0.8),
        stored("ollama/qwen3-coder:30b", EditFormat::ApplyPatch, 0.7),
    ];
    let error = backing(&worse, key, EditFormat::ApplyPatch).unwrap_err();
    assert!(error.contains("70%") && error.contains("80%"), "{error}");
    let tie = [
        stored("ollama/qwen3-coder:30b", EditFormat::StrReplace, 0.8),
        stored("ollama/qwen3-coder:30b", EditFormat::ApplyPatch, 0.8),
    ];
    assert!(backing(&tie, key, EditFormat::ApplyPatch).is_err());
    let better = [
        stored("ollama/qwen3-coder:30b", EditFormat::StrReplace, 0.6),
        stored("ollama/qwen3-coder:30b", EditFormat::ApplyPatch, 0.8),
    ];
    assert!(backing(&better, key, EditFormat::ApplyPatch).is_ok());
}

// Spec: `hashline` is never a built-in default until a checked-in result shows a gain: the same
// rule, for every non-default format.
#[test]
fn hashline_follows_the_same_rule() {
    let key = "openai/gpt-5*";
    assert!(backing(&[], key, EditFormat::Hashline).is_err());
    let gain = [
        stored("openai/gpt-5", EditFormat::StrReplace, 0.5),
        stored("openai/gpt-5-mini", EditFormat::Hashline, 0.7),
    ];
    // The family is what the key matches: both models are in it.
    assert!(backing(&gain, key, EditFormat::Hashline).is_ok());
    // A result for a model outside the family says nothing about it.
    let outside = [
        stored("openai/gpt-5", EditFormat::StrReplace, 0.5),
        stored("anthropic/claude-opus", EditFormat::Hashline, 0.9),
    ];
    assert!(backing(&outside, key, EditFormat::Hashline).is_err());
}

#[test]
fn keys_match_without_regard_to_case() {
    let results = [
        stored("Ollama/Qwen3-Coder:30b", EditFormat::StrReplace, 0.1),
        stored("Ollama/Qwen3-Coder:30b", EditFormat::WholeFile, 0.5),
    ];
    assert!(backing(&results, "*/qwen3-coder*", EditFormat::WholeFile).is_ok());
}

#[test]
fn results_are_read_from_the_json_the_runner_saves() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("2026-10-02-m-apply_patch.json"),
        r#"{"model": "ollama/m", "format": "apply_patch", "date": "2026-10-02", "runs": [],
            "summary": {"runs": 3, "pass_rate": 0.75, "first_try_apply_rate": 1.0,
            "format_error_rate": 0.0, "retries_per_run": 0.0, "output_tokens_per_run": 10.0}}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("notes.md"), "not a result").unwrap();
    let results = load(dir.path()).unwrap();
    assert_eq!(results, [stored("ollama/m", EditFormat::ApplyPatch, 0.75)]);
    // No directory: no results.
    assert!(load(&dir.path().join("none")).unwrap().is_empty());
}

#[test]
fn a_result_file_that_is_not_valid_is_an_error_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.json"), "{").unwrap();
    assert!(load(dir.path()).unwrap_err().contains("bad.json"));
}

// The check itself, on the repository: every built-in profile that sets a format other than
// `str_replace` is backed by a result in `eval/results/`. None does today, since no live run is
// checked in yet.
#[test]
fn every_builtin_profile_format_is_backed_by_a_checked_in_result() {
    let results =
        load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/results")).unwrap();
    for (key, settings) in harness_providers::profiles::builtin_profiles() {
        if let Some(format) = settings.edit_format {
            backing(&results, key, format).unwrap_or_else(|e| panic!("{key}: {e}"));
        }
    }
}
