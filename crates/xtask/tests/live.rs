//! Live mode's parts that need no model: the configuration a run gets, the metrics read from a
//! run's events, and how they add up.

use harness_core::edit_format::EditFormat;
use xtask::live::{
    Events, RunResult, civil_date, home_config, parse_events, result_file_name, summarize,
};

fn lines(events: &[serde_json::Value]) -> String {
    events.iter().map(|e| format!("{e}\n")).collect()
}

fn requested(id: &str, name: &str) -> serde_json::Value {
    serde_json::json!({"type": "tool_call_requested", "id": id, "name": name, "arguments": "{}"})
}

fn finished(id: &str, is_error: bool) -> serde_json::Value {
    serde_json::json!({"type": "tool_call_finished", "id": id, "output": "x", "is_error": is_error})
}

fn stats(output_tokens: u64) -> serde_json::Value {
    serde_json::json!({"type": "turn_stats", "model": "m", "time_to_first_token_ms": 1, "generation_ms": 1,
        "input_tokens": 5, "output_tokens": output_tokens, "cached_tokens": 0})
}

// The configuration a run gets: the user's, with the model and the format set.
#[test]
fn the_run_config_sets_the_model_and_the_models_edit_format() {
    let text = home_config(None, "ollama/qwen3-coder:30b", EditFormat::ApplyPatch).unwrap();
    let table: toml::Table = text.parse().unwrap();
    assert_eq!(table["model"].as_str(), Some("ollama/qwen3-coder:30b"));
    assert_eq!(
        table["profiles"]["ollama/qwen3-coder:30b"]["edit_format"].as_str(),
        Some("apply_patch")
    );
}

#[test]
fn the_users_config_is_kept_and_its_own_profile_for_the_model_is_merged() {
    let mine = "model = \"other/x\"\n[providers.local]\nprotocol = \"openai-chat\"\nbase_url = \"http://localhost:1/v1\"\n[profiles.\"local/m\"]\ncontext_window = 32768\nedit_format = \"hashline\"\n[profiles.\"local/*\"]\ntemperature = 0.1\n";
    let text = home_config(Some(mine), "local/m", EditFormat::WholeFile).unwrap();
    let table: toml::Table = text.parse().unwrap();
    assert_eq!(table["model"].as_str(), Some("local/m"));
    assert_eq!(
        table["providers"]["local"]["base_url"].as_str(),
        Some("http://localhost:1/v1")
    );
    let profile = &table["profiles"]["local/m"];
    assert_eq!(profile["context_window"].as_integer(), Some(32768));
    assert_eq!(profile["edit_format"].as_str(), Some("whole_file"));
    assert_eq!(
        table["profiles"]["local/*"]["temperature"].as_float(),
        Some(0.1)
    );
}

#[test]
fn a_config_that_is_not_toml_is_an_error() {
    assert!(home_config(Some("model = "), "a/b", EditFormat::StrReplace).is_err());
}

#[test]
fn the_events_of_a_run_give_its_edit_calls_and_tokens() {
    let events = lines(&[
        requested("1", "read"),
        finished("1", false),
        requested("2", "edit"),
        finished("2", true),
        requested("3", "edit"),
        finished("3", false),
        requested("4", "bash"),
        finished("4", true),
        stats(120),
    ]);
    let e = parse_events(&events);
    assert_eq!(
        e,
        Events {
            edit_calls: 2,
            edit_errors: 1,
            first_edit_applied: Some(false),
            output_tokens: 120,
        }
    );
}

#[test]
fn every_edit_tool_counts_and_other_tools_do_not() {
    let events = lines(&[
        requested("1", "apply_patch"),
        finished("1", false),
        requested("2", "hashline_edit"),
        finished("2", false),
        requested("3", "write"),
        finished("3", false),
        requested("4", "grep"),
        finished("4", true),
    ]);
    let e = parse_events(&events);
    assert_eq!(
        (e.edit_calls, e.edit_errors, e.first_edit_applied),
        (3, 0, Some(true))
    );
}

