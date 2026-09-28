use std::path::{Path, PathBuf};

use harness_context::commands::{
    CustomCommand, Scope,
    expand::{allowed_tools_rules, expand},
    split_args,
};
use harness_core::{
    engine::{EngineConfig, PermissionEngine, RuleSet},
    permission::Mode,
    turn::InputPart::{self, Shell, Text},
};

fn command(body: &str) -> CustomCommand {
    CustomCommand {
        name: "review".into(),
        path: PathBuf::from("/p/.claude/commands/review.md"),
        scope: Scope::Project,
        description: None,
        argument_hint: None,
        model: None,
        allowed_tools: vec![],
        body: body.into(),
    }
}

fn engine(workspace: &Path, deny: &[&str]) -> PermissionEngine {
    PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: workspace.to_path_buf(),
        read_dirs: vec![],
        rules: RuleSet {
            deny: deny.iter().map(|d| d.to_string()).collect(),
            ..RuleSet::default()
        },
        sandbox_available: true,
        writes_need_approval: false,
    })
}

fn parts(body: &str, args: &str) -> Vec<InputPart> {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    expand(&command(body), args, &ws, &engine(&ws, &[]), false)
        .input
        .parts
}

fn text(s: &str) -> InputPart {
    Text(s.to_string())
}

#[test]
fn positional_arguments_fill_their_placeholders() {
    assert_eq!(
        parts("Review $1 in $2 mode", "\"src/lib.rs\" strict"),
        [text("Review src/lib.rs in strict mode")]
    );
    assert_eq!(parts("[$3]", "a b"), [text("[]")]);
}

#[test]
fn arguments_fill_the_arguments_placeholder_whole() {
    assert_eq!(
        parts("Fix: $ARGUMENTS.", " the login bug "),
        [text("Fix: the login bug.")]
    );
}

#[test]
fn arguments_are_appended_when_the_body_has_no_placeholder() {
    assert_eq!(
        parts("Propose a change.\n", "add-login"),
        [text("Propose a change.\n\nARGUMENTS: add-login")]
    );
    assert_eq!(
        parts("Propose a change.\n", ""),
        [text("Propose a change.\n")]
    );
}

#[test]
fn a_shell_expansion_becomes_a_shell_part() {
    assert_eq!(
        parts("Diff:\n!`git diff`\nReview it.", ""),
        [
            text("Diff:\n"),
            Shell("git diff".into()),
            text("\nReview it.")
        ]
    );
    // Not a shell expansion: no closing backtick on the line, or an empty command.
    assert_eq!(parts("a !`b\nc`", ""), [text("a !`b\nc`")]);
    assert_eq!(parts("!``", ""), [text("!``")]);
}

// Review Focus: arguments often come from scripts; inside a shell expansion they must stay data.
#[test]
fn arguments_inside_a_shell_expansion_are_quoted() {
    assert_eq!(
        parts("!`git log --grep $1`", "\"x'; rm -rf ~; echo '\""),
        [Shell(r"git log --grep 'x'\''; rm -rf ~; echo '\'''".into())]
    );
    assert_eq!(
        parts("!`echo $ARGUMENTS`", "a b"),
        [Shell("echo 'a b'".into())]
    );
}

#[test]
fn arguments_are_never_expanded_themselves() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    std::fs::write(ws.join("notes.md"), "NOTES").unwrap();
    let got = expand(
        &command("Do: $ARGUMENTS"),
        "@notes.md !`ls`",
        &ws,
        &engine(&ws, &[]),
        false,
    );
    assert_eq!(got.input.parts, [text("Do: @notes.md !`ls`")]);
}

#[test]
fn file_references_expand_to_workspace_files_the_policy_allows() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("notes.md"), "NOTES").unwrap();
    std::fs::write(ws.join("secret.md"), "SECRET").unwrap();
    std::fs::write(dir.path().join("outside.txt"), "OUTSIDE").unwrap();
    let body = "See @notes.md. Mail user@example.com, @missing.md, @secret.md and @../outside.txt";
    let got = expand(
        &command(body),
        "",
        &ws,
        &engine(&ws, &["read:secret.md"]),
        false,
    );
    assert_eq!(
        got.input.parts,
        [text(
            "See NOTES. Mail user@example.com, @missing.md, @secret.md and @../outside.txt"
        )]
    );
    assert_eq!(got.warnings.len(), 2, "{:?}", got.warnings);
    assert!(got.warnings[0].contains("needs approval"));
    assert!(got.warnings[1].contains("outside the workspace"));
}

