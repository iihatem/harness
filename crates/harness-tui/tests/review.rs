//! M3d: the results agents leave in worktrees are looked at and applied, kept or discarded from
//! the terminal: the panel says what is ready, `/results` shows the diff and asks, and nothing is
//! applied without the user's choice.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::*;
use harness_core::{
    agent::{Agent, NonInteractive},
    agents::{
        AgentFactory, AgentSpec, AgentStatus, ChildSetup, Endpoint, Limits, ParentContext,
        Supervisor,
    },
    engine::{EngineConfig, PermissionEngine, RuleSet},
    permission::Mode,
    testing::MockProvider,
    worktree::{
        Request, Worktrees,
        testing::{git, init_repo, write},
    },
};
use harness_tui::app::{Host, Prepared};
use tokio_util::sync::CancellationToken;

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

struct NoFactory;

#[async_trait]
impl AgentFactory for NoFactory {
    fn endpoint(&self, _spec: &AgentSpec) -> Endpoint {
        Endpoint {
            key: "mock".into(),
            local: false,
            delegated: false,
        }
    }
    async fn build(&self, _setup: ChildSetup) -> Result<Agent, String> {
        Err("no children in these tests".into())
    }
}

struct Rig {
    main: tempfile::TempDir,
    _home: tempfile::TempDir,
    _dir: tempfile::TempDir,
    wts: Arc<Worktrees>,
    ui: harness_tui::ui::Ui<ratatui::backend::TestBackend>,
}

