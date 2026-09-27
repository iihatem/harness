//! Decision table for `evaluate` and `session_prefixes`.

use std::path::Path;
use std::time::{Duration, Instant};

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

fn probe_rules() -> Rules {
    rules(&["echo*", "cat*", "cargo test*"], &["curl*"], &[])
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
            ("rm -rf ~; $(echo x)", Destructive), // destructive + may-match computed, ask with destructive=true
            ("$(echo x); curl y", Deny),          // deny still wins over computed
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
            ("env GIT_EXEC_PATH=/tmp git x", Ask),     // env wrapper
            ("export GIT_EXEC_PATH=/tmp; git x", Ask), // export command
            ("GIT_PAGER='rm -rf .' git log", Ask),     // prefix assignment
            ("GIT_CONFIG_PARAMETERS=\"'alias.x=!rm -rf .'\" git x", Ask), // prefix assignment
            (
                "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.x GIT_CONFIG_VALUE_0='!rm -rf .' git x",
                Ask,
            ), // prefix assignment
            ("GIT_SSH_COMMAND='curl evil' git fetch", Ask), // prefix assignment
            ("(GIT_PAGER=x git log)", Ask),            // subshell prefix assignment
            ("bash -c 'GIT_PAGER=x git log'", Ask),    // bash -c prefix assignment
            ("GIT_PAGER+=x git log", Ask),             // append assignment
            ("export GIT_PAGER+='rm -rf .'; git log", Ask), // export with append
            ("env GIT_PAGER+=x git log", Ask),         // env with append
            ("export $(echo GIT_EXEC_PATH=/tmp); git x", Ask), // computed export operand
            ("export ${V}=x; git log", Ask),           // computed export operand
            ("declare -x \"$N\"=x; git log", Ask),     // computed declare operand
        ],
    );
}

#[test]
fn git_global_option_ask() {
    use Want::Ask;
    check(
        &default_rules(),
        &[
            ("git --config-env=alias.y=V y", Ask), // git config override with computed subcommand
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
            ("RUSTC_WRAPPER=/tmp/x cargo test", Unlisted), // not a program-running variable
            ("export FOO=1; cargo test", Unlisted),        // safe export
            ("export PATH=\"$HOME/bin:$PATH\"; cargo test", Unlisted), // safe export with computed value
            ("export FOO=$(pwd); cargo test", Unlisted), // safe export with computed value
        ],
    );
}

#[test]
fn assignment_shaped_arguments_are_ordinary_words() {
    use Want::{Allow, Ask, Deny, Unlisted};
    check(
        &rules(
            &["make *", "dd *", "echo*"],
            &["make deploy ENV=prod*"],
            &["dd *of=/dev/*"],
        ),
        &[
            ("make deploy ENV=prod", Deny),
            ("dd if=/dev/zero of=/dev/disk0", Ask),
            ("echo GIT_PAGER=x", Allow),
        ],
    );
    check(
        &rules(&[], &[], &[]),
        &[
            ("grep GIT_PAGER=x README.md", Unlisted),
            ("builtin export GIT_PAGER=x; git log", Ask),
            ("command export \"$N\"=x; git log", Ask),
        ],
    );
    assert_eq!(
        session_prefixes("cargo +nightly test FOO=1"),
        Some(vec!["cargo +nightly test FOO=1".to_string()])
    );
}

#[test]
fn declaration_operands_split_unless_the_builtin_is_plain() {
    use Want::{Ask, Unlisted};
    // bash only expands `NAME=value` operands as assignments (no word splitting or
    // pathname expansion) when the declaration builtin is the unquoted first word;
    // otherwise `x='a GIT_PAGER=…'` in `FOO=$x` adds a second operand.
    check(
        &default_rules(),
        &[
            ("export FOO=$x; git log", Unlisted),
            (">/dev/null export FOO=$x; git log", Unlisted),
            ("command export FOO=$x; git log", Ask),
            ("builtin export FOO=$(pwd); git log", Ask),
            ("\\export FOO=$x; git log", Ask),
            ("'export' FOO=$x; git log", Ask),
            ("FOO=1 export BAR=$x; git log", Ask),
            ("export \"FOO\"=$x; git log", Ask),
            ("\\export GIT_PAGE[R]=x; git log", Ask),
        ],
    );
}

#[test]
fn array_shaped_declaration_operands_are_never_trusted() {
    use Want::Ask;
    // dash has no arrays: it pathname-expands `NAME[i]=…` operands, so a file named
    // `GIT_EXTERNAL_DIFF=…` turns `GIT_EXTERNAL_DIF[F]=*` into that assignment.
    check(
        &rules(&["*"], &[], &[]),
        &[
            (
                "dash -c 'touch \"GIT_EXTERNAL_DIFF=echo PWNED\"; export GIT_EXTERNAL_DIF[F]=*; git diff'",
                Ask,
            ),
            ("export GIT_EXTERNAL_DIF[F]=*; git diff", Ask),
            ("command export GIT_PAGE[R]=x; git log", Ask),
            ("export GIT_PAGE[\"R\"]=evil; git log", Ask),
        ],
    );
}