#[test]
fn a_run_with_no_edit_has_no_first_try_and_lines_that_are_not_events_are_skipped() {
    let e = parse_events("not json\n{\"type\": \"turn_started\"}\n\n");
    assert_eq!(e, Events::default());
    assert_eq!(e.first_edit_applied, None);
}

#[test]
fn tokens_are_summed_over_the_turns() {
    let e = parse_events(&lines(&[stats(10), stats(5)]));
    assert_eq!(e.output_tokens, 15);
}

fn run(passed: bool, calls: u32, errors: u32, first: Option<bool>, tokens: u64) -> RunResult {
    RunResult {
        task: "t".into(),
        run: 1,
        passed,
        events: Events {
            edit_calls: calls,
            edit_errors: errors,
            first_edit_applied: first,
            output_tokens: tokens,
        },
        error: None,
    }
}

// The five metrics: pass rate, first-try apply rate, format-error rate, retries, output tokens.
#[test]
fn the_summary_has_the_five_metrics() {
    let s = summarize(&[
        run(true, 1, 0, Some(true), 100),
        run(true, 3, 2, Some(false), 300),
        run(false, 0, 0, None, 50),
        run(false, 2, 1, Some(true), 150),
    ]);
    assert_eq!(s.runs, 4);
    assert_eq!(s.pass_rate, 0.5);
    // Among the three runs that edited: two applied first time.
    assert!((s.first_try_apply_rate - 2.0 / 3.0).abs() < 1e-9);
    // 3 errors in 6 edit calls.
    assert_eq!(s.format_error_rate, 0.5);
    assert_eq!(s.retries_per_run, 0.75);
    assert_eq!(s.output_tokens_per_run, 150.0);
}

#[test]
fn an_empty_summary_has_zeros_not_nan() {
    let s = summarize(&[]);
    assert_eq!(
        (
            s.runs,
            s.pass_rate,
            s.first_try_apply_rate,
            s.format_error_rate
        ),
        (0, 0.0, 0.0, 0.0)
    );
    let s = summarize(&[run(false, 0, 0, None, 0)]);
    assert_eq!((s.first_try_apply_rate, s.format_error_rate), (0.0, 0.0));
}

#[test]
fn a_date_is_made_from_the_epoch_without_a_clock_crate() {
    assert_eq!(civil_date(0), "1970-01-01");
    assert_eq!(civil_date(86_400 * 365), "1971-01-01");
    assert_eq!(civil_date(1_759_363_200), "2025-10-02");
    assert_eq!(civil_date(951_782_400), "2000-02-29");
}

#[test]
fn a_result_file_is_named_by_date_model_and_format() {
    assert_eq!(
        result_file_name(
            "2026-10-02",
            "ollama/qwen3-coder:30b",
            EditFormat::ApplyPatch
        ),
        "2026-10-02-ollama-qwen3-coder-30b-apply_patch.json"
    );
}

// A run that is stopped for taking too long takes what it started with it.
#[test]
fn a_run_that_times_out_is_stopped_with_everything_it_started() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("child.pid");
    let script = dir.path().join("harness");
    std::fs::write(
        &script,
        "#!/bin/sh\nsleep 300 &\necho $! > \"$PID_FILE\"\nsleep 300\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let task = xtask::task::Task {
        id: "t".into(),
        dir: dir.path().to_path_buf(),
        language: "python".into(),
        title: "t".into(),
        prompt: "do it".into(),
        test: "false".into(),
        before: Default::default(),
        after: Default::default(),
    };
    let options = xtask::live::Options {
        model: "p/m".into(),
        format: EditFormat::StrReplace,
        runs: 1,
        harness: script,
        user_config: None,
        timeout: std::time::Duration::from_secs(1),
        env: vec![("PID_FILE".into(), pid_file.display().to_string())],
    };
    let report = xtask::live::run(&[task], &options).unwrap();
    assert!(
        report.runs[0]
            .error
            .as_deref()
            .unwrap()
            .contains("timed out")
    );
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let alive = |pid| nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
    for _ in 0..100 {
        if !alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("the background process {pid} is still running");
}
