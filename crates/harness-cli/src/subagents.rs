//! The session's subagents: the definitions found for it, and the tool that starts them.

use std::sync::Arc;

use harness_config::config;
use harness_context::{agents, project::project_root};
use harness_core::agents::{Definitions, SpawnSettings};

use crate::{context::home, notices::Notices, setup::Setup};

/// What a session has for starting subagents.
#[derive(Clone)]
pub struct Subagents {
    /// The built-ins and the definitions found in files, a project definition's model dropped
    /// when the workspace is not trusted.
    pub definitions: Arc<Definitions>,
    /// `[agents] max_spawns_per_turn` and the background role's model. (`max_depth` above 1 is
    /// not offered: a subagent never starts subagents.)
    pub settings: SpawnSettings,
    /// `[agents] spawn`: whether the main agent is offered the tool at all.
    pub spawn: bool,
}

/// Finds the definitions for `setup`'s workspace, warning (once for each file) about what was
/// ignored.
pub fn load(setup: &Setup, notices: &mut Notices) -> Subagents {
    let root = project_root(&setup.workspace);
    let found = agents::discover(&root, &setup.paths.config_dir, home().as_deref());
    for warning in &found.warnings {
        notices.warn(warning);
    }
    // Project definitions come with the repository: they choose their model only when the user
    // trusts the directory they come from, as command files do.
    let trusted = config::is_trusted(&setup.paths.global_config_file(), &root, &setup.trust);
    let definitions = Definitions::new(found.definitions).with_trust(trusted);
    let limits = setup.config.agents;
    Subagents {
        definitions: Arc::new(definitions),
        settings: SpawnSettings {
            max_per_turn: limits.max_spawns_per_turn,
            background_model: setup.config.roles.background.clone(),
            // The session's own model and its roles' are read live from the agent (the session
            // gives it in `Started::spawn_tool`); the main role's configured model is the one
            // other model an entry may name.
            parent: harness_core::agents::ParentModel::default(),
            also_in_play: setup.config.roles.main.iter().cloned().collect(),
        },
        spawn: limits.spawn,
    }
}

#[cfg(test)]
mod tests {
    use harness_core::agents::DefinitionSource;

    use super::load;
    use crate::{host::tests::setup_with, notices::Notices};

    fn setup(
        config: &str,
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, std::sync::Arc<crate::setup::Setup>) {
        let dir = tempfile::tempdir().unwrap();
        let (home, workspace) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::write(home.join("config/config.toml"), config).unwrap();
        for (path, text) in files {
            let path = if let Some(rest) = path.strip_prefix("home:") {
                home.join("config").join(rest)
            } else {
                workspace.join(path)
            };
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let workspace = workspace.canonicalize().unwrap();
        (dir, setup_with(&home, &workspace, &[]))
    }

    fn notices(setup: &crate::setup::Setup) -> Notices {
        Notices::quiet(setup.redactor.clone())
    }

    const PROJECT_AGENT: &str =
        "---\nname: triage\ndescription: Sorts failures\nmodel: ollama/qwen3:14b\n---\nLook.\n";

    #[test]
    fn project_and_user_definitions_are_found_beside_the_built_ins() {
        let (_dir, setup) = setup(
            "",
            &[
                (".harness/agents/triage.md", PROJECT_AGENT),
                ("home:agents/mine.md", "---\ndescription: Mine\n---\nx\n"),
            ],
        );
        let found = load(&setup, &mut notices(&setup));
        let names: Vec<&str> = found.definitions.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["explore", "review", "general", "mine", "triage"]);
    }

    #[test]
    fn a_project_definitions_model_is_dropped_until_the_workspace_is_trusted() {
        let (_dir, setup) = setup("", &[(".harness/agents/triage.md", PROJECT_AGENT)]);
        let found = load(&setup, &mut notices(&setup));
        let triage = found.definitions.get("triage").unwrap();
        assert_eq!(triage.model, None);
        assert_eq!(triage.model_ignored.as_deref(), Some("ollama/qwen3:14b"));
        assert!(matches!(triage.source, DefinitionSource::Project(_)));
    }

    #[test]
    fn a_users_own_definition_keeps_its_model_without_trust() {
        let (_dir, setup) = setup(
            "",
            &[(
                "home:agents/mine.md",
                "---\ndescription: Mine\nmodel: ollama/x\n---\nx\n",
            )],
        );
        let found = load(&setup, &mut notices(&setup));
        assert_eq!(
            found.definitions.get("mine").unwrap().model.as_deref(),
            Some("ollama/x")
        );
    }

    #[test]
    fn the_background_role_in_the_configuration_is_what_read_only_agents_default_to() {
        let (_dir, setup) = setup("[roles]\nbackground = \"ollama/small\"\n", &[]);
        let found = load(&setup, &mut notices(&setup));
        assert_eq!(
            found.settings.background_model.as_deref(),
            Some("ollama/small")
        );
        assert_eq!(found.settings.max_per_turn, 10);
    }

    #[test]
    fn the_limit_on_spawns_per_turn_comes_from_the_configuration() {
        let (_dir, setup) = setup("[agents]\nmax_spawns_per_turn = 3\nmax_depth = 5\n", &[]);
        let found = load(&setup, &mut notices(&setup));
        assert_eq!(found.settings.max_per_turn, 3);
    }

    #[test]
    fn a_file_that_cannot_be_used_is_warned_about_once() {
        let (_dir, setup) = setup(
            "",
            &[(
                ".claude/agents/risky.md",
                "---\ndescription: x\npermissionMode: bypassPermissions\n---\nb\n",
            )],
        );
        let mut notices = notices(&setup);
        load(&setup, &mut notices);
        let messages = notices.into_messages();
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(messages[0].contains("permissionMode"), "{messages:?}");
    }

    #[test]
    fn the_main_role_is_a_model_an_entry_may_name_and_spawn_is_on_by_default() {
        let (_dir, setup) = setup("[roles]\nmain = \"ollama/big\"\n", &[]);
        let found = load(&setup, &mut notices(&setup));
        assert_eq!(found.settings.also_in_play, ["ollama/big"]);
        assert!(found.spawn);
    }

    #[test]
    fn spawn_can_be_turned_off_in_the_global_config_and_in_the_project_but_not_on() {
        let (_dir, global) = setup("[agents]\nspawn = false\n", &[]);
        assert!(!load(&global, &mut notices(&global)).spawn);
        let (_dir, project) = setup("", &[(".harness/config.toml", "[agents]\nspawn = false\n")]);
        assert!(!load(&project, &mut notices(&project)).spawn);
        let (_dir, cannot) = setup(
            "[agents]\nspawn = false\n",
            &[(".harness/config.toml", "[agents]\nspawn = true\n")],
        );
        assert!(!load(&cannot, &mut notices(&cannot)).spawn);
    }
}
