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
            ("trap 'curl x' EXIT", Deny),
            ("trap -- 'curl x' EXIT", Deny),
            ("compgen -C 'curl x'", Deny),
            ("compgen -W 'a $(curl x)'", Deny),
            ("complete -C 'curl x' foo", Deny),
            ("complete -W '$(curl x)' foo", Deny),
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
fn parameter_transforms_ask() {
    use Want::{Allow, Ask};
    // `${x@P}` runs prompt expansion of `x`'s value on bash 4.4+, which can run a
    // command substitution the value holds; every `${…@<letter>}` transform is treated
    // the same way, since the value is not known statically. An `@` that instead follows
    // another operator (`${x:-user@host}`) is part of that operator's text, not a
    // transform.
    check(
        &default_rules(),
        &[
            ("echo \"${x@P}\"", Ask),
            ("echo \"${1@P}\"", Ask),
            ("echo \"${x@Q}\"", Ask),
            ("echo \"${x@A}\"", Ask),
            ("echo \"${x@a}\"", Ask),
            ("echo \"${x@E}\"", Ask),
            ("echo \"${x@L}\"", Ask),
            ("echo \"${x@U}\"", Ask),
            ("echo \"${!x@P}\"", Ask),
            ("echo \"${x:-user@host}\"", Allow),
            ("echo \"${x}\"", Allow),
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
fn trap_and_completion_actions_run_as_nested_shell_text() {
    use Want::{Allow, Ask, Unlisted};
    // A literal `trap` action, or a literal `compgen`/`complete` `-C` command or `-W` word
    // list, is analyzed like `bash -c '…'`: harmless text is unlisted or allowed, and a
    // denied command inside is caught (see `deny_sees_through_wrappers_and_expansions`).
    // Text that is not a literal, and `-F` (a shell function this analysis cannot see),
    // ask instead of running unseen. `trap -p`, `trap -l`, `trap - SIG` and `trap '' SIG`
    // run nothing and are unaffected.
    check(
        &default_rules(),
        &[
            ("trap 'echo hi' EXIT", Allow),
            ("trap 'git status' INT TERM", Allow),
            ("trap \"$cmd\" EXIT", Ask),
            ("trap \"echo $1\" EXIT", Ask),
            ("trap -p", Unlisted),
            ("trap -l", Unlisted),
            ("trap - EXIT", Unlisted),
            ("trap '' EXIT", Unlisted),
            ("trap", Unlisted),
            // `--` ends option parsing; the word after it is the action, even when that
            // word looks like an option (bash reads no options past `--`, though `-` and
            // `''` keep their special meaning there too).
            ("trap -- 'echo hi' EXIT", Allow),
            ("trap -- -p EXIT", Unlisted),
            ("trap -- -l EXIT", Unlisted),
            ("trap -- - EXIT", Unlisted),
            ("trap -- '' EXIT", Unlisted),
            ("trap --", Unlisted),
            // `-l` or `-p`, alone, repeated, or combined, only list or print: no action,
            // regardless of anything after (bash ignores it, even another `--`).
            ("trap -l 'echo hi' EXIT", Unlisted),
            ("trap -p 'echo hi' EXIT", Unlisted),
            ("trap -lp", Unlisted),
            ("trap -pl", Unlisted),
            ("trap -p -p EXIT", Unlisted),
            ("trap -p --", Unlisted),
            ("trap -l -- 'echo hi' EXIT", Unlisted),
            // An option this analysis does not recognize (bash 3.2 and 5.2 accept only
            // `-l`/`-p`; a later bash could add another) asks rather than guessing.
            ("trap -x 'echo hi' EXIT", Ask),
            ("trap -P 'echo hi' EXIT", Ask),
            ("compgen -W 'a b c'", Unlisted),
            ("compgen -W \"$list\"", Ask),
            ("compgen -C \"$cmd\"", Ask),
            ("complete -F _myfunc foo", Ask),
            ("compgen -F _myfunc", Ask),
        ],
    );
}

#[test]
fn trap_double_dash_still_analyses_the_action() {
    use Want::Deny;
    // Before the fix, `trap`'s wrapper treated a leading `--` itself as the literal
    // action to analyse (harmless) and silently dropped the real action after it. `rm`
    // isn't in `default_rules`, so this needs its own deny rule to show the nested text
    // is actually reached, not just that the same `curl`-based case above still denies.
    check(
        &rules(&[], &["rm -f*"], &[]),
        &[("trap -- 'rm -f x' EXIT", Deny)],
    );
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
fn rough_scan_sees_past_redirections_before_the_command_name() {
    use Want::{Allow, Ask, Deny, Unlisted};
    // bash runs a denied command in each of these: redirections, with the file descriptor
    // number or `{name}` before one, may come before the command name.
    let denied = [
        ">/dev/null curl x",
        "> /dev/null curl x",
        ">>log curl x",
        "<in curl x",
        "<input curl x",
        "<>f curl x",
        ">|f curl x",
        "&>f curl x",
        "&>>f curl x",
        "2>/dev/null curl x",
        "2>&1 curl x",
        "0<&3 curl x",
        ">&- curl x",
        "2>&- curl x",
        ">& log curl x",
        // After `>&` or `<&`, bash reads a `-` as a word of its own: the rest runs.
        ">&-curl x",
        "2>& -curl x",
        "<&-'curl' x",
        "2>&-0<input curl x",
        "2>x 3>y curl z",
        "{fd}>out curl x",
        "{fd}<file curl x",
        "{fd[1]}>out curl x",
        "<<<word curl x",
        "<<< 'a b' curl x",
        "3<<<x curl y",
        "<<EOF curl x\nbody\nEOF",
        "<<-EOF curl x\n\tbody\n\tEOF",
        "<<- EOF curl x\n\tbody\n\tEOF",
        ">$(echo out) curl x",
        "echo `2>/dev/null curl x`",
        "echo $(2>/dev/null curl x)",
        "echo a; >x curl y",
        "A=1 >x curl y",
        ">x A=1 curl y",
        "! >x curl y",
        "sudo >x curl y",
        "cat <(>/dev/null curl x)",
        ">x git push",
        "git >x push",
        "git 2>&1 push -f",
        "git &>>log push",
    ];
    // bash runs no denied command in these, or the verdict is already right.
    let kept = [
        ("echo >x curl y", Ask),
        (">&--curl x", Ask),
        ("2>&-1 curl x", Ask),
        (">&\"-curl\" x", Ask),
        // bash 5 runs `echo a curl x`; bash 3.2 reads `&>` and `>`, a syntax error.
        ("echo a &>>log curl x", Ask),
        ("\"2\">x curl y", Ask),
        ("2\\>x curl y", Ask),
        ("99999999999>x curl y", Ask),
        // A closing backquote goes on with the word, which is no file descriptor number.
        ("`echo`2>x curl y", Ask),
        (">x echo hi", Ask),
        ("cat <(curl x)", Deny),
        ("<(echo x) curl y", Deny),
        // The scan as it was before it followed bash reads `<(` as a parenthesis.
        ("echo <(echo x) curl y", Deny),
        ("curl x >/dev/null", Deny),
    ];
    // Each program is refused as a whole, so the rough scan decides.
    let long = format!("echo {}", "a".repeat(10_000));
    let mut table = Vec::new();
    for refusal in ["(", "export a[${a[${b}]}]=1", long.as_str()] {
        table.extend(denied.map(|p| (format!("{p}\n{refusal}"), Deny)));
        table.extend(kept.map(|(p, want)| (format!("{p}\n{refusal}"), want)));
    }
    // The review's mutant: bash reads the backquoted text as `<<'E\OF'# curl x`, a
    // here-document whose delimiter the scan reads as a word before `curl x`.
    table.push((
        "cat <<-EOF\n\t` <<'E\\OF'# \\\n\tcurl x`\n\tEOF".into(),
        Deny,
    ));
    // Parsed programs keep their verdicts.
    table.extend(
        [
            (">/dev/null curl x", Deny),
            ("2>&1 curl x", Deny),
            (">/dev/null cargo test", Allow),
            (">x echo hi", Unlisted),
            ("cat <(curl x)", Deny),
        ]
        .map(|(p, want)| (p.to_string(), want)),
    );
    let table: Vec<(&str, Want)> = table.iter().map(|(c, w)| (c.as_str(), *w)).collect();
    check(&default_rules(), &table);
}

#[test]
fn redirections_bash_reads_differently_are_scanned() {
    use Want::{Allow, Ask, Deny, Unlisted};
    // bash reads a `-` right after `>&` or `<&` as a word of its own, and bash 4.1 and
    // later read `{NAME}` right before a redirection as a variable for its file
    // descriptor. brush-parser reads one word in both, so these commands run `curl x`
    // where it sees another command.
    check(
        &default_rules(),
        &[
            (">&-curl x", Deny),
            ("<&-curl x", Deny),
            ("2>&-curl x", Deny),
            (">& -curl x", Deny),
            ("2>& -curl x", Deny),
            (">&-'curl' x", Deny),
            ("echo a; >&-curl x", Deny),
            ("{fd}>out curl x", Deny),
            ("{fd}<file curl x", Deny),
            ("{fd}>>out curl x", Deny),
            ("{fd}<<<w curl x", Deny),
            ("{fd}>&2 curl x", Deny),
            ("{a[1]}>f curl x", Deny),
            ("{_x9}>f curl x", Deny),
            ("A=1 {fd}>f curl x", Deny),
            ("bash -c '{fd}>f curl x'", Deny),
            // bash joins the lines first; the positions count characters.
            ("{fd}\\\n>out curl x", Deny),
            ("echo é; {fd}>out curl x", Deny),
            // bash's names are letters in the locale, so any non-ASCII character may be one.
            ("git {æ}>/dev/null push", Deny),
            ("git {µ}>/dev/null push --force", Deny),
            ("git {aõ}>/dev/null push", Deny),
            ("{æ}>/dev/null curl x", Deny),
            // Any subscript.
            ("git {a[$i]}>/dev/null push", Deny),
            ("git {a[\"k\"]}>/dev/null push", Deny),
            ("git {a[i+1]}>/dev/null push", Deny),
            // bash removes a backslash-newline before it reads the operator.
            (">&\\\n-curl x", Deny),
            (">& \\\n-curl x", Deny),
            // Elsewhere bash reads these words as brush-parser does.
            ("echo hi >&-", Allow),
            ("echo hi 2>&-", Allow),
            ("echo hi <&-", Allow),
            ("echo hi >&2", Allow),
            ("cargo test 2>&1", Allow),
            ("exec 3>&-", Unlisted),
            (">&\"-curl\" x", Unlisted),
            ("echo {fd} >/dev/null", Allow),
            ("echo é; echo {fd} >/dev/null", Allow),
            // A command given a `{NAME}` redirection is not fully understood.
            ("echo hi {fd}>/dev/null", Ask),
            ("cargo test {fd}>/dev/null", Ask),
            ("echo hi >&-#c", Ask),
        ],
    );
}

#[test]
fn rough_scan_sees_past_redirections_split_by_line_continuations() {
    use Want::Deny;
    // bash removes a backslash-newline before it reads the operator, file descriptor
    // number or `{NAME}` around it. Each program is refused as a whole.
    let commands = [
        "2\\\n>/dev/null curl x",
        "3<\\\n&1 curl x",
        ">&\\\n-curl x",
        "git 2\\\n>f push",
        "git {fd}\\\n>f push",
        "git {æ}>/dev/null push",
        "git {a[\"k\"]}>/dev/null push",
        "git {a[$i]}>/dev/null push",
        "git {a[i+1]}>/dev/null push",
    ];
    let table: Vec<(String, Want)> = commands.iter().map(|c| (format!("{c}\n("), Deny)).collect();
    let table: Vec<(&str, Want)> = table.iter().map(|(c, w)| (c.as_str(), *w)).collect();
    check(&default_rules(), &table);
    // Parsed programs too.
    check(&default_rules(), &[("3<\\\n&1 curl x", Deny)]);
}

#[test]
fn rough_scan_tracks_here_documents_split_by_line_continuations() {
    use Want::Deny;
    // bash removes a backslash-newline before it reads a here-document operator, so one
    // splitting `<<`, `<<-` or `<<<` does not hide it. Each program is refused as a whole
    // (by the trailing `(`), so the rough scan decides; an unterminated quote in the body
    // shows the operator was tracked, since otherwise nothing bounds where the body ends
    // and the quote swallows `curl x` into one opaque word (asking instead of denying).
    let table = [
        // `<<` split between its two `<`.
        ("cat <\\\n<EOF\n\"\nEOF\ncurl x\n(", Deny),
        // `<<-` split between its two `<`, before the `-`.
        ("cat <\\\n<-EOF\n\"\n\tEOF\ncurl x\n(", Deny),
        // `<<-` split between `<<` and `-`.
        ("cat <<\\\n-EOF\n\"\n\tEOF\ncurl x\n(", Deny),
        // `<<<` (here-string) split between its 2nd and 3rd `<`.
        ("cat <\\\n<<x\ncurl x\n(", Deny),
        ("echo <\\\n<<'y'\ncurl x\n(", Deny),
    ];
    check(&default_rules(), &table);
}

#[test]
fn a_heredoc_operator_only_found_by_joining_a_continuation_is_not_trusted() {
    use Want::Deny;
    // A mutfuzz find: recognizing `<<'EOF'` here needs the same join as above, but this
    // program is a syntax error to bash (an unmatched `(` right after the delimiter), so
    // nothing bash would call the body ever runs. Trusting the delimiter anyway would read
    // `zzmark` after it as here-document data (never denied, only asked, since data may
    // hide a command bash disagrees about) instead of a command of its own, turning the
    // old Deny into an Ask: looser. Not trusting a delimiter recognized this way falls back
    // to reading the text line by line, which still finds `zzmark` as its own line.
    check(
        &rules(&[], &["zzmark*"], &[]),
        &[("echo $<\\\n<'EOF'(\n# c)\nzzmar\\\nk\n)", Deny)],
    );
}

#[test]
fn process_substitutions_after_redirection_operators_stay_substitutions() {
    use Want::{Deny, Destructive};
    // `>(` after `>`, `<`, `&>` or `&>>` is a process substitution. bash 3.2 has no `&>>`
    // and reads `&>>(…)` as `&>` and `>(…)`: it runs `git push`, and in the others reads
    // the here-document in the substitution as its own, so `curl x` runs.
    check(
        &default_rules(),
        &[
            ("echo &>>(cat <<'E'\n)\ncurl x\nE\n)", Deny),
            ("echo &>(cat <<'E'\n)\ncurl x\nE\n)", Deny),
            ("echo >>(cat <<'E'\n)\ncurl x\nE\n)", Deny),
            ("echo <>(cat <<'E'\n)\ncurl x\nE\n)", Deny),
            ("git &>>(true) push", Deny),
            ("rm -rf >>(true) /", Destructive),
            ("rm -rf <>(true) /", Destructive),
            ("echo >>(curl x) y", Deny),
            ("echo <>(curl x) y", Deny),
            ("echo &>(curl x) y", Deny),
            ("echo &>>(curl x) y", Deny),
        ],
    );
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
            // An escaped backslash does not join the lines.
            ("cat <<EOF\n` # \\\\\ncurl x`\nEOF", Deny),
        ],
    );
    // Joined, the backquoted command is ` # curl x`: a comment, which runs nothing.
    check(
        &probe_rules(),
        &[("cat <<EOF\n` # \\\ncurl x`\nEOF", Want::Allow)],
    );
}

/// Asserts that each command asks, flagged as possibly hiding a denied command.
fn check_asks_may_deny(rules: &Rules, commands: &[&str]) {
    let failures: Vec<String> = commands
        .iter()
        .filter_map(|cmd| match eval_with(rules, cmd) {
            Verdict::Ask { may_deny: true, .. } => None,
            other => Some(format!("{cmd:?}: want Ask with may_deny, got {other:?}")),
        })
        .collect();
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn bash32_substitution_ends_are_checked() {
    use Want::Deny;
    // bash 3.2 (macOS /bin/bash) reads a `$(…)` by counting parentheses and pairing quotes,
    // with no notion of here-documents or `${…}`. It ends these later or earlier than
    // brush-parser and bash 5 do, and runs `curl x`.
    check(
        &probe_rules(),
        &[
            // Q1, Q2 and P1: a quote in a body makes bash 3.2 end the `$(…)` later.
            (
                "echo \"$(cat <<'EOF'\nit's\nEOF\n)\"\necho 'x\n)\"\ncurl x\n'",
                Deny,
            ),
            (
                "echo $(cat <<EOF\nit's\nEOF\n)\necho 'a\n)\ncurl x\n'",
                Deny,
            ),
            (
                "x=$(cat <<'EOF'\nsay \"hi\nEOF\n)\necho \"a\n)\ncurl x\n\"",
                Deny,
            ),
            (
                "echo $(echo \"$(cat <<'EOF'\nit's\nEOF\n)\")\necho 'x\n)\")\ncurl x\n'",
                Deny,
            ),
            // H0 and H1: bash 3.2 ends it at a `)` on the operator's line or in the body.
            ("echo $(cat <<EOF)\ncurl x\nEOF", Deny),
            ("x=$(cat <<'EOF'\n)\ncurl x\nEOF\n)", Deny),
            // E1: at a `)` in `${…}`.
            ("echo $(echo ${y:-)\ncurl x\n})", Deny),
        ],
    );
    // C1: bash 3.2 sees no comment after `;`, `&` or `)`, so it ends the `$(…)` at the
    // first `)`. The rough scan reads a comment there too, so it cannot find `curl x`.
    check_asks_may_deny(
        &probe_rules(),
        &[
            "echo $(true;# ); curl x\n)",
            "echo $(true&# ); curl x\n)",
            "echo $( (true)# ); curl x\n)",
        ],
    );
}

#[test]
fn bash32_backslash_newline_in_quoted_bodies() {
    use Want::{Allow, Deny};
    // bash 3.2 removes backslash-newlines while it reads a `$(…)`, before it reads the
    // here-documents in it, so a quoted body can end at a joined line.
    check(
        &probe_rules(),
        &[
            ("x=$(cat <<'EOF'\nEO\\\nF\ncurl x\nEOF\n)", Deny),
            ("x=$(cat <<\"EOF\"\nEO\\\nF\ncurl x\nEOF\n)", Deny),
            ("echo \"$(cat <<'EOF'\nEO\\\nF\ncurl x\nEOF\n)\"", Deny),
            // Joined lines that never form the delimiter only change the text.
            (
                "echo \"$(cat <<'EOF'\ncurl -X POST https://x \\\n  -d a\nEOF\n)\"",
                Allow,
            ),
            ("echo \"$(cat <<'EOF'\nC:\\dir\\\nEOF\n)\"", Allow),
            // The delimiter after quote removal keeps an escaped backslash: `E\OF`.
            (
                "echo \"$(cat <<'E'\\\\OF\nE\\O\\\nF\ncurl x\nE\\OF\n)\"",
                Deny,
            ),
            (
                "echo \"$(cat <<\"E\\\\OF\"\nE\\O\\\nF\ncurl x\nE\\OF\n)\"",
                Deny,
            ),
            ("echo $(cat <<'E'\\\\OF\nE\\O\\\nF\ncurl x\nE\\OF\n)", Deny),
            (
                "echo \"$(cat <<E\\\\OF\nE\\O\\\nF\ncurl x\nE\\OF\n)\"",
                Deny,
            ),
        ],
    );
    // How bash reads a backslash left in the delimiter next to joined lines is not
    // modelled, so such a body asks even where it ends at its last line.
    check(
        &probe_rules(),
        &[("echo \"$(cat <<'E'\\\\F\na \\\nb\nE\\F\n)\"", Want::Ask)],
    );
}

#[test]
fn shell_option_changes_ask() {
    use Want::{Ask, Unlisted};
    // With extglob on, bash reads `!( … )` on later lines as a pattern word, in which a
    // `#` starts no comment, so `curl x` runs; the analysis parses with extglob off.
    // Aliases likewise change what a later word runs.
    check_asks_may_deny(
        &probe_rules(),
        &[
            "shopt -s extglob\n!( true # '\necho ' )\ncurl x\n' )",
            "bash -O extglob -c \"!( true # '\necho ' )\ncurl x\n' )\"",
        ],
    );
    check(
        &probe_rules(),
        &[
            ("shopt -s expand_aliases", Ask),
            ("shopt -u extglob", Ask),
            ("shopt -o -s posix", Ask),
            ("shopt -qs extglob", Ask),
            ("shopt -po errexit", Ask),
            ("shopt $opt extglob", Ask),
            ("bash +O extglob -c 'echo'", Ask),
            // Queries change nothing.
            ("shopt", Unlisted),
            ("shopt -p", Unlisted),
            ("shopt -q extglob", Unlisted),
            ("shopt extglob -s", Unlisted),
        ],
    );
}

#[test]
fn bash32_reads_assignment_subscripts_as_text() {
    use Want::Unlisted;
    // Where an assignment may start a command, bash 3.2 reads `name[…]` by pairing only
    // brackets and quotes: the `$(` there is text, so the word ends at the `]`, and the
    // lines after it run. brush-parser and bash 5 read the `$(…)`, with its comment.
    check_asks_may_deny(
        &probe_rules(),
        &[
            "a[$( \n# '\n'x]=1 true\ncurl x\n' )]=1 true",
            "a[$(  # # '\n'x]=1 true\ncurl x\n' )]=1 true",
        ],
    );
    check(
        &probe_rules(),
        &[
            ("a[$(echo 1)]=x echo hi", Unlisted),
            ("a[`echo 1`]=x echo hi", Unlisted),
            ("a[1]=x echo hi", Unlisted),
        ],
    );
}

#[test]
fn brush_misreads_comments_in_substitutions() {
    use Want::Deny;
    // bash 5 bypasses: bash reads a comment in these substitutions and runs `curl x`;
    // brush-parser does not see it and ends them at the `)` in the comment.
    check(
        &probe_rules(),
        &[
            ("echo \"$( # c)\ncurl x\n)\"", Deny),
            ("echo \"$(\t# c)\ncurl x\n)\"", Deny),
            ("echo \"$(true; # c)\ncurl x\n)\"", Deny),
            ("echo \"$(true\n # c)\ncurl x\n)\"", Deny),
            ("echo \"x$( # c)\ncurl x\n)\"", Deny),
            // bash 5 ends a comment at the newline even after a backslash.
            ("echo $( # \\\n curl x ;)", Deny),
            // After a construct where bash 3.2 stops reading, bash 5 still runs the rest.
            (
                "echo \"$(cat <<'EOF'\nit's\nEOF\n)\"; echo \"$( # c) \ncurl x\n)\"",
                Deny,
            ),
        ],
    );
}

#[test]
fn comments_in_expanded_substitutions_are_checked() {
    use Want::Deny;
    // The substitutions in a here-document body or in `${…}` are only read when they are
    // expanded, by both bash versions, and bash reads a comment there that brush-parser
    // does not see.
    check(
        &probe_rules(),
        &[
            ("cat <<EOF\n$( # c)\ncurl x\n)\nEOF", Deny),
            ("cat <<EOF\n$(true;# )\ncurl x\n)\nEOF", Deny),
            ("cat <<EOF\n$(#c)\ncurl x\n)\nEOF", Deny),
            ("cat <<EOF\n${x:-$( # c)\ncurl x\n)}\nEOF", Deny),
            ("echo ${x:-$( # c)\ncurl x\n)}", Deny),
            ("echo \"${x:-$( # c)\ncurl x\n)}\"", Deny),
            (
                "echo \"$(cat <<'EOF'\nit's\nEOF\n)\"\ncat <<X\n$( # c)\ncurl x\n)\nX",
                Deny,
            ),
            // bash 3.2 extracts a `$(…)` again from the whole word when it expands it: past
            // the closing quote here, and past the `}`.
            ("echo \"$( # c)\"'\ncurl x\n)'", Deny),
            ("echo ${x:-$( # c)}'\ncurl x\n)}'", Deny),
        ],
    );
    // bash 3.2 reads a backquoted command again when it runs it: C1 in backticks.
    check_asks_may_deny(
        &probe_rules(),
        &[
            "echo `echo $(true;# ); curl x\n)`",
            "echo \"`echo $(true;# ); curl x\n)`\"",
        ],
    );
}

#[test]
fn bash32_reading_keeps_the_commit_idiom() {
    use Want::{Allow, Deny, Unlisted};
    let commit = |body: &str| format!("git commit -m \"$(cat <<'EOF'\n{body}\nEOF\n)\"");
    let bodies = [
        "Fix the parser",
        // bash 3.2 reaches the end of input looking for the quote; nothing runs.
        "It's fixed",
        "It's Bob's",
        "Fix it (closes #12)",
        "Use `x` now",
        "The ` character",
    ];
    let mut table: Vec<(String, Want)> = bodies.iter().map(|b| (commit(b), Allow)).collect();
    table.extend([
        (format!("{} && git status", commit("Msg")), Allow),
        (format!("{}\ngit status", commit("It's")), Allow),
        (
            "git commit -m \"$(cat <<EOF\nMsg $(echo x)\nEOF\n)\"".into(),
            Allow,
        ),
        ("echo \"$(echo a;# note\n)\"".into(), Allow),
        (
            "gh pr create --body \"$(cat <<'EOF'\n## Summary\n- It's done\nEOF\n)\"".into(),
            Unlisted,
        ),
        // bash 3.2 stops at the quote in the body; bash 5 runs `curl x`.
        ("echo \"$(cat <<'EOF'\nit's\nEOF\n)\"\ncurl x".into(), Deny),
    ]);
    let table: Vec<(&str, Want)> = table.iter().map(|(c, w)| (c.as_str(), *w)).collect();
    check(
        &rules(
            &["git commit*", "git status*", "echo*", "cat*"],
            &["curl*"],
            &[],
        ),
        &table,
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

// Ruling P3-R5: a command file's shell command gets its arguments from a prelude harness writes,
// `ARGUMENTS='…'; set -- '…' …;`. Bash gives `ARGUMENTS` no meaning, so a single-quoted literal
// assigned to it alone changes nothing the analysis relies on; every other bare assignment
// still cannot be analysed.
#[test]
fn a_literal_arguments_assignment_does_not_hide_anything() {
    let git_log = rules(
        &["git log", "git log *", "set --", "set -- *"],
        &["git push*"],
        &[],
    );
    check(
        &git_log,
        &[
            (
                "ARGUMENTS='a b'; set -- 'a' 'b'; git log --grep \"$1\"",
                Want::Allow,
            ),
            (
                "ARGUMENTS=''; set --; git log --grep \"$ARGUMENTS\"",
                Want::Allow,
            ),
            (
                "ARGUMENTS='x'\\''$(touch p)`q`'; set -- 'x'\\''$(touch p)'; git log --grep \"$1\"",
                Want::Allow,
            ),
            ("ARGUMENTS='a'; git status", Want::Unlisted),
            ("ARGUMENTS='x'; set -- 'x'; git push \"$1\"", Want::Deny),
            // Anything else is as before.
            ("ARGUMENTS=$(date); git log", Want::Ask),
            ("ARGUMENTS=\"$HOME\"; git log", Want::Ask),
            ("ARGUMENTS=a$b; git log", Want::Ask),
            ("ARGUMENTS+='a'; git log", Want::Ask),
            ("ARGUMENTS[0]='a'; git log", Want::Ask),
            ("ARGUMENTS='a' B='b'; git log", Want::Ask),
            ("ARGUMENTS='a' >out; git log", Want::Ask),
            ("PATH='/tmp'; git log", Want::Ask),
            ("X='a'; git log", Want::Ask),
        ],
    );
}
