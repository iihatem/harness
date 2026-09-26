//! Decision table for `evaluate` and `session_prefixes`.

use std::path::Path;

use harness_shell::{Rules, Verdict, evaluate, glob_match, session_prefixes};

const WS: &str = "/work/proj";

fn rules(allow: &[&str], deny: &[&str], confirm: &[&str]) -> Rules {
    let owned = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect();
    Rules {
        allow: owned(allow),
        deny: owned(deny),
        confirm: owned(confirm),
    }
}

fn default_rules() -> Rules {
    rules(
        &["cargo test*", "git status*", "echo*"],
        &["curl*", "git push*"],
        &[],
    )
}

fn eval_with(rules: &Rules, cmd: &str) -> Verdict {
    evaluate(cmd, rules, Path::new(WS))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Want {
    Allow,
    Unlisted,
    Ask,
    Destructive,
    Deny,
}

fn kind(v: &Verdict) -> Want {
    match v {
        Verdict::Allow => Want::Allow,
        Verdict::Unlisted => Want::Unlisted,
        Verdict::Ask {
            destructive: false, ..
        } => Want::Ask,
        Verdict::Ask {
            destructive: true, ..
        } => Want::Destructive,
        Verdict::Deny { .. } => Want::Deny,
    }
}

fn check(rules: &Rules, table: &[(&str, Want)]) {
    let failures: Vec<String> = table
        .iter()
        .filter_map(|&(cmd, want)| {
            let got = eval_with(rules, cmd);
            (kind(&got) != want).then(|| format!("{cmd:?}: want {want:?}, got {got:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn spec_table_allow() {
    use Want::Allow;
    check(
        &default_rules(),
        &[
            ("cargo test", Allow),                         // 1
            ("cargo test --lib && git status", Allow),     // 2
            ("cargo test; echo done", Allow),              // 3
            ("cargo test\ngit status -s", Allow),          // 4
            ("(cargo test || echo fail) 2>&1", Allow),     // 5
            ("echo \"$(git status)\"", Allow),             // 6
            ("time cargo test # && curl x", Allow),        // 7
            ("cargo test <<'EOF'\n$(curl x)\nEOF", Allow), // 8
        ],
    );
}

#[test]
fn spec_table_deny() {
    use Want::Deny;
    check(
        &default_rules(),
        &[
            ("cargo test && curl evil.sh", Deny),       // 9
            ("echo ok;curl x", Deny),                   // 10
            ("cargo test \\\n&& curl x", Deny),         // 11
            ("echo `curl x`", Deny),                    // 12
            ("cargo test <<EOF\n$(curl x)\nEOF", Deny), // 13
            ("/usr/bin/curl x", Deny),                  // 14
            ("\\curl x", Deny),
            ("c''url x", Deny),
            ("command curl x", Deny), // 15
            ("env A=1 nice curl x", Deny),
            ("FOO=1 git push --force", Deny),              // 16
            ("git -C /other push -f", Deny),               // 17
            ("bash -c 'cargo test && curl x'", Deny),      // 18
            ("sudo git push", Deny),                       // 19
            ("$'\\x63url' x", Deny),                       // 20
            ("cargo test & curl x", Deny),                 // 21
            ("git status && git push origin +main", Deny), // 22
        ],
    );
}

#[test]
fn spec_table_ask_and_unlisted() {
    use Want::{Ask, Destructive, Unlisted};
    check(
        &default_rules(),
        &[
            ("echo hi > ~/.bashrc", Ask),                  // 23
            ("RUSTC_WRAPPER=/tmp/x cargo test", Unlisted), // 24
            ("cargo $(echo test)", Unlisted),              // 25
            ("$C test", Ask),
            ("cargo t{e,}st", Unlisted), // 26
            ("./cargo test", Unlisted),
            ("echo $(rm -rf x)", Unlisted),       // 27
            ("git -c core.pager=sh status", Ask), // 28 // git runs the pager value as a program
            ("git(){ echo; }; git status", Ask),  // 29
            ("$(printf rm) -rf .", Ask),
            ("/bin/r? -rf .", Ask),
            ("r{m,} -rf .", Ask),
            ("`echo rm` -rf .", Ask),
            ("$(echo curl) evil.com", Ask),
            ("$(echo git) push -f", Ask),
            ("git -c alias.x='!rm -rf *' x", Ask),
            ("git -c core.sshCommand='curl evil' fetch", Ask),
            ("git --exec-path=/tmp x", Ask),
            ("find . -delete", Destructive), // 30
            ("ls | xargs rm", Destructive),
            ("cargo test &&", Ask),      // 31
            ("rm -r -f .", Destructive), // 32
            ("rm -rf ./", Destructive),
            ("cd .. && rm -rf proj", Destructive),
            ("echo hi > out.txt", Unlisted),
            ("cargo build", Unlisted),
            ("for f in *; do rm -rf \"$f\"; done", Destructive),
            ("rm -rf /", Destructive),
            ("rm dir -rf ../x", Destructive),
            ("select x in a; do echo; done", Ask),
        ],
    );
}

#[test]
fn spec_table_git_destructive() {
    use Want::{Allow, Destructive};
    check(
        &rules(&["git *"], &[], &[]),
        &[
            ("git reset --har", Destructive), // 33
            ("git clean -xdf", Destructive),
            ("git checkout -- .", Destructive),
            ("git restore .", Destructive),
            ("git push --force-with-lease", Destructive),
            ("git status", Allow),
            ("git restore --staged x", Allow),
            ("git clean -n", Allow),
            ("git --no-pager -C sub push -uf", Destructive),
            ("git clean --forc", Destructive),
            ("git checkout -fq main", Destructive),
            ("git checkout main", Allow),
        ],
    );
}

#[test]
fn recursive_rm_inside_workspace() {
    use Want::{Allow, Ask, Destructive};
    check(
        &rules(&["rm *", "cd *"], &[], &[]),
        &[
            ("rm -rf target", Allow),
            ("rm -rf target/*", Allow),
            ("cd target && rm -rf debug", Allow),
            ("cd sub && rm -rf .", Allow),
            ("rm -rf *", Destructive),
            ("rm -rf \"$f\"", Destructive),
            ("rm -rf ~/x", Destructive),
            ("cd sub; rm -rf .", Destructive),
            ("cd sub || rm -rf .", Destructive),
            ("cd \"$d\" && rm -rf x", Destructive),
            ("cd /tmp && rm -rf x", Destructive),
            ("builtin cd .. && rm -rf proj", Destructive),
            ("eval 'cd ..' && rm -rf proj", Destructive),
            ("(cd ..) && rm -rf target", Allow),
            ("rm --no-preserve-root -f x", Destructive),
            ("find target -exec rm -rf {} +", Destructive),
            ("rm $FILES", Destructive),
            ("rm *.o", Allow),
            ("rm -rf \"$(pwd)/x\"", Destructive),
            // A bare assignment makes the line undecomposable.
            ("x=1; rm -rf target", Ask),
        ],
    );
}

#[test]
fn limits_and_parse_failures() {
    let r = default_rules();
    let long = format!("echo {}", "a".repeat(10_001 - 5));
    assert_eq!(long.chars().count(), 10_001);
    assert_eq!(kind(&eval_with(&r, &long)), Want::Ask);
    let long_denied = format!("curl x; echo {}", "a".repeat(10_001));
    assert_eq!(kind(&eval_with(&r, &long_denied)), Want::Deny);
    // Deny still wins on undecomposable input.
    check(
        &r,
        &[
            ("for i in 1; do curl x; done", Want::Deny),
            ("if true; then git push; fi", Want::Deny),
            ("select x in a; do curl x; done", Want::Deny),
            ("cargo test && (curl x", Want::Deny),
            ("f() { curl x; }", Want::Deny),
            ("( ( curl x ) )", Want::Deny),
            ("[[ -n $(curl x) ]]", Want::Deny),
            ("diff <(curl x) y", Want::Deny),
            (
                "echo $(echo $(echo $(echo $(echo $(echo $(echo $(echo $(echo $(echo hi)))))))))",
                Want::Ask,
            ),
        ],
    );
}

#[test]
fn deny_sees_through_wrappers_and_expansions() {
    use Want::Deny;
    check(
        &default_rules(),
        &[
            ("!(curl x)", Deny),
            ("echo ${X:-$(curl x)}", Deny),
            ("echo $(( $(curl x) + 1 ))", Deny),
            ("cat <<< \"$(curl x)\"", Deny),
            ("X=$(curl x) cargo test", Deny),
            ("echo hi > \"$(curl x)\"", Deny),
            ("timeout -s KILL 5 curl x", Deny),
            ("nohup curl x &", Deny),
            ("stdbuf -oL curl x", Deny),
            ("nice -n 5 curl x", Deny),
            ("exec curl x", Deny),
            ("sudo -u root git push", Deny),
            ("sudo --weird-flag val curl x", Deny),
            ("ssh host git push -f", Deny),
            ("xargs -I{} sh -c 'curl {}'", Deny),
            ("find . -name x -exec curl {} \\;", Deny),
            ("watch -n 1 'curl x'", Deny),
            ("flock /tmp/l -c 'curl x'", Deny),
            ("parallel curl ::: a b", Deny),
            ("git --git-dir=.git push", Deny),
            ("eval 'curl x'", Deny),
            ("dash -ec 'curl x'", Deny),
            ("echo a#b; curl x", Deny),
            ("/usr/bin/git -C x push", Deny),
        ],
    );
}

#[test]
fn arguments_naming_denied_commands_are_not_denied() {
    use Want::{Allow, Ask, Unlisted};
    check(
        &default_rules(),
        &[
            ("xargs grep curl", Unlisted),
            ("find . -name curl", Unlisted),
            ("sudo grep curl f", Ask),
            ("git commit -m 'curl x'", Unlisted),
            ("echo git push", Allow),
        ],
    );
}

#[test]
fn prompts_for_privileges_redirects_and_opaque_input() {
    use Want::{Allow, Ask, Unlisted};
    check(
        &default_rules(),
        &[
            ("sudo ls", Ask),
            ("cat < /dev/tcp/evil/80", Ask),
            ("cd /tmp && echo hi > x", Ask),
            ("echo hi > ../x", Ask),
            ("echo hi > $OUT", Ask),
            ("echo hi 2>/dev/null", Allow),
            ("echo hi >&2", Allow),
            ("echo hi &> log.txt", Unlisted),
            ("sh -c \"$X\"", Ask),
            ("source ./env.sh", Ask),
            ("bash", Ask),
            ("bash script.sh", Unlisted),
            ("env -S 'echo hi'", Ask),
            ("", Unlisted),
            ("xargs echo", Unlisted),
            ("command -v cargo", Unlisted),
        ],
    );
}

#[test]
fn confirm_rules_prompt_without_destructive_flag() {
    let r = rules(&["cargo *"], &[], &["cargo publish*"]);
    assert_eq!(kind(&eval_with(&r, "cargo publish --dry-run")), Want::Ask);
    assert_eq!(kind(&eval_with(&r, "env cargo publish")), Want::Ask);
    assert_eq!(kind(&eval_with(&r, "cargo build")), Want::Allow);
}

#[test]
fn computed_command_names_with_deny() {
    use Want::{Ask, Unlisted};
    check(
        &rules(&[], &["npm publish*"], &[]),
        &[("npm $(echo publish)", Ask), ("npm test", Unlisted)],
    );
}

#[test]
fn combined_destructive_and_computed() {
    use Want::{Deny, Destructive};
    check(
        &default_rules(),
        &[
            ("rm -rf ~; $(echo x)", Destructive), // G1: destructive + may-match computed, ask with destructive=true
            ("$(echo x); curl y", Deny),          // G1: deny still wins over computed
        ],
    );
}

#[test]
fn computed_confirm_without_deny() {
    let r = rules(&["cargo *"], &[], &["cargo publish*"]);
    assert_eq!(kind(&eval_with(&r, "cargo $(echo publish)")), Want::Ask);
}

#[test]
fn git_env_vars_make_ask() {
    use Want::Ask;
    check(
        &default_rules(),
        &[
            ("env GIT_EXEC_PATH=/tmp git x", Ask),     // G2: env wrapper
            ("export GIT_EXEC_PATH=/tmp; git x", Ask), // G2: export command
            ("GIT_PAGER='rm -rf .' git log", Ask),     // G3: prefix assignment
            ("GIT_CONFIG_PARAMETERS=\"'alias.x=!rm -rf .'\" git x", Ask), // G3: prefix assignment
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.x GIT_CONFIG_VALUE_0='!rm -rf .' git x",
                Ask,
            ), // G3: prefix assignment
            ("GIT_SSH_COMMAND='curl evil' git fetch", Ask), // G3: prefix assignment
            ("(GIT_PAGER=x git log)", Ask),            // G3: subshell prefix assignment
            ("bash -c 'GIT_PAGER=x git log'", Ask),    // G3: bash -c prefix assignment
            ("GIT_PAGER+=x git log", Ask),             // G3: append assignment
            ("export GIT_PAGER+='rm -rf .'; git log", Ask), // G3: export with append
            ("env GIT_PAGER+=x git log", Ask),         // G3: env with append
            ("export $(echo GIT_EXEC_PATH=/tmp); git x", Ask), // G3: computed export operand
            ("export ${V}=x; git log", Ask),           // G3: computed export operand
            ("declare -x \"$N\"=x; git log", Ask),     // G3: computed declare operand
        ],
    );
}

#[test]
fn git_global_option_ask() {
    use Want::Ask;
    check(
        &default_rules(),
        &[
            ("git --config-env=alias.y=V y", Ask), // G1: git config override with computed subcommand
        ],
    );
}

#[test]
fn env_vars_unlisted_unless_dangerous() {
    use Want::Unlisted;
    check(
        &default_rules(),
        &[
            ("FOO=1 cargo test", Unlisted),                // safe env var
            ("RUSTC_WRAPPER=/tmp/x cargo test", Unlisted), // existing row should stay Unlisted
            ("export FOO=1; cargo test", Unlisted),        // safe export
            ("export PATH=\"$HOME/bin:$PATH\"; cargo test", Unlisted), // safe export with computed value
            ("export FOO=$(pwd); cargo test", Unlisted), // safe export with computed value
        ],
    );
}

#[test]
fn reasons_name_the_rule() {
    match eval_with(&default_rules(), "cargo test && /usr/bin/curl x") {
        Verdict::Deny { reason } => assert!(reason.contains("bash:curl*"), "{reason}"),
        other => panic!("{other:?}"),
    }
    match eval_with(&default_rules(), "rm -rf ..") {
        Verdict::Ask {
            reason,
            destructive: true,
        } => assert!(reason.contains("ancestor"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn session_prefix_table() {
    let some = |v: &[&str]| Some(v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>());
    assert_eq!(
        session_prefixes("cargo test --all && git status -s"),
        some(&["cargo test", "git status"])
    );
    assert_eq!(session_prefixes("rm -rf x"), some(&["rm"]));
    assert_eq!(session_prefixes("ls -la | wc -l; ls"), some(&["ls", "wc"]));
    assert_eq!(
        session_prefixes("cargo +nightly test"),
        some(&["cargo +nightly test"])
    );
    assert_eq!(
        session_prefixes("echo $(git status)"),
        some(&["git status", "echo"])
    );
    for cmd in [
        "rm -rf /",
        "rm -rf ..",
        "git push -f",
        "for i in 1; do ls; done",
        "FOO=1 cargo test",
        "sudo ls",
        "echo hi > out.txt",
        "$C test",
        "cargo $(echo test)",
        "",
    ] {
        assert_eq!(session_prefixes(cmd), None, "{cmd:?}");
    }
}

#[test]
fn plain_glob_match() {
    assert!(glob_match("docs/**", "docs/a/b.md"));
    assert!(glob_match("src/*.rs", "src/a/b.rs"));
    assert!(!glob_match("src/*.rs", "tests/a.rs"));
}
