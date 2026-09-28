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
// Ruling P3-R5: they are never written into the command; a prelude harness writes sets them as
// shell parameters, each single-quoted, and the command is the body as written.
#[test]
fn arguments_inside_a_shell_expansion_are_quoted() {
    assert_eq!(
        parts("!`git log --grep \"$1\"`", "\"x'; rm -rf ~; echo '\""),
        [Shell(
            [
                r#"ARGUMENTS='"x'\''; rm -rf ~; echo '\''"'; "#,
                r#"set -- 'x'\''; rm -rf ~; echo '\'''; "#,
                r#"git log --grep "$1""#,
            ]
            .concat()
        )]
    );
    assert_eq!(
        parts("!`echo \"$ARGUMENTS\"`", "a b"),
        [Shell(
            r#"ARGUMENTS='a b'; set -- 'a' 'b'; echo "$ARGUMENTS""#.into()
        )]
    );
    assert_eq!(
        parts("!`echo \"$@\"`", ""),
        [Shell(r#"ARGUMENTS=''; set --; echo "$@""#.into())]
    );
}

// A shell command that does not use the arguments runs as written.
#[test]
fn a_shell_expansion_without_arguments_has_no_prelude() {
    assert_eq!(parts("!`git diff`", ""), [Shell("git diff".into())]);
    // `$10` is `$1` then `0` to the shell, so it is a use.
    for body in [
        "!`echo ${1}`",
        "!`echo $10`",
        "!`echo $*`",
        "!`echo ${#}`",
        "!`echo ${!1}x`",
    ] {
        let got = parts(body, "x");
        assert!(
            matches!(got.as_slice(), [Shell(c)] if c.starts_with("ARGUMENTS='x'; set -- 'x'; "))
                || matches!(got.as_slice(), [Text(t)] if t.starts_with("[not expanded")),
            "{body}: {got:?}"
        );
    }
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

/// Runs `command` with `/bin/bash -c` in an empty directory, and returns its stdout.
fn run_bash(command: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new("/bin/bash")
        .arg("-c")
        .arg(command)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "{command:?} wrote a file"
    );
    out.stdout
}

/// Every shell metacharacter, every control character but NUL, and whitespace, as plain text.
fn sentinel() -> String {
    let mut text = String::from("A'B\"C$D`E\\F;G|H&I(J)K<L>M[N]O{P}Q*R?S~T#U!V%W^X");
    text.push_str("\n\t =");
    text.extend((1u8..0x20).chain([0x7f]).map(char::from));
    text.push_str("Y'$(Z)'");
    text
}

/// `values`, each followed by a NUL, as `printf '%s\0'` prints them.
fn nul_separated(values: &[&str]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|v| [v.as_bytes(), b"\0"].concat())
        .collect()
}

// Ruling P3-R5: the arguments reach a command file's shell command as its parameters, byte for
// byte, whatever they contain.
#[test]
fn arguments_reach_a_shell_expansion_byte_for_byte() {
    let sentinel = sentinel();
    let one = arg(&sentinel);
    assert_eq!(split_args(&one), [sentinel.as_str()]);
    let flag = format!("--flag={sentinel}");
    for (body, args, expected) in [
        (
            r#"!`printf '%s\0' "$1"`"#,
            one.clone(),
            vec![sentinel.as_str()],
        ),
        (r#"!`printf '%s\0' "${1}"`"#, one.clone(), vec![&sentinel]),
        (r#"!`printf '%s\0' --flag="$1"`"#, one.clone(), vec![&flag]),
        (
            r#"!`printf '%s\0' "$ARGUMENTS"`"#,
            one.clone(),
            vec![one.trim()],
        ),
        (
            r#"!`printf '%s\0' "${ARGUMENTS}"`"#,
            one.clone(),
            vec![one.trim()],
        ),
        // An unquoted parameter is split and globbed as in any script; this value is neither.
        (
            r#"!`printf '%s\0' $1`"#,
            "plain-value_1.x".into(),
            vec!["plain-value_1.x"],
        ),
        // In single quotes, `$1` is text, as in any script.
        (r#"!`printf '%s\0' '$1'`"#, one.clone(), vec!["$1"]),
    ] {
        let command = shell_part(body, &args);
        assert_eq!(
            run_bash(&command),
            nul_separated(&expected),
            "{body} ran {command:?}"
        );
    }
    // Several arguments, in order.
    let several = format!("{one} \"second word\" \"\" -x");
    let command = shell_part(
        r#"!`printf '%s\0' "$#" "$@"; printf '%s\0' "$2" "$1"`"#,
        &several,
    );
    assert_eq!(
        run_bash(&command),
        nul_separated(&[
            "4",
            &sentinel,
            "second word",
            "",
            "-x",
            "second word",
            &sentinel
        ]),
        "{command:?}"
    );
}

/// A `!` command that uses the arguments with one of these, where bash may evaluate a parameter's
/// value as code, arithmetic or a variable name, and the construct the warning names.
const TRIGGERS: [(&str, &str); 36] = [
    (r#"echo $(( "$1" + 1 ))"#, "`((`"),
    (r#"(( $1 > 0 )) && echo big"#, "`((`"),
    (r#"echo $[ "$1" ]"#, "`$[`"),
    (r#"let "n = $1""#, "`let`"),
    (r#"declare -i n="$1""#, "`declare`"),
    (r#"typeset -i n="$1""#, "`typeset`"),
    (r#"f() { local -i n="$1"; }; f"#, "`local`"),
    (r#"declare "$1"=x"#, "`declare`"),
    (r#"a[0]="$1""#, "a subscript"),
    (r#"echo "${a[$1]}""#, "a subscript"),
    (r#"a=("$1")"#, "an array assignment"),
    (r#"a+=("$1")"#, "an array assignment"),
    (r#"b=x declare -a c=([0]="$1")"#, "an array assignment"),
    (r#"eval "$1""#, "`eval`"),
    (r#"trap "$1" EXIT"#, "`trap`"),
    (r#"read -r "$1" <<< x"#, "`read`"),
    (r#"echo "$1" | read -r x"#, "`read`"),
    (r#"printf -v "$1" %s x"#, "`printf -v`"),
    (r#"printf -vx %s "$1""#, "`printf -v`"),
    (r#"source "$1""#, "`source`"),
    (r#". "$1""#, "`.` as a command"),
    (r#"true && . ./env "$1""#, "`.` as a command"),
    (r#"echo "${!1}""#, "`${!`"),
    (r#"x=$1; echo "${!x}""#, "`${!`"),
    (r#"[[ "$1" -eq 1 ]] && echo one"#, "`[[` with"),
    (r#"[[ -v "$1" ]] && echo set"#, "`[[` with"),
    (r#"[ -v "$1" ] && echo set"#, "`-v` in a test"),
    (r#"test -v "$1" && echo set"#, "`-v` in a test"),
    (r#"unset "$1""#, "`unset`"),
    (r#"mapfile -t "$1" < /dev/null"#, "`mapfile`"),
    (r#"readarray -t "$1" < /dev/null"#, "`readarray`"),
    (r#"x=abc; echo "${x:$1}""#, "a substring"),
    (r#"x=abc; echo "${x:0:$1}""#, "a substring"),
    // Quotes and backslashes do not hide a word from the check.
    (r#"e''val "$1""#, "`eval`"),
    (r#"\eval "$1""#, "`eval`"),
    (r#""read" -r x <<< "$1""#, "`read`"),
];

// Ruling P3-R5: a command that uses the arguments together with any of these is never run.
#[test]
fn a_shell_expansion_using_arguments_where_bash_evaluates_them_is_never_run() {
    for (shell, why) in TRIGGERS {
        let body = format!("Before !`{shell}` after");
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let got = expand(&command(&body), "x y", &ws, &engine(&ws, &[]), false);
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

// The same constructs in a command that does not use the arguments run as written.
#[test]
fn the_same_constructs_without_arguments_run_as_written() {
    for (shell, _) in TRIGGERS {
        let plain = shell.replace("$1", "v").replace("${!1}", "${!v}");
        assert_eq!(
            parts(&format!("!`{plain}`"), ""),
            [Shell(plain.clone())],
            "{plain}"
        );
    }
}

// Ordinary uses of the arguments expand, including words that only look like the triggers.
#[test]
fn ordinary_shell_expansions_using_arguments_expand() {
    for shell in [
        r#"git log --oneline --grep "$1""#,
        r#"echo "$ARGUMENTS" | wc -c"#,
        r#"grep -rn -- "$1" ."#,
        r#"ls . "$1""#,
        r#"grep -v "$1" README.md"#,
        r#"git log -- ./readme "$1" --reads"#,
        r#"[ -n "$1" ] && echo "$1""#,
        r#"[[ -n "$1" && "$1" != x* ]] && echo "$1""#,
        r#"echo "${1:-none}" "${1#x}" "${1%.rs}" "${#1}""#,
        r#"x="$1"; echo "$x""#,
        r#"export NAME="$1"; env | grep -c NAME"#,
        r#"for f in "$@"; do echo "$f"; done"#,
        r#"case "$1" in a) echo a;; esac"#,
        r#"echo 'text [with] brackets' "$1""#,
        r#"echo evaluate reader sourced "$1""#,
    ] {
        let got = parts(&format!("!`{shell}`"), "x");
        assert_eq!(
            got,
            [Shell(format!("ARGUMENTS='x'; set -- 'x'; {shell}"))],
            "{shell}"
        );
    }
}

// A body whose only use of the arguments is in its shell commands takes them there, not appended.
#[test]
fn arguments_used_only_by_a_shell_expansion_are_not_appended() {
    assert_eq!(
        parts("!`printf %s \"$@\"`", "a b"),
        [Shell(
            r#"ARGUMENTS='a b'; set -- 'a' 'b'; printf %s "$@""#.into()
        )]
    );
}

// The prelude never makes a command the command file pre-approves ask (ask mode).
#[test]
fn allowed_tools_cover_the_prelude() {
    use harness_core::permission::{Action, Decision, PermissionPolicy};
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let mut cmd = command("!`git log --grep \"$1\"`");
    cmd.allowed_tools = vec!["Bash(git log:*)".into()];
    let got = expand(&cmd, "\"a b\" c", &ws, &engine(&ws, &[]), false);
    let [Shell(shell)] = got.input.parts.as_slice() else {
        panic!("{:?}", got.input.parts)
    };
    let policy = PermissionEngine::new(EngineConfig {
        mode: Mode::Ask,
        workspace: ws.clone(),
        read_dirs: vec![],
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    });
    assert!(matches!(
        policy.check(&Action::Bash(shell.clone())),
        Decision::Ask(_)
    ));
    policy.set_turn_rules(Some(got.input.rules.clone()));
    assert_eq!(policy.check(&Action::Bash(shell.clone())), Decision::Allow);
    // Without allowed-tools, the command file adds no rules.
    let plain = expand(
        &command("!`git log --grep \"$1\"`"),
        "x",
        &ws,
        &engine(&ws, &[]),
        false,
    );
    assert!(
        plain.input.rules.allow.is_empty(),
        "{:?}",
        plain.input.rules
    );
}
