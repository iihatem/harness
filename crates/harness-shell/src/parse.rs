//! AST walk: decomposes a command line into the simple commands it runs and
//! collects everything the verdict needs.

use brush_parser::ast::{
    self, AndOr, Command, CommandPrefixOrSuffixItem as Item, CompoundCommand, ExtendedTestExpr,
    IoFileRedirectKind as Kind, IoFileRedirectTarget as Target, IoRedirect, SeparatorOperator,
};

use crate::argv::{self, Hidden, MAX_DEPTH, Scan, Tok, basename, command_name};
use crate::destructive;
use crate::fallback;
use crate::git;
use crate::paths::{Cwd, Workspace};
use crate::wrappers::{self, Next, assigned_name, runs_programs};

/// Longer input is not parsed (it is only roughly scanned for deny matches).
const MAX_INPUT_CHARS: usize = 10_000;
/// Builtins whose `NAME=value` operands set shell variables.
const DECLARATION_BUILTINS: &[&str] = &["export", "declare", "typeset", "local", "readonly"];
/// Builtins that evaluate their operands' text as arithmetic, subscripts or variable
/// names (`printf` only with `-v`).
const EVALUATING_BUILTINS: &[&str] = &[
    "let",
    "declare",
    "typeset",
    "local",
    "readonly",
    "export",
    "unset",
    "read",
    "mapfile",
    "readarray",
];

/// A declaration operand the shell expands as an assignment, at a known argv position.
enum Operand<'a> {
    /// `NAME=…`, whose NAME is literal.
    Name(&'a str),
    /// `NAME[i]=…`; a shell without arrays glob-expands the subscript, so the NAME is
    /// not trustworthy.
    Array,
}

/// What the words of a simple command say about the operands of the argv it runs.
#[derive(Default)]
struct Operands<'a> {
    /// `NAME=value` operands the shell expands as assignments, by argv position (only
    /// for the argv as written, not once a wrapper is unwrapped).
    assignments: Vec<(usize, Operand<'a>)>,
    /// Literal text, in the words after the command name, that bash may evaluate later.
    hidden: Hidden,
}

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
    /// Forms found in text that may be here-document data rather than commands: a deny
    /// rule matching one of them only asks.
    pub data_forms: Vec<Vec<Tok>>,
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
        data: 0,
        misread: None,
    };
    walker.program(src, &mut Cwd::at(ws.root()), 0);
    walker.out
}

fn push_unique(list: &mut Vec<String>, reason: String) {
    if !list.contains(&reason) {
        list.push(reason);
    }
}

/// The variable an assignment sets (`A` for `A[i]=x`).
fn assignment_name(assignment: &ast::Assignment) -> &str {
    match &assignment.name {
        ast::AssignmentName::VariableName(n) | ast::AssignmentName::ArrayElementName(n, _) => n,
    }
}