#[test]
fn an_expansion_carries_what_was_typed_and_the_allowed_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = command("Go.");
    cmd.name = "opsx:propose".into();
    cmd.allowed_tools = vec!["Bash(openspec:*)".into()];
    cmd.model = Some("ollama/x".into());
    let got = expand(
        &cmd,
        " add-login ",
        dir.path(),
        &engine(dir.path(), &[]),
        true,
    );
    assert_eq!(
        got.input.display.as_deref(),
        Some("/opsx:propose add-login")
    );
    assert_eq!(got.input.rules.allow, ["bash:openspec", "bash:openspec *"]);
    assert_eq!(got.model.as_deref(), Some("ollama/x"));
}

// Decision 5 (as changed): a repository's command file may pick the model only once the user
// trusted the workspace; the user's own command files always may.
#[test]
fn a_project_commands_model_applies_only_in_a_trusted_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let policy = engine(dir.path(), &[]);
    let mut cmd = command("Go.");
    cmd.allowed_tools = vec!["Bash(openspec:*)".into()];
    cmd.model = Some("paid/big".into());
    let untrusted = expand(&cmd, "", dir.path(), &policy, false);
    assert_eq!(untrusted.model, None);
    assert_eq!(untrusted.notes.len(), 1, "{:?}", untrusted.notes);
    assert!(
        untrusted.notes[0].contains("/review asks for model paid/big")
            && untrusted.notes[0].contains("trusted workspace"),
        "{}",
        untrusted.notes[0]
    );
    assert!(untrusted.warnings.is_empty(), "{:?}", untrusted.warnings);
    // Only the model is dropped: the rest of the command is expanded as before.
    assert_eq!(untrusted.input.parts, [text("Go.")]);
    assert_eq!(
        untrusted.input.rules.allow,
        ["bash:openspec", "bash:openspec *"]
    );
    let trusted = expand(&cmd, "", dir.path(), &policy, true);
    assert_eq!(trusted.model.as_deref(), Some("paid/big"));
    assert!(trusted.notes.is_empty(), "{:?}", trusted.notes);
    // Without a model there is nothing to note.
    let plain = expand(&command("Go."), "", dir.path(), &policy, false);
    assert!(plain.notes.is_empty(), "{:?}", plain.notes);
}

#[test]
fn a_global_commands_model_applies_in_any_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = command("Go.");
    cmd.scope = Scope::Global;
    cmd.model = Some("paid/big".into());
    for trusted in [false, true] {
        let got = expand(&cmd, "", dir.path(), &engine(dir.path(), &[]), trusted);
        assert_eq!(got.model.as_deref(), Some("paid/big"), "trusted: {trusted}");
        assert!(got.notes.is_empty(), "{:?}", got.notes);
    }
}

#[test]
fn claude_code_tool_names_map_to_harness_rules() {
    let entries: Vec<String> = [
        "Bash",
        "Bash(git add:*)",
        "Bash(npm run test)",
        "Edit",
        "Write(./docs/**)",
        "Edit(//tmp/x)",
        "Read",
        "Grep(src/**)",
        "WebFetch",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let (rules, warnings) = allowed_tools_rules(&entries);
    assert_eq!(
        rules,
        [
            "bash:*",
            "bash:git add",
            "bash:git add *",
            "bash:npm run test",
            "write:*",
            "write:docs/**",
            "write:/tmp/x",
        ]
    );
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("WebFetch"));
}

// Review C, critical 1: a hostile argument must stay data however the command file quotes its
// placeholder.
const HOSTILE: [&str; 13] = [
    "$(touch P)",
    "`touch P`",
    "\"; touch P; \"",
    "'",
    "\\",
    "\n",
    "a\nb",
    "$HOME",
    "'; touch P; '",
    "\\'; touch P; '",
    "x\"$(touch P)\"y",
    "\\\"; touch P; #",
    "",
];

