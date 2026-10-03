//! Which sandbox a session gets once the Linux git-protection tier is known, and what to warn.

use std::sync::Arc;

use harness_core::{
    agent::Sandboxes,
    permission::FsAccess,
    tool::{CommandSandbox, GitProtection},
};

/// The session's sandbox, and a warning to print at startup.
pub struct Choice {
    pub sandbox: Option<Arc<dyn CommandSandbox>>,
    pub warning: Option<String>,
}

/// Picks the session's sandbox from the detected one. Only a workspace-write session in the Linux
/// basic tier is affected: it gets a warning, and with `required` (`sandbox.linux_git_protection =
/// "required"`) no sandbox at all, so every shell command asks first. A read-only sandbox already
/// protects git metadata completely.
pub fn choose(
    detected: Option<Arc<dyn CommandSandbox>>,
    access: FsAccess,
    required: bool,
) -> Choice {
    let Some(sandbox) = detected else {
        return Choice {
            sandbox: None,
            warning: None,
        };
    };
    let GitProtection::Basic { reason } = sandbox.git_protection() else {
        return Choice {
            sandbox: Some(sandbox),
            warning: None,
        };
    };
    if access == FsAccess::ReadOnly {
        return Choice {
            sandbox: Some(sandbox),
            warning: None,
        };
    }
    if required {
        return Choice {
            sandbox: None,
            warning: Some(format!(
                "git metadata protection is required (sandbox.linux_git_protection = \"required\"), but user namespaces are unavailable ({reason}); every shell command will need approval. Run `harness sandbox doctor` to see how to enable them"
            )),
        };
    }
    Choice {
        sandbox: Some(sandbox),
        warning: Some(format!(
            "user namespaces are unavailable ({reason}), so the sandbox can only check git hooks and config after each command; run `harness sandbox doctor` to see how to enable them"
        )),
    }
}

/// The sandbox for each mode from the detected one, and the warning for the modes that write:
/// read-only access keeps it (in the Linux basic tier too, since a read-only sandbox protects git
/// metadata completely), and workspace-write access gets none in a workspace too broad to make
/// writable (`too_broad`), or as [`choose`] says.
pub fn for_modes(
    detected: Option<Arc<dyn CommandSandbox>>,
    too_broad: bool,
    required: bool,
) -> (Sandboxes, Option<String>) {
    let write = if too_broad {
        Choice {
            sandbox: None,
            warning: None,
        }
    } else {
        choose(detected.clone(), FsAccess::WorkspaceWrite, required)
    };
    let sandboxes = Sandboxes {
        read_only: choose(detected, FsAccess::ReadOnly, required).sandbox,
        workspace_write: write.sandbox,
    };
    (sandboxes, write.warning)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[derive(Debug)]
    struct Fake(GitProtection);

    impl CommandSandbox for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn command(
            &self,
            _access: FsAccess,
            _workspace: &Path,
            program: &str,
            _args: &[&str],
        ) -> std::io::Result<tokio::process::Command> {
            Ok(tokio::process::Command::new(program))
        }
        fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
            false
        }
        fn git_protection(&self) -> GitProtection {
            self.0.clone()
        }
    }

    fn basic() -> Option<Arc<dyn CommandSandbox>> {
        Some(Arc::new(Fake(GitProtection::Basic {
            reason: "writing /proc/self/uid_map failed".into(),
        })))
    }

    #[test]
    fn the_full_tier_and_macos_are_used_as_they_are() {
        let full: Option<Arc<dyn CommandSandbox>> = Some(Arc::new(Fake(GitProtection::Full)));
        let choice = choose(full, FsAccess::WorkspaceWrite, true);
        assert!(choice.sandbox.is_some() && choice.warning.is_none());
    }

    #[test]
    fn the_basic_tier_warns_and_names_the_doctor() {
        let choice = choose(basic(), FsAccess::WorkspaceWrite, false);
        assert!(choice.sandbox.is_some());
        let warning = choice.warning.unwrap();
        assert!(
            warning.contains("writing /proc/self/uid_map failed"),
            "{warning}"
        );
        assert!(warning.contains("harness sandbox doctor"), "{warning}");
    }

    #[test]
    fn required_protection_turns_the_basic_tier_into_no_sandbox() {
        let choice = choose(basic(), FsAccess::WorkspaceWrite, true);
        assert!(choice.sandbox.is_none());
        let warning = choice.warning.unwrap();
        assert!(
            warning.contains("every shell command will need approval"),
            "{warning}"
        );
        assert!(warning.contains("harness sandbox doctor"), "{warning}");
    }

    #[test]
    fn a_read_only_session_keeps_its_sandbox_without_a_warning() {
        for required in [false, true] {
            let choice = choose(basic(), FsAccess::ReadOnly, required);
            assert!(choice.sandbox.is_some() && choice.warning.is_none());
        }
    }

    #[test]
    fn no_detected_sandbox_stays_none_without_a_warning_of_its_own() {
        let choice = choose(None, FsAccess::WorkspaceWrite, true);
        assert!(choice.sandbox.is_none() && choice.warning.is_none());
    }

    #[test]
    fn each_access_gets_the_sandbox_it_can_use() {
        let full: Option<Arc<dyn CommandSandbox>> = Some(Arc::new(Fake(GitProtection::Full)));
        let (both, warning) = for_modes(full.clone(), false, true);
        assert!(both.read_only.is_some() && both.workspace_write.is_some() && warning.is_none());
        // Too broad to make writable: read-only only, and the startup warning is its own.
        let (broad, warning) = for_modes(full, true, false);
        assert!(broad.read_only.is_some() && broad.workspace_write.is_none() && warning.is_none());
        // The basic tier with `required`: read-only only, with the warning.
        let (basic, warning) = for_modes(basic(), false, true);
        assert!(basic.read_only.is_some() && basic.workspace_write.is_none());
        assert!(
            warning
                .unwrap()
                .contains("every shell command will need approval")
        );
        let (none, warning) = for_modes(None, false, false);
        assert!(none.read_only.is_none() && none.workspace_write.is_none() && warning.is_none());
    }
}