struct Walker<'a> {
    ws: &'a Workspace,
    out: Analysis,
    /// Command lists currently being walked (across nested programs too).
    lists: usize,
    /// Rough commands from possible here-document data currently being walked.
    data: usize,
    /// Why a here-document in the program being walked may end elsewhere in bash.
    misread: Option<&'static str>,
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
        if let Some(why) = argv::unsafe_heredoc(src).or_else(|| argv::program_too_nested(src)) {
            self.undecomposable(why.into());
            return self.rough_scan(src, depth);
        }
        let panics = argv::parser_panics();
        let mut parser =
            brush_parser::Parser::new(std::io::Cursor::new(src), &argv::parser_options());
        match argv::guarded(|| parser.parse_program()) {
            None => {
                self.undecomposable(argv::PARSER_PANICKED.into());
                self.rough_scan(src, depth);
            }
            Some(Ok(program)) => {
                let outer = self.misread.take();
                for list in &program.complete_commands {
                    self.list(list, cwd, depth);
                }
                // What follows such a here-document may be commands brush-parser took
                // for its body, in this program or in any enclosing one.
                let misread = self.misread;
                self.misread = outer.or(misread);
                let panicked = argv::parser_panics() > panics;
                if let Some(why) = misread.or(panicked.then_some(argv::PARSER_PANICKED)) {
                    self.undecomposable(why.into());
                    self.rough_scan(src, depth);
                }
            }
            Some(Err(e)) => {
                self.undecomposable(format!("shell syntax not understood ({e})"));
                self.rough_scan(src, depth);
            }
        }
    }

    /// Best-effort deny/destructive scan of text the parser could not handle.
    fn rough_scan(&mut self, src: &str, depth: usize) {
        self.rough(fallback::rough_commands(src), depth);
    }

    fn rough(&mut self, commands: Vec<fallback::Rough>, depth: usize) {
        for command in commands {
            self.data += usize::from(command.data);
            let operands = Operands::default();
            self.exec(
                command.words,
                &operands,
                &mut Cwd::unknown(),
                depth + 1,
                0,
                false,
            );
            self.data -= usize::from(command.data);
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
        let mut dangerous_env = false;
        for item in cmd.prefix.iter().flat_map(|p| &p.0) {
            if let Item::AssignmentWord(assignment, _) = item {
                assigns = true;
                dangerous_env |= runs_programs(assignment_name(assignment));
                self.assignment(assignment, cwd, depth);
            } else {
                self.item(item, &mut argv, cwd, depth);
            }
        }
        if let Some(name) = &cmd.word_or_name {
            argv.push(self.word(&name.value, cwd, depth));
        }
        // bash expands `NAME=value` operands as assignments (no word splitting or
        // pathname expansion, so NAME is what it looks like) only when a declaration
        // builtin is the unquoted command word and no variable is assigned before it;
        // otherwise `FOO=$x` may expand to several operands.
        let expands_assignments = !assigns
            && cmd
                .word_or_name
                .as_ref()
                .is_some_and(|w| DECLARATION_BUILTINS.contains(&w.value.as_str()));
        // Those operands, by argv position.
        let mut operands = Operands::default();
        for item in cmd.suffix.iter().flat_map(|s| &s.0) {
            if let Item::AssignmentWord(assignment, _) = item
                && expands_assignments
            {
                let operand = match &assignment.name {
                    ast::AssignmentName::VariableName(n) => Operand::Name(n),
                    ast::AssignmentName::ArrayElementName(..) => Operand::Array,
                };
                operands.assignments.push((argv.len(), operand));
            }
            let hidden = self.item(item, &mut argv, cwd, depth);
            operands.hidden = operands.hidden.union(hidden);
        }
        if argv.is_empty() {
            if assigns {
                self.undecomposable("sets shell variables".into());
            }
            return;
        }
        if dangerous_env {
            self.undecomposable(
                "sets an environment variable that makes programs run other commands".into(),
            );
        } else if assigns {
            self.unlisted("sets environment variables for the command".into());
        }
        self.exec(argv, &operands, cwd, depth, 0, true);
    }

    /// Adds an argv word (if `item` is one) and returns its hidden literal text.
    fn item(&mut self, item: &Item, argv: &mut Vec<Tok>, cwd: &Cwd, depth: usize) -> Hidden {
        match item {
            // An assignment-looking argument (`export A=1`) is an ordinary word here.
            Item::Word(w) | Item::AssignmentWord(_, w) => {
                let (tok, hidden) = self.scan_word(&w.value, cwd, depth);
                argv.push(tok);
                return hidden;
            }
            Item::IoRedirect(r) => self.redirect(r, cwd, depth),
            Item::ProcessSubstitution(_, sub) => {
                self.undecomposable("uses process substitution".into());
                self.list(&sub.list, &mut cwd.clone(), depth);
                argv.push(Tok::Dyn);
            }
        }
        Hidden::default()
    }

    fn assignment(&mut self, assignment: &ast::Assignment, cwd: &Cwd, depth: usize) {
        if let ast::AssignmentName::ArrayElementName(_, index) = &assignment.name {
            self.arith(index, cwd, depth);
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
        self.scan_word(raw, cwd, depth).0
    }

    /// Like [`Self::word`], also returning the word's hidden literal text.
    fn scan_word(&mut self, raw: &str, cwd: &Cwd, depth: usize) -> (Tok, Hidden) {
        if self.too_nested(raw, false, depth) {
            return (Tok::Dyn, Hidden::default());
        }
        let mut scan = Scan::default();
        let tok = argv::word_to_tok(raw, &mut scan).unwrap_or_else(|why| {
            self.undecomposable(why);
            Tok::Dyn
        });
        let hidden = scan.hidden;
        self.scanned(scan, cwd, depth);
        (tok, hidden)
    }

    /// Text nested too deeply to parse in bounded time is undecomposable; it is only
    /// roughly scanned for deny matches.
    fn too_nested(&mut self, text: &str, heredoc: bool, depth: usize) -> bool {
        let Some(why) = argv::too_nested(text, heredoc) else {
            return false;
        };
        self.undecomposable(why.into());
        if heredoc {
            self.rough(fallback::rough_heredoc(text), depth);
        } else {
            self.rough_scan(text, depth);
        }
        true
    }

    /// Analyzes the command substitutions inside arithmetic or array-index text.
    fn expansion(&mut self, text: &str, cwd: &Cwd, depth: usize) {
        let mut scan = Scan::default();
        if let Err(why) = argv::substitutions_in(text, &mut scan) {
            self.undecomposable(why);
        }
        self.scanned(scan, cwd, depth);
    }

    /// Analyzes text the shell evaluates as arithmetic (a subscript, `let` operand, …);
    /// a substitution surviving quote removal there is undecomposable.
    fn arith(&mut self, text: &str, cwd: &Cwd, depth: usize) {
        let mut scan = Scan::default();
        if let Err(why) = argv::arithmetic_in(text, &mut scan) {
            self.undecomposable(why);
        }
        self.scanned(scan, cwd, depth);
    }

    /// Records what scanning a text found: reasons it is opaque, and substitutions.
    fn scanned(&mut self, scan: Scan, cwd: &Cwd, depth: usize) {
        for why in scan.opaque {
            self.undecomposable(why);
        }
        for sub in scan.subs {
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
                let (delimiter, body) = (&doc.here_end.value, &doc.doc.value);
                if let Some(why) =
                    argv::heredoc_misread(delimiter, body, doc.requires_expansion, doc.remove_tabs)
                {
                    self.misread = Some(why);
                }
                // A quoted delimiter (`<<'EOF'`) makes the body literal.
                if doc.requires_expansion && !self.too_nested(&doc.doc.value, true, depth) {
                    let mut scan = Scan::default();
                    if let Err(why) = argv::heredoc_substitutions(&doc.doc.value, &mut scan) {
                        self.undecomposable(why);
                    }
                    self.scanned(scan, cwd, depth);
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
        operands: &Operands,
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
            return self.run(argv, operands, cwd, same_shell);
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
        // The wrapped command's operands come from the same words.
        let inner_operands = Operands {
            hidden: operands.hidden,
            ..Default::default()
        };
        for next in w.next {
            let mut own_cwd = inner_cwd.clone();
            let target = if same_shell { &mut *cwd } else { &mut own_cwd };
            match next {
                Next::Argv(inner) => {
                    self.exec(inner, &inner_operands, target, depth, layer + 1, same_shell)
                }
                Next::Script(src) => self.program(&src, target, depth + 1),
            }
        }
    }

    /// Records the command that actually runs once all wrappers are unwrapped. Its name is
    /// matched in any case (a case-insensitive file system finds `READ` as `/usr/bin/read`,
    /// which runs the builtin), except for directory changes: only the builtin `cd`
    /// changes the shell's directory.
    fn run(&mut self, argv: Vec<Tok>, operands: &Operands, cwd: &mut Cwd, same_shell: bool) {
        match &argv[0] {
            Tok::Lit(name) if !name.is_ascii() => self.undecomposable(
                "the command name has non-ASCII characters, which the file system may match to another name"
                    .into(),
            ),
            Tok::Lit(name) if name.contains('/') => self.unlisted(format!("runs `{name}` by path")),
            Tok::Lit(_) => {}
            _ => self.undecomposable("the command name is only known at run time".into()),
        }
        let word = argv[0].lit().map(basename).unwrap_or_default();
        let name = command_name(word);
        let name = name.as_str();
        if name == "git" && git::parse(&argv).overrides_config {
            self.undecomposable(
                "git `-c`/`--config-env`/`--exec-path` can run arbitrary programs".into(),
            );
        }
        if evaluates_operands(name, &argv)
            && let Some(why) = hidden_reason(name, operands.hidden)
        {
            self.undecomposable(why);
        }
        if DECLARATION_BUILTINS.contains(&name) {
            self.declaration_operands(name, &argv, &operands.assignments);
        }
        if name == "alias" {
            self.alias_operands(&argv);
        }
        if same_shell {
            match word {
                "cd" => cwd.cd(cd_target(&argv[1..])),
                "pushd" | "popd" => cwd.cd(None),
                _ => {}
            }
        }
        self.out.commands.push(argv);
    }

    /// Setting a variable that makes programs run other commands is as opaque as
    /// running them; so is an operand whose NAME is only known at run time, and one
    /// whose NAME has a subscript or glob character (bash evaluates the subscript, and a
    /// shell without arrays glob-expands the whole operand into another name).
    fn declaration_operands(
        &mut self,
        builtin: &str,
        argv: &[Tok],
        assignments: &[(usize, Operand)],
    ) {
        let subscripted = |walker: &mut Self| {
            walker.undecomposable(format!(
                "`{builtin}` operand name contains `[`, `]`, `*` or `?`, so the variable it sets is not known"
            ));
        };
        for (i, arg) in argv.iter().enumerate().skip(1) {
            let recorded = assignments.iter().find(|&(at, _)| *at == i).map(|(_, o)| o);
            let var = match arg {
                Tok::Lit(s) if subscripted_or_glob_name(s) => return subscripted(self),
                _ if matches!(recorded, Some(Operand::Array)) => return subscripted(self),
                Tok::Lit(s) => assigned_name(s),
                _ => match recorded {
                    Some(Operand::Name(name)) => name,
                    _ => {
                        return self.undecomposable(format!(
                            "`{builtin}` operand is only known at run time"
                        ));
                    }
                },
            };
            if runs_programs(var) {
                return self.undecomposable(format!(
                    "sets `{var}`, which makes programs run other commands"
                ));
            }
        }
    }

    /// An alias definition can change what a later command runs.
    fn alias_operands(&mut self, argv: &[Tok]) {
        for arg in &argv[1..] {
            let defines = match arg {
                Tok::Lit(s) => s.contains('='),
                _ => true,
            };
            if defines {
                return self.undecomposable(
                    "defines an alias, which can change what later commands run".into(),
                );
            }
        }
    }

    fn add_forms(&mut self, argv: &[Tok], cwd: &Cwd) {
        if let Some(why) = destructive::check(argv, cwd, self.ws) {
            push_unique(&mut self.out.destructive, why);
        }
        let forms = if self.data > 0 {
            &mut self.out.data_forms
        } else {
            &mut self.out.forms
        };
        forms.push(argv.to_vec());
        if let Some(Tok::Lit(name)) = argv.first()
            && name.contains('/')
        {
            let mut form = argv.to_vec();
            form[0] = Tok::Lit(basename(name).to_string());
            forms.push(form);
        }
        if let Some(form) = git::without_globals(argv) {
            forms.push(form);
        }
    }
}

/// Why literal text in the operands of the evaluating builtin `name` makes it
/// undecomposable, if it does. In a declaration builtin's values (after the first `=`)
/// only substitution markers count: a bracket there is plain text (`[::1]`, `^[a-z]+$`).
fn hidden_reason(name: &str, hidden: Hidden) -> Option<String> {
    let declaration = DECLARATION_BUILTINS.contains(&name);
    if hidden.substitution {
        Some(format!(
            "`{name}` operand contains quoted command-substitution text that bash may evaluate"
        ))
    } else if hidden.undecodable {
        Some(format!(
            "`{name}` operand contains `$'…'` text that cannot be decoded, which bash may evaluate"
        ))
    } else if hidden.name_bracket || (hidden.value_bracket && !declaration) {
        Some(format!(
            "`{name}` operand contains a quoted `[` or `]`, which bash may evaluate as a subscript"
        ))
    } else {
        None
    }
}

/// Whether the builtin `name` evaluates its operands' text (`printf` only assigns, and
/// so evaluates a subscript, with `-v`, which a run-time first operand may be).
fn evaluates_operands(name: &str, argv: &[Tok]) -> bool {
    EVALUATING_BUILTINS.contains(&name)
        || (name == "printf"
            && argv
                .get(1)
                .is_some_and(|t| t.lit().is_none_or(|s| s.starts_with("-v"))))
}

/// Whether the NAME of a `NAME=value` operand (or a bare NAME) has a subscript or glob
/// character.
fn subscripted_or_glob_name(operand: &str) -> bool {
    let name = operand.split('=').next().unwrap_or(operand);
    name.contains(['[', ']', '*', '?'])
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