/// `word` written so that [`split_args`] reads it back as one argument.
fn arg(word: &str) -> String {
    format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The one shell part `body` expands to with `args`.
fn shell_part(body: &str, args: &str) -> String {
    match parts(body, args).as_slice() {
        [Shell(command)] => command.clone(),
        other => panic!("{body} with {args:?}: {other:?}"),
    }
}

/// Runs `command` with `/bin/bash -c` in an empty directory. Returns its stdout, and whether the
/// directory is still empty afterwards.
fn run_bash(command: &str) -> (Vec<u8>, bool) {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new("/bin/bash")
        .arg("-c")
        .arg(command)
        .current_dir(dir.path())
        .output()
        .unwrap();
    let empty = std::fs::read_dir(dir.path()).unwrap().next().is_none();
    (out.stdout, empty)
}

#[test]
fn quoted_placeholders_are_encoded_for_where_they_land() {
    let hostile = arg("$(touch P)'`\"\\");
    assert_eq!(
        shell_part("!`echo $1`", &hostile),
        r#"echo '$(touch P)'\''`"\'"#
    );
    assert_eq!(
        shell_part("!`echo \"$1\"`", &hostile),
        r#"echo "\$(touch P)'\`\"\\""#
    );
    assert_eq!(
        shell_part("!`echo '$1'`", &hostile),
        r#"echo '$(touch P)'\''`"\'"#
    );
    assert_eq!(
        shell_part("!`echo --x=\"$ARGUMENTS\"`", "a $b"),
        r#"echo --x="a \$b""#
    );
}

#[test]
fn arguments_reach_a_shell_expansion_byte_for_byte() {
    for word in HOSTILE {
        let args = arg(word);
        assert_eq!(split_args(&args), [word], "{args}");
        let trimmed_newlines = word.trim_end_matches('\n');
        for (body, expected) in [
            ("!`printf %s $1`", word.to_string()),
            ("!`printf %s \"$1\"`", word.to_string()),
            ("!`printf %s '$1'`", word.to_string()),
            ("!`printf %s --x=\"$1\"`", format!("--x={word}")),
            ("!`printf %s --x=$1`", format!("--x={word}")),
            ("!`printf %s \"[$1]\" '<$1>'`", format!("[{word}]<{word}>")),
            ("!`printf %s a#$1`", format!("a#{word}")),
            ("!`printf %s \\$$1`", format!("${word}")),
            ("!`printf %s $\"$1\"`", word.to_string()),
            ("!`printf %s \"${HOME:+}$1\"`", word.to_string()),
            ("!`[[ -n x ]] && printf %s \"$1\"`", word.to_string()),
            ("!`[ -n x ] && printf %s $1`", word.to_string()),
            (
                "!`x=$(printf %s \"$1\"); printf %s \"$x\"`",
                trimmed_newlines.to_string(),
            ),
        ] {
            let command = shell_part(body, &args);
            let (out, empty) = run_bash(&command);
            assert_eq!(
                String::from_utf8_lossy(&out),
                expected,
                "{body} with {word:?} ran {command:?}"
            );
            assert!(empty, "{body} with {word:?} created a file: {command:?}");
        }
    }
    // `$ARGUMENTS` is the whole argument text.
    for text in HOSTILE.iter().map(|w| format!("x {w} y")) {
        for body in [
            "!`printf %s $ARGUMENTS`",
            "!`printf %s \"$ARGUMENTS\"`",
            "!`printf %s '$ARGUMENTS'`",
        ] {
            let command = shell_part(body, &text);
            let (out, empty) = run_bash(&command);
            assert_eq!(
                String::from_utf8_lossy(&out),
                text.trim(),
                "{body} ran {command:?}"
            );
            assert!(empty, "{body} with {text:?} created a file: {command:?}");
        }
    }
}

#[test]
fn a_placeholder_that_cannot_be_quoted_safely_is_never_run() {
    for (shell, why) in [
        (r"echo \$1", "follows a backslash"),
        (r#"echo "\$1""#, "follows a backslash"),
        (r#"echo "$(echo $1)""#, "`$(` inside double quotes"),
        (r#"echo "$(date)" $1"#, "`$(` inside double quotes"),
        ("echo $$1", "follows a `$`"),
        (r#"echo "$$1""#, "follows a `$`"),
        ("echo $'$1'", "inside `$'…'`"),
        ("echo hi # $1", "in a comment"),
        ("[[ $1 -eq 1 ]] && echo yes", "inside `[[ … ]]`"),
        (r#"[[ "$1" -eq 1 ]] && echo yes"#, "inside `[[ … ]]`"),
        ("echo $(( $1 + 1 ))", "arithmetic"),
        ("(( $1 )) && echo yes", "arithmetic"),
        ("echo $[$1]", "inside `[…]`"),
        ("a[$1]=x", "inside `[…]`"),
        ("echo ${X:-$1}", "inside `${…}`"),
        (r#"echo "${X:-$1}""#, "inside `${…}`"),
        (r#"echo ${X:-"a"} $1"#, "inside `${…}`"),
    ] {
        let body = format!("Before !`{shell}` after");
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let got = expand(
            &command(&body),
            "\"$(touch P)\"",
            &ws,
            &engine(&ws, &[]),
            false,
        );
        assert_eq!(
            got.input.parts,
            [text(&format!("Before [not expanded: `{shell}`] after"))],
            "{shell}"
        );
        assert_eq!(got.warnings.len(), 1, "{shell}: {:?}", got.warnings);
        let warning = &got.warnings[0];
        assert!(
            warning.contains("/review") && warning.contains(shell) && warning.contains(why),
            "{shell}: {warning}"
        );
    }
}

#[test]
fn a_file_reference_through_a_symlink_inside_the_workspace_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    std::fs::create_dir(ws.join("docs")).unwrap();
    std::fs::write(ws.join("docs/notes.md"), "NOTES").unwrap();
    std::os::unix::fs::symlink("docs/notes.md", ws.join("notes.md")).unwrap();
    let got = expand(&command("See @notes.md"), "", &ws, &engine(&ws, &[]), false);
    assert_eq!(got.input.parts, [text("See NOTES")]);
}