#[test]
fn substitutions_hidden_in_arithmetic_text_ask() {
    use Want::{Allow, Ask, Unlisted};
    check(
        &default_rules(),
        &[
            ("echo $(( 'a[$(curl evil)]' ))", Ask),
            ("cargo test $(( 'a[$(curl evil)]' ))", Ask),
            ("let 'x=a[$(curl evil)]'", Ask),
            ("declare -i F='a[$(curl evil)]'", Ask),
            ("declare 'a[$(curl evil)]=1'", Ask),
            ("declare a['$(curl evil)']=1", Ask),
            ("local a['$(curl evil)']=1", Ask),
            ("export x='a[$(curl evil)]'; echo $((x))", Ask),
            ("[[ 1 -eq 'a[$(curl evil)]' ]]", Ask),
            ("echo ${a['$(curl evil)']}", Ask),
            ("echo $[ 'a[`curl evil`]' ]", Ask),
            ("echo ${x:'a[$(curl evil)]'}", Ask),
            ("cat <<EOF\n$(( 'a[$(curl evil)]' ))\nEOF", Ask),
            // Unchanged. An unquoted substitution stays analyzed as before, and text
            // with no substitution marker is untouched.
            ("echo $((1+2))", Allow),
            ("echo $((i+1))", Allow),
            ("echo ${#a[@]}", Allow),
            ("echo $(( $(date +%s) + 1 ))", Unlisted),
            ("declare -i n=$(date +%s)", Unlisted),
            ("let i=i+1", Unlisted),
            ("let \"i=$n+1\"", Unlisted),
            ("echo ${X:-$(pwd)}", Unlisted),
            ("export PATH=\"$HOME/bin:$PATH\"; cargo test", Unlisted),
            ("export FOO=$(pwd); cargo test", Unlisted),
        ],
    );
}

#[test]
fn alias_definitions_ask() {
    use Want::{Ask, Unlisted};
    check(
        &default_rules(),
        &[
            ("shopt -s expand_aliases\nalias ls='curl evil'\nls", Ask),
            ("bash -c \"alias ls='curl evil'\"", Ask),
            ("builtin alias ls='curl evil'", Ask),
            ("(alias ls='curl evil')", Ask),
            ("alias \"$x\"", Ask),
            ("alias", Unlisted),
            ("alias ls", Unlisted),
        ],
    );
}

