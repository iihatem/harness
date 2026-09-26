//! AST walk: decomposes a command line into the simple commands it runs and
//! collects everything the verdict needs.

use brush_parser::ast::{
    self, AndOr, Command, CommandPrefixOrSuffixItem as Item, CompoundCommand, ExtendedTestExpr,
    IoFileRedirectKind as Kind, IoFileRedirectTarget as Target, IoRedirect, SeparatorOperator,
};

use crate::argv::{self, MAX_DEPTH, Tok, basename};
use crate::destructive;
use crate::fallback;
use crate::git;
use crate::paths::{Cwd, Workspace};
use crate::wrappers::{self, Next};

/// Longer input is not parsed (it is only roughly scanned for deny matches).
const MAX_INPUT_CHARS: usize = 10_000;
/// Nesting limit for wrapper commands (`sudo env nice …`).
const MAX_LAYERS: usize = 16;
/// Nesting limit for brackets, compound commands and command lists; keeps both the
/// parser and this walker far from exhausting a 2 MiB thread stack.
const MAX_NESTING: usize = 32;

#[derive(Debug, Default)]
pub(crate) struct Analysis {
    /// Final argv of every simple command that runs (wrappers unwrapped).
    pub commands: Vec<Vec<Tok>>,
    /// Every argv deny/confirm rules are matched against: each wrapper layer,
    /// basename and git-global-option-free variants, and alternative readings of
    /// wrapper options.
    pub forms: Vec<Vec<Tok>>,
    pub destructive: Vec<String>,
    /// Why auto-allow is impossible although the command is fully understood.
    pub unlisted: Vec<String>,
    /// Why a prompt is required although nothing is destructive.
    pub ask: Vec<String>,
    /// Why the command could not be fully decomposed.
    pub undecomposable: Vec<String>,
}

pub(crate) fn analyze(src: &str, ws: &Workspace) -> Analysis {
    let mut walker = Walker {
        ws,
        out: Analysis::default(),
        lists: 0,
    };
    walker.program(src, &mut Cwd::at(ws.root()), 0);
    walker.out
}

fn push_unique(list: &mut Vec<String>, reason: String) {
    if !list.contains(&reason) {
        list.push(reason);
    }
}

struct Walker<'a> {
    ws: &'a Workspace,
    out: Analysis,
    /// Command lists currently being walked (across nested programs too).
    lists: usize,
}

