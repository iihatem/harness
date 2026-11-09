use std::{ffi::OsString, os::unix::fs::PermissionsExt, path::Path, time::Duration};

use harness_delegate::{
    BackendKind, DelegateError,
    detect::{DetectOptions, Version, detect, parse_version, tested_range},
};

fn script(dir: &Path, name: &str, body: &str, mode: u32) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn options(dir: &Path) -> DetectOptions {
    DetectOptions {
        path_var: Some(OsString::from(dir)),
        ..DetectOptions::default()
    }
}

#[test]
fn both_programs_version_lines_parse() {
    assert_eq!(
        parse_version("2.1.312 (Claude Code)\n"),
        Some(Version {
            major: 2,
            minor: 1,
            patch: 312
        })
    );
    assert_eq!(
        parse_version("codex-cli 0.154.0\n"),
        Some(Version {
            major: 0,
            minor: 154,
            patch: 0
        })
    );
    assert_eq!(
        parse_version("v1.2.3-beta.1"),
        Some(Version {
            major: 1,
            minor: 2,
            patch: 3
        })
    );
    assert_eq!(parse_version("no version here"), None);
}

#[test]
fn the_tested_ranges_hold_the_tested_versions_and_not_the_next_minor() {
    let claude = tested_range(BackendKind::Claude);
    assert!(claude.contains(Version {
        major: 2,
        minor: 1,
        patch: 312
    }));
    assert!(!claude.contains(Version {
        major: 2,
        minor: 2,
        patch: 0
    }));
    assert!(!claude.contains(Version {
        major: 2,
        minor: 1,
        patch: 10
    }));
    let codex = tested_range(BackendKind::Codex);
    assert!(codex.contains(Version {
        major: 0,
        minor: 154,
        patch: 0
    }));
    assert!(codex.contains(Version {
        major: 0,
        minor: 154,
        patch: 9
    }));
    assert!(!codex.contains(Version {
        major: 0,
        minor: 155,
        patch: 0
    }));
    assert!(!codex.contains(Version {
        major: 1,
        minor: 154,
        patch: 0
    }));
}

#[tokio::test]
async fn a_binary_on_path_in_range_is_found_and_read() {
    let dir = tempfile::tempdir().unwrap();
    script(dir.path(), "claude", "echo '2.1.312 (Claude Code)'", 0o755);
    let found = detect(BackendKind::Claude, &options(dir.path()))
        .await
        .unwrap();
    assert_eq!(found.path, dir.path().join("claude"));
    assert_eq!(
        found.version,
        Version {
            major: 2,
            minor: 1,
            patch: 312
        }
    );
    assert!(found.tested);
}

#[tokio::test]
async fn a_missing_binary_says_what_to_install() {
    let dir = tempfile::tempdir().unwrap();
    let err = detect(BackendKind::Codex, &options(dir.path()))
        .await
        .unwrap_err();
    assert!(matches!(err, DelegateError::NotFound("codex")), "{err}");
    assert!(err.to_string().contains("codex"));
}

#[tokio::test]
async fn a_file_that_is_not_executable_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    script(dir.path(), "codex", "echo 'codex-cli 0.154.0'", 0o644);
    let err = detect(BackendKind::Codex, &options(dir.path()))
        .await
        .unwrap_err();
    assert!(matches!(err, DelegateError::NotFound(_)), "{err}");
}

#[tokio::test]
async fn the_first_path_entry_wins() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    script(a.path(), "codex", "echo 'codex-cli 0.154.1'", 0o755);
    script(b.path(), "codex", "echo 'codex-cli 0.154.2'", 0o755);
    let path = std::env::join_paths([a.path(), b.path()]).unwrap();
    let opts = DetectOptions {
        path_var: Some(path),
        ..DetectOptions::default()
    };
    let found = detect(BackendKind::Codex, &opts).await.unwrap();
    assert_eq!(found.version.patch, 1);
}

#[tokio::test]
async fn a_version_outside_the_range_is_refused_with_the_range_in_the_message() {
    let dir = tempfile::tempdir().unwrap();
    script(dir.path(), "codex", "echo 'codex-cli 0.160.0'", 0o755);
    let err = detect(BackendKind::Codex, &options(dir.path()))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(
        matches!(err, DelegateError::VersionOutOfRange { .. }),
        "{text}"
    );
    assert!(text.contains("0.160.0") && text.contains("0.154"), "{text}");
}

#[tokio::test]
async fn allowing_untested_versions_lets_it_through_and_marks_it() {
    let dir = tempfile::tempdir().unwrap();
    script(dir.path(), "claude", "echo '3.0.0 (Claude Code)'", 0o755);
    let opts = DetectOptions {
        allow_untested: true,
        ..options(dir.path())
    };
    let found = detect(BackendKind::Claude, &opts).await.unwrap();
    assert!(!found.tested);
}

#[tokio::test]
async fn unreadable_version_output_is_an_error_naming_the_output() {
    let dir = tempfile::tempdir().unwrap();
    script(dir.path(), "claude", "echo 'hello there'", 0o755);
    let err = detect(BackendKind::Claude, &options(dir.path()))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DelegateError::VersionUnreadable { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("hello there"));
}

#[tokio::test]
async fn a_binary_that_hangs_on_version_is_stopped() {
    let dir = tempfile::tempdir().unwrap();
    script(dir.path(), "claude", "sleep 30", 0o755);
    let opts = DetectOptions {
        version_timeout: Duration::from_millis(300),
        ..options(dir.path())
    };
    let started = std::time::Instant::now();
    let err = detect(BackendKind::Claude, &opts).await.unwrap_err();
    assert!(
        matches!(err, DelegateError::VersionUnreadable { .. }),
        "{err}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn an_explicit_binary_is_used_without_searching_path() {
    let dir = tempfile::tempdir().unwrap();
    script(
        dir.path(),
        "my-claude",
        "echo '2.1.400 (Claude Code)'",
        0o755,
    );
    let opts = DetectOptions {
        binary: Some(dir.path().join("my-claude")),
        path_var: Some(OsString::new()),
        ..DetectOptions::default()
    };
    let found = detect(BackendKind::Claude, &opts).await.unwrap();
    assert_eq!(found.version.patch, 400);
}