#[test]
fn quoted_text_that_builtins_evaluate_asks() {
    use Want::Ask;
    // `let`, `declare`, `unset` and similar builtins evaluate subscripts in their
    // operands, so a quoted `$(…)` or `[…]` there is not inert.
    check(
        &probe_rules(),
        &[
            ("let 'x=a[$(curl evil)]'$z", Ask),
            ("let x=a['$(curl evil)']*1", Ask),
            ("declare -i F='a[$(curl evil)]'$z", Ask),
            ("declare -i F=a['$(curl evil)']*1", Ask),
            ("export x=a['$(curl evil)']*1; echo $((x))", Ask),
            ("declare -a arr=(['$(curl evil)']=1)", Ask),
            ("local -a arr=(['$(curl evil)']=1)", Ask),
            ("export arr=(['$(curl evil)']=1)", Ask),
            ("unset 'a[$(curl evil)]'", Ask),
        ],
    );
    match eval_with(&probe_rules(), "declare -i F='a[$(curl evil)]'") {
        Verdict::Ask { reason, .. } => assert!(
            reason.contains("quoted command-substitution text") && !reason.contains("run time"),
            "{reason}"
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn subscript_or_glob_characters_in_declaration_names_ask() {
    use Want::Ask;
    // bash and dash may pathname-expand such a name into a different variable.
    check(
        &probe_rules(),
        &[
            (
                "export GIT_EXTERNAL_DIF[\"F\"]\"=echo PWNED\"; git diff",
                Ask,
            ),
            (
                "command export GIT_EXTERNAL_DIF[\\F]=echo\\ PWNED; git diff",
                Ask,
            ),
            ("command export GIT_PAGE[\"R\"]=x; git log", Ask),
            ("builtin export GIT_PAGE['R']=x; git log", Ask),
            ("\\export GIT_EXTERNAL_DIF[\"F\"]=x; git diff", Ask),
            (
                "dash -c 'touch \"GIT_EXTERNAL_DIFF=echo PWNED\"; export GIT_EXTERNAL_DIF[\"F\"]\"=echo PWNED\"; git diff'",
                Ask,
            ),
        ],
    );
}

#[test]
fn quoted_or_nested_text_in_expansion_subscripts_asks() {
    use Want::Ask;
    check(
        &probe_rules(),
        &[
            ("echo ${a['b[$(curl evil)]']:-x}", Ask),
            ("echo ${a[b[\\$\\(curl evil\\)]]:-x}", Ask),
            ("echo ${a['b[$(curl evil)]']:=x}", Ask),
            ("echo ${a['b[$(curl evil)]']#x}", Ask),
            ("echo ${a['b[$(curl evil)]']:0:1}", Ask),
            ("echo ${a['b[$(curl evil)]']/x/y}", Ask),
            ("cat <<EOF\n${a['b[$(curl evil)]']:-x}\nEOF", Ask),
        ],
    );
}

#[test]
fn inert_text_outside_those_contexts_is_unchanged() {
    use Want::{Allow, Unlisted};
    check(
        &probe_rules(),
        &[
            ("export MSG='hello world'; cargo test", Unlisted),
            ("export PATH=\"$HOME/bin:$PATH\"; cargo test", Unlisted),
            ("export FOO=\"${HOME}/x\"; cargo test", Unlisted),
            ("export FOO=$(pwd); cargo test", Unlisted),
            ("echo ${arr[0]} ${arr[$i]} ${#arr[@]}", Allow),
            ("echo \"${arr[i]:-none}\" ${x:0:3}", Allow),
            ("echo ${x:$((n-1)):1} $(( ${#arr[@]} - 1 ))", Allow),
            ("echo $((1+2))", Allow),
            ("awk '{print $1}' f", Unlisted),
            ("jq '.a[0]' f", Unlisted),
            ("grep 'a\\[\\$(x)\\]' f", Unlisted),
            ("git log --format='${x}'", Unlisted),
            ("printf '%s' '$(x)'", Unlisted),
        ],
    );
}

#[test]
fn deeply_nested_expansions_ask_quickly() {
    let nest = |open: &str, close: &str, levels: usize, inner: String| {
        (0..levels).fold(inner, |s, _| format!("{open}{s}{close}"))
    };
    // Six nested substring offsets around a command substitution, four times over.
    let offsets = (0..4).fold("true".to_string(), |s, _| {
        format!("echo {}", nest("${x:", ":1}", 6, format!("$({s})")))
    });
    let subscripts = format!("echo {}", nest("${a[", "]}", 12, "$(true)".into()));
    // brush-parser backtracks over unterminated expansions in here-document bodies.
    let heredoc = format!("cat <<EOF\n{}1\nEOF", "$((".repeat(12));
    let many = format!(
        "echo {}",
        (0..600)
            .map(|i| nest("${a[", "]}", 2, i.to_string()))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert!(many.len() <= 10_000);
    for cmd in [offsets, subscripts, heredoc, many] {
        let start = Instant::now();
        let got = eval_with(&probe_rules(), &cmd);
        let took = start.elapsed();
        assert_eq!(kind(&got), Want::Ask, "{cmd}: {got:?}");
        assert!(took < Duration::from_secs(1), "{cmd}: took {took:?}");
    }
}

#[test]
fn command_names_match_regardless_of_case() {
    use Want::{Allow, Ask, Deny, Destructive, Unlisted};
    // macOS resolves command names case-insensitively: `CURL` runs curl.
    check(
        &default_rules(),
        &[
            ("CURL x", Deny),
            ("Curl x", Deny),
            ("/usr/bin/CURL x", Deny),
            ("git PUSH origin main", Deny),
            ("GIT push origin main", Deny),
            ("Git -C sub push", Deny),
            ("SUDO curl x", Deny),
            ("Sudo ls", Ask),
            ("ENV curl x", Deny),
            ("Env GIT_PAGER=x git log", Ask),
            ("BASH -c 'curl x'", Deny),
            ("Bash -c 'rm -rf /'", Destructive),
            ("COMMAND curl x", Deny),
            ("EVAL 'curl x'", Deny),
            ("RM -rf /", Destructive),
            ("Rm -rf .", Destructive),
            ("FIND . -delete", Destructive),
            ("GIT -c core.pager=x status", Ask),
            // `/usr/bin/read` runs the builtin, which evaluates the subscript.
            ("echo x | READ 'a[$(curl evil)]'", Ask),
            // A differently cased `cd` is `/usr/bin/cd`, which cannot change the directory.
            ("CD sub && rm -rf .", Destructive),
            ("COMMAND cd sub && rm -rf .", Destructive),
            // Allow rules stay case-sensitive.
            ("CARGO test", Unlisted),
            ("NOHUP cargo test", Unlisted),
            ("nohup cargo test", Allow),
            // Non-ASCII names may fold to another command.
            ("c\u{fc}rl x", Ask),
            ("\u{ff23}\u{ff35}\u{ff32}\u{ff2c} x", Ask),
        ],
    );
    check(
        &rules(&["git *"], &[], &[]),
        &[
            ("GIT reset --hard", Destructive),
            ("Git push -f", Destructive),
            ("GIT checkout -f main", Destructive),
            ("GIT status", Unlisted),
        ],
    );
    check(
        &rules(&["cargo *"], &["kill*"], &["cargo publish*"]),
        &[
            ("\u{212a}ill 1", Ask),
            ("Cargo publish", Ask),
            ("cargo PUBLISH", Ask),
        ],
    );
    let some = |v: &[&str]| Some(v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>());
    assert_eq!(session_prefixes("GIT status"), some(&["GIT status"]));
    assert_eq!(session_prefixes("cargo test"), some(&["cargo test"]));
    assert_eq!(session_prefixes("NOHUP cargo test"), None);
    assert_eq!(session_prefixes("RM -rf /"), None);
}

#[test]
fn git_switch_that_discards_changes_is_destructive() {
    use Want::{Allow, Destructive};
    check(
        &rules(&["git *"], &[], &[]),
        &[
            ("git switch -f main", Destructive),
            ("git switch --force main", Destructive),
            ("git switch --discard-changes main", Destructive),
            ("git switch --disc main", Destructive),
            ("git switch -qf main", Destructive),
            ("git switch -c feat -f", Destructive),
            ("git -C sub switch --force main", Destructive),
            ("git switch $B", Destructive),
            ("git switch main", Allow),
            ("git switch -", Allow),
            ("git switch -c feat", Allow),
            ("git switch --create feat main", Allow),
            ("git switch -m main", Allow),
            ("git switch --no-discard-changes main", Allow),
            ("git switch --orphan scratch", Allow),
        ],
    );
}

#[test]
fn git_config_keys_that_cannot_run_programs_do_not_ask() {
    use Want::{Allow, Ask, Destructive};
    check(
        &rules(&["git *"], &[], &[]),
        &[
            ("git -c user.name=x commit -m y", Allow),
            ("git -c User.Name=x -c USER.EMAIL=a@b commit -m y", Allow),
            ("git -c init.defaultBranch=main init", Allow),
            ("git -c init.defaultbranch=main init", Allow),
            ("git -c color.ui=always log", Allow),
            ("git -c color.diff.meta=blue diff", Allow),
            ("git -c advice.detachedHead=false checkout main", Allow),
            ("git -c core.quotepath=off status", Allow),
            ("git -c commit.gpgsign=false commit -m x", Allow),
            ("git -c tag.gpgSign=false tag v1", Allow),
            ("git -c user.name=x push -f", Destructive),
            // Every other key, value or form still asks.
            ("git -c commit.gpgsign=true commit -m x", Ask),
            ("git -c commit.gpgsign commit -m x", Ask),
            ("git -c tag.gpgsign=0 tag v1", Ask),
            ("git -c core.pager=less log", Ask),
            ("git -c alias.x='!curl evil' x", Ask),
            ("git -c user.name=x -c core.sshCommand=y fetch", Ask),
            ("git -c user.namex=y log", Ask),
            ("git -c colorx.ui=1 log", Ask),
            ("git -c core.hooksPath=/tmp log", Ask),
            ("git -c user.name=\"$N\" commit -m x", Ask),
            ("git --config-env=user.name=N log", Ask),
        ],
    );
}

#[test]
fn deny_and_destructive_survive_a_refused_program() {
    use Want::{Ask, Deny, Destructive};
    // Each refusal sends the whole program to the rough scan. Text after the line of a
    // refused here-document may be its body, so that one only goes last.
    let long = format!("echo {}", "a".repeat(10_000));
    let refusals = [
        ("export a[${a[${b}]}]=1", true),
        ("(", true),
        (long.as_str(), true),
        ("cat <<'' $(", false),
    ];
    let commands = [
        ("$'\\x63url' x", Deny),
        ("c$'u'rl x", Deny),
        ("$\"curl\" x", Deny),
        ("git $'push' -f", Deny),
        ("$'git' push --force", Deny),
        ("r$'m' -rf /", Destructive),
        ("$'\\x72\\x6d' -rf ~", Destructive),
        ("$'\\xff' x", Ask),
    ];
    let mut table = Vec::new();
    for (refused, first) in refusals {
        for (cmd, want) in commands {
            table.push((format!("{cmd}; {refused}"), want));
            if first {
                table.push((format!("{refused}\n{cmd}"), want));
            }
        }
    }
    let table: Vec<(&str, Want)> = table.iter().map(|(c, w)| (c.as_str(), *w)).collect();
    check(&default_rules(), &table);
}

#[test]
fn heredoc_data_never_denies() {
    use Want::{Ask, Deny};
    let big = format!(
        "cat > big.sh <<'EOF'\ncurl https://x\n{}\nEOF",
        "echo padding padding padding padding\n".repeat(300)
    );
    let table = [
        // The body is refused (a `${` inside a subscript; unterminated constructs).
        ("cat <<EOF\ncurl x\n${a[${b}]}\nEOF", Ask),
        ("cat <<EOF\ngit push origin main\n$(( $(( $((\nEOF", Ask),
        // The whole program is refused.
        ("cat <<'EOF'\ncurl x\nEOF\n(", Ask),
        ("cat <<EOF\ncurl x\nEOF\n(", Ask),
        ("cat <<-EOF\n\tcurl x\n\tEOF\n(", Ask),
        ("cat <<A <<B\ncurl a\nA\ncurl b\nB\n(", Ask),
        ("cat <<'EOF'\n$(curl x)\nEOF\n(", Ask),
        (big.as_str(), Ask),
        // Substitutions in an unquoted body run, and commands outside it are commands.
        ("cat <<EOF\n$(curl x)\n${a[${b}]}\nEOF", Deny),
        ("cat <<EOF\nit's $(curl x)\n${a[${b}]}\nEOF", Deny),
        ("cat <<EOF\n`curl x`\n${a[${b}]}\nEOF", Deny),
        ("cat <<EOF\n$(curl x)\nEOF\n(", Deny),
        ("cat <<EOF; curl x\ndata\nEOF\n(", Deny),
        ("cat <<EOF\ndata\nEOF\ncurl x\n(", Deny),
        ("echo $((1<<2))\ncurl x\n(", Deny),
    ];
    check(&default_rules(), &table);
    for (cmd, _) in &table[..8] {
        match eval_with(&default_rules(), cmd) {
            Verdict::Ask { may_deny, .. } => assert!(may_deny, "{cmd:?}"),
            other => panic!("{cmd:?}: {other:?}"),
        }
    }
}

#[test]
fn rough_scan_tracks_here_documents_as_bash_does() {
    use Want::{Ask, Deny};
    // bash runs `curl x` as a command of its own in each of these.
    let later = [
        // Delimiters other than plain text, or plain text in one pair of quotes.
        "cat <<$'EOF'\nbody\nEOF\ncurl x",
        "cat <<$'E\\x4fF'\nbody\nEOF\ncurl x",
        "cat <<$\"EOF\"\nbody\nEOF\ncurl x",
        "cat <<\"E\\OF\"\nbody\nE\\OF\ncurl x",
        "cat <<'E\"OF'\nbody\nE\"OF\ncurl x",
        "cat <<$(x)\nbody\n$(x)\ncurl x",
        "cat <<${X}\nbody\n${X}\ncurl x",
        "cat <<\\EOF\nbody\nEOF\ncurl x",
        "cat <<E\\OF\nbody\nEOF\ncurl x",
        "cat <<'E'OF\nbody\nEOF\ncurl x",
        "cat <<'END OF'\nbody\nEND OF\ncurl x",
        "cat <<$'EOF'\nit's\nEOF\ncurl x",
        // `<<` inside `${…}`, `$[…]` or arithmetic is not an operator.
        "echo ${x:-<<EOF}\ncurl x\nEOF",
        "echo ${x:-\"}\"<<EOF}\ncurl x\nEOF",
        "echo \"${x:-'}'<<EOF}\"\ncurl x\nEOF",
        "echo $[1<<2]\ncurl x\n2]",
        "echo $[a[1]<<2]\ncurl x\n2]",
        "(( x <<= 1 ))\ncurl x",
        // bash joins a body line ending in a backslash with the next before comparing.
        "cat <<EOF\nEO\\\nF\ncurl x\nEOF",
        "cat <<-EOF\n\tEO\\\nF\ncurl x\n\tEOF",
        "cat <<EOF\nit's\\\nEOF\nEOF\ncurl x",
        // A body starts after the next newline of the substitution holding the operator,
        // and bash 3.2 ends a `$(…)` at a `)` in a here-document body.
        "echo $(cat <<EOF)\ncurl x\nEOF",
        "echo `cat <<EOF`\ncurl x\nEOF",
        "cat <<EOF $(\ncurl x\n)\nbody\nEOF",
        "cat <<'EOF' ${x:-\n$(curl x)}\nbody\nEOF",
        "cat <<'EOF' $((1+\n$(curl x)))\nbody\nEOF",
        "x=$(cat <<EOF\n)\ncurl x\nEOF\n)",
        "x=$(cat <<EOF\nit's\nEOF\n)\ncurl x",
    ];
    // `curl x` is here-document data in each of these.
    let data = [
        "cat <<EOF\ncurl x\nEOF",
        "cat <<'EOF'\ncurl x\nEOF",
        "cat <<\"EOF\"\ncurl x\nEOF",
        "cat << EOF_1.x\ncurl x\nEOF_1.x",
        "echo \"${HOME}\" ${x:-a} $[1+2] $((3<<1))\ncat <<EOF\ncurl x\nEOF",
        "x=$(cat <<EOF\ncurl x\nEOF\n)",
        "cat <<EOF $(echo a\necho b)\ncurl x\nEOF",
        "cat <<A <<'B'\ncurl a\nA\ncurl b\nB",
    ];
    // Each program is refused as a whole, so the rough scan decides.
    let mut table = Vec::new();
    for refusal in ["(", "export a[${a[${b}]}]=1"] {
        table.extend(later.map(|p| (format!("{p}\n{refusal}"), Deny)));
        table.extend(data.map(|p| (format!("{p}\n{refusal}"), Ask)));
    }
    let table: Vec<(&str, Want)> = table.iter().map(|(c, w)| (c.as_str(), *w)).collect();
    check(&default_rules(), &table);
    for (cmd, _) in table.iter().filter(|(_, want)| *want == Ask) {
        match eval_with(&default_rules(), cmd) {
            Verdict::Ask { may_deny, .. } => assert!(may_deny, "{cmd:?}"),
            other => panic!("{cmd:?}: {other:?}"),
        }
    }
}

#[test]
fn here_documents_bash_ends_elsewhere_are_not_trusted() {
    use Want::{Allow, Deny, Unlisted};
    // brush-parser takes these bodies to run to the last line; bash ends them earlier or
    // later and runs `curl x`.
    check(
        &probe_rules(),
        &[
            ("cat <<$'EOF'\nbody\nEOF\ncurl x\n$EOF", Deny),
            ("cat <<$\"EOF\"\nbody\nEOF\ncurl x\n$EOF", Deny),
            ("cat <<\"E\\OF\"\nbody\nE\\OF\ncurl x\nEOF", Deny),
            ("cat <<\"E'OF\"\nbody\nE'OF\ncurl x\nEOF", Deny),
            ("cat <<'E\"OF'\nbody\nE\"OF\ncurl x\nEOF", Deny),
            ("cat <<'E\\OF'\nbody\nE\\OF\ncurl x\nEOF", Deny),
            ("cat <<EOF\nEO\\\nF\ncurl x\nEOF", Deny),
            ("cat <<EOF\nfoo\\\nEOF\ncat <<X\nEOF\ncurl x\nX", Deny),
            ("cat <<-EOF\n\tEO\\\nF\ncurl x\n\tEOF", Deny),
            ("echo \"$(cat <<EOF\nEO\\\nF\ncurl x\nEOF\n)\"", Deny),
            // bash reads no body for a here-document still waiting when a process
            // substitution ends.
            ("cat <(cat <<EOF)\ncurl x\nEOF", Deny),
            ("cat >(cat <<EOF)\ncurl x\nEOF", Deny),
            ("cat <(cat <<EOF)\ncurl x\nEOF\n(", Deny),
            // Delimiters and bodies both read alike.
            ("cat <<\\EOF\nbody\nEOF", Allow),
            ("cat <<E\\OF\nbody\nEOF", Allow),
            ("cat <<E\"OF\"\nbody\nEOF", Allow),
            ("cat <<\"E\\$OF\"\nbody\nE$OF", Allow),
            ("cat <<'EOF'\nEO\\\nF\nEOF", Allow),
            ("cat <<EOF\nfoo\\\\\nEOF", Allow),
            (
                "cat > Dockerfile <<EOF\nRUN apt-get update && \\\n    apt-get install -y x\nEOF",
                Unlisted,
            ),
            // Under `<<-`, bash strips tabs from the joined line, not from each line.
            ("cat <<-EOF\n\tEO\\\n\tF\ncurl x\n\tEOF", Allow),
            (
                "cat > Dockerfile <<-EOF\n\tFROM debian\n\tRUN apt-get update && \\\n\t    apt-get install -y x\n\tEOF",
                Unlisted,
            ),
            (
                "cat > deploy.sh <<-EOF\n\tcurl -fsSL https://x/install.sh \\\n\t  -o install.sh\n\tEOF",
                Unlisted,
            ),
        ],
    );
}

#[test]
fn a_lost_here_document_does_not_hide_later_commands() {
    use Want::Deny;
    // The scan cannot tell where bash ends these bodies. Their quotes would flip the
    // quote state of text after them, where a quoted string spans lines before `curl x`.
    let mut programs = Vec::new();
    for delimiter in ["\\EOF", "E\\OF", "'E'OF", "E\"OF\""] {
        for body in ["it's", "say \"hi"] {
            for later in ["echo 'a\n'; curl x", "echo \"a\n\"; curl x"] {
                programs.push(format!("cat <<{delimiter}\n{body}\nEOF\n{later}"));
            }
        }
    }
    programs.extend(
        [
            "cat <<$'EOF'\nit's\nEOF\necho 'a\n'; curl x",
            "cat <<EOF\nit's \\\nx\nEOF\necho 'a\n'; curl x",
            "x=$(cat <<EOF\nit's\nEOF\n)\necho 'a\n'; curl x",
            "echo \"${x:-'a'}\"\ncat <<EOF\nit's\nEOF\necho 'a\n'; curl x",
            "cat <<\\A\nit's\nA\ncat <<B\nx\nB\necho 'a\n'; curl x",
            "cat <<A $(cat <<B)\nit's\nA\necho 'a\n'; curl x",
            "echo ${x:-<<EOF}\nEOF\necho 'a\n'; curl x",
            "cat <(cat <<EOF)\nit's\nEOF\necho 'a\n'; curl x",
            // Only the line joined across a continuation ends the second body.
            "cat <<\\A\nA\ncat <<EOF\nit's\nEO\\\nF\necho 'a\n'; curl x",
            // The `<<` in `((…))` is no here-document, so the body of the next one starts
            // after this line, not after an end of the first.
            "(( y <<= 1 )); cat <<$'EOF'\nit's\nEOF\necho 'a\n'; curl x",
        ]
        .map(String::from),
    );
    let mut table = Vec::new();
    for refusal in ["(", "export a[${a[${b}]}]=1"] {
        table.extend(programs.iter().map(|p| (format!("{p}\n{refusal}"), Deny)));
    }
    let table: Vec<(&str, Want)> = table.iter().map(|(c, w)| (c.as_str(), *w)).collect();
    check(&default_rules(), &table);
}

#[test]
fn unquoted_heredoc_bodies_join_continuation_lines() {
    use Want::Deny;
    // bash joins a body line ending in a backslash with the next before it expands the
    // body, so `$\⏎(` is a `$(` and `curl x` runs.
    check(
        &probe_rules(),
        &[
            ("cat <<EOF\n$\\\n(curl x)\nEOF", Deny),
            ("cat <<EOF\nx $\\\n(echo a\ncurl x\n)\nEOF", Deny),
            ("echo \"$(cat <<EOF\n$\\\n(curl x)\nEOF\n)\"", Deny),
            (
                "echo \"$(cat <<EOF\nx $\\\n(echo a\ncurl x\n)\nEOF\n)\"",
                Deny,
            ),
        ],
    );
}

#[test]
fn heredoc_operators_in_expansions_are_not_trusted() {
    use Want::{Ask, Deny};
    // brush-parser takes a `<<` in `${…}` for a here-document, so it reads `curl x` as
    // body text; to bash the `<<` is text and `curl x` runs.
    let programs = [
        "echo ${x:-a <<EOF b}\ncurl x\nEOF",
        "echo ${x:=a <<EOF b}\ncurl x\nEOF",
        "echo ${x//a/<<EOF b}\ncurl x\nEOF",
        "echo \"${x:-a <<EOF b}\"\ncurl x\nEOF",
        "echo $(echo ${x:-a <<EOF b})\ncurl x\nEOF",
        "echo ${<<EOF\n}\nEOF\ncurl x",
        "x\r#${<<EOF\n}\nEOF\ncurl x",
    ];
    let denied: Vec<(&str, Want)> = programs.iter().map(|p| (*p, Deny)).collect();
    check(&default_rules(), &denied);
    let asked: Vec<(&str, Want)> = programs.iter().map(|p| (*p, Ask)).collect();
    check(&rules(&["echo*"], &[], &[]), &asked);
}

#[test]
fn parser_panics_ask_and_are_scanned() {
    use Want::{Ask, Deny};
    // brush-parser 0.4 panics on these (`tokenizer.rs:674` and `:1018`).
    let panics = [
        "$(\tEOF$(<<EOF $y| ${#}|\tEOF\nEOF",
        "$(<<-EOF\tEOF ${y}\"$( ${#}\nEOF\n",
        "cat <<< $(<<EOF ${#} ${#}\nEOF",
        "echo \"'\"  $y<<EOF ${#}\tEOF<<EOF ${#}$(\nEOF",
        "echo \"'\" |$(<<EOF ${#}x\nEOF",
        "$(<<-EOF $y) ${#}\"$( ${ ${y}\nEOF",
        "x\r# ${y}<<-EOF|;${  ${#};\nEOF",
        "$(;<<-EOF ${y}\n<<EOFEOF\n\tEOF",
        "$(\nx\n<<EOF $y)${<<EOF\nEOF",
        "cat <<< $(cat <<-EOF\tEOF ${y}EOF$( $(\n\tEOF",
        "cat <<EOF;|$(  ${#} ${y}x${\nEOF",
        "echo \"'\" )${<<EOF ${#}\n<<-EOF\nEOF",
        "$\"|$(;<<-EOF\n) \n\tEOF",
        "$(<<'EOF' ${y}<<EOF<<EOF\nEOF",
        "$(cat <<'EOF' ${y}$(\nEOF\n",
        "$(<<EOF ${#}<<EOF\nEOF",
        "x\r#<<EOF; ${#}\"${<<-EOF<<EOF\tEOF}\"$(\nEOF",
        "echo \"'\" }` ${#}\n<<-EOF )${  ${#}<<'EOF'\nEOF`",
        "x\r#${<<EOF\n}\nEOF",
        "$(cat <<EOF ${y}\nEOF",
    ];
    let mut table = Vec::new();
    for cmd in panics {
        table.push((cmd.to_string(), Ask));
        table.push((format!("curl x; {cmd}"), Deny));
    }
    let failures: Vec<String> = table
        .iter()
        .filter_map(|(cmd, want)| {
            let got = std::panic::catch_unwind(|| eval_with(&default_rules(), cmd));
            match got {
                Ok(v) if kind(&v) == *want => None,
                Ok(v) => Some(format!("{cmd:?}: want {want:?}, got {v:?}")),
                Err(_) => Some(format!("{cmd:?}: evaluate panicked")),
            }
        })
        .collect();
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn brackets_in_declaration_values_are_inert() {
    use Want::{Ask, Unlisted};
    check(
        &probe_rules(),
        &[
            (
                "export DATABASE_URL='postgres://u@[::1]:5432/db'; cargo test",
                Unlisted,
            ),
            ("export PATTERN='^[a-z]+$'; cargo test", Unlisted),
            ("readonly GREEN=$'\\e[32m'", Unlisted),
            ("export RED='\\033[0;31m'", Unlisted),
            ("local MSG='[info] done'", Unlisted),
            ("export A='x' B='[y]'", Unlisted),
            ("command export X='[a]'", Unlisted),
            // Markers stay significant in values, and brackets in names and in the
            // operands of other evaluating builtins.
            ("export X='[$(curl evil)]'", Ask),
            ("export X='${y}'", Ask),
            ("declare -i F='a[$(curl evil)]'", Ask),
            ("export 'FOO[x]=1'", Ask),
            ("export FOO'[x]'=1", Ask),
            ("export A='[x]' 'B[1]'=2", Ask),
            ("unset 'a[1]'", Ask),
            ("read 'a[1]'", Ask),
            ("mapfile -t 'a[1]' < f", Ask),
            ("printf -v 'a[1]' x", Ask),
            ("let 'x=a[1]'", Ask),
        ],
    );
}

#[test]
fn undecodable_ansi_c_text_that_bash_evaluates_asks() {
    use Want::{Allow, Ask};
    // bash truncates `$'…'` at a NUL, so the subscript before it still runs.
    check(
        &probe_rules(),
        &[
            ("declare -i F=$'a[$(curl evil)]\\x00junk'", Ask),
            ("let $'x=a[$(curl evil)]\\x00'", Ask),
            ("export X=$'\\xff[x]'", Ask),
            ("unset $'a[$(curl evil)]\\xff'", Ask),
            ("local X=$'abc\\c'", Ask),
            ("echo $(( $'a[$(curl evil)]\\x00' ))", Ask),
            ("echo $'\\x00'", Allow),
        ],
    );
}

#[test]
fn heredoc_delimiters_the_parser_mishandles_ask_quickly() {
    use Want::{Ask, Deny, Unlisted};
    use std::sync::mpsc;
    // brush-parser 0.4 loops allocating without bound, or panics, on these.
    let table = [
        ("x <<$(( )", Ask),
        ("cat <<$(( )EOF a b ", Ask),
        ("cat <<'' $(", Ask),
        ("cat <<\"'\" $(", Ask),
        ("$(<< <$[", Ask),
        ("$(cat <<  F ", Ask),
        ("cat <<)|$(\n)", Ask),
        ("cat <<`x`\nbody\n`x`", Ask),
        ("curl x; cat <<'' $(", Deny),
        // Ordinary here-documents are unchanged.
        ("cat <<EOF\nhi\nEOF", Unlisted),
        ("cat << 'EOF'\n$(curl x)\nEOF", Unlisted),
        ("cat <<-\"EOF\"\n\thi\n\tEOF", Unlisted),
        ("echo $(cat <<EOF\nhi\nEOF\n)", Unlisted),
        // Single-quoted text is not searched for operators.
        ("grep -rn '<<[A-Z]' .", Unlisted),
        ("rg 'x << (1' src", Unlisted),
        ("awk 'BEGIN{print 1<<2}'", Unlisted),
        (
            "echo \"$HOME\" '<<$(( )' ${PWD} $(printf '<<`x`')",
            Unlisted,
        ),
        // Unless the tokenizer might not read the quote as one.
        ("echo \"'\" <<$(( ) '", Ask),
        ("echo \"$(echo \"'\")\" <<$(( ) '", Ask),
        ("\\$'a\\'' <<$(( ) '", Ask),
        ("echo $'\\'' <<$(( ) '", Ask),
        ("echo $\\\n'x' <<$(( ) '", Ask),
        ("echo a#'<<$(( )'", Unlisted),
        ("echo x #'\nx <<$(( )\n'", Ask),
        ("# it's\nx <<$(( )\n'", Ask),
        ("echo `echo '` <<$(( ) '", Ask),
        ("echo ${x:-'} <<$(( ) '} '", Ask),
        ("echo $((1<<2)) ' <<$(( ) '", Ask),
        ("cat <<EOF\nit's\nEOF\nx <<$(( )\n'", Ask),
        // An escaped `<` is a word character, so the operator starts after it.
        ("$(\\<<<  ", Ask),
        ("echo \")}'${x\\<<<'' ", Ask),
        ("x \\\\<<'' $(", Ask),
        ("cat \\<<<< \"${y}\"", Unlisted),
    ];
    for (cmd, want) in table {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let start = Instant::now();
            let got = eval_with(&rules(&[], &["curl*"], &[]), cmd);
            let _ = tx.send((got, start.elapsed()));
        });
        let (got, took) = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("{cmd:?}: evaluate panicked"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // The parser may still be allocating; stop before it exhausts memory.
                eprintln!("{cmd:?}: evaluate did not return within 5 s");
                std::process::abort();
            }
        };
        assert_eq!(kind(&got), want, "{cmd:?}: {got:?}");
        assert!(took < Duration::from_secs(1), "{cmd:?}: took {took:?}");
    }
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
            ..
        } => assert!(reason.contains("ancestor"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn may_deny_flags_hidden_or_possible_deny_matches() {
    fn may_deny(v: &Verdict) -> bool {
        match v {
            Verdict::Ask { may_deny, .. } => *may_deny,
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    let deny_rules = rules(&[], &["curl*", "git push*"], &[]);

    // A possible deny match (the command name, or an argument, is only known at run time):
    // may_deny is set even though the match isn't definite.
    assert!(may_deny(&eval_with(&deny_rules, "$(echo curl) https://x")));
    assert!(may_deny(&eval_with(&deny_rules, "c=curl; $c https://x")));
    assert!(may_deny(&eval_with(&deny_rules, "git $X origin")));

    // Undecomposable (not a possible-match, just unanalyzable) while any deny rule exists:
    // it could be hiding a denied command, so may_deny is set too.
    assert!(may_deny(&eval_with(&deny_rules, "echo curl x | sh")));
    assert!(may_deny(&eval_with(&deny_rules, "git -c alias.p=push p")));

    // Destructive-only ask with no deny rules configured: nothing to hide, never may_deny.
    let no_deny = rules(&[], &[], &[]);
    assert!(!may_deny(&eval_with(&no_deny, "git reset --hard HEAD~1")));

    // Undecomposable, but no deny rules configured: still nothing to hide.
    assert!(!may_deny(&eval_with(&no_deny, "echo curl x | sh")));

    // A confirm-only ask is not a deny concern.
    let confirm_rules = rules(&[], &["curl*"], &["terraform apply*"]);
    assert!(!may_deny(&eval_with(
        &confirm_rules,
        "terraform apply -auto-approve"
    )));
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