impl Walker<'_> {
    fn undecomposable(&mut self, why: String) {
        push_unique(&mut self.out.undecomposable, why);
    }

    fn unlisted(&mut self, why: String) {
        push_unique(&mut self.out.unlisted, why);
    }

    fn ask(&mut self, why: String) {
        push_unique(&mut self.out.ask, why);
    }

    fn program(&mut self, src: &str, cwd: &mut Cwd, depth: usize) {
        if depth > MAX_DEPTH {
            return self.undecomposable("nested too deeply".into());
        }
        if src.chars().count() > MAX_INPUT_CHARS {
            self.undecomposable(format!("longer than {MAX_INPUT_CHARS} characters"));
            return self.rough_scan(src, depth);
        }
        if fallback::rough_nesting(src) > MAX_NESTING {
            self.undecomposable("nested too deeply".into());
            return self.rough_scan(src, depth);
        }
        let mut parser =
            brush_parser::Parser::new(std::io::Cursor::new(src), &argv::parser_options());
        match parser.parse_program() {
            Ok(program) => {
                for list in &program.complete_commands {
                    self.list(list, cwd, depth);
                }
            }
            Err(e) => {
                self.undecomposable(format!("shell syntax not understood ({e})"));
                self.rough_scan(src, depth);
            }
        }
    }

    /// Best-effort deny/destructive scan of text the parser could not handle.
    fn rough_scan(&mut self, src: &str, depth: usize) {
        for words in fallback::rough_commands(src) {
            let argv = words.into_iter().map(Tok::Lit).collect();
            self.exec(argv, &mut Cwd::unknown(), depth + 1, 0, false);
        }
    }

    fn list(&mut self, list: &ast::CompoundList, cwd: &mut Cwd, depth: usize) {
        if self.lists >= MAX_NESTING {
            return self.undecomposable("nested too deeply".into());
        }
        self.lists += 1;
        for ast::CompoundListItem(and_or, separator) in &list.0 {
            let before = cwd.clone();
            self.and_or(and_or, cwd, depth);
            // A background job runs in a subshell.
            if matches!(separator, SeparatorOperator::Async) {
                *cwd = before;
            }
        }
        self.lists -= 1;
    }

    /// Tracks the directories possible when the chain so far succeeded (`ok`) or
    /// failed (`failed`); a `cd` that fails leaves the directory unchanged.
    fn and_or(&mut self, list: &ast::AndOrList, cwd: &mut Cwd, depth: usize) {
        let mut ok = cwd.clone();
        self.pipeline(&list.first, &mut ok, depth);
        let mut failed = cwd.clone();
        failed.union(&ok);
        for next in &list.additional {
            match next {
                AndOr::And(p) => {
                    let mut out = ok.clone();
                    self.pipeline(p, &mut out, depth);
                    failed.union(&ok);
                    failed.union(&out);
                    ok = out;
                }
                AndOr::Or(p) => {
                    let mut out = failed.clone();
                    self.pipeline(p, &mut out, depth);
                    failed.union(&out);
                    ok.union(&out);
                }
            }
        }
        ok.union(&failed);
        *cwd = ok;
    }

    fn pipeline(&mut self, pipeline: &ast::Pipeline, cwd: &mut Cwd, depth: usize) {
        if let [only] = pipeline.seq.as_slice() {
            return self.command(only, cwd, depth);
        }
        // Each stage of a multi-command pipeline runs in a subshell.
        for cmd in &pipeline.seq {
            self.command(cmd, &mut cwd.clone(), depth);
        }
    }

    fn command(&mut self, cmd: &Command, cwd: &mut Cwd, depth: usize) {
        match cmd {
            Command::Simple(simple) => self.simple(simple, cwd, depth),
            Command::Compound(compound, redirects) => {
                self.redirects(redirects.as_ref(), cwd, depth);
                self.compound(compound, cwd, depth);
            }
            Command::Function(def) => {
                self.undecomposable(format!("defines the shell function `{}`", def.fname.value));
                self.redirects(def.body.1.as_ref(), cwd, depth);
                self.control_flow(&def.body.0, &mut cwd.clone(), depth);
            }
            Command::ExtendedTest(test, redirects) => {
                self.undecomposable("contains a `[[ … ]]` test".into());
                self.redirects(redirects.as_ref(), cwd, depth);
                self.test_expr(&test.expr, cwd, depth);
            }
        }
    }

    fn compound(&mut self, compound: &CompoundCommand, cwd: &mut Cwd, depth: usize) {
        match compound {
            CompoundCommand::BraceGroup(group) => self.list(&group.list, cwd, depth),
            CompoundCommand::Subshell(sub) => self.list(&sub.list, &mut cwd.clone(), depth),
            other => {
                let what = match other {
                    CompoundCommand::ForClause(_) | CompoundCommand::ArithmeticForClause(_) => {
                        "a `for` loop"
                    }
                    CompoundCommand::WhileClause(_) | CompoundCommand::UntilClause(_) => {
                        "a `while`/`until` loop"
                    }
                    CompoundCommand::CaseClause(_) => "a `case` statement",
                    CompoundCommand::IfClause(_) => "an `if` statement",
                    CompoundCommand::Arithmetic(_) => "an arithmetic command",
                    _ => "a coprocess",
                };
                self.undecomposable(format!("contains {what}"));
                let mut inner = cwd.clone();
                self.control_flow(other, &mut inner, depth);
                cwd.union(&inner);
            }
        }
    }

    /// Best effort: walks the bodies of unsupported constructs so deny and
    /// destructive matches inside them are still found.
    fn control_flow(&mut self, compound: &CompoundCommand, cwd: &mut Cwd, depth: usize) {
        match compound {
            CompoundCommand::BraceGroup(_) | CompoundCommand::Subshell(_) => {
                self.compound(compound, cwd, depth)
            }
            CompoundCommand::ForClause(f) => {
                for w in f.values.iter().flatten() {
                    self.word(&w.value, cwd, depth);
                }
                self.list(&f.body.list, cwd, depth);
            }
            CompoundCommand::ArithmeticForClause(f) => {
                for e in [&f.initializer, &f.condition, &f.updater]
                    .into_iter()
                    .flatten()
                {
                    self.expansion(&e.value, cwd, depth);
                }
                self.list(&f.body.list, cwd, depth);
            }
            CompoundCommand::CaseClause(c) => {
                self.word(&c.value.value, cwd, depth);
                for item in &c.cases {
                    for pattern in &item.patterns {
                        self.word(&pattern.value, cwd, depth);
                    }
                    if let Some(body) = &item.cmd {
                        self.list(body, cwd, depth);
                    }
                }
            }
            CompoundCommand::IfClause(c) => {
                self.list(&c.condition, cwd, depth);
                self.list(&c.then, cwd, depth);
                for branch in c.elses.iter().flatten() {
                    if let Some(condition) = &branch.condition {
                        self.list(condition, cwd, depth);
                    }
                    self.list(&branch.body, cwd, depth);
                }
            }
            CompoundCommand::WhileClause(c) | CompoundCommand::UntilClause(c) => {
                self.list(&c.0, cwd, depth);
                self.list(&c.1.list, cwd, depth);
            }
            CompoundCommand::Arithmetic(a) => {
                self.expansion(&a.expr.value, cwd, depth);
                // The parser also reads nested subshells `( (cmd) )` as `((…))`.
                self.rough_scan(&a.expr.value, depth);
            }
            CompoundCommand::Coprocess(c) => self.command(&c.body, cwd, depth),
        }
    }

    fn test_expr(&mut self, expr: &ExtendedTestExpr, cwd: &Cwd, depth: usize) {
        match expr {
            ExtendedTestExpr::And(a, b) | ExtendedTestExpr::Or(a, b) => {
                self.test_expr(a, cwd, depth);
                self.test_expr(b, cwd, depth);
            }
            ExtendedTestExpr::Not(e) | ExtendedTestExpr::Parenthesized(e) => {
                self.test_expr(e, cwd, depth)
            }
            ExtendedTestExpr::UnaryTest(_, w) => {
                self.word(&w.value, cwd, depth);
            }
            ExtendedTestExpr::BinaryTest(_, a, b) => {
                self.word(&a.value, cwd, depth);
                self.word(&b.value, cwd, depth);
            }
        }
    }

    fn simple(&mut self, cmd: &ast::SimpleCommand, cwd: &mut Cwd, depth: usize) {
        let mut argv = Vec::new();
        let mut assigns = false;
        for item in cmd.prefix.iter().flat_map(|p| &p.0) {
            if let Item::AssignmentWord(assignment, _) = item {
                assigns = true;
                self.assignment(assignment, cwd, depth);
            } else {
                self.item(item, &mut argv, cwd, depth);
            }
        }
        if let Some(name) = &cmd.word_or_name {
            argv.push(self.word(&name.value, cwd, depth));
        }
        for item in cmd.suffix.iter().flat_map(|s| &s.0) {
            self.item(item, &mut argv, cwd, depth);
        }
        if argv.is_empty() {
            if assigns {
                self.undecomposable("sets shell variables".into());
            }
            return;
        }
        if assigns {
            self.unlisted("sets environment variables for the command".into());
        }
        self.exec(argv, cwd, depth, 0, true);
    }

    fn item(&mut self, item: &Item, argv: &mut Vec<Tok>, cwd: &Cwd, depth: usize) {
        match item {
            // An assignment-looking argument (`export A=1`) is an ordinary word here.
            Item::Word(w) | Item::AssignmentWord(_, w) => {
                argv.push(self.word(&w.value, cwd, depth))
            }
            Item::IoRedirect(r) => self.redirect(r, cwd, depth),
            Item::ProcessSubstitution(_, sub) => {
                self.undecomposable("uses process substitution".into());
                self.list(&sub.list, &mut cwd.clone(), depth);
                argv.push(Tok::Dyn);
            }
        }
    }

    fn assignment(&mut self, assignment: &ast::Assignment, cwd: &Cwd, depth: usize) {
        if let ast::AssignmentName::ArrayElementName(_, index) = &assignment.name {
            self.expansion(index, cwd, depth);
        }
        match &assignment.value {
            ast::AssignmentValue::Scalar(w) => {
                self.word(&w.value, cwd, depth);
            }
            ast::AssignmentValue::Array(items) => {
                for (key, value) in items {
                    if let Some(key) = key {
                        self.word(&key.value, cwd, depth);
                    }
                    self.word(&value.value, cwd, depth);
                }
            }
        }
    }

    /// Converts a word; its command substitutions are analyzed as sub-commands
    /// (they run in a subshell, so `cd` inside them does not leak out).
    fn word(&mut self, raw: &str, cwd: &Cwd, depth: usize) -> Tok {
        let mut subs = Vec::new();
        let tok = argv::word_to_tok(raw, &mut subs).unwrap_or_else(|why| {
            self.undecomposable(why);
            Tok::Dyn
        });
        self.substitutions(subs, cwd, depth);
        tok
    }

    /// Analyzes the command substitutions inside arithmetic or array-index text.
    fn expansion(&mut self, text: &str, cwd: &Cwd, depth: usize) {
        let mut subs = Vec::new();
        if let Err(why) = argv::substitutions_in(text, &mut subs) {
            self.undecomposable(why);
        }
        self.substitutions(subs, cwd, depth);
    }

    fn substitutions(&mut self, subs: Vec<String>, cwd: &Cwd, depth: usize) {
        for sub in subs {
            self.program(&sub, &mut cwd.clone(), depth + 1);
        }
    }

    fn redirects(&mut self, list: Option<&ast::RedirectList>, cwd: &Cwd, depth: usize) {
        for r in list.iter().flat_map(|l| &l.0) {
            self.redirect(r, cwd, depth);
        }
    }

    fn redirect(&mut self, redirect: &IoRedirect, cwd: &Cwd, depth: usize) {
        match redirect {
            IoRedirect::File(_, kind, target) => {
                let output = !matches!(kind, Kind::Read | Kind::DuplicateInput);
                match target {
                    Target::Fd(_) => {}
                    Target::Filename(w) => {
                        let tok = self.word(&w.value, cwd, depth);
                        self.redirect_path(&tok, output, cwd);
                    }
                    Target::Duplicate(w) => {
                        let tok = self.word(&w.value, cwd, depth);
                        let fd = tok
                            .lit()
                            .is_some_and(|s| s == "-" || s.parse::<u16>().is_ok());
                        if !fd {
                            // `>&file` redirects stdout and stderr to a file.
                            self.redirect_path(&tok, output, cwd);
                        }
                    }
                    Target::ProcessSubstitution(_, sub) => {
                        self.undecomposable("uses process substitution".into());
                        self.list(&sub.list, &mut cwd.clone(), depth);
                    }
                }
            }
            IoRedirect::HereDocument(_, doc) => {
                // A quoted delimiter (`<<'EOF'`) makes the body literal.
                if doc.requires_expansion {
                    let mut subs = Vec::new();
                    if let Err(why) = argv::heredoc_substitutions(&doc.doc.value, &mut subs) {
                        self.undecomposable(why);
                    }
                    self.substitutions(subs, cwd, depth);
                }
            }
            IoRedirect::HereString(_, w) => {
                self.word(&w.value, cwd, depth);
            }
            IoRedirect::OutputAndError(w, _) => {
                let tok = self.word(&w.value, cwd, depth);
                self.redirect_path(&tok, true, cwd);
            }
        }
    }

    fn redirect_path(&mut self, target: &Tok, output: bool, cwd: &Cwd) {
        match (target, output) {
            (Tok::Lit(p), _) if p.starts_with("/dev/tcp/") || p.starts_with("/dev/udp/") => {
                self.ask(format!("redirection to `{p}` opens a network connection"));
            }
            (Tok::Lit(_), false) => {}
            (_, false) => self.unlisted("reads a file only known at run time".into()),
            (Tok::Lit(p), true)
                if matches!(p.as_str(), "/dev/null" | "/dev/stdout" | "/dev/stderr") => {}
            (Tok::Lit(p), true) => {
                let inside = cwd
                    .resolve(p)
                    .iter()
                    .all(|r| r.as_deref().is_some_and(|r| self.ws.strictly_contains(r)));
                if inside {
                    self.unlisted(format!("writes to `{p}`"));
                } else {
                    self.ask(format!("writes to `{p}` outside the workspace"));
                }
            }
            (_, true) => self.ask("writes to a file only known at run time".into()),
        }
    }

    /// Runs one argv: records its deny forms, then unwraps wrapper commands.
    fn exec(
        &mut self,
        argv: Vec<Tok>,
        cwd: &mut Cwd,
        depth: usize,
        layer: usize,
        same_shell: bool,
    ) {
        if argv.is_empty() {
            return;
        }
        self.add_forms(&argv, cwd);
        if layer > MAX_LAYERS {
            return self.undecomposable("too many nested wrapper commands".into());
        }
        let Some(w) = wrappers::unwrap(&argv) else {
            return self.run(argv, cwd, same_shell);
        };
        if let Some(why) = w.unlisted {
            self.unlisted(why);
        }
        if let Some(why) = w.ask {
            self.ask(why);
        }
        if let Some(why) = w.opaque {
            self.undecomposable(why);
        }
        let inner_cwd = if w.unknown_cwd {
            Cwd::unknown()
        } else {
            cwd.clone()
        };
        for alternative in &w.alternatives {
            self.add_forms(alternative, &inner_cwd);
        }
        let same_shell = same_shell && w.same_shell;
        for next in w.next {
            let mut own_cwd = inner_cwd.clone();
            let target = if same_shell { &mut *cwd } else { &mut own_cwd };
            match next {
                Next::Argv(inner) => self.exec(inner, target, depth, layer + 1, same_shell),
                Next::Script(src) => self.program(&src, target, depth + 1),
            }
        }
    }

    /// Records the command that actually runs once all wrappers are unwrapped.
    fn run(&mut self, argv: Vec<Tok>, cwd: &mut Cwd, same_shell: bool) {
        match &argv[0] {
            Tok::Lit(name) if name.contains('/') => self.unlisted(format!("runs `{name}` by path")),
            Tok::Lit(_) => {}
            _ => self.undecomposable("the command name is only known at run time".into()),
        }
        let name = argv[0].lit().map(basename).unwrap_or_default();
        if name == "git" && git::parse(&argv).overrides_config {
            self.undecomposable(
                "git `-c`/`--config-env`/`--exec-path` can run arbitrary programs".into(),
            );
        }
        if same_shell {
            match name {
                "cd" => cwd.cd(cd_target(&argv[1..])),
                "pushd" | "popd" => cwd.cd(None),
                _ => {}
            }
        }
        self.out.commands.push(argv);
    }

    fn add_forms(&mut self, argv: &[Tok], cwd: &Cwd) {
        if let Some(why) = destructive::check(argv, cwd, self.ws) {
            push_unique(&mut self.out.destructive, why);
        }
        self.out.forms.push(argv.to_vec());
        if let Some(Tok::Lit(name)) = argv.first()
            && name.contains('/')
        {
            let mut form = argv.to_vec();
            form[0] = Tok::Lit(basename(name).to_string());
            self.out.forms.push(form);
        }
        if let Some(form) = git::without_globals(argv) {
            self.out.forms.push(form);
        }
    }
}

/// The directory `cd ARGS` changes to, or `None` if it cannot be known statically.
fn cd_target(args: &[Tok]) -> Option<&str> {
    let mut rest = args.iter().skip_while(|t| {
        t.lit()
            .is_some_and(|s| s.len() > 1 && s.starts_with('-') && s != "--")
    });
    let target = match rest.next() {
        Some(Tok::Lit(s)) if s == "--" => rest.next(),
        other => other,
    };
    // No operand means $HOME; `cd -` means $OLDPWD.
    target.and_then(Tok::lit).filter(|s| *s != "-")
}