fn rig() -> Rig {
    let main = tempfile::tempdir().unwrap();
    init_repo(main.path());
    let home = tempfile::tempdir().unwrap();
    let wts = Arc::new(Worktrees::new(main.path(), home.path().join("p")).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(MockProvider::new(vec![]), dir.path(), Mode::Auto);
    let (ui, _) = start(agent, Box::new(NoCommands), options(dir.path(), Mode::Auto));
    let config = EngineConfig {
        mode: Mode::Auto,
        workspace: main.path().to_path_buf(),
        read_dirs: vec![],
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let sup = Supervisor::new(
        Limits::default(),
        Arc::new(NoFactory),
        ParentContext {
            session: "s".into(),
            cancel: CancellationToken::new(),
            policy: Arc::new(PermissionEngine::new(config.clone())),
            engine: config,
            approver: Arc::new(NonInteractive),
            meter: None,
            sessions_dir: None,
            workspace: main.path().to_path_buf(),
        },
        tx,
    );
    sup.set_worktrees(wts.clone());
    let mut rig = Rig {
        main,
        _home: home,
        _dir: dir,
        wts,
        ui,
    };
    rig.ui.watch_agents(sup);
    rig
}

impl Rig {
    /// An agent that edited `path` and ended `status`: its result committed and recorded.
    fn result(
        &self,
        agent: &str,
        task: &str,
        path: &str,
        text: &str,
        status: AgentStatus,
    ) -> String {
        let wt = self
            .wts
            .create(&Request {
                agent,
                label: "general",
                task,
            })
            .unwrap()
            .worktree;
        write(&wt.path, path, text);
        let outcome = self.wts.commit_result(&wt, "general", task).unwrap();
        self.wts.release(&wt).unwrap();
        self.wts
            .update_record(&wt.name, |r| {
                r.status = status;
                r.summary = Some("did the work".into());
                r.outcome = outcome;
            })
            .unwrap();
        wt.name
    }

    /// Types `/results ...`, and waits for the diff and the question that follow.
    async fn review(&mut self, args: &str) {
        send(&mut self.ui, &format!("/results {args}"));
        until(&mut self.ui, |app| app.picker().is_some()).await;
    }

    async fn choose(&mut self, what: &str) {
        until_armed(&self.ui).await;
        type_text(&mut self.ui, what);
        press(&mut self.ui, ratatui::crossterm::event::KeyCode::Enter);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_result_ready_for_review_is_on_the_panel_with_its_branch() {
    let mut rig = rig();
    let name = rig.result(
        "a1",
        "fix the parser",
        "src/new.rs",
        "pub fn two() {}\n",
        AgentStatus::Done,
    );
    // A result that is not ready: its agent is still running.
    rig.ui.refresh_results();
    rig.ui.draw().unwrap();
    let all = screen(&rig.ui).join("\n");
    assert!(all.contains("ready for review"), "{all}");
    assert!(all.contains(&format!("harness/{name}")), "{all}");
    assert!(all.contains("/results"), "{all}");
}

#[tokio::test(flavor = "multi_thread")]
async fn review_shows_the_diff_and_asks_what_to_do_and_applies_only_when_told() {
    let mut rig = rig();
    let name = rig.result(
        "a1",
        "fix the parser",
        "README.md",
        "hello\nthere\n",
        AgentStatus::Done,
    );
    rig.ui.refresh_results();
    rig.review("").await;
    // The diff is in the transcript, coloured by the theme (here, plain): added and removed
    // lines, the file, and the question has the three choices.
    assert!(shows(&rig.ui, "README.md"));
    assert!(shows(&rig.ui, "+there"));
    assert!(shows(&rig.ui, &format!("harness/{name}")));
    let picker = screen(&rig.ui).join("\n");
    assert!(
        picker.contains("Apply") && picker.contains("Keep") && picker.contains("Discard"),
        "{picker}"
    );
    // Nothing happened yet.
    assert_eq!(
        std::fs::read_to_string(rig.main.path().join("README.md")).unwrap(),
        "hello\n"
    );
    // Not now: closes, changes nothing.
    rig.choose("not now").await;
    assert_eq!(
        std::fs::read_to_string(rig.main.path().join("README.md")).unwrap(),
        "hello\n"
    );
    assert_eq!(rig.wts.records().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn applying_brings_the_commit_into_the_main_checkout_and_the_result_leaves_the_panel() {
    let mut rig = rig();
    let name = rig.result(
        "a1",
        "fix the parser",
        "src/new.rs",
        "pub fn two() {}\n",
        AgentStatus::Done,
    );
    rig.ui.refresh_results();
    rig.review("").await;
    rig.choose("apply").await;
    until(&mut rig.ui, |app| !app.reviewing()).await;
    assert!(rig.main.path().join("src/new.rs").exists());
    assert!(shows(&rig.ui, "applied"), "{:?}", everything(&rig.ui));
    assert!(rig.wts.records().is_empty());
    assert!(
        git(
            rig.main.path(),
            &["branch", "--list", &format!("harness/{name}")]
        )
        .trim()
        .is_empty()
    );
    assert!(!screen(&rig.ui).join("\n").contains("ready for review"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conflict_is_said_and_changes_nothing_and_the_result_stays_for_review() {
    let mut rig = rig();
    let name = rig.result("a1", "edit", "README.md", "agent\n", AgentStatus::Done);
    write(rig.main.path(), "README.md", "user\n");
    git(rig.main.path(), &["commit", "-q", "-am", "user edit"]);
    let head = git(rig.main.path(), &["rev-parse", "HEAD"]);
    rig.ui.refresh_results();
    rig.review("").await;
    rig.choose("apply").await;
    until(&mut rig.ui, |app| !app.reviewing()).await;
    assert!(
        shows_wrapped(&rig.ui, "conflicts in README.md"),
        "{:?}",
        everything(&rig.ui)
    );
    assert!(shows_wrapped(&rig.ui, "nothing was changed"));
    assert_eq!(git(rig.main.path(), &["rev-parse", "HEAD"]), head);
    assert_eq!(git(rig.main.path(), &["status", "--porcelain"]).trim(), "");
    assert_eq!(rig.wts.records().len(), 1);
    assert!(
        screen(&rig.ui)
            .join("\n")
            .contains(&format!("harness/{name}"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn keeping_leaves_the_branch_and_removes_the_worktree() {
    let mut rig = rig();
    let name = rig.result("a1", "edit", "src/new.rs", "x\n", AgentStatus::Done);
    rig.ui.refresh_results();
    rig.review("").await;
    rig.choose("keep").await;
    until(&mut rig.ui, |app| !app.reviewing()).await;
    assert!(shows(&rig.ui, &format!("harness/{name}")));
    assert!(shows(&rig.ui, "kept"));
    assert!(rig.wts.records().is_empty());
    assert!(
        !git(
            rig.main.path(),
            &["branch", "--list", &format!("harness/{name}")]
        )
        .trim()
        .is_empty()
    );
    assert!(!rig.main.path().join("src/new.rs").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn discarding_asks_again_and_then_removes_everything() {
    let mut rig = rig();
    let name = rig.result("a1", "edit", "src/new.rs", "x\n", AgentStatus::Done);
    rig.ui.refresh_results();
    rig.review("").await;
    rig.choose("discard").await;
    // A second question, so one wrong key does not throw work away.
    until(&mut rig.ui, |app| app.picker().is_some()).await;
    assert!(
        screen(&rig.ui).join("\n").contains("cannot be undone"),
        "{:?}",
        screen(&rig.ui)
    );
    assert_eq!(rig.wts.records().len(), 1);
    rig.choose("no").await;
    assert_eq!(rig.wts.records().len(), 1, "No keeps it");
    rig.review("").await;
    rig.choose("discard").await;
    until(&mut rig.ui, |app| app.picker().is_some()).await;
    rig.choose("yes").await;
    until(&mut rig.ui, |app| !app.reviewing()).await;
    assert!(rig.wts.records().is_empty());
    assert!(
        git(
            rig.main.path(),
            &["branch", "--list", &format!("harness/{name}")]
        )
        .trim()
        .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn results_that_changed_the_same_file_are_warned_about() {
    let mut rig = rig();
    rig.result("a1", "one", "README.md", "one\n", AgentStatus::Done);
    let second = rig.result("a2", "two", "README.md", "two\n", AgentStatus::Done);
    rig.ui.refresh_results();
    rig.review(&format!("harness/{second}")).await;
    assert!(
        shows_wrapped(&rig.ui, "also changed by"),
        "{:?}",
        everything(&rig.ui)
    );
    assert!(shows(&rig.ui, "README.md"));
}

#[tokio::test(flavor = "multi_thread")]
async fn with_several_results_review_asks_which_and_a_name_picks_one() {
    let mut rig = rig();
    let a = rig.result("a1", "one", "a.txt", "1\n", AgentStatus::Done);
    rig.result("a2", "two", "b.txt", "2\n", AgentStatus::Failed);
    rig.ui.refresh_results();
    send(&mut rig.ui, "/results");
    until(&mut rig.ui, |app| app.picker().is_some()).await;
    let which = screen(&rig.ui).join("\n");
    assert!(which.contains("Review which"), "{which}");
    assert!(which.contains("failed"), "{which}");
    rig.choose("one").await;
    until(&mut rig.ui, |app| app.picker().is_some()).await;
    assert!(shows(&rig.ui, "a.txt"));
    rig.choose("not now").await;
    // By branch name directly.
    rig.review(&format!("harness/{a}")).await;
    assert!(shows(&rig.ui, "+1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_to_review_says_so_and_an_unknown_name_says_what_is_there() {
    let mut rig = rig();
    send(&mut rig.ui, "/results");
    assert!(
        shows_wrapped(&rig.ui, "nothing to review"),
        "{:?}",
        everything(&rig.ui)
    );
    let name = rig.result("a1", "one", "a.txt", "1\n", AgentStatus::Done);
    rig.ui.refresh_results();
    send(&mut rig.ui, "/results nope");
    assert!(
        shows_wrapped(&rig.ui, "no result named nope"),
        "{:?}",
        everything(&rig.ui)
    );
    assert!(shows(&rig.ui, &format!("harness/{name}")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_very_long_diff_is_cut_and_says_how_to_see_the_rest() {
    let mut rig = rig();
    let long: String = (0..3000).map(|n| format!("line {n}\n")).collect();
    let name = rig.result("a1", "big", "big.txt", &long, AgentStatus::Done);
    rig.ui.refresh_results();
    rig.review("").await;
    // The diff scrolls with Page Down; the end says it was cut and how to see the rest.
    assert!(
        screen(&rig.ui).join("\n").matches("+line").count() < 30,
        "a screenful at a time"
    );
    until_armed(&rig.ui).await;
    for _ in 0..200 {
        press(&mut rig.ui, ratatui::crossterm::event::KeyCode::PageDown);
    }
    let all = screen(&rig.ui).join("\n");
    assert!(all.contains("more lines"), "the cut is said: {all}");
    assert!(all.contains("git diff"), "{all}");
    assert!(all.contains(&format!("harness/{name}")));
    // And back.
    for _ in 0..200 {
        press(&mut rig.ui, ratatui::crossterm::event::KeyCode::PageUp);
    }
    assert!(screen(&rig.ui).join("\n").contains("big.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_lists_the_results_of_earlier_sessions_for_review() {
    let mut rig = rig();
    rig.result("a1", "one", "a.txt", "1\n", AgentStatus::Done);
    rig.result("a2", "two", "b.txt", "2\n", AgentStatus::Failed);
    rig.ui.refresh_results();
    send(&mut rig.ui, "/agents");
    let all = everything(&rig.ui).join("\n");
    assert!(all.contains("Results waiting for review"), "{all}");
    assert!(all.contains("ready for review"), "{all}");
    assert!(all.contains("failed"), "{all}");
}
