# M1 · P2.13 Linux Git-Metadata Protection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** On Linux, stop a sandboxed command from planting git hooks or repository config that later run on the user's own git commands: read-only mounts in a user and mount namespace where the kernel allows it (the full tier), and everywhere a guard that moves new protected names to a quarantine and, where there are no mounts (the basic tier), restores changed protected files; plus `sandbox.linux_git_protection` and `harness sandbox doctor`.

**Architecture:** A platform-neutral `gitmeta` module indexes every gitdir in a workspace, and a platform-neutral `guard` checks and repairs protected git metadata around one command (quarantine, snapshot and restore, a report for the tool result). `harness-core` gains a `CommandGuard` hook, `CommandSandbox::prepare`, which the `bash` tool starts before every sandboxed run and finishes after it, however the run ends. On Linux, `LinuxSandbox` runs the guard with an inotify watcher; in the full tier its `pre_exec` first unshares a user and mount namespace and self-binds every gitdir (read-write pins) and protected entry (read-only) with the fd-based mount API, and in both tiers seccomp then refuses the mount and namespace syscalls. A startup probe picks the tier. The CLI warns in the basic tier, turns it into approval for every command with `"required"`, and `harness sandbox doctor` explains the fix.

**Tech Stack:** Rust 1.98, edition 2024; `libc` (now a dependency on every OS), `ignore` (the gitignore-aware walk, already in the workspace), `landlock 0.4.7`, `seccompiler =0.5.0`, `tokio`; tests with `tempfile`, `assert_cmd`, `wiremock`, and Python 3 on Linux for raw-syscall checks.

**Spec:** `openspec/changes/add-core-agent/` (binding): `design.md` D5 ("Linux git-metadata protection" and "Workspace-write sandboxes") and the Risks entry on blocked user namespaces; `specs/permissions-sandbox/spec.md` ("The sandbox protects repository hooks and config", "No silent unsandboxed fallback"); `specs/cli-interface/spec.md` ("Sandbox diagnosis", "Management subcommands"); `specs/configuration/spec.md` (`linux_git_protection` in the widening rules). Task 1 writes this plan's refinements (below) into `design.md` and `specs/permissions-sandbox/spec.md`.

## How this plan was checked

Every task was built in order, as its own commit, in a scratch clone of this branch, and each commit was checked on its own:

- on macOS: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and the tests of every crate the task touches (after Task 11, `cargo test --workspace` passes 449 tests);
- for Linux: `cargo clippy --all-targets -- -D warnings` of `harness-sandbox` (library, unit tests and the Linux integration tests) for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`, with the lint probe below; and, separately, an isolated crate holding only the Linux-only modules (`mountns`, `mountplan`, `tier`, `watch`, `seccomp`, `preexec`, `fdcleanup`, `fs`, `detect`) that depends on nothing but `libc`, `landlock` and `seccompiler`, linted the same way.

The Linux code has not been run: its integration tests first run in this branch's pull-request CI, on stock `ubuntu-24.04` (the basic tier) and on `ubuntu-24.04` with user namespaces allowed (the full tier, Task 11). Transcribe the code exactly.

### Linting Linux code from macOS

`cargo check --target x86_64-unknown-linux-gnu` of the workspace fails on macOS: `harness-core` pulls in `aws-lc-sys`, which needs a Linux C cross-compiler. This script copies `harness-sandbox` into `target/linux-lint/` (ignored by git) next to a stand-in `harness-core` that holds, verbatim, the sandbox types from `crates/harness-core/src/tool.rs`, and lints it for both Linux targets. Both targets are already installed (`rustup target list --installed`).

Save it as `target/linux-lint.sh` (not committed) in Task 2, and run it with `bash target/linux-lint.sh` from the repository root. Expected: two `Finished` lines and no warnings.

```bash
#!/bin/bash
# Lints harness-sandbox (library, unit tests and integration tests) for both Linux targets, from
# macOS. The workspace itself cannot be cross-checked: harness-core pulls in aws-lc-sys, which needs
# a Linux C cross-compiler. So this copies harness-sandbox into target/linux-lint/ next to a
# stand-in harness-core holding, verbatim, the sandbox types from crates/harness-core/src/tool.rs.
set -euo pipefail
ROOT=$(git rev-parse --show-toplevel)
P="$ROOT/target/linux-lint"
rm -rf "$P/harness-sandbox" "$P/core"
mkdir -p "$P/core/src" "$P/harness-sandbox"
cp "$ROOT/Cargo.lock" "$P/Cargo.lock"
cat > "$P/Cargo.toml" <<'TOML'
[workspace]
resolver = "3"
members = ["core", "harness-sandbox"]
TOML
python3 - "$ROOT" "$P" <<'PY'
import re, sys
root, p = sys.argv[1], sys.argv[2]
# The stand-in harness-core: FsAccess, and the sandbox part of tool.rs.
tool = open(f"{root}/crates/harness-core/src/tool.rs").read()
start = tool.find("/// What a [`CommandGuard`] did")
if start < 0:
    start = tool.index("/// Wraps shell commands so they run inside an OS sandbox.")
body = tool[start:tool.index("/// Tools in a fixed order")].rstrip()
body = "\n".join("    " + l if l.strip() else l for l in body.split("\n"))
open(f"{p}/core/src/lib.rs", "w").write(
    "pub mod permission {\n"
    "    #[derive(Debug, Clone, Copy, PartialEq, Eq)]\n"
    "    pub enum FsAccess {\n        ReadOnly,\n        WorkspaceWrite,\n    }\n}\n"
    "#[allow(unused_imports)]\npub mod tool {\n    use std::path::Path;\n\n"
    "    use crate::permission::FsAccess;\n\n" + body + "\n}\n")
workspace = open(f"{root}/Cargo.toml").read()
tokio = re.search(r"^tokio = (.*)$", workspace, re.M).group(1)
open(f"{p}/core/Cargo.toml", "w").write(
    '[package]\nname = "harness-core"\nversion = "0.1.0"\nedition = "2024"\npublish = false\n\n'
    f"[dependencies]\ntokio = {tokio}\n")
# harness-sandbox's manifest, with workspace dependencies spelled out.
manifest = open(f"{root}/crates/harness-sandbox/Cargo.toml").read()
def spelled(m):
    name = m.group(1)
    if name == "harness-core":
        return 'harness-core = { path = "../core" }'
    return f"{name} = " + re.search(rf"^{name} = (.*)$", workspace, re.M).group(1)
manifest = re.sub(r"^([a-z0-9-]+)\.workspace = true$", spelled, manifest, flags=re.M)
for key, value in [("version", '"0.1.0"'), ("edition", '"2024"'), ("rust-version", '"1.98"'), ("license", '"MIT OR Apache-2.0"')]:
    manifest = manifest.replace(f"{key}.workspace = true", f"{key} = {value}")
open(f"{p}/harness-sandbox/Cargo.toml", "w").write(manifest)
PY
cp -R "$ROOT/crates/harness-sandbox/src" "$ROOT/crates/harness-sandbox/tests" "$P/harness-sandbox/"
cd "$P"
for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
  cargo clippy -p harness-sandbox --all-targets --target "$target" -- -D warnings
done
```

## Decisions this plan asks you to approve

These refine the approved design; Task 1 writes 1, 2, 4, 5, 6 and 7 into it.

1. **Namespace syscalls are refused in both tiers**, and so are `clone` with any `CLONE_NEW*` flag and `clone3`, which fails with `ENOSYS` so glibc and other runtimes fall back to `clone`. Refusing only `unshare` and `setns`, as design.md lists them, would leave `clone(CLONE_NEWUSER)` open. The cost: programs that sandbox themselves with namespaces (Chromium and Electron, bubblewrap, rootless Podman) need their no-sandbox mode or an unsandboxed re-run. On stock Ubuntu they already fail in the sandbox today. Task 11 adds a README limitation.
2. **Before each command the guard re-checks only what it already knew:** new protected names in the gitdirs indexed at the end of the previous command, and `.harness` and `HEAD` at the top. A repository created between commands is left alone, because it may be the user's own clone. What this check finds is reported without blocking the command that runs next.
3. **What the guard undoes counts as a sandbox denial.** The `bash` result gets `sandbox_denied`, so the agent offers an unsandboxed re-run and headless runs exit 3, as when Seatbelt refuses the same write on macOS.
4. **`"required"` applies in `ask` and `auto` only.** In `plan` and `read-only` Landlock already refuses every write, so the basic tier is as safe as the full one there; those modes keep their read-only sandbox. `specs/permissions-sandbox` gains "in `ask` and `auto`".
5. **Mount mechanics.** The full tier uses the fd-based mount API: `openat2` beneath the workspace without following symlinks, a device and inode check against what the parent saw, `open_tree(OPEN_TREE_CLONE | AT_RECURSIVE)`, `mount_setattr(MOUNT_ATTR_RDONLY)` and `move_mount`. A classic `mount(MS_BIND)` plus read-only remount follows symlinks and, inside a user namespace, must restate every locked flag. Propagation is made private (`MS_REC | MS_PRIVATE`). A workspace with nothing to protect skips the namespace. Every gitfile `.git` is bound read-only too, as macOS protects every `.git` entry.
6. **What a mount cannot protect is snapshotted in both tiers:** a protected symlink (a mount cannot cover one) and a protected file with a second hard link (writable through the other name) are saved before the command and restored after it.
7. **The quarantine never loses data.** Entries are renamed into `<data dir>/quarantine/<UTC time>-<pid>-<n>/<path in the workspace>`, a directory private to the user. Across filesystems they are copied and then removed; if copying fails they are renamed in place to `<name>.harness-quarantine-<n>`, which git ignores.
8. **The tier can drop at run time.** The child reports a failed mount step through a close-on-exec pipe; the command has not run, the tool result says so, and the session switches to the basic tier. With `"required"`, every later command then fails to prepare, and the message says to restart harness, which then asks before every command.
9. **inotify through raw `libc`, not a crate:** about 150 lines, no new dependency or licence for cargo-deny, in the style of the crate's other raw syscalls. The watcher only shortens the window; the final check after the command is what the protection rests on.
10. **CI.** A third matrix entry runs the `ubuntu-24.04` job after `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`. `HARNESS_EXPECT_LINUX_TIER` (`basic` on the stock Linux job, `full` on the new one) makes tier-dependent tests fail instead of skipping when the runner does not give the expected tier.

## Global Constraints

- Rust `1.98.0`, edition 2024, licence `MIT OR Apache-2.0`, macOS and Linux; every crate keeps `publish = false`.
- Commits: `git commit -F -` with a conventional-commit subject, a short body, and the trailer lines the controller gives you (written `<trailer lines from the controller>` below).
- The Linux sandbox stays limited to `x86_64` and `aarch64`: `#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]`.
- macOS behaviour does not change. Every existing Seatbelt test passes; the only edit to them is one struct literal that gains `..SandboxSettings::default()`.
- Code in `pre_exec` stays async-signal-safe: raw syscalls on data built in the parent, stack buffers, no allocation, no locks, errors as raw `errno`. Any failure before `execve` stops the command.
- Order in the Linux child: `setsid`, mark inherited fds close-on-exec, (full tier) namespace and mounts, `no_new_privs`, `landlock_restrict_self`, then the two seccomp programs.
- Protected names: in every gitdir `config`, `config.worktree`, `commondir`, `hooks`, `gitweb` and `pid`; at the top of the workspace `.harness` and `HEAD`; and every `.git` entry.
- Nothing is ever deleted: the guard moves what it takes below `<data dir>/quarantine/`, which is `$XDG_DATA_HOME/harness/quarantine` or `$HARNESS_HOME/data/quarantine`.
- The guard's walk skips git-ignored directories and never follows symlinks.
- `sandbox.linux_git_protection` is `"best-effort"` (the default) or `"required"`. A project's `"required"` always applies; a project's `"best-effort"` needs trust when the global value is `"required"`, and is then part of the trust fingerprint.
- `harness sandbox doctor` only reads; it never changes system files. Every value that comes from the system is printed through `terminal_safe`.
- Headless exit codes do not change: `0`, `1`, `2`, `3`, `130`.

## Review Focus

- **A process the previous command left running plants a hook or `commondir` after that command ended** (`(sleep 1; printf … > .git/commondir) &`): the next command must find it and move it to the quarantine before it runs, and report it without blocking. Tests in Task 4 (`names_planted_after_a_command_ends_are_caught_before_the_next`) and Task 6 (`names_a_background_process_plants_later_are_caught_before_the_next_command`).
- **The user changes git metadata while harness runs:** a repository cloned between commands must be left alone (Task 4, `a_repository_created_between_commands_is_left_alone`); a change to `.git/config` during a command is undone in the basic tier and kept in the quarantine, which the README states (Task 11).
- **`.git` itself moved, replaced or repointed** (`mv .git x && mkdir .git`, a `.git` symlink pointed at `/tmp`, a rewritten `sub/.git` gitfile): the full tier must refuse the move with `EBUSY`; the basic tier must quarantine the replacement and put symlinks and gitfiles back. Tests in Task 4 (`a_replaced_dot_git_is_quarantined_and_reported`, `a_repointed_dot_git_symlink_is_put_back`, `a_rewritten_gitfile_is_restored`) and Task 8 (`full_tier_pins_dot_git`).
- **The full tier's setup fails during a session** (user namespaces blocked after the probe, a racing rename): the command must not run, the session must drop to the basic tier, and with `"required"` nothing may run afterwards. Tests in Task 8 (`a_failed_mount_setup_drops_the_session_to_the_basic_tier`) and Task 10 (`required_protection_refuses_to_run_in_the_basic_tier`).
- **Everyday commands keep working:** `git commit`, `checkout -b`, `stash` and `stash pop`; threads and subprocesses despite the `clone3` refusal; a working directory inside `.git`. Tests in Task 4 (`a_real_git_commit_gets_no_report`), Task 5 (`clone3_fails_with_enosys_and_threads_still_work`), Task 6 (`basic_tier_commit_checkout_and_stash_get_no_report`) and Task 8 (`full_tier_allows_commit_checkout_and_stash`, `a_working_directory_inside_dot_git_still_sees_the_mounts`).

---

## File Map

```
openspec/changes/add-core-agent/design.md, specs/permissions-sandbox/spec.md   refinements (Task 1)
crates/harness-sandbox/src/gitmeta/{mod,index,linked}.rs   NEW  gitdir discovery shared by macOS and Linux; linked.rs is macos/gitdir.rs moved
crates/harness-sandbox/src/guard/{mod,quarantine,snapshot}.rs   NEW  the platform-neutral git-metadata guard
crates/harness-sandbox/src/linux/mountns.rs      NEW  child: user and mount namespace, fd-based self-binds (async-signal-safe)
crates/harness-sandbox/src/linux/mountplan.rs    NEW  parent: the mount plan, hooks placeholders, the setup pipe
crates/harness-sandbox/src/linux/tier.rs         NEW  the probe that picks the tier
crates/harness-sandbox/src/linux/watch.rs        NEW  the inotify watcher thread
crates/harness-sandbox/src/linux/{mod,preexec,seccomp,detect}.rs   tiers and the guard; the mount step; mount and namespace denial
crates/harness-sandbox/src/lib.rs                SandboxSettings fields, exports, unavailable_reason
crates/harness-sandbox/src/macos/{mod,profile}.rs    use gitmeta instead of gitdir
crates/harness-sandbox/tests/{git_index,git_guard,linux_git_guard}.rs   NEW; linux_sandbox.rs and seatbelt.rs updated
crates/harness-core/src/tool.rs                  CommandGuard, GuardReport, SandboxedCommand, GitProtection, prepare, git_protection
crates/harness-tools/src/bash.rs                 prepare, run, finish around every sandboxed command
crates/harness-config/src/config.rs              sandbox.linux_git_protection
crates/harness-cli/src/{sandbox,doctor}.rs       NEW; main.rs and ask.rs updated
.github/workflows/ci.yml, README.md              the full-tier CI job; docs (Task 11)
```

---

### Task 1: Write the refinements into the spec

**Files:**
- Modify: `openspec/changes/add-core-agent/design.md`, `openspec/changes/add-core-agent/specs/permissions-sandbox/spec.md`

**Interfaces:**
- Consumes: nothing.
- Produces: the binding text for decisions 1, 2, 4, 5, 6 and 7 above.

- [ ] **Step 1: Update design.md**

In `openspec/changes/add-core-agent/design.md`, in the "Linux git-metadata protection" bullet of D5:

Replace:

```markdown
  - **Full tier.** In the same forked child, before `no_new_privs`, Landlock and seccomp, the child:
    - unshares a user and mount namespace, maps the real uid and gid 1:1, and makes mount propagation private;
    - pins each gitdir (the top-level `.git`, following a symlink or gitfile, then `.git/modules/**`, `.git/worktrees/*`, and nested repositories from the guard's index) with a read-write self-bind, so the entry cannot be renamed, removed or replaced;
    - self-binds read-only every protected entry that exists: `config`, `config.worktree`, `commondir`, `hooks/`, `gitweb/`, `pid` and the matching `worktrees/<id>` entries in each gitdir, plus `.harness/` and a top-level `HEAD`. The parent creates an empty `hooks/` placeholder when it is missing, which git treats exactly like a missing directory;
    - changes directory again so its working directory resolves inside the new mounts.
    
    The seccomp filter additionally refuses the mount and namespace syscalls (`mount`, `umount2`, `mount_setattr`, `move_mount`, `open_tree`, `fsopen`, `fsmount`, `fspick`, `pivot_root`, `unshare`, `setns`). Mounts copied into any nested namespace are locked. If any step fails the command does not run, and the session drops to the basic tier with a warning.
  - **Basic tier.** Landlock, seccomp and the guard, with one startup warning naming `harness sandbox doctor`. That command reports the tier, why it was chosen, and the exact fix (an AppArmor profile for the harness binary, or the sysctl); it never changes system files itself. Setting `sandbox.linux_git_protection = "required"` makes the basic tier ask before every shell command; the default, `"best-effort"`, keeps `auto` usable.
  - **The guard, in both tiers.** Mounts cannot cover a name that does not exist yet, and there is no safe placeholder for `commondir` (git refuses an empty one). The guard runs in the harness process around each sandboxed command. While the command runs, an inotify watcher on each known gitdir and on the workspace root catches a new `commondir`, `config`, `config.worktree`, `hooks`, `gitweb` or `pid` in a gitdir, a top-level `HEAD` and `.harness/`. These are the places where a git process outside the sandbox, such as an editor's background status poll, would pick up a planted file at once. In the basic tier the watcher also catches changes to the existing protected files, which it snapshots before the command. After the command, a scan of the workspace (skipping git-ignored directories) finds any new nested `.git`, and a final check repeats the gitdir checks. The same checks run again before the next command, to catch anything a background process left behind.
    - Anything found is moved, never deleted, to `$XDG_DATA_HOME/harness/quarantine/<timestamp>/`. Changed files are restored from the snapshot.
    - The tool result says what happened, so both the model and the user see it.
    - As on macOS, `git init` and `git clone` into the workspace are undone.
    - Known windows: between a planted gitdir file's creation and its quarantine there are milliseconds in which a git process outside the sandbox could read it. A new nested `.git` is removed only after the command ends. A `.git` inside a git-ignored directory is not detected.
  - **Later provider.** A bubblewrap provider that renders the same mount plan as `bwrap` arguments may be added for systems where only the distribution's `/usr/bin/bwrap` is granted user namespaces.
```

with:

```markdown
  - **Full tier.** In the same forked child, before `no_new_privs`, Landlock and seccomp, the child:
    - unshares a user and mount namespace, maps the real uid and gid 1:1, and makes mount propagation private;
    - pins each gitdir (the top-level `.git`, following a symlink or gitfile, then `.git/modules/**`, `.git/worktrees/*`, and nested repositories from the guard's index) with a read-write self-bind, so the entry cannot be renamed, removed or replaced;
    - self-binds read-only every protected entry that exists: `config`, `config.worktree`, `commondir`, `hooks/`, `gitweb/`, `pid` and the matching `worktrees/<id>` entries in each gitdir, plus every gitfile `.git`, `.harness/` and a top-level `HEAD`. The parent creates an empty `hooks/` placeholder when it is missing, which git treats exactly like a missing directory;
    - changes directory again so its working directory resolves inside the new mounts.
    
    In both tiers the seccomp filter also refuses the mount and namespace syscalls (`mount`, `umount2`, `mount_setattr`, `move_mount`, `open_tree`, `open_tree_attr`, `fsopen`, `fsconfig`, `fsmount`, `fspick`, `pivot_root`, `unshare`, `setns`, and `clone` with any `CLONE_NEW*` flag), and makes `clone3`, whose flags it cannot read, fail with `ENOSYS`, so runtimes fall back to `clone`. Mounts copied into any nested namespace are locked. A workspace with nothing to protect skips the namespace. If any step fails the command does not run, and the session drops to the basic tier with a warning.
  - **Basic tier.** Landlock, seccomp and the guard, with one startup warning naming `harness sandbox doctor`. That command reports the tier, why it was chosen, and the exact fix (an AppArmor profile for the harness binary, or the sysctl); it never changes system files itself. Setting `sandbox.linux_git_protection = "required"` makes the basic tier ask before every shell command in `ask` and `auto` (`plan` and `read-only` keep their read-only sandbox, which already protects git metadata); the default, `"best-effort"`, keeps `auto` usable.
  - **The guard, in both tiers.** Mounts cannot cover a name that does not exist yet, and there is no safe placeholder for `commondir` (git refuses an empty one). The guard runs in the harness process around each sandboxed command. While the command runs, an inotify watcher on each known gitdir and on the workspace root catches a new `commondir`, `config`, `config.worktree`, `hooks`, `gitweb` or `pid` in a gitdir, a top-level `HEAD` and `.harness/`. These are the places where a git process outside the sandbox, such as an editor's background status poll, would pick up a planted file at once. In the basic tier the watcher also catches changes to the existing protected files and gitfiles, which it snapshots before the command; in both tiers the guard snapshots what a mount cannot protect: a protected symlink, and a protected file with a second hard link. After the command, a scan of the workspace (skipping git-ignored directories) finds any new nested `.git`, and a final check repeats the gitdir checks. Before the next command the guard checks again for new protected names in the gitdirs it knew and at the top of the workspace, to catch anything a background process left behind; a repository created between commands, which may be the user's own clone, is left alone.
    - Anything found is moved, never deleted, to `$XDG_DATA_HOME/harness/quarantine/<timestamp>/`. Across filesystems it is copied and the original removed; if copying fails it is renamed in place (`<name>.harness-quarantine-<n>`) so git ignores it. Changed files are restored from the snapshot, which also undoes a change someone else makes to them while the command runs.
    - The tool result says what happened, so both the model and the user see it, and the command counts as blocked by the sandbox: the agent offers to re-run it outside the sandbox, and headless runs exit 3. What the check before a command finds is reported without blocking that command.
    - As on macOS, `git init` and `git clone` into the workspace are undone.
    - Known windows: between a planted gitdir file's creation and its quarantine there are milliseconds in which a git process outside the sandbox could read it. A new nested `.git` is removed only after the command ends. A `.git` inside a git-ignored directory is not detected.
  - **Later provider.** A bubblewrap provider that renders the same mount plan as `bwrap` arguments may be added for systems where only the distribution's `/usr/bin/bwrap` is granted user namespaces.
```

- [ ] **Step 2: Update the permissions-sandbox spec**

In `openspec/changes/add-core-agent/specs/permissions-sandbox/spec.md`:

Replace:

```markdown

On macOS, and on Linux when unprivileged user namespaces are available (the full tier), writes to these paths MUST fail. On Linux, names that do not exist yet MUST be caught by a guard that moves them to a quarantine directory, never deleting them, and reports it in the tool result.

When user namespaces are unavailable (the basic tier), the system MUST warn at startup and point to `harness sandbox doctor`, and the guard MUST also restore changed protected files after each command. With `sandbox.linux_git_protection = "required"`, the basic tier MUST require approval for every shell command.

#### Scenario: Planting a hook
- **WHEN** a sandboxed command runs `echo x > .git/hooks/pre-commit` in `auto` mode on macOS, or on Linux in the full tier
```

with:

```markdown

On macOS, and on Linux when unprivileged user namespaces are available (the full tier), writes to these paths MUST fail. On Linux, names that do not exist yet MUST be caught by a guard that moves them to a quarantine directory, never deleting them, and reports it in the tool result.

When user namespaces are unavailable (the basic tier), the system MUST warn at startup and point to `harness sandbox doctor`, and the guard MUST also restore changed protected files after each command. With `sandbox.linux_git_protection = "required"`, the basic tier MUST require approval for every shell command in `ask` and `auto`.

#### Scenario: Planting a hook
- **WHEN** a sandboxed command runs `echo x > .git/hooks/pre-commit` in `auto` mode on macOS, or on Linux in the full tier
```

- [ ] **Step 3: Validate**

Run: `openspec validate add-core-agent --strict`
Expected: `Change 'add-core-agent' is valid`

- [ ] **Step 4: Commit**

```bash
git add openspec/changes/add-core-agent
git commit -F - <<'EOF'
docs(spec): refine Linux git-metadata protection for the 2.13 plan

Namespace syscalls, clone with a namespace flag and clone3 are refused
in both tiers. The check before a command covers the gitdirs the guard
already knew, so a repository cloned between commands is left alone.
"required" applies in ask and auto. The design also records the mount
mechanics, what the guard snapshots in both tiers, and how the
quarantine avoids losing data.

<trailer lines from the controller>
EOF
```

---

### Task 2: `gitmeta`: gitdir discovery shared by macOS and Linux

**Files:**
- Move: `crates/harness-sandbox/src/macos/gitdir.rs` to `crates/harness-sandbox/src/gitmeta/linked.rs`
- Create: `crates/harness-sandbox/src/gitmeta/mod.rs`, `crates/harness-sandbox/src/gitmeta/index.rs`, `crates/harness-sandbox/tests/git_index.rs`
- Modify: `crates/harness-sandbox/Cargo.toml`, `crates/harness-sandbox/src/lib.rs`, `crates/harness-sandbox/src/macos/mod.rs`, `crates/harness-sandbox/src/macos/profile.rs`, `Cargo.lock`

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `harness_sandbox::gitmeta::GitIndex { dot_gits, gitdirs, links: BTreeSet<PathBuf> }` (all absolute; a parent sorts before its children).
  - `harness_sandbox::gitmeta::discover(workspace: &Path, skip: Option<&Path>) -> GitIndex`: `workspace` is canonical; the walk skips git-ignored directories, `.git` directories and `skip`, and never follows symlinks.
  - `harness_sandbox::gitmeta::{GITDIR_PROTECTED: [&str; 6], WORKSPACE_PROTECTED: [&str; 2]}`.
  - Crate-internal: `gitmeta::index::nested_gitdirs(gitdir: &Path) -> Vec<PathBuf>` (the `worktrees/*` and `modules/**` gitdirs; re-exported in Task 4), `gitmeta::linked::linked_gitdirs_at(holder: &Path, workspace: &Path) -> LinkedGitdirs`, and, on macOS only, `gitmeta::{LinkedGitdirs, linked_gitdirs}` for the Seatbelt profile, unchanged in behaviour.

The macOS code in `gitdir.rs` is platform-neutral std code, so it moves to a shared module. Two changes are behaviour-neutral for macOS: `linked_gitdirs_at` generalizes `linked_gitdirs` to the `.git` of any directory (a gitfile's path is relative to that directory), and `within` compares names case-insensitively only on macOS.

- [ ] **Step 1: Create the Linux lint probe**

Save the script from "Linting Linux code from macOS" above as `target/linux-lint.sh`. Do not commit it.

- [ ] **Step 2: Write the failing test**

`crates/harness-sandbox/tests/git_index.rs`:

```rust
//! `gitmeta::discover` on real directory trees. Platform-neutral: runs on macOS and Linux.

use std::collections::BTreeSet;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use harness_sandbox::gitmeta::discover;

fn workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let canon = dir.path().canonicalize().unwrap();
    (dir, canon)
}

/// A minimal gitdir: git itself would add more, but discovery only needs the directory.
fn gitdir(ws: &Path, rel: &str) {
    std::fs::create_dir_all(ws.join(rel).join("hooks")).unwrap();
    std::fs::write(ws.join(rel).join("HEAD"), "ref: refs/heads/main\n").unwrap();
}

fn set(ws: &Path, rels: &[&str]) -> BTreeSet<PathBuf> {
    rels.iter().map(|rel| ws.join(rel)).collect()
}

#[test]
fn a_workspace_without_git_has_nothing_to_protect() {
    let (_d, ws) = workspace();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    let index = discover(&ws, None);
    assert!(index.dot_gits.is_empty() && index.gitdirs.is_empty() && index.links.is_empty());
}

#[test]
fn worktrees_and_submodules_are_gitdirs_at_any_depth() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    std::fs::create_dir_all(ws.join(".git/worktrees/wt1")).unwrap();
    // A submodule named `a/b`, and a submodule `c` with its own submodule `d`.
    gitdir(&ws, ".git/modules/a/b");
    gitdir(&ws, ".git/modules/c");
    gitdir(&ws, ".git/modules/c/modules/d");
    let index = discover(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git"]));
    assert_eq!(
        index.gitdirs,
        set(
            &ws,
            &[
                ".git",
                ".git/modules/a/b",
                ".git/modules/c",
                ".git/modules/c/modules/d",
                ".git/worktrees/wt1"
            ]
        )
    );
}

#[test]
fn nested_repositories_are_found_but_not_in_ignored_directories() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    std::fs::write(ws.join(".gitignore"), "ignored/\n").unwrap();
    gitdir(&ws, "vendor/lib/.git");
    gitdir(&ws, "ignored/repo/.git");
    let index = discover(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "vendor/lib/.git"]));
    assert_eq!(index.gitdirs, set(&ws, &[".git", "vendor/lib/.git"]));
}

#[test]
fn a_submodule_gitfile_leads_to_its_gitdir() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    gitdir(&ws, ".git/modules/sub");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    std::fs::write(ws.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
    let index = discover(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "sub/.git"]));
    assert_eq!(index.gitdirs, set(&ws, &[".git", ".git/modules/sub"]));
    assert!(
        index.links.contains(&ws.join(".git/modules/sub")),
        "{index:?}"
    );
}

#[test]
fn a_linked_worktree_leads_to_its_gitdir_and_the_common_one() {
    let (_d, ws) = workspace();
    gitdir(&ws, "main/.git");
    gitdir(&ws, "main/.git/worktrees/wt");
    std::fs::write(ws.join("main/.git/worktrees/wt/commondir"), "../..\n").unwrap();
    std::fs::create_dir_all(ws.join("wt")).unwrap();
    std::fs::write(ws.join("wt/.git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
    let index = discover(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &["main/.git", "wt/.git"]));
    assert_eq!(
        index.gitdirs,
        set(&ws, &["main/.git", "main/.git/worktrees/wt"])
    );
}

#[test]
fn symlinks_are_not_followed() {
    let (_d, ws) = workspace();
    let (_o, outside) = workspace();
    gitdir(&ws, ".git");
    gitdir(&outside, "repo/.git");
    std::fs::create_dir_all(outside.join("wts/x")).unwrap();
    symlink(outside.join("repo"), ws.join("linked")).unwrap();
    symlink(outside.join("wts"), ws.join(".git/worktrees")).unwrap();
    let index = discover(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git"]));
    assert_eq!(index.gitdirs, set(&ws, &[".git"]));
}

#[test]
fn the_skipped_directory_is_not_walked() {
    let (_d, ws) = workspace();
    gitdir(&ws, "quarantine/sub/.git");
    gitdir(&ws, "kept/.git");
    let index = discover(&ws, Some(&ws.join("quarantine")));
    assert_eq!(index.dot_gits, set(&ws, &["kept/.git"]));
}
```

- [ ] **Step 3: Run it to make sure it fails**

Run: `cargo test -p harness-sandbox --test git_index`
Expected: FAIL to compile: ``could not find `gitmeta` in `harness_sandbox` ``.

- [ ] **Step 4: Move gitdir.rs and generalize it**

Run: `mkdir -p crates/harness-sandbox/src/gitmeta && git mv crates/harness-sandbox/src/macos/gitdir.rs crates/harness-sandbox/src/gitmeta/linked.rs`

Then in `crates/harness-sandbox/src/gitmeta/linked.rs`:

Replace (1 of 11):

```rust
//! Finds the gitdir a workspace's `.git` leads to when `.git` is a symlink or
//! a gitfile, so the Seatbelt profile can protect it like `.git` itself.
//!
//! Git follows a `.git` symlink, and reads a `.git` file (a gitfile, also
//! when reached through a symlink) as `gitdir: <path>`, relative to the
```

with:

```rust
//! Finds the gitdir a `.git` leads to when it is a symlink or a gitfile, so
//! the Seatbelt profile (for the workspace's own `.git`) and the Linux guard
//! (for every `.git`) can protect it like a `.git` directory.
//!
//! Git follows a `.git` symlink, and reads a `.git` file (a gitfile, also
//! when reached through a symlink) as `gitdir: <path>`, relative to the
```

Replace (2 of 11):

```rust
/// The most of a gitfile or `commondir` file read. A path is shorter.
const MAX_POINTER_BYTES: u64 = 4096;

/// What the profile protects for a workspace whose `.git` is a symlink or a
/// gitfile. Only paths inside the workspace are listed; nothing outside it is
/// writable from the workspace-write sandbox except the temp and cache roots.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LinkedGitdirs {
    /// Gitdirs git uses in this workspace: the one `.git` leads to, and the
```

with:

```rust
/// The most of a gitfile or `commondir` file read. A path is shorter.
const MAX_POINTER_BYTES: u64 = 4096;

/// What to protect for a `.git` that is a symlink or a gitfile. Only paths
/// inside the workspace are listed; nothing outside it is writable from the
/// workspace-write sandbox except the temp and cache roots.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LinkedGitdirs {
    /// Gitdirs git uses in this workspace: the one `.git` leads to, and the
```

Replace (3 of 11):

```rust
    pub gitdirs: Vec<PathBuf>,
    /// Entries below the workspace that the way to those gitdirs passes
    /// through: each symlink and directory, the gitfile, and the gitdirs
    /// themselves. `.git` itself is left out: the profile protects every
    /// `.git` entry already.
    pub entries: Vec<PathBuf>,
}

```

with:

```rust
    pub gitdirs: Vec<PathBuf>,
    /// Entries below the workspace that the way to those gitdirs passes
    /// through: each symlink and directory, the gitfile, and the gitdirs
    /// themselves. `.git` itself is left out, and so is the directory that
    /// holds it and everything above that: the profile and the guard protect
    /// every `.git` entry already.
    pub entries: Vec<PathBuf>,
}

```

Replace (4 of 11):

```rust
/// names where git would look once it is created). The entries on the way
/// are listed even when no gitdir is found, so a symlinked gitfile git
/// rejects today cannot be rewritten into one it accepts.
pub(crate) fn linked_gitdirs(workspace: &Path) -> LinkedGitdirs {
    let dot_git = workspace.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
        return LinkedGitdirs::default();
    };
```

with:

```rust
/// names where git would look once it is created). The entries on the way
/// are listed even when no gitdir is found, so a symlinked gitfile git
/// rejects today cannot be rewritten into one it accepts.
#[cfg(target_os = "macos")]
pub(crate) fn linked_gitdirs(workspace: &Path) -> LinkedGitdirs {
    linked_gitdirs_at(workspace, workspace)
}

/// [`linked_gitdirs`] for the `.git` in `holder`, a directory in the
/// canonical `workspace` (or the workspace itself). A gitfile's path is
/// relative to `holder`.
pub(crate) fn linked_gitdirs_at(holder: &Path, workspace: &Path) -> LinkedGitdirs {
    let dot_git = holder.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
        return LinkedGitdirs::default();
    };
```

Replace (5 of 11):

```rust

    let mut visited = Vec::new();
    let mut gitdirs = Vec::new();
    if let Some(gitdir) = gitdir_of(workspace, &dot_git, &mut visited) {
        let common = pointer(&gitdir.join("commondir"), b"")
            .and_then(|common| follow(&gitdir.join(common), &mut visited));
        gitdirs.push(gitdir);
```

with:

```rust

    let mut visited = Vec::new();
    let mut gitdirs = Vec::new();
    if let Some(gitdir) = gitdir_of(holder, &dot_git, &mut visited) {
        let common = pointer(&gitdir.join("commondir"), b"")
            .and_then(|common| follow(&gitdir.join(common), &mut visited));
        gitdirs.push(gitdir);
```

Replace (6 of 11):

```rust

    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in visited {
        if entry != workspace
            && entry != dot_git
            && within(&entry, workspace)
            && !entries.contains(&entry)
```

with:

```rust

    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in visited {
        if !holder.starts_with(&entry)
            && entry != dot_git
            && within(&entry, workspace)
            && !entries.contains(&entry)
```

Replace (7 of 11):

```rust

/// Where git finds the gitdir through `dot_git`: the directory it resolves
/// to, or, when it resolves to a regular file, the path that gitfile names.
fn gitdir_of(workspace: &Path, dot_git: &Path, visited: &mut Vec<PathBuf>) -> Option<PathBuf> {
    let target = follow(dot_git, visited)?;
    if !std::fs::metadata(&target).is_ok_and(|m| m.is_file()) {
        // A directory, a path that does not exist, or something git cannot
```

with:

```rust

/// Where git finds the gitdir through `dot_git`: the directory it resolves
/// to, or, when it resolves to a regular file, the path that gitfile names.
fn gitdir_of(holder: &Path, dot_git: &Path, visited: &mut Vec<PathBuf>) -> Option<PathBuf> {
    let target = follow(dot_git, visited)?;
    if !std::fs::metadata(&target).is_ok_and(|m| m.is_file()) {
        // A directory, a path that does not exist, or something git cannot
```

Replace (8 of 11):

```rust
    }
    let named = pointer(&target, b"gitdir: ")?;
    // Relative to the directory holding `.git`, even through a symlink.
    follow(&workspace.join(named), visited)
}

/// The path a gitfile (`prefix` `gitdir: `) or a `commondir` file (no prefix)
/// holds, as git reads it: up to the first NUL, without trailing newlines.
/// `None` if it is not a regular file, lacks the prefix, or names nothing.
fn pointer(file: &Path, prefix: &[u8]) -> Option<PathBuf> {
    if !std::fs::metadata(file).is_ok_and(|m| m.is_file()) {
        return None;
    }
```

with:

```rust
    }
    let named = pointer(&target, b"gitdir: ")?;
    // Relative to the directory holding `.git`, even through a symlink.
    follow(&holder.join(named), visited)
}

/// The path a gitfile (`prefix` `gitdir: `) or a `commondir` file (no prefix)
/// holds, as git reads it: up to the first NUL, without trailing newlines.
/// `None` if it is not a regular file, lacks the prefix, or names nothing.
pub(super) fn pointer(file: &Path, prefix: &[u8]) -> Option<PathBuf> {
    if !std::fs::metadata(file).is_ok_and(|m| m.is_file()) {
        return None;
    }
```

Replace (9 of 11):

```rust
/// is taken as written, and so is everything after it. Every entry looked at
/// is pushed to `visited`, each symlink before it is followed. `None` after
/// [`MAX_SYMLINKS`] symlinks.
fn follow(path: &Path, visited: &mut Vec<PathBuf>) -> Option<PathBuf> {
    let mut resolved = PathBuf::from("/");
    let mut pending = VecDeque::new();
    prepend(&mut pending, path);
```

with:

```rust
/// is taken as written, and so is everything after it. Every entry looked at
/// is pushed to `visited`, each symlink before it is followed. `None` after
/// [`MAX_SYMLINKS`] symlinks.
pub(super) fn follow(path: &Path, visited: &mut Vec<PathBuf>) -> Option<PathBuf> {
    let mut resolved = PathBuf::from("/");
    let mut pending = VecDeque::new();
    prepend(&mut pending, path);
```

Replace (10 of 11):

```rust
    }
}

/// Whether `path` is `base` or below it, comparing names ASCII
/// case-insensitively: on the default case-insensitive volume a symlink may
/// spell the workspace in another case, and Seatbelt's rules match either.
fn within(path: &Path, base: &Path) -> bool {
    let mut components = path.components();
    base.components().all(|b| {
        components.next().is_some_and(|c| {
            c.as_os_str()
                .as_bytes()
                .eq_ignore_ascii_case(b.as_os_str().as_bytes())
        })
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
```

with:

```rust
    }
}

/// Whether `path` is `base` or below it. On macOS names are compared ASCII
/// case-insensitively: on the default case-insensitive volume a symlink may
/// spell the workspace in another case, and Seatbelt's rules match either.
/// Elsewhere names must match exactly.
pub(crate) fn within(path: &Path, base: &Path) -> bool {
    let mut components = path.components();
    base.components().all(|b| {
        components
            .next()
            .is_some_and(|c| same_name(c.as_os_str(), b.as_os_str()))
    })
}

fn same_name(a: &OsStr, b: &OsStr) -> bool {
    if cfg!(target_os = "macos") {
        a.as_bytes().eq_ignore_ascii_case(b.as_bytes())
    } else {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    /// The workspace's own `.git`, as the macOS profile asks for it.
    fn linked_gitdirs(workspace: &Path) -> LinkedGitdirs {
        linked_gitdirs_at(workspace, workspace)
    }

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
```

Replace (11 of 11):

```rust
        check(&ws, &[], &["meta", "meta/gitfile"]);
    }

    #[test]
    fn a_fifo_is_never_opened() {
        let (_d, ws) = workspace();
```

with:

```rust
        check(&ws, &[], &["meta", "meta/gitfile"]);
    }

    #[test]
    fn a_nested_gitfile_is_read_relative_to_its_own_directory() {
        let (_d, ws) = workspace();
        mkdirs(&ws, ".git/modules/sub");
        mkdirs(&ws, "sub");
        std::fs::write(ws.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        let found = linked_gitdirs_at(&ws.join("sub"), &ws);
        assert_eq!(
            found.gitdirs,
            vec![ws.join(".git/modules/sub")],
            "{found:?}"
        );
        assert!(found.entries.contains(&ws.join(".git/modules/sub")));
        // The directory holding `.git`, and everything above it, is not "on the way".
        assert!(!found.entries.contains(&ws.join("sub")), "{found:?}");
        assert!(!found.entries.contains(&ws), "{found:?}");
    }

    #[test]
    fn names_are_case_insensitive_only_on_macos() {
        let inside = within(Path::new("/Work/Repo/x"), Path::new("/work/repo"));
        assert_eq!(inside, cfg!(target_os = "macos"));
        assert!(within(Path::new("/work/repo/x"), Path::new("/work/repo")));
        assert!(!within(
            Path::new("/work/repository"),
            Path::new("/work/repo")
        ));
    }

    #[test]
    fn a_fifo_is_never_opened() {
        let (_d, ws) = workspace();
```

- [ ] **Step 5: Add the index and the module**

`crates/harness-sandbox/src/gitmeta/index.rs`:

```rust
//! Finds every gitdir in a workspace.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::linked::{follow, linked_gitdirs_at, pointer, within};

/// How many directories below `modules/` a submodule's gitdir is looked for:
/// a submodule's name can contain `/`.
const MAX_MODULE_DEPTH: usize = 8;

/// Where git keeps metadata in one workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitIndex {
    /// Every `.git` entry (directory, gitfile or symlink) in the workspace
    /// outside git-ignored directories, the top-level one included.
    pub dot_gits: BTreeSet<PathBuf>,
    /// Every gitdir inside the workspace: each `.git` directory; the gitdir a
    /// gitfile or symlinked `.git` leads to and the one its `commondir` names;
    /// and in each, the gitdirs of linked worktrees (`worktrees/*`) and
    /// submodules (`modules/**`). A parent sorts before its children.
    pub gitdirs: BTreeSet<PathBuf>,
    /// Entries inside the workspace on the way from a gitfile or symlinked
    /// `.git` to its gitdirs: each symlink, directory and gitfile.
    pub links: BTreeSet<PathBuf>,
}

/// Indexes the canonical `workspace`. Directories git ignores are not
/// walked, nor is `skip` (harness's quarantine directory, should it be inside
/// the workspace), and symlinks are not followed.
pub fn discover(workspace: &Path, skip: Option<&Path>) -> GitIndex {
    let mut index = GitIndex::default();
    for holder in holders(workspace, skip) {
        let dot_git = holder.join(".git");
        let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
            continue;
        };
        index.dot_gits.insert(dot_git.clone());
        if meta.is_dir() {
            index.gitdirs.insert(dot_git);
        } else {
            let linked = linked_gitdirs_at(&holder, workspace);
            index.gitdirs.extend(linked.gitdirs);
            index.links.extend(linked.entries);
        }
    }
    let mut pending: Vec<PathBuf> = index.gitdirs.iter().cloned().collect();
    while let Some(gitdir) = pending.pop() {
        let common = pointer(&gitdir.join("commondir"), b"")
            .and_then(|common| follow(&gitdir.join(common), &mut Vec::new()));
        for found in common.into_iter().chain(nested_gitdirs(&gitdir)) {
            if within(&found, workspace) && index.gitdirs.insert(found.clone()) {
                pending.push(found);
            }
        }
    }
    index
}

/// The gitdirs of `gitdir`'s linked worktrees (every directory in
/// `worktrees/`) and submodules (every directory below `modules/` that holds
/// a `HEAD`). Symlinks are not followed.
pub(crate) fn nested_gitdirs(gitdir: &Path) -> Vec<PathBuf> {
    let mut found = subdirs(&gitdir.join("worktrees"));
    module_gitdirs(&gitdir.join("modules"), 0, &mut found);
    found
}

fn module_gitdirs(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth >= MAX_MODULE_DEPTH {
        return;
    }
    for sub in subdirs(dir) {
        if std::fs::symlink_metadata(sub.join("HEAD")).is_ok() {
            found.push(sub);
        } else {
            module_gitdirs(&sub, depth + 1, found);
        }
    }
}

/// The directories directly in `dir`, when `dir` is itself a directory (not
/// a symlink to one).
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

/// The workspace and every directory below it that git does not ignore,
/// except `.git` directories, what is below them, and `skip`.
fn holders(workspace: &Path, skip: Option<&Path>) -> Vec<PathBuf> {
    let skip = skip.map(Path::to_path_buf);
    ignore::WalkBuilder::new(workspace)
        .hidden(false)
        .ignore(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            entry.file_name() != ".git" && skip.as_deref() != Some(entry.path())
        })
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_dir()))
        .map(ignore::DirEntry::into_path)
        .collect()
}
```

`crates/harness-sandbox/src/gitmeta/mod.rs`:

```rust
//! Where git keeps its metadata in a workspace. Platform-neutral: the macOS
//! Seatbelt profile uses `linked_gitdirs` for a symlinked or gitfile `.git`,
//! and the Linux sandbox uses [`discover`] for the guard and the mounts.

mod index;
mod linked;

pub use index::{GitIndex, discover};
#[cfg(target_os = "macos")]
pub(crate) use linked::{LinkedGitdirs, linked_gitdirs};

/// The entries in every gitdir that decide where git loads config and hooks
/// from, or that hold them: the set the macOS profile protects
/// (`GITDIR_FILES` in `macos/profile.rs`).
pub const GITDIR_PROTECTED: [&str; 6] = [
    "config",
    "config.worktree",
    "commondir",
    "hooks",
    "gitweb",
    "pid",
];

/// The entries at the top of the workspace that are protected: harness's
/// project settings, and a `HEAD` that would make the workspace look like a
/// bare repository to git.
pub const WORKSPACE_PROTECTED: [&str; 2] = [".harness", "HEAD"];
```

- [ ] **Step 6: Wire it in**

`crates/harness-sandbox/Cargo.toml`:

Replace:

```toml

[dependencies]
harness-core.workspace = true
tokio.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
```

with:

```toml

[dependencies]
harness-core.workspace = true
ignore.workspace = true
tokio.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
```

`crates/harness-sandbox/src/lib.rs`:

Replace:

```rust
//! Both implement [`harness_core::tool::CommandSandbox`]; [`detect`] picks the one this host supports.

mod denial;
// Only x86_64/aarch64 are supported: `linux::seccomp` only knows how to
// target those two architectures. Any other Linux architecture skips this
// module entirely and compiles as if no sandbox backend were available,
```

with:

```rust
//! Both implement [`harness_core::tool::CommandSandbox`]; [`detect`] picks the one this host supports.

mod denial;
pub mod gitmeta;
// Only x86_64/aarch64 are supported: `linux::seccomp` only knows how to
// target those two architectures. Any other Linux architecture skips this
// module entirely and compiles as if no sandbox backend were available,
```

`crates/harness-sandbox/src/macos/mod.rs`:

Replace:

```rust

mod availability;
mod command;
mod gitdir;
mod profile;

use std::{io, path::Path};
```

with:

```rust

mod availability;
mod command;
mod profile;

use std::{io, path::Path};
```

`crates/harness-sandbox/src/macos/profile.rs`:

Replace (1 of 2):

```rust
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use super::gitdir::{LinkedGitdirs, linked_gitdirs};
use crate::policy::{FsAccess, SandboxPolicy};
use crate::roots::{home_dir, safe_root};

```

with:

```rust
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::gitmeta::{LinkedGitdirs, linked_gitdirs};
use crate::policy::{FsAccess, SandboxPolicy};
use crate::roots::{home_dir, safe_root};

```

Replace (2 of 2):

```rust
; it are protected, and so is every entry on the way there (each symlink and
; directory, the gitfile, and the gitdir itself), so none of them can be
; moved out and back, repointed or swapped. They are looked up again for
; every command (see gitdir.rs); one outside the workspace is not covered.
;
; These are path rules: moving a parent dir of any other gitdir (a nested
; repo, `.git/modules/*`, `.git/worktrees/<id>`) out to a writable root,
```

with:

```rust
; it are protected, and so is every entry on the way there (each symlink and
; directory, the gitfile, and the gitdir itself), so none of them can be
; moved out and back, repointed or swapped. They are looked up again for
; every command (see gitmeta/linked.rs); one outside the workspace is not covered.
;
; These are path rules: moving a parent dir of any other gitdir (a nested
; repo, `.git/modules/*`, `.git/worktrees/<id>`) out to a writable root,
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p harness-sandbox`
Expected: PASS, including the 7 tests in `git_index`, the moved `gitmeta::linked::tests` (two new: `a_nested_gitfile_is_read_relative_to_its_own_directory`, `names_are_case_insensitive_only_on_macos`) and every Seatbelt test.

- [ ] **Step 8: Lint**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && bash target/linux-lint.sh`
Expected: clean. (On Linux, `linked_gitdirs` and its re-export are compiled out; the lint catches a stray use.)

- [ ] **Step 9: Commit**

```bash
git add Cargo.lock crates/harness-sandbox
git commit -F - <<'EOF'
feat(sandbox): index every gitdir in a workspace

The gitdir code the Seatbelt profile uses moves to a shared gitmeta
module. discover walks the workspace (skipping git-ignored directories,
never following symlinks) and finds every .git entry and every gitdir:
.git directories, where a gitfile or symlink leads, commondir targets,
worktrees/* and modules/**. The Linux guard and mounts build on it.

<trailer lines from the controller>
EOF
```

---

### Task 3: `CommandGuard`: a guard around every sandboxed command

**Files:**
- Modify: `crates/harness-core/src/tool.rs`, `crates/harness-core/tests/tool.rs`, `crates/harness-tools/src/bash.rs`, `crates/harness-tools/tests/bash_tool.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces (in `harness_core::tool`):
  - `GuardReport { message: String, blocked: bool }`.
  - `trait CommandGuard: Send { fn finish(self: Box<Self>) -> Option<GuardReport>; }`
  - `SandboxedCommand { command: tokio::process::Command, guard: Option<Box<dyn CommandGuard>> }`.
  - `enum GitProtection { Full, Basic { reason: String } }`.
  - `CommandSandbox::prepare(&self, access: FsAccess, workspace: &Path, program: &str, args: &[&str]) -> std::io::Result<SandboxedCommand>`, by default `command()` with no guard; `CommandSandbox::git_protection(&self) -> GitProtection`, by default `Full`. Seatbelt and every test double keep working unchanged.
  - The `bash` tool calls `prepare` and `finish` off the async runtime (`spawn_blocking`), finishes the guard after the command exits, times out, is interrupted or fails to start, appends `"\n" + message`, and on `blocked` sets `is_error` and `sandbox_denied`. An unsandboxed re-run starts no guard.

- [ ] **Step 1: Write the failing tests**

Append to `crates/harness-core/tests/tool.rs`, after a blank line:

```rust
use harness_core::permission::FsAccess;
use harness_core::tool::{CommandSandbox, GitProtection};

/// A sandbox that only implements the required methods.
#[derive(Debug)]
struct Plain;

impl CommandSandbox for Plain {
    fn name(&self) -> &'static str {
        "plain"
    }
    fn command(
        &self,
        _access: FsAccess,
        _workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        Ok(cmd)
    }
    fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
        false
    }
}

#[test]
fn a_sandbox_without_a_guard_prepares_its_plain_command() {
    let prepared = Plain
        .prepare(FsAccess::WorkspaceWrite, Path::new("/w"), "echo", &["hi"])
        .unwrap();
    assert!(prepared.guard.is_none());
    let std = prepared.command.as_std();
    assert_eq!(std.get_program(), "echo");
    assert_eq!(std.get_args().collect::<Vec<_>>(), ["hi"]);
    assert_eq!(Plain.git_protection(), GitProtection::Full);
}
```

Append to `crates/harness-tools/tests/bash_tool.rs`, after a blank line:

```rust
use std::sync::Mutex;

use harness_core::tool::{CommandGuard, GuardReport, SandboxedCommand};

/// Records what happened to the guards a [`GuardedSandbox`] hands out.
#[derive(Debug, Default)]
struct GuardLog {
    events: Mutex<Vec<String>>,
}

impl GuardLog {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

/// Runs `program` (or `/nonexistent/program` with `broken`) directly, with a guard that logs
/// `finish` and returns `report`.
#[derive(Debug)]
struct GuardedSandbox {
    log: Arc<GuardLog>,
    report: Option<GuardReport>,
    broken: bool,
    fail_prepare: bool,
}

struct LoggingGuard {
    log: Arc<GuardLog>,
    report: Option<GuardReport>,
}

impl CommandGuard for LoggingGuard {
    fn finish(self: Box<Self>) -> Option<GuardReport> {
        self.log.events.lock().unwrap().push("finished".into());
        self.report
    }
}

impl CommandSandbox for GuardedSandbox {
    fn name(&self) -> &'static str {
        "guarded"
    }

    fn command(
        &self,
        _access: FsAccess,
        _workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<tokio::process::Command> {
        let program = if self.broken {
            "/nonexistent/program"
        } else {
            program
        };
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args).process_group(0);
        Ok(cmd)
    }

    fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
        false
    }

    fn prepare(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<SandboxedCommand> {
        if self.fail_prepare {
            return Err(std::io::Error::other("no way"));
        }
        self.log.events.lock().unwrap().push("prepared".into());
        Ok(SandboxedCommand {
            command: self.command(access, workspace, program, args)?,
            guard: Some(Box::new(LoggingGuard {
                log: self.log.clone(),
                report: self.report.clone(),
            })),
        })
    }
}

fn guarded(
    report: Option<GuardReport>,
    broken: bool,
) -> (tempfile::TempDir, ToolContext, Arc<GuardLog>) {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(GuardLog::default());
    let sandbox = GuardedSandbox {
        log: log.clone(),
        report,
        broken,
        fail_prepare: false,
    };
    let ctx = ToolContext::new(dir.path())
        .with_sandbox(Some(Arc::new(sandbox)), FsAccess::WorkspaceWrite);
    (dir, ctx, log)
}

fn blocking_report() -> Option<GuardReport> {
    Some(GuardReport {
        message: "[the sandbox undid changes: .git/hooks/pre-commit]\n".into(),
        blocked: true,
    })
}

#[tokio::test]
async fn a_guard_report_is_appended_and_a_blocking_one_marks_a_denial() {
    let (_dir, ctx, log) = guarded(blocking_report(), false);
    let out = BashTool.run(json!({"command": "echo hi"}), &ctx).await;
    assert_eq!(log.events(), ["prepared", "finished"]);
    assert!(out.is_error && out.sandbox_denied);
    assert_eq!(
        out.content,
        "exit code 0\nhi\n\n[the sandbox undid changes: .git/hooks/pre-commit]\n"
    );
}

#[tokio::test]
async fn a_report_that_blocks_nothing_leaves_the_result_as_it_was() {
    let report = GuardReport {
        message: "[before this command ran, harness found …]\n".into(),
        blocked: false,
    };
    let (_dir, ctx, _log) = guarded(Some(report), false);
    let out = BashTool.run(json!({"command": "echo hi"}), &ctx).await;
    assert!(!out.is_error && !out.sandbox_denied, "{}", out.content);
    assert!(
        out.content
            .ends_with("[before this command ran, harness found …]\n")
    );
}

#[tokio::test]
async fn no_report_leaves_the_output_unchanged() {
    let (_dir, ctx, log) = guarded(None, false);
    let out = BashTool.run(json!({"command": "echo hi"}), &ctx).await;
    assert_eq!(log.events(), ["prepared", "finished"]);
    assert_eq!(out.content, "exit code 0\nhi\n");
}

#[tokio::test]
async fn the_guard_finishes_after_a_timeout() {
    let (_dir, ctx, log) = guarded(blocking_report(), false);
    let out = BashTool
        .run(json!({"command": "sleep 30", "timeout_secs": 1}), &ctx)
        .await;
    assert_eq!(log.events(), ["prepared", "finished"]);
    assert!(out.content.contains("timed out"), "{}", out.content);
    assert!(out.sandbox_denied);
}

#[tokio::test]
async fn the_guard_finishes_after_an_interrupt() {
    let (_dir, ctx, log) = guarded(None, false);
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
    });
    let out = BashTool.run(json!({"command": "sleep 30"}), &ctx).await;
    assert_eq!(log.events(), ["prepared", "finished"]);
    assert!(out.content.contains("interrupted"), "{}", out.content);
}

#[tokio::test]
async fn the_guard_finishes_when_the_command_cannot_start() {
    let (_dir, ctx, log) = guarded(None, true);
    let out = BashTool.run(json!({"command": "echo hi"}), &ctx).await;
    assert_eq!(log.events(), ["prepared", "finished"]);
    assert!(
        out.is_error && out.content.contains("failed to start"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn an_unsandboxed_rerun_starts_no_guard() {
    let (_dir, mut ctx, log) = guarded(blocking_report(), false);
    ctx.unsandboxed = true;
    let out = BashTool.run(json!({"command": "echo hi"}), &ctx).await;
    assert!(log.events().is_empty());
    assert!(!out.sandbox_denied);
}

#[tokio::test]
async fn a_sandbox_that_cannot_prepare_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = GuardedSandbox {
        log: Arc::new(GuardLog::default()),
        report: None,
        broken: false,
        fail_prepare: true,
    };
    let ctx = ToolContext::new(dir.path())
        .with_sandbox(Some(Arc::new(sandbox)), FsAccess::WorkspaceWrite);
    let out = BashTool
        .run(json!({"command": "touch made.txt"}), &ctx)
        .await;
    assert!(out.is_error);
    assert_eq!(out.content, "failed to prepare the sandbox: no way");
    assert!(!dir.path().join("made.txt").exists());
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-core --test tool; cargo test -p harness-tools --test bash_tool`
Expected: FAIL to compile: ``unresolved import `harness_core::tool::GitProtection` `` in `tool`, and ``unresolved imports `harness_core::tool::CommandGuard`, `harness_core::tool::GuardReport`, `harness_core::tool::SandboxedCommand` `` in `bash_tool`.

- [ ] **Step 3: Add the types to harness-core**

In `crates/harness-core/src/tool.rs`:

Replace (1 of 2):

```rust
    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput;
}

/// Wraps shell commands so they run inside an OS sandbox. Implemented by `harness-sandbox`.
pub trait CommandSandbox: Send + Sync + std::fmt::Debug {
    /// Mechanism name for messages, e.g. `seatbelt` or `landlock+seccomp`.
```

with:

```rust
    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput;
}

/// What a [`CommandGuard`] did around one command, for the tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardReport {
    /// Appended to the command's output, so the model and the user both see it.
    pub message: String,
    /// The guard undid something the command did: the command counts as blocked by the sandbox.
    pub blocked: bool,
}

/// Checks and repairs protected git metadata around one sandboxed command. Implemented by
/// `harness-sandbox` on Linux.
pub trait CommandGuard: Send {
    /// Called once the command has ended: it exited, timed out, was interrupted, or never started.
    fn finish(self: Box<Self>) -> Option<GuardReport>;
}

/// A sandboxed command, and the guard to finish once it has ended.
pub struct SandboxedCommand {
    pub command: tokio::process::Command,
    pub guard: Option<Box<dyn CommandGuard>>,
}

/// How a sandbox protects git metadata (hooks, config, `commondir`, `.harness/`, a top-level
/// `HEAD`) inside a writable workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitProtection {
    /// Writes to protected git metadata fail: Seatbelt on macOS, the full tier on Linux.
    Full,
    /// Protected git metadata is checked after each command, and changes are moved to quarantine
    /// or restored: the Linux basic tier. `reason` says why the full tier is unavailable.
    Basic { reason: String },
}

/// Wraps shell commands so they run inside an OS sandbox. Implemented by `harness-sandbox`.
pub trait CommandSandbox: Send + Sync + std::fmt::Debug {
    /// Mechanism name for messages, e.g. `seatbelt` or `landlock+seccomp`.
```

Replace (2 of 2):

```rust
    ) -> std::io::Result<tokio::process::Command>;
    /// Whether a failed command's output looks like the sandbox blocked it.
    fn is_denial(&self, exit_code: Option<i32>, output: &str) -> bool;
}

/// Tools in a fixed order, so tool definitions are byte-identical across requests.
```

with:

```rust
    ) -> std::io::Result<tokio::process::Command>;
    /// Whether a failed command's output looks like the sandbox blocked it.
    fn is_denial(&self, exit_code: Option<i32>, output: &str) -> bool;
    /// [`command`](Self::command), plus a guard already started for it. The caller must finish the
    /// guard after the command ends, however it ends. The default starts no guard.
    fn prepare(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<SandboxedCommand> {
        Ok(SandboxedCommand {
            command: self.command(access, workspace, program, args)?,
            guard: None,
        })
    }
    /// How git metadata is protected in workspace-write mode. The default is
    /// [`GitProtection::Full`].
    fn git_protection(&self) -> GitProtection {
        GitProtection::Full
    }
}

/// Tools in a fixed order, so tool definitions are byte-identical across requests.
```

- [ ] **Step 4: Run the bash tool through prepare and finish**

Replace `crates/harness-tools/src/bash.rs` with:

```rust
use std::{path::Path, process::Stdio, sync::Arc, sync::Mutex, time::Duration};

use async_trait::async_trait;
use harness_core::{
    message::ToolSpec,
    permission::{Action, FsAccess},
    tool::{
        CommandGuard, CommandSandbox, GuardReport, SandboxedCommand, Tool, ToolContext, ToolOutput,
    },
};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;

/// Where bash is looked for, in order. Fixed absolute paths, so no `bash` on `PATH` is ever used.
const BASH_PATHS: [&str; 3] = [
    "/bin/bash",
    "/usr/bin/bash",
    "/run/current-system/sw/bin/bash",
];

/// The shell to run commands with, and its arguments before the script: the first bash in
/// `BASH_PATHS` that `is_file` finds, without startup files, else `/bin/sh`.
fn shell(is_file: impl Fn(&Path) -> bool) -> (&'static str, Vec<&'static str>) {
    match BASH_PATHS.into_iter().find(|p| is_file(Path::new(p))) {
        Some(bash) => (bash, vec!["--noprofile", "--norc", "-c"]),
        None => ("/bin/sh", vec!["-c"]),
    }
}

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a non-interactive shell command in the workspace. Returns the exit code and combined stdout/stderr. Default timeout 120s, max 600s.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn action(&self, args: &Value, _ctx: &ToolContext) -> Action {
        Action::Bash(args["command"].as_str().unwrap_or_default().to_string())
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> ToolOutput {
        let command = args["command"].as_str().unwrap_or_default();
        let secs = args["timeout_secs"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        // `exec 2>&1` merges stderr into stdout for the whole script, preserving interleaving.
        let script = format!("exec 2>&1\n{command}");
        let (shell, mut args) = shell(Path::is_file);
        args.push(&script);

        let sandbox = ctx.sandbox.clone().filter(|_| !ctx.unsandboxed);
        let (cmd, guard) = match &sandbox {
            Some(sandbox) => {
                match prepare(sandbox.clone(), ctx.access, &ctx.workspace, shell, &args).await {
                    Ok(prepared) => (prepared.command, prepared.guard),
                    Err(e) => {
                        return ToolOutput::error(format!("failed to prepare the sandbox: {e}"));
                    }
                }
            }
            None => {
                let mut cmd = tokio::process::Command::new(shell);
                cmd.args(&args).process_group(0);
                (cmd, None)
            }
        };
        let mut output = run_command(cmd, ctx, shell, secs, sandbox.as_deref()).await;
        // The guard is finished however the command ended: exited, timed out, interrupted, or
        // never started.
        if let Some(guard) = guard
            && let Some(report) = finish(guard).await
        {
            output.content.push('\n');
            output.content.push_str(&report.message);
            if report.blocked {
                output.is_error = true;
                output.sandbox_denied = true;
            }
        }
        output
    }
}

/// The sandboxed command and its guard, prepared off the async runtime: the guard walks the
/// workspace, which takes a while in a large one.
async fn prepare(
    sandbox: Arc<dyn CommandSandbox>,
    access: FsAccess,
    workspace: &Path,
    shell: &str,
    args: &[&str],
) -> std::io::Result<SandboxedCommand> {
    let workspace = workspace.to_path_buf();
    let shell = shell.to_string();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        sandbox.prepare(access, &workspace, &shell, &args)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Finishes the guard off the async runtime. A guard that panics leaves the command's effect on
/// git metadata unchecked, so the command counts as blocked.
async fn finish(guard: Box<dyn CommandGuard>) -> Option<GuardReport> {
    match tokio::task::spawn_blocking(move || guard.finish()).await {
        Ok(report) => report,
        Err(e) => Some(GuardReport {
            message: format!("[harness could not check git metadata after this command: {e}]"),
            blocked: true,
        }),
    }
}

/// Runs `cmd` until it exits, times out after `secs`, or the user interrupts it, and reports its
/// exit code and output.
async fn run_command(
    mut cmd: tokio::process::Command,
    ctx: &ToolContext,
    shell: &str,
    secs: u64,
    sandbox: Option<&dyn CommandSandbox>,
) -> ToolOutput {
    let mut child = match cmd
        .current_dir(&ctx.workspace)
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return ToolOutput::error(format!("failed to start {shell}: {e}")),
    };
    let pgid = child.id().map(|id| id as i32);
    let mut stdout = child.stdout.take().expect("stdout is piped");

    // Read stdout on a separate task into a buffer that outlives the `select!` below: if a
    // branch other than `finished` wins (timeout/interrupt), `finished` (and any buffer local
    // to it) is dropped, but this task keeps draining into `output`, so whatever the command
    // already printed is not lost.
    let output = Arc::new(Mutex::new(Vec::new()));
    let reader_output = output.clone();
    let mut reader = tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => reader_output
                    .lock()
                    .expect("bash output lock")
                    .extend_from_slice(&chunk[..n]),
            }
        }
    });
    let partial_text = |output: &Arc<Mutex<Vec<u8>>>| {
        let buf = output.lock().expect("bash output lock");
        String::from_utf8_lossy(&buf).into_owned()
    };

    let finished = async {
        let status = child.wait().await;
        // Drain whatever is left so a fast-exiting command's full output is captured.
        let _ = (&mut reader).await;
        status
    };

    tokio::select! {
        status = finished => {
            let text = partial_text(&output);
            match status {
                Ok(status) => {
                    let code = status.code().map_or_else(|| "signal".to_string(), |c| c.to_string());
                    let body = format!("exit code {code}\n{text}");
                    if status.success() {
                        ToolOutput::ok(body)
                    } else if sandbox.is_some_and(|s| s.is_denial(status.code(), &text)) {
                        let mut out = ToolOutput::error(format!("{body}\n[the sandbox may have blocked part of this command]"));
                        out.sandbox_denied = true;
                        out
                    } else {
                        ToolOutput::error(body)
                    }
                }
                Err(e) => ToolOutput::error(format!("failed to wait for command: {e}\n{text}")),
            }
        }
        _ = tokio::time::sleep(Duration::from_secs(secs)) => {
            kill_group(pgid);
            reader.abort();
            let text = partial_text(&output);
            ToolOutput::error(format!("command timed out after {secs}s and was terminated\n{text}"))
        }
        _ = ctx.cancel.cancelled() => {
            kill_group(pgid);
            reader.abort();
            let text = partial_text(&output);
            ToolOutput::error(format!("command interrupted by the user\n{text}"))
        }
    }
}

/// Kills the command and everything it started (it runs in its own process group).
fn kill_group(pgid: Option<i32>) {
    if let Some(pgid) = pgid {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_is_looked_for_at_fixed_paths_before_falling_back_to_sh() {
        let only = |path: &'static str| move |p: &Path| p == Path::new(path);
        for bash in [
            "/bin/bash",
            "/usr/bin/bash",
            "/run/current-system/sw/bin/bash",
        ] {
            assert_eq!(
                shell(only(bash)),
                (bash, vec!["--noprofile", "--norc", "-c"])
            );
        }
        assert_eq!(shell(|_| true).0, "/bin/bash");
        assert_eq!(shell(only("/usr/local/bin/bash")), ("/bin/sh", vec!["-c"]));
        assert_eq!(shell(|_| false), ("/bin/sh", vec!["-c"]));
    }
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p harness-core && cargo test -p harness-tools`
Expected: PASS, including `a_sandbox_without_a_guard_prepares_its_plain_command` and the eight new bash tests (`a_guard_report_is_appended_and_a_blocking_one_marks_a_denial`, `the_guard_finishes_after_a_timeout`, `the_guard_finishes_after_an_interrupt`, `the_guard_finishes_when_the_command_cannot_start`, …).

- [ ] **Step 6: Lint**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/harness-core crates/harness-tools
git commit -F - <<'EOF'
feat(core): let a sandbox guard each shell command

CommandSandbox::prepare returns the command plus an optional guard,
which the bash tool finishes after the command exits, times out, is
interrupted or fails to start. A report that says the guard undid
something marks the result as a sandbox denial. git_protection says
whether writes to git metadata fail or are checked after the fact.

<trailer lines from the controller>
EOF
```

---

### Task 4: The git-metadata guard

**Files:**
- Create: `crates/harness-sandbox/src/guard/mod.rs`, `crates/harness-sandbox/src/guard/quarantine.rs`, `crates/harness-sandbox/src/guard/snapshot.rs`, `crates/harness-sandbox/tests/git_guard.rs`
- Modify: `crates/harness-sandbox/Cargo.toml`, `crates/harness-sandbox/src/lib.rs`, `crates/harness-sandbox/src/gitmeta/mod.rs`

**Interfaces:**
- Consumes: `gitmeta::{discover, nested_gitdirs, GitIndex, GITDIR_PROTECTED, WORKSPACE_PROTECTED}` (Task 2); `harness_core::tool::GuardReport` (Task 3).
- Produces (in `harness_sandbox::guard`, platform-neutral):
  - `GuardSession::new(quarantine_root: &Path) -> Arc<GuardSession>`.
  - `GuardSession::begin(self: &Arc<Self>, workspace: &Path, save_all: bool, placeholders: impl FnOnce(&GitIndex)) -> GitGuard`: quarantines names planted since the previous command (reported, not blocking), indexes the workspace, runs `placeholders`, records the existing protected names and identities, and snapshots (every protected file with `save_all`; otherwise only symlinks and multiply linked files).
  - `GitGuard::index(&self) -> GitIndex`, `GitGuard::watch_handle(&self) -> WatchHandle`, `GitGuard::finish(self) -> Option<GuardReport>`.
  - `WatchHandle::{dirs(&self) -> Vec<PathBuf>, relevant(&self, dir: &Path, name: Option<&OsStr>) -> bool, check(&self)}`.
  - Report lines `- <path relative to the workspace>: <what>; <outcome>`, under `[before this command ran, …]` (not blocking) and `[the sandbox undid changes …]` (blocking).

The guard is platform-neutral so it is tested here on macOS; only Linux uses it. `tests/git_guard.rs` simulates what a sandboxed command does between `begin` and `finish`.

- [ ] **Step 1: Write the failing test**

`crates/harness-sandbox/tests/git_guard.rs`:

```rust
//! The git-metadata guard on real directory trees, simulating what a sandboxed command does
//! between `begin` and `finish`. Platform-neutral: runs on macOS and Linux.

use std::ffi::OsStr;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use harness_sandbox::guard::GuardSession;

struct Env {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    quarantine: PathBuf,
    session: Arc<GuardSession>,
}

/// A workspace holding a repository with `config`, `HEAD` and one hook, and a quarantine
/// directory next to it.
fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let ws = base.join("ws");
    std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
    std::fs::create_dir_all(ws.join(".git/objects")).unwrap();
    std::fs::write(ws.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(ws.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
    std::fs::write(ws.join(".git/hooks/pre-commit"), "exit 0\n").unwrap();
    let quarantine = base.join("quarantine");
    let session = GuardSession::new(&quarantine);
    Env {
        _dir: dir,
        ws,
        quarantine,
        session,
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

fn gone(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_err()
}

/// The one entry the quarantine holds at `rel` (relative to the workspace).
fn quarantined(env: &Env, rel: &str) -> PathBuf {
    let mut found: Vec<PathBuf> = std::fs::read_dir(&env.quarantine)
        .unwrap()
        .map(|e| e.unwrap().path().join(rel))
        .filter(|p| std::fs::symlink_metadata(p).is_ok())
        .collect();
    assert_eq!(found.len(), 1, "{rel} in quarantine: {found:?}");
    found.remove(0)
}

#[test]
fn a_command_that_leaves_git_metadata_alone_gets_no_report() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    // What git writes during commit, checkout and stash.
    std::fs::write(env.ws.join(".git/index.lock"), "x").unwrap();
    std::fs::rename(env.ws.join(".git/index.lock"), env.ws.join(".git/index")).unwrap();
    std::fs::write(env.ws.join(".git/COMMIT_EDITMSG"), "msg\n").unwrap();
    std::fs::create_dir_all(env.ws.join(".git/objects/ab")).unwrap();
    std::fs::write(env.ws.join(".git/objects/ab/cdef"), "blob").unwrap();
    std::fs::write(env.ws.join("src.txt"), "work").unwrap();
    assert_eq!(guard.finish(), None);
    assert!(!env.quarantine.exists());
}

#[test]
fn a_real_git_commit_gets_no_report() {
    let env = env();
    std::fs::remove_dir_all(env.ws.join(".git")).unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&env.ws)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(env.ws.join("a.txt"), "a").unwrap();
    assert!(git(&["add", "a.txt"]).status.success());
    let commit = git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "x",
    ]);
    assert!(commit.status.success(), "{commit:?}");
    assert!(git(&["checkout", "-q", "-b", "other"]).status.success());
    assert_eq!(guard.finish(), None);
}

#[test]
fn a_planted_hook_is_quarantined_and_blocks_the_command() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(env.ws.join(".git/hooks/post-checkout"), "echo pwned\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(gone(&env.ws.join(".git/hooks/post-checkout")));
    let moved = quarantined(&env, ".git/hooks/post-checkout");
    assert_eq!(read(&moved), "echo pwned\n");
    assert!(
        report.message.contains(&format!(
            "- .git/hooks/post-checkout: new in a protected directory; moved to {}",
            moved.display()
        )),
        "{}",
        report.message
    );
}

#[test]
fn a_changed_config_is_restored_and_the_change_kept() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(
        env.ws.join(".git/config"),
        "[core]\n\tfsmonitor = /tmp/evil\n",
    )
    .unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "[core]\n\tbare = false\n"
    );
    assert_eq!(
        read(&quarantined(&env, ".git/config")),
        "[core]\n\tfsmonitor = /tmp/evil\n"
    );
    assert!(
        report
            .message
            .contains("- .git/config: changed; restored the earlier version")
    );
}

#[test]
fn a_deleted_hook_is_restored() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::remove_file(env.ws.join(".git/hooks/pre-commit")).unwrap();
    let report = guard.finish().expect("a report");
    assert_eq!(read(&env.ws.join(".git/hooks/pre-commit")), "exit 0\n");
    assert!(
        report
            .message
            .contains("- .git/hooks/pre-commit: deleted; restored the earlier version")
    );
}

#[test]
fn without_saving_everything_only_new_names_are_undone() {
    // The Linux full tier: read-only mounts stop changes to existing entries, so they are not
    // saved; new names still appear, because mounts cannot cover what does not exist.
    let env = env();
    let guard = env.session.begin(&env.ws, false, |_| {});
    std::fs::write(
        env.ws.join(".git/config"),
        "changed through a mount that was not there\n",
    )
    .unwrap();
    std::fs::write(env.ws.join(".git/commondir"), "/tmp/evil\n").unwrap();
    let report = guard.finish().expect("a report");
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "changed through a mount that was not there\n"
    );
    assert!(gone(&env.ws.join(".git/commondir")));
    assert_eq!(read(&quarantined(&env, ".git/commondir")), "/tmp/evil\n");
    assert!(
        !report.message.contains(".git/config"),
        "{}",
        report.message
    );
}

#[test]
fn a_repointed_hooks_symlink_is_put_back_even_without_saving_everything() {
    // Read-only mounts cannot cover a symlink, so the full tier relies on the guard for it.
    let env = env();
    std::fs::rename(env.ws.join(".git/hooks"), env.ws.join("tracked-hooks")).unwrap();
    symlink("../tracked-hooks", env.ws.join(".git/hooks")).unwrap();
    let guard = env.session.begin(&env.ws, false, |_| {});
    std::fs::remove_file(env.ws.join(".git/hooks")).unwrap();
    symlink("/tmp/evil-hooks", env.ws.join(".git/hooks")).unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert_eq!(
        std::fs::read_link(env.ws.join(".git/hooks")).unwrap(),
        PathBuf::from("../tracked-hooks")
    );
    assert!(
        report
            .message
            .contains("- .git/hooks: changed; restored the earlier version"),
        "{}",
        report.message
    );
}

#[test]
fn a_hard_linked_config_is_restored_even_without_saving_everything() {
    let env = env();
    std::fs::hard_link(env.ws.join(".git/config"), env.ws.join("alias")).unwrap();
    let guard = env.session.begin(&env.ws, false, |_| {});
    std::fs::write(env.ws.join("alias"), "[core]\n\thooksPath = /tmp/evil\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert_eq!(
        read(&env.ws.join(".git/config")),
        "[core]\n\tbare = false\n"
    );
}

#[test]
fn new_protected_names_in_every_gitdir_are_quarantined() {
    let env = env();
    std::fs::create_dir_all(env.ws.join(".git/modules/sub")).unwrap();
    std::fs::write(env.ws.join(".git/modules/sub/HEAD"), "ref: x\n").unwrap();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(env.ws.join(".git/modules/sub/commondir"), "/tmp/evil\n").unwrap();
    std::fs::write(env.ws.join(".git/config.worktree"), "[core]\n").unwrap();
    std::fs::create_dir(env.ws.join(".git/gitweb")).unwrap();
    std::fs::write(env.ws.join(".git/pid"), "1\n").unwrap();
    let report = guard.finish().expect("a report");
    for rel in [
        ".git/modules/sub/commondir",
        ".git/config.worktree",
        ".git/gitweb",
        ".git/pid",
    ] {
        assert!(gone(&env.ws.join(rel)), "{rel}");
        quarantined(&env, rel);
        assert!(
            report.message.contains(&format!("- {rel}: new; moved to ")),
            "{rel}"
        );
    }
}

#[test]
fn a_top_level_head_and_harness_dir_are_quarantined() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(env.ws.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::create_dir(env.ws.join(".harness")).unwrap();
    std::fs::write(
        env.ws.join(".harness/config.toml"),
        "mode = \"full-access\"\n",
    )
    .unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(gone(&env.ws.join("HEAD")) && gone(&env.ws.join(".harness")));
    assert_eq!(
        read(&quarantined(&env, ".harness").join("config.toml")),
        "mode = \"full-access\"\n"
    );
}

#[test]
fn new_repositories_worktrees_and_submodules_are_quarantined() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::create_dir_all(env.ws.join("sub/.git/hooks")).unwrap();
    std::fs::write(env.ws.join("sub/file.txt"), "kept").unwrap();
    std::fs::create_dir_all(env.ws.join(".git/worktrees/wt")).unwrap();
    std::fs::write(env.ws.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
    std::fs::create_dir_all(env.ws.join(".git/modules/m")).unwrap();
    std::fs::write(env.ws.join(".git/modules/m/HEAD"), "ref: x\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(gone(&env.ws.join("sub/.git")));
    assert_eq!(read(&env.ws.join("sub/file.txt")), "kept");
    quarantined(&env, "sub/.git");
    quarantined(&env, ".git/worktrees/wt");
    quarantined(&env, ".git/modules/m");
    assert!(
        report
            .message
            .contains("- sub/.git: a new repository; moved to ")
    );
    assert!(
        report
            .message
            .contains("- .git/worktrees/wt: a new worktree or submodule gitdir; moved to ")
    );
}

#[test]
fn a_new_repository_in_an_ignored_directory_is_not_found() {
    // A known window (design.md): the walk skips git-ignored directories.
    let env = env();
    std::fs::write(env.ws.join(".gitignore"), "build/\n").unwrap();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::create_dir_all(env.ws.join("build/.git")).unwrap();
    assert_eq!(guard.finish(), None);
    assert!(env.ws.join("build/.git").exists());
}

#[test]
fn a_replaced_dot_git_is_quarantined_and_reported() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::rename(env.ws.join(".git"), env.ws.join("moved")).unwrap();
    std::fs::create_dir_all(env.ws.join(".git/hooks")).unwrap();
    std::fs::write(env.ws.join(".git/hooks/pre-commit"), "echo pwned\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert!(gone(&env.ws.join(".git")));
    assert_eq!(
        read(&quarantined(&env, ".git").join("hooks/pre-commit")),
        "echo pwned\n"
    );
    assert!(
        report
            .message
            .contains("- .git: moved or replaced; moved to "),
        "{}",
        report.message
    );
    // The repository the command moved away is left where it is.
    assert!(env.ws.join("moved/HEAD").exists());
}

#[test]
fn a_repointed_dot_git_symlink_is_put_back() {
    let env = env();
    std::fs::rename(env.ws.join(".git"), env.ws.join("real")).unwrap();
    symlink("real", env.ws.join(".git")).unwrap();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::remove_file(env.ws.join(".git")).unwrap();
    symlink("/tmp", env.ws.join(".git")).unwrap();
    let report = guard.finish().expect("a report");
    assert_eq!(
        std::fs::read_link(env.ws.join(".git")).unwrap(),
        PathBuf::from("real")
    );
    assert_eq!(
        std::fs::read_link(quarantined(&env, ".git")).unwrap(),
        PathBuf::from("/tmp")
    );
    assert!(
        report
            .message
            .contains("- .git: moved or replaced; restored the earlier version")
    );
}

#[test]
fn a_rewritten_gitfile_is_restored() {
    let env = env();
    std::fs::create_dir_all(env.ws.join(".git/modules/sub")).unwrap();
    std::fs::write(env.ws.join(".git/modules/sub/HEAD"), "ref: x\n").unwrap();
    std::fs::create_dir_all(env.ws.join("sub")).unwrap();
    std::fs::write(env.ws.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
    let guard = env.session.begin(&env.ws, true, |_| {});
    std::fs::write(env.ws.join("sub/.git"), "gitdir: /tmp/evil\n").unwrap();
    let report = guard.finish().expect("a report");
    assert!(report.blocked);
    assert_eq!(
        read(&env.ws.join("sub/.git")),
        "gitdir: ../.git/modules/sub\n"
    );
}

#[test]
fn names_planted_after_a_command_ends_are_caught_before_the_next() {
    let env = env();
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
    // A process the first command left running plants these after it ended.
    std::fs::write(env.ws.join(".git/commondir"), "/tmp/evil\n").unwrap();
    std::fs::write(env.ws.join("HEAD"), "ref: x\n").unwrap();
    let report = env
        .session
        .begin(&env.ws, true, |_| {})
        .finish()
        .expect("a report");
    assert!(!report.blocked, "the second command did nothing wrong");
    assert!(
        report.message.starts_with("[before this command ran"),
        "{}",
        report.message
    );
    assert!(gone(&env.ws.join(".git/commondir")) && gone(&env.ws.join("HEAD")));
}

#[test]
fn a_repository_created_between_commands_is_left_alone() {
    // The user may clone into the workspace while harness runs; only names inside gitdirs
    // that were already known are checked before a command.
    let env = env();
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
    std::fs::create_dir_all(env.ws.join("cloned/.git/hooks")).unwrap();
    std::fs::write(env.ws.join("cloned/.git/config"), "[core]\n").unwrap();
    assert_eq!(env.session.begin(&env.ws, true, |_| {}).finish(), None);
    assert!(env.ws.join("cloned/.git/config").exists());
}

#[test]
fn placeholders_run_before_existing_names_are_recorded() {
    let env = env();
    std::fs::remove_dir_all(env.ws.join(".git/hooks")).unwrap();
    let guard = env.session.begin(&env.ws, false, |index| {
        for gitdir in &index.gitdirs {
            std::fs::create_dir(gitdir.join("hooks")).unwrap();
        }
    });
    assert_eq!(guard.finish(), None);
    assert!(env.ws.join(".git/hooks").is_dir());
}

#[test]
fn the_quarantine_inside_the_workspace_is_not_walked() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
    let session = GuardSession::new(&ws.join(".quarantine"));
    let guard = session.begin(&ws, true, |_| {});
    std::fs::create_dir_all(ws.join("sub/.git")).unwrap();
    let report = guard.finish().expect("a report");
    assert_eq!(
        report.message.matches("\n- sub/.git:").count(),
        1,
        "{}",
        report.message
    );
    assert_eq!(session.begin(&ws, true, |_| {}).finish(), None);
}

#[test]
fn a_watcher_can_undo_changes_while_the_command_runs() {
    let env = env();
    let guard = env.session.begin(&env.ws, true, |_| {});
    let handle = guard.watch_handle();
    let dirs = handle.dirs();
    for dir in [&env.ws, &env.ws.join(".git"), &env.ws.join(".git/hooks")] {
        assert!(dirs.contains(dir), "{dir:?} not in {dirs:?}");
    }
    let git = env.ws.join(".git");
    assert!(handle.relevant(&env.ws, Some(OsStr::new(".git"))));
    assert!(handle.relevant(&env.ws, Some(OsStr::new("HEAD"))));
    assert!(!handle.relevant(&env.ws, Some(OsStr::new("src.txt"))));
    assert!(handle.relevant(&git, Some(OsStr::new("commondir"))));
    assert!(handle.relevant(&git, Some(OsStr::new("worktrees"))));
    assert!(!handle.relevant(&git, Some(OsStr::new("index.lock"))));
    assert!(handle.relevant(&git.join("hooks"), Some(OsStr::new("post-checkout"))));
    assert!(handle.relevant(&git, None));

    std::fs::write(git.join("commondir"), "/tmp/evil\n").unwrap();
    handle.check();
    assert!(
        gone(&git.join("commondir")),
        "moved while the command still runs"
    );
    handle.check();
    let report = guard.finish().expect("a report");
    assert_eq!(
        report.message.matches("\n- .git/commondir:").count(),
        1,
        "{}",
        report.message
    );
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test -p harness-sandbox --test git_guard`
Expected: FAIL to compile: ``could not find `guard` in `harness_sandbox` ``.

- [ ] **Step 3: Add the quarantine**

`crates/harness-sandbox/src/guard/quarantine.rs`:

```rust
//! Moves protected git metadata out of the workspace without deleting it.

use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// One command's quarantine directory, `<root>/<UTC time>-<pid>-<n>`, created
/// (private to the user) the first time something is moved into it.
#[derive(Debug)]
pub(crate) struct Quarantine {
    root: PathBuf,
    workspace: PathBuf,
    dir: Option<PathBuf>,
}

impl Quarantine {
    pub(crate) fn new(root: &Path, workspace: &Path) -> Quarantine {
        Quarantine {
            root: root.to_path_buf(),
            workspace: workspace.to_path_buf(),
            dir: None,
        }
    }

    /// Moves `path`, which is inside the workspace, into the quarantine
    /// directory at the same path relative to the workspace, and returns where
    /// it went. Across filesystems it is copied and the original removed; if
    /// that fails, it is renamed in place (`<name>.harness-quarantine-<n>`) so
    /// git no longer uses it. `NotFound` means it was already gone.
    pub(crate) fn take(&mut self, path: &Path) -> io::Result<PathBuf> {
        self.take_with(path, |from, to| std::fs::rename(from, to))
    }

    fn take_with(
        &mut self,
        path: &Path,
        rename: impl Fn(&Path, &Path) -> io::Result<()>,
    ) -> io::Result<PathBuf> {
        std::fs::symlink_metadata(path)?;
        let Ok(dest) = self.destination(path) else {
            return rename_in_place(path, &rename);
        };
        match rename(path, &dest) {
            Ok(()) => Ok(dest),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(e),
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                if copy_tree(path, &dest).is_err() {
                    let _ = remove_tree(&dest);
                    return rename_in_place(path, &rename);
                }
                if remove_tree(path).is_err() {
                    // The copy is complete; make sure git ignores what is left.
                    let _ = rename_in_place(path, &rename);
                }
                Ok(dest)
            }
            Err(_) => rename_in_place(path, &rename),
        }
    }

    /// A free path in the quarantine directory for `path`, with its parent
    /// directories created.
    fn destination(&mut self, path: &Path) -> io::Result<PathBuf> {
        let rel = path
            .strip_prefix(&self.workspace)
            .ok()
            .filter(|rel| !rel.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .or_else(|| path.file_name().map(PathBuf::from))
            .ok_or_else(|| io::Error::other("nothing to quarantine"))?;
        let dir = self.dir()?;
        let first = dir.join(&rel);
        if let Some(parent) = first.parent() {
            private_dirs(parent)?;
        }
        let mut dest = first.clone();
        let mut n = 1;
        while std::fs::symlink_metadata(&dest).is_ok() {
            let mut name = first.file_name().unwrap_or_default().to_os_string();
            name.push(format!(".{n}"));
            dest = first.with_file_name(name);
            n += 1;
        }
        Ok(dest)
    }

    fn dir(&mut self) -> io::Result<PathBuf> {
        if let Some(dir) = &self.dir {
            return Ok(dir.clone());
        }
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dir = self.root.join(format!(
            "{}-{}-{}",
            utc_stamp(now),
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        private_dirs(&dir)?;
        self.dir = Some(dir.clone());
        Ok(dir)
    }
}

/// Creates `dir` and its missing parents, readable only by the user.
fn private_dirs(dir: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Renames `path` to `<name>.harness-quarantine-<n>` next to it.
fn rename_in_place(
    path: &Path,
    rename: &impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("nothing to quarantine"))?;
    for n in 0..100 {
        let mut new_name = name.to_os_string();
        new_name.push(format!(".harness-quarantine-{n}"));
        let target = path.with_file_name(new_name);
        if std::fs::symlink_metadata(&target).is_err() {
            return rename(path, &target).map(|()| target);
        }
    }
    Err(io::Error::other("no free name to rename it to"))
}

/// Copies `src` to `dst` without following symlinks: directories, regular
/// files (with their permissions) and symlinks. Anything else is an error.
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
    } else if kind.is_file() {
        std::fs::copy(src, dst).map(|_| ())
    } else if kind.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        std::fs::set_permissions(dst, meta.permissions())
    } else {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

fn remove_tree(path: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// `secs` since the Unix epoch as a UTC time such as `20260927T143012Z`.
pub(crate) fn utc_stamp(secs: u64) -> String {
    // Howard Hinnant's days-to-civil algorithm.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let rest = secs % 86_400;
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let ws = base.join("ws");
        std::fs::create_dir(&ws).unwrap();
        (dir, ws, base.join("quarantine"))
    }

    #[test]
    fn stamps_are_utc_calendar_times() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(1_709_164_800), "20240229T000000Z");
        assert_eq!(utc_stamp(1_709_251_199), "20240229T235959Z");
    }

    #[test]
    fn a_moved_entry_keeps_its_path_below_a_private_directory() {
        let (_d, ws, root) = dirs();
        std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
        std::fs::write(ws.join(".git/hooks/pre-commit"), "echo hi\n").unwrap();
        let mut q = Quarantine::new(&root, &ws);
        let dest = q.take(&ws.join(".git/hooks/pre-commit")).unwrap();
        assert!(dest.ends_with(".git/hooks/pre-commit"), "{dest:?}");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "echo hi\n");
        assert!(!ws.join(".git/hooks/pre-commit").exists());
        let command_dir = std::fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mode = std::fs::metadata(&command_dir)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn the_same_path_twice_gets_a_numbered_name() {
        let (_d, ws, root) = dirs();
        let mut q = Quarantine::new(&root, &ws);
        std::fs::write(ws.join("HEAD"), "one").unwrap();
        let first = q.take(&ws.join("HEAD")).unwrap();
        std::fs::write(ws.join("HEAD"), "two").unwrap();
        let second = q.take(&ws.join("HEAD")).unwrap();
        assert_eq!(second, first.with_file_name("HEAD.1"));
        assert_eq!(std::fs::read_to_string(second).unwrap(), "two");
    }

    #[test]
    fn a_missing_entry_is_not_found() {
        let (_d, ws, root) = dirs();
        let err = Quarantine::new(&root, &ws)
            .take(&ws.join("HEAD"))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!root.exists(), "nothing to quarantine creates no directory");
    }

    #[test]
    fn across_filesystems_a_tree_is_copied_then_removed() {
        let (_d, ws, root) = dirs();
        std::fs::create_dir_all(ws.join("sub/.git/hooks")).unwrap();
        std::fs::write(ws.join("sub/.git/hooks/x"), "hook").unwrap();
        std::os::unix::fs::symlink("hooks/x", ws.join("sub/.git/link")).unwrap();
        let cross = |from: &Path, to: &Path| {
            if to.starts_with(&root) {
                Err(io::Error::from(io::ErrorKind::CrossesDevices))
            } else {
                std::fs::rename(from, to)
            }
        };
        let mut q = Quarantine::new(&root, &ws);
        let dest = q.take_with(&ws.join("sub/.git"), cross).unwrap();
        assert!(dest.starts_with(&root));
        assert_eq!(
            std::fs::read_to_string(dest.join("hooks/x")).unwrap(),
            "hook"
        );
        assert_eq!(
            std::fs::read_link(dest.join("link")).unwrap(),
            PathBuf::from("hooks/x")
        );
        assert!(!ws.join("sub/.git").exists());
    }

    #[test]
    fn what_cannot_be_copied_is_renamed_in_place() {
        let (_d, ws, root) = dirs();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(ws.join(".git/commondir"))
            .status()
            .unwrap();
        assert!(status.success());
        let cross = |from: &Path, to: &Path| {
            if to.starts_with(&root) {
                Err(io::Error::from(io::ErrorKind::CrossesDevices))
            } else {
                std::fs::rename(from, to)
            }
        };
        let mut q = Quarantine::new(&root, &ws);
        let dest = q.take_with(&ws.join(".git/commondir"), cross).unwrap();
        assert_eq!(dest, ws.join(".git/commondir.harness-quarantine-0"));
        assert!(std::fs::symlink_metadata(&dest).is_ok());
        assert!(std::fs::symlink_metadata(ws.join(".git/commondir")).is_err());
    }
}
```

- [ ] **Step 4: Add the snapshot**

`crates/harness-sandbox/src/guard/snapshot.rs`:

```rust
//! What protected git metadata looked like before a command, so that changes
//! to it can be found and undone afterwards.

use std::collections::BTreeMap;
use std::fs::Metadata;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Larger files are compared by size, times and inode, and are not saved.
const MAX_FILE_BYTES: u64 = 1 << 20;
/// Once this many bytes are saved, later files are only compared.
const MAX_TOTAL_BYTES: u64 = 16 << 20;
/// Entries deeper than this below a protected entry are not recorded.
const MAX_DEPTH: usize = 16;
/// At most this many entries are recorded.
const MAX_ENTRIES: usize = 20_000;

/// One recorded entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Dir {
        mode: u32,
    },
    File {
        mode: u32,
        content: Content,
    },
    Symlink {
        target: PathBuf,
    },
    /// A FIFO, socket or device node: compared by inode, never opened.
    Other {
        dev: u64,
        ino: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    Saved(Vec<u8>),
    /// Too large to save: compared by size, times and inode.
    Unsaved(Stamp),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    ino: u64,
}

impl Stamp {
    fn of(meta: &Metadata) -> Stamp {
        Stamp {
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
            ino: meta.ino(),
        }
    }
}

/// How an entry differs from its recorded state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Difference {
    /// Something else is there now.
    Changed,
    /// Still a directory, with other permissions.
    Permissions,
    /// Nothing is there now.
    Missing,
    /// New in a recorded directory.
    Added,
}

/// The recorded entries, parents before children.
#[derive(Debug, Default)]
pub(crate) struct Snapshot {
    nodes: BTreeMap<PathBuf, Node>,
    saved_bytes: u64,
}

impl Snapshot {
    /// Records `roots` and everything below them, without following symlinks.
    /// With `everything`, every entry is recorded and regular files' bytes are
    /// saved (up to the size limits). Without it, only what a read-only mount
    /// cannot protect is: symlinks, which cannot be mounted over, and regular
    /// files with more than one hard link, which can be written through
    /// another name.
    pub(crate) fn take(roots: &[PathBuf], everything: bool) -> Snapshot {
        let mut snapshot = Snapshot::default();
        for root in roots {
            snapshot.record(root, 0, everything);
        }
        snapshot
    }

    fn record(&mut self, path: &Path, depth: usize, everything: bool) {
        if depth > MAX_DEPTH || self.nodes.len() >= MAX_ENTRIES {
            return;
        }
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        let kind = meta.file_type();
        if kind.is_file() && (everything || meta.nlink() > 1) {
            let content = match read_regular(path, MAX_FILE_BYTES) {
                Some(bytes) if self.saved_bytes + (bytes.len() as u64) <= MAX_TOTAL_BYTES => {
                    self.saved_bytes += bytes.len() as u64;
                    Content::Saved(bytes)
                }
                _ => Content::Unsaved(Stamp::of(&meta)),
            };
            let mode = meta.permissions().mode();
            self.nodes
                .insert(path.to_path_buf(), Node::File { mode, content });
        } else if kind.is_dir() {
            if everything {
                let mode = meta.permissions().mode();
                self.nodes.insert(path.to_path_buf(), Node::Dir { mode });
            }
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            let mut children: Vec<PathBuf> =
                entries.filter_map(Result::ok).map(|e| e.path()).collect();
            children.sort();
            for child in children {
                self.record(&child, depth + 1, everything);
            }
        } else if kind.is_symlink() {
            if let Ok(target) = std::fs::read_link(path) {
                self.nodes
                    .insert(path.to_path_buf(), Node::Symlink { target });
            }
        } else if everything {
            let node = Node::Other {
                dev: meta.dev(),
                ino: meta.ino(),
            };
            self.nodes.insert(path.to_path_buf(), node);
        }
    }

    /// Every recorded directory: where a watcher should look for changes.
    pub(crate) fn dirs(&self) -> impl Iterator<Item = &Path> {
        self.nodes
            .iter()
            .filter(|(_, node)| matches!(node, Node::Dir { .. }))
            .map(|(path, _)| path.as_path())
    }

    /// Whether `path` is inside a recorded directory.
    pub(crate) fn covers(&self, path: &Path) -> bool {
        path.ancestors()
            .skip(1)
            .any(|dir| matches!(self.nodes.get(dir), Some(Node::Dir { .. })))
    }

    /// What differs from the recorded state now, parents before children.
    pub(crate) fn differences(&self) -> Vec<(PathBuf, Difference)> {
        let mut found = Vec::new();
        for (path, node) in &self.nodes {
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                found.push((path.clone(), Difference::Missing));
                continue;
            };
            if !unchanged(path, &meta, node) {
                let difference = match node {
                    Node::Dir { .. } if meta.is_dir() => Difference::Permissions,
                    _ => Difference::Changed,
                };
                found.push((path.clone(), difference));
                continue;
            }
            if let Node::Dir { .. } = node
                && let Ok(entries) = std::fs::read_dir(path)
            {
                let mut added: Vec<PathBuf> = entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|child| !self.nodes.contains_key(child))
                    .collect();
                added.sort();
                found.extend(added.into_iter().map(|p| (p, Difference::Added)));
            }
        }
        found
    }

    /// Puts the recorded entry back at `path`, where nothing is now (or, for a
    /// directory, a directory with the wrong permissions is).
    pub(crate) fn restore(&self, path: &Path) -> io::Result<()> {
        match self.nodes.get(path) {
            Some(Node::Dir { mode }) => {
                if !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
                    std::fs::create_dir(path)?;
                }
                std::fs::set_permissions(path, PermissionsExt::from_mode(*mode))
            }
            Some(Node::File {
                mode,
                content: Content::Saved(bytes),
            }) => replace_with(path, |tmp| {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .mode(0o600)
                    .open(tmp)?;
                file.write_all(bytes)?;
                file.set_permissions(PermissionsExt::from_mode(*mode))
            }),
            Some(Node::Symlink { target }) => {
                replace_with(path, |tmp| std::os::unix::fs::symlink(target, tmp))
            }
            Some(_) => Err(io::Error::other(
                "it was too large to save before the command, so it cannot be restored",
            )),
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }
}

/// Whether the entry at `path` (with `meta`, from `lstat`) still matches `node`.
fn unchanged(path: &Path, meta: &Metadata, node: &Node) -> bool {
    let kind = meta.file_type();
    match node {
        Node::Dir { mode } => kind.is_dir() && meta.permissions().mode() == *mode,
        Node::File { mode, content } => {
            kind.is_file()
                && meta.permissions().mode() == *mode
                && match content {
                    Content::Saved(bytes) => {
                        meta.len() == bytes.len() as u64
                            && read_regular(path, bytes.len() as u64).as_ref() == Some(bytes)
                    }
                    Content::Unsaved(stamp) => Stamp::of(meta) == *stamp,
                }
        }
        Node::Symlink { target } => {
            kind.is_symlink() && std::fs::read_link(path).is_ok_and(|t| t == *target)
        }
        Node::Other { dev, ino } => meta.dev() == *dev && meta.ino() == *ino,
    }
}

/// The bytes of the regular file at `path`, when it has at most `limit` of
/// them. Never follows a symlink and never blocks on a FIFO swapped in.
fn read_regular(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

/// Creates the entry with `create` under a temporary name next to `path`,
/// then renames it to `path`.
fn replace_with(path: &Path, create: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_file_name(format!(
        ".harness-restore-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(e) = create(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gitdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().canonicalize().unwrap().join(".git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::write(git.join("config"), "[core]\n").unwrap();
        std::fs::write(git.join("hooks/pre-commit"), "exit 0\n").unwrap();
        (dir, git)
    }

    fn roots(git: &Path) -> Vec<PathBuf> {
        vec![git.join("config"), git.join("hooks")]
    }

    #[test]
    fn nothing_changed_means_no_differences() {
        let (_d, git) = gitdir();
        let snapshot = Snapshot::take(&roots(&git), true);
        assert!(snapshot.differences().is_empty());
    }

    #[test]
    fn changes_deletions_and_additions_are_found_and_undone() {
        let (_d, git) = gitdir();
        let snapshot = Snapshot::take(&roots(&git), true);
        std::fs::write(git.join("config"), "[core]\n\thooksPath = /tmp\n").unwrap();
        std::fs::remove_file(git.join("hooks/pre-commit")).unwrap();
        std::fs::write(git.join("hooks/post-checkout"), "evil\n").unwrap();
        assert_eq!(
            snapshot.differences(),
            vec![
                (git.join("config"), Difference::Changed),
                (git.join("hooks/post-checkout"), Difference::Added),
                (git.join("hooks/pre-commit"), Difference::Missing),
            ]
        );
        std::fs::remove_file(git.join("config")).unwrap();
        snapshot.restore(&git.join("config")).unwrap();
        snapshot.restore(&git.join("hooks/pre-commit")).unwrap();
        std::fs::remove_file(git.join("hooks/post-checkout")).unwrap();
        assert_eq!(
            std::fs::read_to_string(git.join("config")).unwrap(),
            "[core]\n"
        );
        assert!(snapshot.differences().is_empty());
    }

    #[test]
    fn a_directory_replaced_by_a_file_is_changed() {
        let (_d, git) = gitdir();
        let snapshot = Snapshot::take(&roots(&git), true);
        std::fs::remove_dir_all(git.join("hooks")).unwrap();
        std::fs::write(git.join("hooks"), "not a directory").unwrap();
        let found = snapshot.differences();
        assert!(
            found.contains(&(git.join("hooks"), Difference::Changed)),
            "{found:?}"
        );
        std::fs::remove_file(git.join("hooks")).unwrap();
        snapshot.restore(&git.join("hooks")).unwrap();
        snapshot.restore(&git.join("hooks/pre-commit")).unwrap();
        assert!(snapshot.differences().is_empty());
    }

    #[test]
    fn a_permission_change_is_a_change() {
        let (_d, git) = gitdir();
        let hook = git.join("hooks/pre-commit");
        std::fs::set_permissions(&hook, PermissionsExt::from_mode(0o644)).unwrap();
        let snapshot = Snapshot::take(&roots(&git), true);
        std::fs::set_permissions(&hook, PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(
            snapshot.differences(),
            vec![(hook.clone(), Difference::Changed)]
        );
        std::fs::set_permissions(&hook, PermissionsExt::from_mode(0o644)).unwrap();
        let hooks = git.join("hooks");
        std::fs::set_permissions(&hooks, PermissionsExt::from_mode(0o777)).unwrap();
        assert_eq!(
            snapshot.differences(),
            vec![(hooks.clone(), Difference::Permissions)]
        );
        snapshot.restore(&hooks).unwrap();
        assert!(snapshot.differences().is_empty());
    }

    #[test]
    fn without_everything_only_hard_linked_files_and_symlinks_are_recorded() {
        let (_d, git) = gitdir();
        std::fs::hard_link(git.join("config"), git.parent().unwrap().join("alias")).unwrap();
        std::os::unix::fs::symlink("../shared-hooks", git.join("hooks/link")).unwrap();
        let snapshot = Snapshot::take(&roots(&git), false);
        std::fs::write(git.join("hooks/pre-commit"), "changed\n").unwrap();
        std::fs::write(git.join("hooks/new"), "added\n").unwrap();
        assert!(
            snapshot.differences().is_empty(),
            "other hooks are not recorded"
        );
        std::fs::remove_file(git.join("hooks/link")).unwrap();
        std::os::unix::fs::symlink("/tmp/evil", git.join("hooks/link")).unwrap();
        assert_eq!(
            snapshot.differences(),
            vec![(git.join("hooks/link"), Difference::Changed)]
        );
        std::fs::remove_file(git.join("hooks/link")).unwrap();
        snapshot.restore(&git.join("hooks/link")).unwrap();
        std::fs::write(
            git.parent().unwrap().join("alias"),
            "[core]\n\tfsmonitor = x\n",
        )
        .unwrap();
        assert_eq!(
            snapshot.differences(),
            vec![(git.join("config"), Difference::Changed)]
        );
        std::fs::remove_file(git.join("config")).unwrap();
        snapshot.restore(&git.join("config")).unwrap();
        assert_eq!(
            std::fs::read_to_string(git.join("config")).unwrap(),
            "[core]\n"
        );
    }

    #[test]
    fn a_large_file_is_compared_but_cannot_be_restored() {
        let (_d, git) = gitdir();
        let big = vec![b'x'; (MAX_FILE_BYTES + 1) as usize];
        std::fs::write(git.join("hooks/big"), &big).unwrap();
        let snapshot = Snapshot::take(&roots(&git), true);
        std::fs::write(git.join("hooks/big"), b"small").unwrap();
        assert_eq!(
            snapshot.differences(),
            vec![(git.join("hooks/big"), Difference::Changed)]
        );
        std::fs::remove_file(git.join("hooks/big")).unwrap();
        let err = snapshot.restore(&git.join("hooks/big")).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn a_fifo_swapped_in_is_changed_and_never_blocks() {
        let (_d, git) = gitdir();
        let snapshot = Snapshot::take(&roots(&git), true);
        std::fs::remove_file(git.join("config")).unwrap();
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(git.join("config"))
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            snapshot.differences(),
            vec![(git.join("config"), Difference::Changed)]
        );
    }

    #[test]
    fn a_symlink_is_restored_with_its_target() {
        let (_d, git) = gitdir();
        std::os::unix::fs::symlink("../shared-hooks", git.join("hooks/link")).unwrap();
        let snapshot = Snapshot::take(&roots(&git), true);
        std::fs::remove_file(git.join("hooks/link")).unwrap();
        std::os::unix::fs::symlink("/tmp/evil", git.join("hooks/link")).unwrap();
        assert_eq!(
            snapshot.differences(),
            vec![(git.join("hooks/link"), Difference::Changed)]
        );
        std::fs::remove_file(git.join("hooks/link")).unwrap();
        snapshot.restore(&git.join("hooks/link")).unwrap();
        assert_eq!(
            std::fs::read_link(git.join("hooks/link")).unwrap(),
            PathBuf::from("../shared-hooks")
        );
    }
}
```

- [ ] **Step 5: Add the guard**

`crates/harness-sandbox/src/guard/mod.rs`:

```rust
//! The git-metadata guard: finds and undoes changes to protected git metadata
//! around each sandboxed command, for the Linux sandbox. Platform-neutral, so
//! it is tested on every host.
//!
//! Mounts cannot cover a name that does not exist yet, and in the Linux basic
//! tier there are no mounts at all. So around each command the guard:
//!
//! - before it, moves to quarantine protected names that appeared in a known
//!   gitdir, or at the top of the workspace, since the previous command
//!   ended (a process that command left running may have planted them);
//! - indexes the workspace ([`discover`]) and records which protected names
//!   exist, and in the basic tier saves the protected files;
//! - while it runs (a watcher calls [`WatchHandle::check`]) and after it
//!   ends, moves to quarantine every new protected name, new gitdir and
//!   replaced `.git`, and restores changed protected files;
//! - after it ends, also walks the workspace for new `.git` entries.
//!
//! Nothing is ever deleted: it is moved to `<quarantine root>/<time>-<pid>-<n>/`
//! at its path relative to the workspace.

mod quarantine;
mod snapshot;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use harness_core::tool::GuardReport;

use crate::gitmeta::{GITDIR_PROTECTED, GitIndex, WORKSPACE_PROTECTED, discover, nested_gitdirs};
use quarantine::Quarantine;
use snapshot::{Difference, Snapshot};

/// Guards every command of one session. Remembers which protected names
/// existed when the previous command's guard finished.
#[derive(Debug)]
pub struct GuardSession {
    quarantine_root: PathBuf,
    baseline: Mutex<Option<Baseline>>,
}

#[derive(Debug)]
struct Baseline {
    workspace: PathBuf,
    candidates: BTreeSet<PathBuf>,
    existing: BTreeSet<PathBuf>,
}

impl GuardSession {
    /// A session whose quarantined entries go below `quarantine_root`.
    pub fn new(quarantine_root: &Path) -> Arc<GuardSession> {
        Arc::new(GuardSession {
            quarantine_root: quarantine_root.to_path_buf(),
            baseline: Mutex::new(None),
        })
    }

    /// Starts guarding one command in the canonical `workspace`: see the
    /// module docs. `placeholders` runs after the workspace is indexed and
    /// before the existing protected names are recorded (the Linux full tier
    /// creates empty `hooks/` directories there). With `save_all`, every
    /// protected file is saved so it can be restored (the Linux basic tier);
    /// without it, only protected files with more than one hard link are.
    pub fn begin(
        self: &Arc<Self>,
        workspace: &Path,
        save_all: bool,
        placeholders: impl FnOnce(&GitIndex),
    ) -> GitGuard {
        let mut quarantine = Quarantine::new(&self.quarantine_root, workspace);
        let mut before = Vec::new();
        let baseline = lock(&self.baseline).take();
        if let Some(baseline) = baseline.filter(|b| b.workspace == workspace) {
            for path in baseline.candidates.difference(&baseline.existing) {
                before.extend(take(&mut quarantine, path, What::New));
            }
        }

        let index = discover(workspace, Some(&self.quarantine_root));
        placeholders(&index);
        let candidates = candidates(workspace, &index);
        let existing: BTreeSet<PathBuf> =
            candidates.iter().filter(|p| exists(p)).cloned().collect();
        let identities = index
            .dot_gits
            .iter()
            .chain(&index.gitdirs)
            .chain(&index.links)
            .map(|path| (path.clone(), Identity::of(path)))
            .collect();
        // A gitfile names the gitdir git uses, so it is saved like the
        // protected files.
        let gitfiles = index
            .dot_gits
            .iter()
            .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file()));
        let roots: Vec<PathBuf> = existing.iter().chain(gitfiles).cloned().collect();
        let snapshot = Snapshot::take(&roots, save_all);
        GitGuard {
            session: Arc::clone(self),
            state: Arc::new(Mutex::new(State {
                workspace: workspace.to_path_buf(),
                index,
                candidates,
                existing,
                identities,
                snapshot,
                quarantine,
                before,
                after: Vec::new(),
            })),
        }
    }
}

/// Guards one command. Give [`watch_handle`](Self::watch_handle) to a file
/// watcher, then call [`finish`](Self::finish) once the command has ended.
#[derive(Debug)]
pub struct GitGuard {
    session: Arc<GuardSession>,
    state: Arc<Mutex<State>>,
}

impl GitGuard {
    /// The workspace's git metadata as it was when the command started.
    pub fn index(&self) -> GitIndex {
        lock(&self.state).index.clone()
    }

    /// Lets a file watcher run the checks while the command runs.
    pub fn watch_handle(&self) -> WatchHandle {
        WatchHandle {
            state: Arc::clone(&self.state),
        }
    }

    /// Runs every check once more, walks the workspace for new `.git`
    /// entries, and says what was done, if anything.
    pub fn finish(self) -> Option<GuardReport> {
        let mut state = lock(&self.state);
        state.check();
        let now = discover(&state.workspace, Some(&self.session.quarantine_root));
        let new: Vec<PathBuf> = now
            .dot_gits
            .difference(&state.index.dot_gits)
            .cloned()
            .collect();
        for dot_git in new {
            state.quarantine(&dot_git, What::Repository);
        }
        let existing = state
            .candidates
            .iter()
            .filter(|p| exists(p))
            .cloned()
            .collect();
        *lock(&self.session.baseline) = Some(Baseline {
            workspace: state.workspace.clone(),
            candidates: state.candidates.clone(),
            existing,
        });
        state.report()
    }
}

/// Runs the guard's checks for a file watcher, while the command runs.
#[derive(Debug, Clone)]
pub struct WatchHandle {
    state: Arc<Mutex<State>>,
}

impl WatchHandle {
    /// The directories to watch: the workspace, every gitdir and the
    /// `worktrees` and `modules` directories in it, and every saved directory
    /// inside a protected entry.
    pub fn dirs(&self) -> Vec<PathBuf> {
        let state = lock(&self.state);
        let mut dirs = vec![state.workspace.clone()];
        for gitdir in &state.index.gitdirs {
            dirs.push(gitdir.clone());
            dirs.push(gitdir.join("worktrees"));
            dirs.push(gitdir.join("modules"));
        }
        dirs.extend(state.snapshot.dirs().map(Path::to_path_buf));
        dirs.retain(|dir| std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()));
        dirs.sort();
        dirs.dedup();
        dirs
    }

    /// Whether a change to `name` in the watched directory `dir`, or to `dir`
    /// itself when `name` is `None`, can concern protected metadata.
    pub fn relevant(&self, dir: &Path, name: Option<&OsStr>) -> bool {
        let Some(name) = name else {
            return true;
        };
        let state = lock(&self.state);
        let named = |names: &[&str]| names.iter().any(|n| OsStr::new(n) == name);
        if dir == state.workspace {
            return named(&[".git"]) || named(&WORKSPACE_PROTECTED);
        }
        if state.index.gitdirs.contains(dir) {
            return named(&GITDIR_PROTECTED) || named(&["worktrees", "modules"]);
        }
        let in_gitdir = |sub: &str| {
            dir.file_name() == Some(OsStr::new(sub))
                && dir
                    .parent()
                    .is_some_and(|p| state.index.gitdirs.contains(p))
        };
        in_gitdir("worktrees") || in_gitdir("modules") || state.snapshot.covers(&dir.join(name))
    }

    /// Runs the checks that need no walk of the workspace, undoing what they
    /// find.
    pub fn check(&self) {
        lock(&self.state).check();
    }
}

#[derive(Debug)]
struct State {
    workspace: PathBuf,
    index: GitIndex,
    /// Every path a protected name could appear at: the protected names in
    /// each gitdir, and `.harness` and `HEAD` at the top of the workspace.
    candidates: BTreeSet<PathBuf>,
    /// The candidates that existed when the command started.
    existing: BTreeSet<PathBuf>,
    /// What each `.git` entry, gitdir and link was when the command started.
    identities: BTreeMap<PathBuf, Identity>,
    snapshot: Snapshot,
    quarantine: Quarantine,
    /// Found before the command started.
    before: Vec<Finding>,
    /// Found while the command ran or after it ended.
    after: Vec<Finding>,
}

impl State {
    fn check(&mut self) {
        let broken = self.check_identities();
        let new: Vec<PathBuf> = self
            .candidates
            .difference(&self.existing)
            .filter(|p| exists(p))
            .cloned()
            .collect();
        for path in new {
            self.quarantine(&path, What::New);
        }
        let gitdirs: Vec<PathBuf> = self.index.gitdirs.iter().cloned().collect();
        for gitdir in gitdirs {
            for found in nested_gitdirs(&gitdir) {
                if !self.index.gitdirs.contains(&found) {
                    self.quarantine(&found, What::Gitdir);
                }
            }
        }
        let top = self.workspace.join(".git");
        if !self.index.dot_gits.contains(&top) && exists(&top) {
            self.quarantine(&top, What::Repository);
        }
        for (path, difference) in self.snapshot.differences() {
            if broken.iter().any(|b| path.starts_with(b)) {
                continue;
            }
            match difference {
                Difference::Added => self.quarantine(&path, What::Added),
                Difference::Changed => {
                    let moved = self.quarantine.take(&path).ok();
                    self.restore(&path, What::Changed, moved);
                }
                Difference::Missing => self.restore(&path, What::Deleted, None),
                Difference::Permissions => self.restore(&path, What::Changed, None),
            }
        }
    }

    /// Quarantines whatever replaced a `.git` entry, gitdir or link, and puts
    /// a symlink back. Returns the paths that are no longer what they were.
    fn check_identities(&mut self) -> Vec<PathBuf> {
        let mut broken = Vec::new();
        let recorded: Vec<(PathBuf, Identity)> = self
            .identities
            .iter()
            .map(|(p, i)| (p.clone(), i.clone()))
            .collect();
        for (path, was) in recorded {
            let now = Identity::of(&path);
            if now == was {
                continue;
            }
            let outcome = if now == Identity::Missing {
                Outcome::Failed("it was moved or deleted".into())
            } else {
                match self.quarantine.take(&path) {
                    Ok(to) => Outcome::Moved(to),
                    Err(e) => Outcome::Failed(format!("could not move it: {e}")),
                }
            };
            let outcome = match (&was, outcome) {
                (Identity::Symlink { target }, Outcome::Moved(to)) => {
                    match std::os::unix::fs::symlink(target, &path) {
                        Ok(()) => Outcome::Restored(Some(to)),
                        Err(_) => Outcome::Moved(to),
                    }
                }
                (_, outcome) => outcome,
            };
            self.after.push(Finding {
                path: path.clone(),
                what: What::Replaced,
                outcome,
            });
            self.identities.insert(path.clone(), Identity::of(&path));
            if Identity::of(&path) != was {
                broken.push(path);
            }
        }
        broken
    }

    fn quarantine(&mut self, path: &Path, what: What) {
        self.after.extend(take(&mut self.quarantine, path, what));
    }

    fn restore(&mut self, path: &Path, what: What, moved: Option<PathBuf>) {
        let outcome = match self.snapshot.restore(path) {
            Ok(()) => Outcome::Restored(moved),
            Err(e) => match moved {
                Some(to) => Outcome::Failed(format!(
                    "moved to {}, but could not restore the earlier version: {e}",
                    to.display()
                )),
                None => Outcome::Failed(format!("could not restore the earlier version: {e}")),
            },
        };
        self.after.push(Finding {
            path: path.to_path_buf(),
            what,
            outcome,
        });
    }

    fn report(&self) -> Option<GuardReport> {
        if self.before.is_empty() && self.after.is_empty() {
            return None;
        }
        let mut message = String::new();
        if !self.before.is_empty() {
            message.push_str(
                "[before this command ran, harness found protected git metadata created after the previous command ended, probably by a process it left running:",
            );
            self.lines(&self.before, &mut message);
            message.push_str("]\n");
        }
        if !self.after.is_empty() {
            message.push_str(
                "[the sandbox undid changes this command made to protected git metadata (hooks, config, commondir, repositories, .harness/, a top-level HEAD), which git outside the sandbox would otherwise use:",
            );
            self.lines(&self.after, &mut message);
            message.push_str("]\n");
        }
        Some(GuardReport {
            message,
            blocked: !self.after.is_empty(),
        })
    }

    /// One line per finding. A check that runs again while a failure
    /// persists finds it again, so repeated lines are left out.
    fn lines(&self, findings: &[Finding], out: &mut String) {
        let mut seen = BTreeSet::new();
        for finding in findings {
            let rel = finding
                .path
                .strip_prefix(&self.workspace)
                .unwrap_or(&finding.path);
            let line = format!(
                "\n- {}: {}; {}",
                rel.display(),
                finding.what.describe(),
                finding.outcome.describe()
            );
            if seen.insert(line.clone()) {
                out.push_str(&line);
            }
        }
    }
}

/// Moves `path` to quarantine, if it is there.
fn take(quarantine: &mut Quarantine, path: &Path, what: What) -> Option<Finding> {
    let outcome = match quarantine.take(path) {
        Ok(to) => Outcome::Moved(to),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => Outcome::Failed(format!("could not move it: {e}")),
    };
    Some(Finding {
        path: path.to_path_buf(),
        what,
        outcome,
    })
}

/// Every path a protected name could appear at in `index`.
fn candidates(workspace: &Path, index: &GitIndex) -> BTreeSet<PathBuf> {
    let mut candidates: BTreeSet<PathBuf> = index
        .gitdirs
        .iter()
        .flat_map(|gitdir| GITDIR_PROTECTED.iter().map(move |name| gitdir.join(name)))
        .collect();
    candidates.extend(WORKSPACE_PROTECTED.iter().map(|name| workspace.join(name)));
    candidates
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What an entry is, for noticing that it was replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Identity {
    Missing,
    Symlink { target: PathBuf },
    Inode { dir: bool, dev: u64, ino: u64 },
}

impl Identity {
    fn of(path: &Path) -> Identity {
        match std::fs::symlink_metadata(path) {
            Err(_) => Identity::Missing,
            Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(path) {
                Ok(target) => Identity::Symlink { target },
                Err(_) => Identity::Missing,
            },
            Ok(meta) => Identity::Inode {
                dir: meta.is_dir(),
                dev: meta.dev(),
                ino: meta.ino(),
            },
        }
    }
}

#[derive(Debug)]
struct Finding {
    path: PathBuf,
    what: What,
    outcome: Outcome,
}

#[derive(Debug, Clone, Copy)]
enum What {
    /// A protected name that did not exist before.
    New,
    /// A new `.git` entry.
    Repository,
    /// A new linked-worktree or submodule gitdir.
    Gitdir,
    /// A `.git` entry, gitdir or link that is not what it was.
    Replaced,
    /// A protected file whose content, type or permissions changed.
    Changed,
    /// A protected file that is gone.
    Deleted,
    /// A new entry in a protected directory.
    Added,
}

impl What {
    fn describe(self) -> &'static str {
        match self {
            What::New => "new",
            What::Repository => "a new repository",
            What::Gitdir => "a new worktree or submodule gitdir",
            What::Replaced => "moved or replaced",
            What::Changed => "changed",
            What::Deleted => "deleted",
            What::Added => "new in a protected directory",
        }
    }
}

#[derive(Debug)]
enum Outcome {
    Moved(PathBuf),
    /// Put back as it was; the changed version, if any, is in quarantine.
    Restored(Option<PathBuf>),
    Failed(String),
}

impl Outcome {
    fn describe(&self) -> String {
        match self {
            Outcome::Moved(to) => format!("moved to {}", to.display()),
            Outcome::Restored(Some(to)) => format!(
                "restored the earlier version (the changed one is in {})",
                to.display()
            ),
            Outcome::Restored(None) => "restored the earlier version".into(),
            Outcome::Failed(why) => why.clone(),
        }
    }
}
```

- [ ] **Step 6: Wire it in**

`crates/harness-sandbox/Cargo.toml` (`libc` is now used on every OS: the snapshot opens files with `O_NOFOLLOW | O_NONBLOCK`):

Replace:

```toml
[dependencies]
harness-core.workspace = true
ignore.workspace = true
tokio.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
landlock.workspace = true
libc.workspace = true
seccompiler.workspace = true

[dev-dependencies]
```

with:

```toml
[dependencies]
harness-core.workspace = true
ignore.workspace = true
libc.workspace = true
tokio.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
landlock.workspace = true
seccompiler.workspace = true

[dev-dependencies]
```

`crates/harness-sandbox/src/lib.rs`:

Replace:

```rust

mod denial;
pub mod gitmeta;
// Only x86_64/aarch64 are supported: `linux::seccomp` only knows how to
// target those two architectures. Any other Linux architecture skips this
// module entirely and compiles as if no sandbox backend were available,
```

with:

```rust

mod denial;
pub mod gitmeta;
pub mod guard;
// Only x86_64/aarch64 are supported: `linux::seccomp` only knows how to
// target those two architectures. Any other Linux architecture skips this
// module entirely and compiles as if no sandbox backend were available,
```

`crates/harness-sandbox/src/gitmeta/mod.rs`:

Replace:

```rust
mod index;
mod linked;

pub use index::{GitIndex, discover};
#[cfg(target_os = "macos")]
pub(crate) use linked::{LinkedGitdirs, linked_gitdirs};
```

with:

```rust
mod index;
mod linked;

pub(crate) use index::nested_gitdirs;
pub use index::{GitIndex, discover};
#[cfg(target_os = "macos")]
pub(crate) use linked::{LinkedGitdirs, linked_gitdirs};
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p harness-sandbox`
Expected: PASS: the 20 tests in `git_guard` and the `guard::quarantine` and `guard::snapshot` unit tests.

- [ ] **Step 8: Lint**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && bash target/linux-lint.sh`
Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add crates/harness-sandbox
git commit -F - <<'EOF'
feat(sandbox): add the git-metadata guard

Around one command the guard indexes the workspace, records which
protected names exist, and snapshots protected files. Afterwards (and
whenever a watcher asks) it moves new protected names, new gitdirs,
new repositories and replaced .git entries to a quarantine, restores
changed files, and reports what it did. Before the next command it
catches names planted after the previous one ended. Nothing is ever
deleted.

<trailer lines from the controller>
EOF
```

---

### Task 5: Refuse mount and namespace syscalls on Linux

**Files:**
- Modify: `crates/harness-sandbox/src/linux/seccomp.rs`, `crates/harness-sandbox/src/linux/detect.rs`, `crates/harness-sandbox/src/linux/preexec.rs`, `crates/harness-sandbox/src/linux/mod.rs`, `crates/harness-sandbox/tests/linux_sandbox.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces (Linux, crate-internal): `seccomp::build_deny_filter()` (renamed from `build_network_deny_filter`; it now also refuses `mount`, `umount2`, `mount_setattr`, `move_mount`, `open_tree`, `open_tree_attr` (467), `fsopen`, `fsconfig`, `fsmount`, `fspick`, `pivot_root`, `unshare`, `setns`, and `clone` with any `CLONE_NEW*` flag, all with `EPERM`); `seccomp::build_clone3_filter()` (`clone3` fails with `ENOSYS`); `PreparedSandbox.clone3_program`, installed right after the main program. `linux_sandbox_available` also requires the second program to build.

The x32 splice is unaffected: adding rules does not change the first four instructions `verify_prologue` checks. Everything here is Linux-only, so it cannot run on macOS; the lint shows it compiles, and CI runs the tests.

- [ ] **Step 1: Write the failing tests**

In `crates/harness-sandbox/tests/linux_sandbox.rs`, add a section before `// Process hierarchy`:

Replace:

```rust
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

// ---------------------------------------------------------------------------
// Process hierarchy
// ---------------------------------------------------------------------------
```

with:

```rust
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

// ---------------------------------------------------------------------------
// Mounts and namespaces
// ---------------------------------------------------------------------------

/// Every mount-changing and namespace-entering syscall fails with `EPERM` — a value only the
/// seccomp filter gives for all of them: without it, `setns(-1)` is `EBADF`, `open_tree` of `/`
/// succeeds, and `open_tree_attr` is `ENOSYS` on kernels before 6.15.
#[tokio::test]
async fn mount_and_namespace_syscalls_fail_with_eperm() {
    let Some(ws) = new_workspace() else {
        return;
    };
    if require_command("python3") {
        return;
    }
    let policy = workspace_write_policy(ws.path());
    let script = r#"
import ctypes, errno, platform, sys
libc = ctypes.CDLL(None, use_errno=True)
x86 = platform.machine() == "x86_64"
AT_FDCWD = -100
calls = [
    ("mount", 165 if x86 else 40, (b"none", b"/tmp", b"tmpfs", 0, None)),
    ("umount2", 166 if x86 else 39, (b"/tmp", 0)),
    ("pivot_root", 155 if x86 else 41, (b".", b".")),
    ("unshare", 272 if x86 else 97, (0x10000000,)),
    ("setns", 308 if x86 else 268, (-1, 0)),
    ("open_tree", 428, (AT_FDCWD, b"/", 0)),
    ("move_mount", 429, (AT_FDCWD, b"/", AT_FDCWD, b"/tmp", 0)),
    ("fsopen", 430, (b"tmpfs", 0)),
    ("fsconfig", 431, (-1, 0, None, None, 0)),
    ("fsmount", 432, (-1, 0, 0)),
    ("fspick", 433, (AT_FDCWD, b"/", 0)),
    ("mount_setattr", 442, (AT_FDCWD, b"/", 0, None, 0)),
    ("open_tree_attr", 467, (AT_FDCWD, b"/", 0, None, 0)),
]
for name, nr, args in calls:
    rc = libc.syscall(nr, *args)
    err = ctypes.get_errno()
    if rc != -1 or err != errno.EPERM:
        sys.exit(f"{name}: expected EPERM, got rc={rc} errno={err}")
print("ok")
"#;

    let output = run(&policy, "python3", &["-c", script]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

/// `clone` with a namespace flag fails with `EPERM`; plain `clone` (a fork) still works.
#[tokio::test]
async fn clone_with_a_namespace_flag_fails_with_eperm() {
    let Some(ws) = new_workspace() else {
        return;
    };
    if require_command("python3") {
        return;
    }
    let policy = workspace_write_policy(ws.path());
    let script = r#"
import ctypes, errno, os, platform, sys
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
SYS_clone = ctypes.c_long(56 if platform.machine() == "x86_64" else 220)
SIGCHLD = 17
NULL = ctypes.c_void_p(None)
for flag in [0x20000, 0x02000000, 0x04000000, 0x08000000, 0x10000000, 0x20000000, 0x40000000]:
    rc = libc.syscall(SYS_clone, ctypes.c_ulong(flag | SIGCHLD), NULL, NULL, NULL, NULL)
    if rc == 0:
        os._exit(0)
    err = ctypes.get_errno()
    if rc != -1 or err != errno.EPERM:
        sys.exit(f"clone({flag:#x}): expected EPERM, got rc={rc} errno={err}")
pid = libc.syscall(SYS_clone, ctypes.c_ulong(SIGCHLD), NULL, NULL, NULL, NULL)
if pid == 0:
    os._exit(7)
if pid < 0:
    sys.exit(f"a plain clone failed: errno={ctypes.get_errno()}")
_, status = os.waitpid(pid, 0)
if os.WEXITSTATUS(status) != 7:
    sys.exit(f"the child exited with {status}")
print("ok")
"#;

    let output = run(&policy, "python3", &["-c", script]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

/// `clone3` fails with `ENOSYS`, so runtimes fall back to `clone`: threads and subprocesses
/// keep working.
#[tokio::test]
async fn clone3_fails_with_enosys_and_threads_still_work() {
    let Some(ws) = new_workspace() else {
        return;
    };
    if require_command("python3") {
        return;
    }
    let policy = workspace_write_policy(ws.path());
    let script = r#"
import ctypes, errno, subprocess, sys, threading
libc = ctypes.CDLL(None, use_errno=True)
rc = libc.syscall(435, None, 0)
if rc != -1 or ctypes.get_errno() != errno.ENOSYS:
    sys.exit(f"clone3: expected ENOSYS, got rc={rc} errno={ctypes.get_errno()}")
done = []
thread = threading.Thread(target=lambda: done.append(1))
thread.start()
thread.join()
if done != [1]:
    sys.exit("the thread did not run")
if subprocess.run(["true"]).returncode != 0:
    sys.exit("a subprocess failed")
print("ok")
"#;

    let output = run(&policy, "python3", &["-c", script]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
}

#[tokio::test]
async fn child_proc_status_reports_both_seccomp_filters() {
    let Some(ws) = new_workspace() else {
        return;
    };
    let policy = workspace_write_policy(ws.path());

    let output = run(&policy, "cat", &["/proc/self/status"]).await;

    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line == "Seccomp_filters:\t2"),
        "missing Seccomp_filters:\\t2 in:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// Process hierarchy
// ---------------------------------------------------------------------------
```

In the unit tests at the bottom of `crates/harness-sandbox/src/linux/seccomp.rs` (the program builder is renamed in Step 3):

Replace (1 of 3):

```rust
    use super::*;

    #[test]
    fn network_deny_filter_compiles_to_a_non_empty_program() {
        let program = build_network_deny_filter().expect("filter should compile");
        assert!(!program.is_empty());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn compile_rules_prologue_matches_seccompiler_0_5_0() {
```

with:

```rust
    use super::*;

    #[test]
    fn deny_filter_compiles_to_a_non_empty_program() {
        let program = build_deny_filter().expect("filter should compile");
        assert!(!program.is_empty());
    }

    #[test]
    fn clone3_filter_compiles_to_a_non_empty_program() {
        let program = build_clone3_filter().expect("filter should compile");
        assert!(!program.is_empty());
    }

    #[test]
    fn mount_and_namespace_syscalls_are_denied_unconditionally() {
        for nr in [
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_mount_setattr,
            libc::SYS_move_mount,
            libc::SYS_open_tree,
            SYS_OPEN_TREE_ATTR,
            libc::SYS_fsopen,
            libc::SYS_fsconfig,
            libc::SYS_fsmount,
            libc::SYS_fspick,
            libc::SYS_pivot_root,
            libc::SYS_unshare,
            libc::SYS_setns,
        ] {
            assert!(DENY_UNCONDITIONALLY.contains(&nr), "{nr}");
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn compile_rules_prologue_matches_seccompiler_0_5_0() {
```

Replace (2 of 3):

```rust
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x32_denial_block_lands_exactly_after_the_prologue() {
        let program = build_network_deny_filter().expect("filter should compile");
        let inserted = &program[ARCH_PROLOGUE_LEN..ARCH_PROLOGUE_LEN + 3];

        assert_eq!(inserted[0].code, bpf_opcode::LD_W_ABS);
```

with:

```rust
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x32_denial_block_lands_exactly_after_the_prologue() {
        let program = build_deny_filter().expect("filter should compile");
        let inserted = &program[ARCH_PROLOGUE_LEN..ARCH_PROLOGUE_LEN + 3];

        assert_eq!(inserted[0].code, bpf_opcode::LD_W_ABS);
```

Replace (3 of 3):

```rust
    #[test]
    fn rules_after_the_x32_block_are_unchanged_and_still_reachable() {
        let plain = compile_rules().expect("filter should compile");
        let patched = build_network_deny_filter().expect("filter should compile");

        assert_eq!(patched.len(), plain.len() + 3);
        assert_eq!(
```

with:

```rust
    #[test]
    fn rules_after_the_x32_block_are_unchanged_and_still_reachable() {
        let plain = compile_rules().expect("filter should compile");
        let patched = build_deny_filter().expect("filter should compile");

        assert_eq!(patched.len(), plain.len() + 3);
        assert_eq!(
```

- [ ] **Step 2: Run the lint to make sure it fails**

Run: `bash target/linux-lint.sh`
Expected: FAIL: ``cannot find function `build_deny_filter` in this scope `` (and the same for `build_clone3_filter` and `SYS_OPEN_TREE_ATTR`).

- [ ] **Step 3: Extend the filter**

`crates/harness-sandbox/src/linux/seccomp.rs` (Step 1 already changed its unit tests):

Replace (1 of 8):

```rust
//! Compiles the seccomp-BPF program that denies network access, in the
//! parent process.
//!
//! Like [`super::fs::build_ruleset_fd`], this runs entirely before `fork()`.
//! `seccompiler::SeccompFilter::try_into::<BpfProgram>()` allocates the
```

with:

```rust
//! Compiles the seccomp-BPF programs that deny network access, mount
//! changes and new namespaces, in the parent process.
//!
//! Like [`super::fs::build_ruleset_fd`], this runs entirely before `fork()`.
//! `seccompiler::SeccompFilter::try_into::<BpfProgram>()` allocates the
```

Replace (2 of 8):

```rust
//! cheap to check and the consequence of skipping it (installing a filter
//! whose x32 block landed in the wrong place, or not at all) is silent and
//! severe.

use std::collections::BTreeMap;
use std::io;
```

with:

```rust
//! cheap to check and the consequence of skipping it (installing a filter
//! whose x32 block landed in the wrong place, or not at all) is silent and
//! severe.
//!
//! ## Mounts and namespaces
//!
//! The full tier's read-only binds (`mountns.rs`) are set up before this
//! filter is installed; afterwards the filter refuses every call that changes
//! mounts (`mount`, `umount2`, `mount_setattr`, `move_mount`, `open_tree`,
//! `open_tree_attr`, `fsopen`, `fsconfig`, `fsmount`, `fspick`, `pivot_root`)
//! or enters a namespace (`unshare`, `setns`, and `clone` with any
//! `CLONE_NEW*` flag), in both tiers. The sandboxed command has no
//! capability over the mount namespace anyway, and Landlock refuses mount
//! changes too; this closes the door independently of both. `clone3` passes
//! its flags in memory a filter cannot read, so a second, one-rule program
//! ([`build_clone3_filter`]) makes it fail with `ENOSYS`, which glibc and
//! other runtimes answer by falling back to `clone`.

use std::collections::BTreeMap;
use std::io;
```

Replace (3 of 8):

```rust
    SeccompFilter, SeccompRule, TargetArch,
};

/// Syscalls denied unconditionally (regardless of arguments): the rest of
/// the socket lifecycle beyond creation, plus `io_uring`, which can perform
/// network I/O (including creating `AF_VSOCK`/`AF_INET` sockets under the
/// hood on newer kernels) without ever calling `socket(2)` itself.
const DENY_UNCONDITIONALLY: &[i64] = &[
    libc::SYS_connect,
    libc::SYS_bind,
```

with:

```rust
    SeccompFilter, SeccompRule, TargetArch,
};

/// `open_tree_attr` (Linux 6.15), which `libc` does not name yet. New
/// syscalls share one number on every architecture.
const SYS_OPEN_TREE_ATTR: i64 = 467;

/// Syscalls denied unconditionally (regardless of arguments): the rest of
/// the socket lifecycle beyond creation; `io_uring`, which can perform
/// network I/O (including creating `AF_VSOCK`/`AF_INET` sockets under the
/// hood on newer kernels) without ever calling `socket(2)` itself; and every
/// call that changes mounts or enters a namespace (see the module docs).
const DENY_UNCONDITIONALLY: &[i64] = &[
    libc::SYS_connect,
    libc::SYS_bind,
```

Replace (4 of 8):

```rust
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
];

/// Builds the network-denying seccomp-BPF program.
///
/// Default action is `Allow` (every syscall not mentioned below runs
/// normally); the on-match action is `Errno(EPERM)`, so a denied call fails
```

with:

```rust
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_mount_setattr,
    libc::SYS_move_mount,
    libc::SYS_open_tree,
    SYS_OPEN_TREE_ATTR,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_pivot_root,
    libc::SYS_unshare,
    libc::SYS_setns,
];

/// The `clone` flags that create a namespace. (`CLONE_NEWTIME` is left out:
/// `clone` reads that bit as part of the exit signal; only `clone3` and
/// `unshare` accept it, and both are refused outright.)
const CLONE_NAMESPACE_FLAGS: [libc::c_int; 7] = [
    libc::CLONE_NEWNS,
    libc::CLONE_NEWCGROUP,
    libc::CLONE_NEWUTS,
    libc::CLONE_NEWIPC,
    libc::CLONE_NEWUSER,
    libc::CLONE_NEWPID,
    libc::CLONE_NEWNET,
];

/// Builds the main seccomp-BPF program: network, mounts and namespaces.
///
/// Default action is `Allow` (every syscall not mentioned below runs
/// normally); the on-match action is `Errno(EPERM)`, so a denied call fails
```

Replace (5 of 8):

```rust
/// `Err` (in the parent, before any child exists, so the spawn simply fails
/// rather than running unfiltered) if seccompiler's compiled output does
/// not have the exact shape [`deny_x32_syscalls`] depends on.
pub fn build_network_deny_filter() -> io::Result<BpfProgram> {
    #[allow(unused_mut)]
    let mut program = compile_rules().map_err(io::Error::other)?;
    #[cfg(target_arch = "x86_64")]
```

with:

```rust
/// `Err` (in the parent, before any child exists, so the spawn simply fails
/// rather than running unfiltered) if seccompiler's compiled output does
/// not have the exact shape [`deny_x32_syscalls`] depends on.
pub fn build_deny_filter() -> io::Result<BpfProgram> {
    #[allow(unused_mut)]
    let mut program = compile_rules().map_err(io::Error::other)?;
    #[cfg(target_arch = "x86_64")]
```

Replace (6 of 8):

```rust
    Ok(program)
}

/// The rule-based part of [`build_network_deny_filter`], without the x32
/// post-processing — factored out so the x32 unit tests below can compare
/// against the program as seccompiler itself compiled it.
fn compile_rules() -> Result<BpfProgram, Error> {
```

with:

```rust
    Ok(program)
}

/// The rule-based part of [`build_deny_filter`], without the x32
/// post-processing — factored out so the x32 unit tests below can compare
/// against the program as seccompiler itself compiled it.
fn compile_rules() -> Result<BpfProgram, Error> {
```

Replace (7 of 8):

```rust

    rules.insert(libc::SYS_socket, vec![not_af_unix(0)?]);
    rules.insert(libc::SYS_socketpair, vec![not_af_unix(0)?]);

    let filter = SeccompFilter::new(
        rules,
```

with:

```rust

    rules.insert(libc::SYS_socket, vec![not_af_unix(0)?]);
    rules.insert(libc::SYS_socketpair, vec![not_af_unix(0)?]);
    // A syscall's rules match when any one of them does: one rule per flag.
    let namespace_rules = CLONE_NAMESPACE_FLAGS
        .iter()
        .map(|&flag| has_flag(0, flag))
        .collect::<Result<Vec<_>, Error>>()?;
    rules.insert(libc::SYS_clone, namespace_rules);

    let filter = SeccompFilter::new(
        rules,
```

Replace (8 of 8):

```rust
    Ok(filter.try_into()?)
}

/// A rule matching "argument `arg_index` (the socket domain) is not
/// `AF_UNIX`", i.e. exactly the sockets we want to deny.
fn not_af_unix(arg_index: u8) -> Result<SeccompRule, Error> {
```

with:

```rust
    Ok(filter.try_into()?)
}

/// The program that makes `clone3` fail with `ENOSYS` (see the module docs).
/// Installed after [`build_deny_filter`]'s; everything else is allowed.
pub fn build_clone3_filter() -> io::Result<BpfProgram> {
    let rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::from([(libc::SYS_clone3, vec![])]);
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        target_arch(),
    )
    .map_err(io::Error::other)?;
    filter.try_into().map_err(io::Error::other)
}

/// A rule matching "argument `arg_index` has `flag` set".
fn has_flag(arg_index: u8, flag: libc::c_int) -> Result<SeccompRule, Error> {
    let flag = flag as u64;
    Ok(SeccompRule::new(vec![SeccompCondition::new(
        arg_index,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::MaskedEq(flag),
        flag,
    )?])?)
}

/// A rule matching "argument `arg_index` (the socket domain) is not
/// `AF_UNIX`", i.e. exactly the sockets we want to deny.
fn not_af_unix(arg_index: u8) -> Result<SeccompRule, Error> {
```

`crates/harness-sandbox/src/linux/detect.rs`:

Replace (1 of 3):

```rust

/// Whether this process can rely on the Linux sandbox backend: the Landlock
/// half requires kernel ABI >= 3, and the seccomp half requires the
/// network-deny filter to actually build (see [`super::seccomp::build_network_deny_filter`],
/// which includes its own prologue-shape check). The architecture check
/// that used to live here is now a compile-time gate instead: the whole
/// `linux` module (see `lib.rs`) only builds on `x86_64`/`aarch64`, the two
```

with:

```rust

/// Whether this process can rely on the Linux sandbox backend: the Landlock
/// half requires kernel ABI >= 3, and the seccomp half requires the
/// deny filters to actually build (see [`super::seccomp::build_deny_filter`],
/// which includes its own prologue-shape check). The architecture check
/// that used to live here is now a compile-time gate instead: the whole
/// `linux` module (see `lib.rs`) only builds on `x86_64`/`aarch64`, the two
```

Replace (2 of 3):

```rust
/// outright.
pub fn linux_sandbox_available() -> bool {
    landlock_abi().is_some_and(|abi| abi >= MIN_SUPPORTED_ABI)
        && super::seccomp::build_network_deny_filter().is_ok()
}

#[cfg(test)]
```

with:

```rust
/// outright.
pub fn linux_sandbox_available() -> bool {
    landlock_abi().is_some_and(|abi| abi >= MIN_SUPPORTED_ABI)
        && super::seccomp::build_deny_filter().is_ok()
        && super::seccomp::build_clone3_filter().is_ok()
}

#[cfg(test)]
```

Replace (3 of 3):

```rust
        // seccomp half should always succeed on x86_64/aarch64, the only
        // architectures this module compiles for at all.
        assert!(
            crate::linux::seccomp::build_network_deny_filter().is_ok(),
            "the seccomp filter should always build on this architecture"
        );
    }
```

with:

```rust
        // seccomp half should always succeed on x86_64/aarch64, the only
        // architectures this module compiles for at all.
        assert!(
            crate::linux::seccomp::build_deny_filter().is_ok(),
            "the seccomp filter should always build on this architecture"
        );
    }
```

`crates/harness-sandbox/src/linux/preexec.rs`:

Replace (1 of 2):

```rust
    /// errors in the parent, before the child is ever forked, rather than
    /// handing back a `PreparedSandbox` with nothing to restrict.
    pub(super) landlock_ruleset_fd: OwnedFd,
    /// The compiled network-deny seccomp-BPF program.
    pub(super) seccomp_program: BpfProgram,
}

/// Installs the sandbox in the calling process. Must only be invoked from a
```

with:

```rust
    /// errors in the parent, before the child is ever forked, rather than
    /// handing back a `PreparedSandbox` with nothing to restrict.
    pub(super) landlock_ruleset_fd: OwnedFd,
    /// The compiled seccomp-BPF programs: network, mounts and namespaces;
    /// then `clone3`.
    pub(super) seccomp_program: BpfProgram,
    pub(super) clone3_program: BpfProgram,
}

/// Installs the sandbox in the calling process. Must only be invoked from a
```

Replace (2 of 2):

```rust
    //    already rejected in the parent, before fork.
    landlock_restrict_self(prepared.landlock_ruleset_fd.as_raw_fd())?;

    // 5. Network restriction. Installed last so none of the syscalls above
    //    can themselves be filtered.
    seccompiler::apply_filter(&prepared.seccomp_program).map_err(seccomp_apply_error)?;

    Ok(())
}
```

with:

```rust
    //    already rejected in the parent, before fork.
    landlock_restrict_self(prepared.landlock_ruleset_fd.as_raw_fd())?;

    // 5. Network, mount and namespace restriction. Installed last so none of
    //    the syscalls above can themselves be filtered.
    seccompiler::apply_filter(&prepared.seccomp_program).map_err(seccomp_apply_error)?;
    seccompiler::apply_filter(&prepared.clone3_program).map_err(seccomp_apply_error)?;

    Ok(())
}
```

`crates/harness-sandbox/src/linux/mod.rs`:

Replace (1 of 5):

```rust
//! Linux sandbox backend: a Landlock ruleset (filesystem) plus a
//! seccomp-BPF program (network) installed from `pre_exec`, in the
//! forked child, before `execve`.
//!
//! ## Split between parent and child
```

with:

```rust
//! Linux sandbox backend: a Landlock ruleset (filesystem) plus seccomp-BPF
//! programs (network, mounts, namespaces) installed from `pre_exec`, in the
//! forked child, before `execve`.
//!
//! ## Split between parent and child
```

Replace (2 of 5):

```rust
//! Everything that can allocate, open files, or otherwise take locks (the
//! Landlock ruleset with its `path_beneath` rules, the compiled seccomp-BPF
//! program) is built in the **parent**, by [`fs::build_ruleset_fd`] and
//! [`seccomp::build_network_deny_filter`]. [`linux_sandbox_command`] hands
//! the results to [`preexec::apply`], which is the only code that runs in
//! the forked child's `pre_exec` closure. That function is restricted to
//! async-signal-safe operations: raw syscalls (`setsid`, `prctl`,
```

with:

```rust
//! Everything that can allocate, open files, or otherwise take locks (the
//! Landlock ruleset with its `path_beneath` rules, the compiled seccomp-BPF
//! program) is built in the **parent**, by [`fs::build_ruleset_fd`] and
//! [`seccomp::build_deny_filter`]. [`linux_sandbox_command`] hands
//! the results to [`preexec::apply`], which is the only code that runs in
//! the forked child's `pre_exec` closure. That function is restricted to
//! async-signal-safe operations: raw syscalls (`setsid`, `prctl`,
```

Replace (3 of 5):

```rust
//!    install a filter, and applied before Landlock too so nothing between
//!    here and `execve` could regain privileges via a setuid/setgid binary.
//! 4. `landlock_restrict_self` on the ruleset fd built in the parent.
//! 5. Install the seccomp-BPF program. Last, so none of the syscalls above
//!    can be filtered by it.

mod detect;
mod fdcleanup;
```

with:

```rust
//!    install a filter, and applied before Landlock too so nothing between
//!    here and `execve` could regain privileges via a setuid/setgid binary.
//! 4. `landlock_restrict_self` on the ruleset fd built in the parent.
//! 5. Install the seccomp-BPF programs. Last, so none of the syscalls above
//!    can be filtered by them.

mod detect;
mod fdcleanup;
```

Replace (4 of 5):

```rust
/// Builds a [`tokio::process::Command`] for `program`/`args` with `policy`'s
/// Landlock + seccomp sandbox installed via `pre_exec`.
///
/// The Landlock ruleset and the seccomp-BPF program are both compiled here,
/// in the caller's process, before the child ever exists; `pre_exec` only
/// has to hand already-prepared data to the kernel. See the [`linux`
/// module docs](self) for the full ordering rationale.
```

with:

```rust
/// Builds a [`tokio::process::Command`] for `program`/`args` with `policy`'s
/// Landlock + seccomp sandbox installed via `pre_exec`.
///
/// The Landlock ruleset and the seccomp-BPF programs are all compiled here,
/// in the caller's process, before the child ever exists; `pre_exec` only
/// has to hand already-prepared data to the kernel. See the [`linux`
/// module docs](self) for the full ordering rationale.
```

Replace (5 of 5):

```rust
    args: &[&str],
) -> io::Result<Command> {
    let landlock_ruleset_fd = fs::build_ruleset_fd(policy)?;
    let seccomp_program = seccomp::build_network_deny_filter()?;

    let prepared = PreparedSandbox {
        landlock_ruleset_fd,
        seccomp_program,
    };

    let mut command = Command::new(program);
```

with:

```rust
    args: &[&str],
) -> io::Result<Command> {
    let landlock_ruleset_fd = fs::build_ruleset_fd(policy)?;
    let seccomp_program = seccomp::build_deny_filter()?;
    let clone3_program = seccomp::build_clone3_filter()?;

    let prepared = PreparedSandbox {
        landlock_ruleset_fd,
        seccomp_program,
        clone3_program,
    };

    let mut command = Command::new(program);
```

- [ ] **Step 4: Lint and run what runs here**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && bash target/linux-lint.sh && cargo test -p harness-sandbox`
Expected: clean, and the macOS tests pass. On Linux (CI), `mount_and_namespace_syscalls_fail_with_eperm`, `clone_with_a_namespace_flag_fails_with_eperm`, `clone3_fails_with_enosys_and_threads_still_work` and `child_proc_status_reports_both_seccomp_filters` pass, and every existing Linux test still does.

- [ ] **Step 5: Commit**

```bash
git add crates/harness-sandbox
git commit -F - <<'EOF'
feat(sandbox): refuse mount and namespace syscalls on Linux

The seccomp filter now also refuses every mount-changing call, unshare,
setns and clone with a namespace flag, with EPERM. clone3 hides its
flags from the filter, so a second program makes it fail with ENOSYS,
and runtimes fall back to clone.

<trailer lines from the controller>
EOF
```

---

### Task 6: Guard every Linux command (the basic tier)

**Files:**
- Create: `crates/harness-sandbox/tests/linux_git_guard.rs`
- Modify: `crates/harness-sandbox/src/lib.rs`, `crates/harness-sandbox/src/linux/mod.rs`, `crates/harness-sandbox/tests/seatbelt.rs`, `crates/harness-cli/src/ask.rs`

**Interfaces:**
- Consumes: `guard::{GuardSession, GitGuard}` (Task 4); `CommandGuard`, `SandboxedCommand`, `GitProtection` (Task 3).
- Produces:
  - `SandboxSettings.quarantine_dir: Option<PathBuf>` (`None`: `harness-quarantine` in the temp directory); the CLI passes `<data dir>/quarantine`.
  - `LinuxSandbox::with_git_protection(settings: SandboxSettings, tier: GitProtection) -> LinuxSandbox`; `LinuxSandbox::new(settings)` reports the basic tier until Task 8 adds the probe.
  - `LinuxSandbox::prepare`: for workspace-write access, begins the guard with `save_all = true` and returns it; for read-only access, no guard. `LinuxSandbox::command` stays the plain Landlock + seccomp command.

- [ ] **Step 1: Write the failing test**

`crates/harness-sandbox/tests/linux_git_guard.rs`:

```rust
//! Linux git-metadata protection end to end: `LinuxSandbox::prepare` runs real commands with
//! the guard, in the basic tier.
//!
//! Like `linux_sandbox.rs`, a test skips when the sandbox or a tool it needs is missing, unless
//! `HARNESS_REQUIRE_LINUX_SANDBOX=1`.

#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use harness_core::tool::{CommandSandbox, GitProtection, GuardReport};
use harness_sandbox::{FsAccess, LinuxSandbox, SandboxSettings, linux_sandbox_available};

fn skip_or_require(reason: &str) -> bool {
    assert!(
        std::env::var("HARNESS_REQUIRE_LINUX_SANDBOX").as_deref() != Ok("1"),
        "{reason} (HARNESS_REQUIRE_LINUX_SANDBOX=1 is set)"
    );
    eprintln!("skipping: {reason}");
    true
}

/// A git repository under `$HOME` (not `/tmp`, which the sandbox always makes writable), with a
/// commit identity, and a quarantine directory next to it. Both are removed on drop.
struct Env {
    ws: PathBuf,
    quarantine: PathBuf,
}

impl Env {
    fn new() -> Option<Env> {
        if !linux_sandbox_available() {
            skip_or_require("linux sandbox unavailable");
            return None;
        }
        let Some(home) = std::env::var_os("HOME") else {
            skip_or_require("$HOME not set");
            return None;
        };
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = format!(
            "{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let ws = PathBuf::from(&home).join(format!(".harness-guard-test-ws-{id}"));
        let quarantine = PathBuf::from(&home).join(format!(".harness-guard-test-q-{id}"));
        std::fs::create_dir(&ws).unwrap();
        let ws = ws.canonicalize().unwrap();
        let env = Env { ws, quarantine };
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "t@example.com"],
            &["config", "user.name", "original"],
            &["config", "commit.gpgsign", "false"],
        ] {
            if !env.git(args).status.success() {
                skip_or_require("git is not installed");
                return None;
            }
        }
        Some(env)
    }

    fn git(&self, args: &[&str]) -> Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&self.ws)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    fn settings(&self) -> SandboxSettings {
        SandboxSettings {
            quarantine_dir: Some(self.quarantine.clone()),
            ..SandboxSettings::default()
        }
    }

    fn basic(&self) -> LinuxSandbox {
        LinuxSandbox::with_git_protection(
            self.settings(),
            GitProtection::Basic {
                reason: "forced by the test".into(),
            },
        )
    }

    /// The one entry the quarantine holds at `rel` (relative to the workspace).
    fn quarantined(&self, rel: &str) -> PathBuf {
        let found: Vec<PathBuf> = std::fs::read_dir(&self.quarantine)
            .unwrap_or_else(|e| panic!("no quarantine at {:?}: {e}", self.quarantine))
            .map(|e| e.unwrap().path().join(rel))
            .filter(|p| std::fs::symlink_metadata(p).is_ok())
            .collect();
        assert_eq!(found.len(), 1, "{rel} in quarantine: {found:?}");
        found.into_iter().next().unwrap()
    }

    fn exists(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.ws.join(rel)).is_ok()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.ws);
        let _ = std::fs::remove_dir_all(&self.quarantine);
    }
}

/// Runs `script` with `/bin/sh -c` through `sandbox.prepare`, in `cwd`, then finishes the guard.
async fn run_in(
    sandbox: &LinuxSandbox,
    ws: &Path,
    cwd: &Path,
    script: &str,
) -> (std::io::Result<Output>, Option<GuardReport>) {
    let prepared = sandbox
        .prepare(FsAccess::WorkspaceWrite, ws, "/bin/sh", &["-c", script])
        .expect("prepare the sandboxed command");
    let mut cmd = prepared.command;
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().await;
    let report = prepared.guard.expect("a workspace-write guard").finish();
    (output, report)
}

async fn run(sandbox: &LinuxSandbox, env: &Env, script: &str) -> (Output, Option<GuardReport>) {
    let (output, report) = run_in(sandbox, &env.ws, &env.ws, script).await;
    (output.expect("spawn the sandboxed command"), report)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ---------------------------------------------------------------------------
// The basic tier: undone after the fact
// ---------------------------------------------------------------------------

#[tokio::test]
async fn basic_tier_quarantines_a_planted_hook() {
    let Some(env) = Env::new() else { return };
    let (output, report) = run(
        &env.basic(),
        &env,
        "echo 'echo pwned' > .git/hooks/pre-commit",
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let report = report.expect("a report");
    assert!(report.blocked);
    assert!(
        report.message.contains("- .git/hooks/pre-commit: "),
        "{}",
        report.message
    );
    assert!(!env.exists(".git/hooks/pre-commit"));
    assert_eq!(
        std::fs::read_to_string(env.quarantined(".git/hooks/pre-commit")).unwrap(),
        "echo pwned\n"
    );
}

#[tokio::test]
async fn basic_tier_restores_a_changed_config() {
    let Some(env) = Env::new() else { return };
    let before = std::fs::read(env.ws.join(".git/config")).unwrap();
    let (output, report) = run(&env.basic(), &env, "git config user.name evil").await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(report.expect("a report").blocked);
    assert_eq!(std::fs::read(env.ws.join(".git/config")).unwrap(), before);
    let changed = std::fs::read_to_string(env.quarantined(".git/config")).unwrap();
    assert!(changed.contains("evil"), "{changed}");
}

#[tokio::test]
async fn basic_tier_quarantines_a_new_commondir() {
    let Some(env) = Env::new() else { return };
    let (_, report) = run(&env.basic(), &env, "printf /tmp/elsewhere > .git/commondir").await;
    assert!(report.expect("a report").blocked);
    assert!(!env.exists(".git/commondir"));
    env.quarantined(".git/commondir");
}

#[tokio::test]
async fn git_init_of_a_nested_repository_is_undone() {
    let Some(env) = Env::new() else { return };
    let (output, report) = run(
        &env.basic(),
        &env,
        "git init -q sub && echo kept > sub/file",
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let report = report.expect("a report");
    assert!(
        report
            .message
            .contains("- sub/.git: a new repository; moved to "),
        "{}",
        report.message
    );
    assert!(!env.exists("sub/.git"));
    assert!(env.exists("sub/file"));
    assert!(env.quarantined("sub/.git").join("HEAD").exists());
}

#[tokio::test]
async fn basic_tier_commit_checkout_and_stash_get_no_report() {
    let Some(env) = Env::new() else { return };
    let script = "set -e; git commit -q --allow-empty -m one; git checkout -q -b topic; \
                  echo x > f; git add f; git stash -q; git stash pop -q";
    let (output, report) = run(&env.basic(), &env, script).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
}

#[tokio::test]
async fn names_a_background_process_plants_later_are_caught_before_the_next_command() {
    let Some(env) = Env::new() else { return };
    let sandbox = env.basic();
    let (_, report) = run(
        &sandbox,
        &env,
        "(sleep 1; printf /tmp/elsewhere > .git/commondir) > /dev/null 2>&1 &",
    )
    .await;
    assert_eq!(report, None);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        env.exists(".git/commondir"),
        "the background process planted it"
    );
    let (_, report) = run(&sandbox, &env, "true").await;
    let report = report.expect("a report");
    assert!(!report.blocked);
    assert!(
        report.message.starts_with("[before this command ran"),
        "{}",
        report.message
    );
    assert!(!env.exists(".git/commondir"));
}
```

- [ ] **Step 2: Run the lint to make sure it fails**

Run: `bash target/linux-lint.sh`
Expected: FAIL: ``struct `harness_sandbox::SandboxSettings` has no field named `quarantine_dir` `` and ``no associated function or constant named `with_git_protection` found for struct `harness_sandbox::LinuxSandbox` ``.

- [ ] **Step 3: Add the setting**

`crates/harness-sandbox/src/lib.rs`:

Replace:

```rust
    pub extra_writable: Vec<PathBuf>,
    /// Allow loopback networking (`sandbox.allow_localhost`; macOS only).
    pub allow_localhost: bool,
}

impl SandboxSettings {
```

with:

```rust
    pub extra_writable: Vec<PathBuf>,
    /// Allow loopback networking (`sandbox.allow_localhost`; macOS only).
    pub allow_localhost: bool,
    /// Where the Linux git-metadata guard moves what it takes out of the workspace. The CLI
    /// passes `<data dir>/quarantine`; `None` means `harness-quarantine` in the temp directory.
    pub quarantine_dir: Option<PathBuf>,
}

impl SandboxSettings {
```

`crates/harness-sandbox/tests/seatbelt.rs`:

Replace:

```rust
    let settings = SandboxSettings {
        extra_writable: vec![extra.clone()],
        allow_localhost: false,
    };

    let (code, out) = sh_in(
```

with:

```rust
    let settings = SandboxSettings {
        extra_writable: vec![extra.clone()],
        allow_localhost: false,
        ..SandboxSettings::default()
    };

    let (code, out) = sh_in(
```

`crates/harness-cli/src/ask.rs`:

Replace:

```rust
        harness_sandbox::detect(harness_sandbox::SandboxSettings {
            extra_writable: setup.config.writable_roots.clone(),
            allow_localhost: setup.config.allow_localhost,
        })
    };
    let sandboxed = sandbox.is_some();
```

with:

```rust
        harness_sandbox::detect(harness_sandbox::SandboxSettings {
            extra_writable: setup.config.writable_roots.clone(),
            allow_localhost: setup.config.allow_localhost,
            quarantine_dir: Some(setup.paths.data_dir.join("quarantine")),
        })
    };
    let sandboxed = sandbox.is_some();
```

- [ ] **Step 4: Start the guard in LinuxSandbox**

Replace `crates/harness-sandbox/src/linux/mod.rs` with:

```rust
//! Linux sandbox backend: a Landlock ruleset (filesystem) plus seccomp-BPF
//! programs (network, mounts, namespaces) installed from `pre_exec`, in the
//! forked child, before `execve`; and around every workspace-write
//! command, the git-metadata guard (`crate::guard`).
//!
//! ## Split between parent and child
//!
//! Everything that can allocate, open files, or otherwise take locks (the
//! Landlock ruleset with its `path_beneath` rules, the compiled seccomp-BPF
//! program) is built in the **parent**, by [`fs::build_ruleset_fd`] and
//! [`seccomp::build_deny_filter`]. [`linux_sandbox_command`] hands
//! the results to [`preexec::apply`], which is the only code that runs in
//! the forked child's `pre_exec` closure. That function is restricted to
//! async-signal-safe operations: raw syscalls (`setsid`, `prctl`,
//! `landlock_restrict_self`, `seccomp`) and reads of the already-prepared
//! data. See `preexec.rs` for the full rationale.
//!
//! ## Ordering inside `pre_exec`
//!
//! 1. `setsid()` — the child becomes its own session/process-group leader,
//!    so `killpg(child_pid)` reaches grandchildren too. We deliberately do
//!    *not* also call `setpgid(0, 0)`: once `setsid()` has run, the process
//!    is already its own group leader and a subsequent `setpgid` targeting
//!    it fails with `EPERM`.
//! 2. Mark every inherited fd above stderr close-on-exec, so a writable or
//!    connectable fd cannot leak into the sandboxed program through
//!    inheritance. This runs before Landlock is restricted because it needs
//!    to open `/proc/self/fd`.
//! 3. `prctl(PR_SET_NO_NEW_PRIVS)` — required before `seccomp(2)` will
//!    install a filter, and applied before Landlock too so nothing between
//!    here and `execve` could regain privileges via a setuid/setgid binary.
//! 4. `landlock_restrict_self` on the ruleset fd built in the parent.
//! 5. Install the seccomp-BPF programs. Last, so none of the syscalls above
//!    can be filtered by them.

mod detect;
mod fdcleanup;
mod fs;
mod preexec;
mod seccomp;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use harness_core::tool::{
    CommandGuard, CommandSandbox, GitProtection, GuardReport, SandboxedCommand,
};
use tokio::process::Command;

use crate::guard::{GitGuard, GuardSession};
use crate::{FsAccess, SandboxPolicy, SandboxSettings};
use preexec::PreparedSandbox;

pub use detect::{landlock_abi, linux_sandbox_available};

/// Builds a [`tokio::process::Command`] for `program`/`args` with `policy`'s
/// Landlock + seccomp sandbox installed via `pre_exec`.
///
/// The Landlock ruleset and the seccomp-BPF programs are all compiled here,
/// in the caller's process, before the child ever exists; `pre_exec` only
/// has to hand already-prepared data to the kernel. See the [`linux`
/// module docs](self) for the full ordering rationale.
///
/// Returns `Err` for setup failures in *this* process (e.g. a seccomp rule
/// that failed to validate) **and** whenever the running kernel cannot
/// fully enforce the Landlock ABI-3 floor this crate requires, including a
/// kernel with no Landlock support at all — see [`fs::build_ruleset_fd`].
/// This function never hands back a command that merely *looks* sandboxed;
/// call [`linux_sandbox_available`] first if the caller wants to know
/// ahead of time whether that floor is met.
pub fn linux_sandbox_command(
    policy: &SandboxPolicy,
    program: &str,
    args: &[&str],
) -> io::Result<Command> {
    let landlock_ruleset_fd = fs::build_ruleset_fd(policy)?;
    let seccomp_program = seccomp::build_deny_filter()?;
    let clone3_program = seccomp::build_clone3_filter()?;

    let prepared = PreparedSandbox {
        landlock_ruleset_fd,
        seccomp_program,
        clone3_program,
    };

    let mut command = Command::new(program);
    command.args(args);

    // A rejected `TMPDIR` (see `fs::tmpdir_override`) is excluded from the
    // Landlock ruleset above, but the child process would otherwise still
    // see the original, now-unwritable value in its environment; override
    // it to `/tmp`, which the ruleset always makes writable, so `mktemp`
    // and friends keep working inside the sandbox.
    if let Some(tmpdir) = fs::tmpdir_override(
        policy.access,
        std::env::var_os("TMPDIR").as_deref(),
        crate::roots::home_dir().as_deref(),
    ) {
        command.env("TMPDIR", tmpdir);
    }

    // SAFETY: `preexec::apply` performs only the async-signal-safe
    // operations documented on it (raw syscalls plus reads of `prepared`,
    // which was fully built above, in the parent, before this closure was
    // constructed). `prepared` is moved into the closure and so stays alive
    // — keeping the Landlock ruleset fd open — for as long as `command`
    // does, which is at least until `fork()` happens inside `spawn()`.
    unsafe {
        command.pre_exec(move || preexec::apply(&prepared));
    }

    Ok(command)
}

/// [`CommandSandbox`] backed by Landlock + seccomp, with the git-metadata
/// guard around every workspace-write command.
#[derive(Debug)]
pub struct LinuxSandbox {
    settings: SandboxSettings,
    guards: Arc<GuardSession>,
    tier: GitProtection,
}

impl LinuxSandbox {
    /// A sandbox in the basic tier: this build has no read-only mounts yet.
    pub fn new(settings: SandboxSettings) -> Self {
        Self::with_git_protection(
            settings,
            GitProtection::Basic {
                reason: "this build of harness has no read-only mounts over git metadata".into(),
            },
        )
    }

    /// A sandbox that reports `tier` from [`CommandSandbox::git_protection`].
    pub fn with_git_protection(settings: SandboxSettings, tier: GitProtection) -> Self {
        let quarantine = settings
            .quarantine_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("harness-quarantine"));
        LinuxSandbox {
            guards: GuardSession::new(&quarantine),
            settings,
            tier,
        }
    }
}

impl CommandSandbox for LinuxSandbox {
    fn name(&self) -> &'static str {
        "landlock+seccomp"
    }

    /// Callers must not call [`tokio::process::Command::process_group`] on
    /// the returned command: `pre_exec` calls `setsid()` (see the [`linux`
    /// module docs](self)), which fails with `EPERM` if something has
    /// already changed this process's process-group membership before it
    /// runs. `setsid()` alone already makes the child lead its own process
    /// group, which is what `process_group(0)` would otherwise be for.
    ///
    /// This is the command without the guard: use
    /// [`prepare`](CommandSandbox::prepare) for that.
    fn command(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> io::Result<Command> {
        linux_sandbox_command(&self.settings.policy(access, workspace), program, args)
    }

    fn is_denial(&self, exit_code: Option<i32>, output: &str) -> bool {
        if crate::looks_like_sandbox_denial(exit_code, output, true) {
            return true;
        }
        if exit_code == Some(0) {
            return false;
        }
        // Landlock's `Refer` right denies a rename/link that would cross a
        // rule boundary with `EXDEV`, which the kernel reports through this
        // exact message (glibc's `strerror(EXDEV)` on Linux) — a denial
        // signal `looks_like_sandbox_denial`'s shared keyword list does not
        // cover, since it is Linux-specific wording for a Linux-specific
        // Landlock behavior.
        output.to_lowercase().contains("invalid cross-device link")
    }

    /// Starts the guard for a workspace-write command, saving every
    /// protected file so it can be restored, then builds the command.
    fn prepare(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> io::Result<SandboxedCommand> {
        if access == FsAccess::ReadOnly {
            return Ok(SandboxedCommand {
                command: self.command(access, workspace, program, args)?,
                guard: None,
            });
        }
        let workspace = canonical(workspace);
        let guard = self.guards.begin(&workspace, true, |_| {});
        let command = match self.command(access, &workspace, program, args) {
            Ok(command) => command,
            Err(e) => {
                // Nothing ran; finishing still records where things stand
                // for the next command's check.
                let _ = guard.finish();
                return Err(e);
            }
        };
        Ok(SandboxedCommand {
            command,
            guard: Some(Box::new(LinuxGuard { guard })),
        })
    }

    fn git_protection(&self) -> GitProtection {
        self.tier.clone()
    }
}

fn canonical(workspace: &Path) -> PathBuf {
    std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
}

/// The guard for one command.
struct LinuxGuard {
    guard: GitGuard,
}

impl CommandGuard for LinuxGuard {
    fn finish(self: Box<Self>) -> Option<GuardReport> {
        self.guard.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::tool::CommandSandbox;

    fn sandbox() -> LinuxSandbox {
        LinuxSandbox::new(crate::SandboxSettings::default())
    }

    #[test]
    fn cross_device_link_message_is_a_denial() {
        assert!(sandbox().is_denial(
            Some(1),
            "mv: cannot move 'a' to 'b': Invalid cross-device link\n"
        ));
    }

    #[test]
    fn success_is_never_a_denial_even_with_the_keyword() {
        assert!(!sandbox().is_denial(Some(0), "Invalid cross-device link\n"));
    }

    #[test]
    fn plain_failure_without_any_keyword_is_not_a_denial() {
        assert!(!sandbox().is_denial(Some(1), "some ordinary error\n"));
    }
}
```

- [ ] **Step 5: Lint and run what runs here**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && bash target/linux-lint.sh && cargo test -p harness-sandbox && cargo test -p harness-cli`
Expected: clean and passing. On Linux (CI) the six tests in `linux_git_guard` pass.

- [ ] **Step 6: Commit**

```bash
git add crates/harness-sandbox crates/harness-cli/src/ask.rs
git commit -F - <<'EOF'
feat(sandbox): guard git metadata around every Linux command

LinuxSandbox::prepare starts the guard for workspace-write commands and
saves every protected file, so after the command planted hooks, new
commondir files and new repositories are moved to the quarantine and
changed config is restored. The CLI puts the quarantine in the data
directory.

<trailer lines from the controller>
EOF
```

---

### Task 7: Watch git metadata while a Linux command runs

**Files:**
- Create: `crates/harness-sandbox/src/linux/watch.rs`
- Modify: `crates/harness-sandbox/src/linux/mod.rs`, `crates/harness-sandbox/tests/linux_git_guard.rs`

**Interfaces:**
- Consumes: `guard::WatchHandle` (Task 4); `LinuxSandbox::prepare` (Task 6).
- Produces (Linux, crate-internal): `watch::Target` (implemented for `WatchHandle`), `watch::Watcher::start(target: impl Target) -> io::Result<Watcher>`, `Watcher::stop(self)` (joins the thread; also on drop), `watch::events(buf: &[u8]) -> Vec<Event>`. `LinuxGuard` stops the watcher before the guard's final check. A watcher that cannot start is skipped: the final check still runs.

- [ ] **Step 1: Write the failing test**

In `crates/harness-sandbox/tests/linux_git_guard.rs`:

Replace (1 of 3):

```rust
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use harness_core::tool::{CommandSandbox, GitProtection, GuardReport};
use harness_sandbox::{FsAccess, LinuxSandbox, SandboxSettings, linux_sandbox_available};
```

with:

```rust
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use harness_core::tool::{CommandSandbox, GitProtection, GuardReport};
use harness_sandbox::{FsAccess, LinuxSandbox, SandboxSettings, linux_sandbox_available};
```

Replace (2 of 3):

```rust
        found.into_iter().next().unwrap()
    }

    fn exists(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.ws.join(rel)).is_ok()
    }
```

with:

```rust
        found.into_iter().next().unwrap()
    }

    /// Whether the quarantine holds anything at `rel` yet.
    fn in_quarantine(&self, rel: &str) -> bool {
        std::fs::read_dir(&self.quarantine).is_ok_and(|dirs| {
            dirs.filter_map(Result::ok)
                .any(|d| std::fs::symlink_metadata(d.path().join(rel)).is_ok())
        })
    }

    /// Waits up to four seconds for the watcher to quarantine `rel`.
    async fn wait_for_quarantine(&self, rel: &str) {
        let deadline = Instant::now() + Duration::from_secs(4);
        while !self.in_quarantine(rel) {
            assert!(
                Instant::now() < deadline,
                "{rel} was not moved while the command ran"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn exists(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.ws.join(rel)).is_ok()
    }
```

Replace (3 of 3):

```rust
    assert_eq!(report, None);
}

#[tokio::test]
async fn names_a_background_process_plants_later_are_caught_before_the_next_command() {
    let Some(env) = Env::new() else { return };
```

with:

```rust
    assert_eq!(report, None);
}

#[tokio::test]
async fn the_watcher_quarantines_a_hook_while_the_command_runs() {
    let Some(env) = Env::new() else { return };
    let sandbox = env.basic();
    let prepared = sandbox
        .prepare(
            FsAccess::WorkspaceWrite,
            &env.ws,
            "/bin/sh",
            &[
                "-c",
                "echo 'echo pwned' > .git/hooks/post-checkout; sleep 5",
            ],
        )
        .unwrap();
    let mut cmd = prepared.command;
    cmd.current_dir(&env.ws)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    env.wait_for_quarantine(".git/hooks/post-checkout").await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "the command should still be running"
    );
    assert!(!env.exists(".git/hooks/post-checkout"));
    let _ = child.kill().await;
    let report = prepared.guard.unwrap().finish().expect("a report");
    assert!(report.blocked);
}

#[tokio::test]
async fn names_a_background_process_plants_later_are_caught_before_the_next_command() {
    let Some(env) = Env::new() else { return };
```

The new test compiles against Task 6's API, so the lint passes; on Linux it fails until the watcher exists, because the hook stays until the command ends.

- [ ] **Step 2: Add the watcher**

`crates/harness-sandbox/src/linux/watch.rs`:

```rust
//! Runs the git-metadata guard's checks while a command runs, from an
//! inotify watcher on a thread in the harness process.
//!
//! Raw `inotify(7)` through `libc`, which this crate already uses for its
//! other syscalls: a watch per directory, one non-blocking fd, and a pipe to
//! stop the thread. It watches each known gitdir, the `worktrees` and
//! `modules` directories in it, the workspace root, and in the basic tier the
//! directories inside protected entries. Recursive watches of the whole
//! workspace would cost as much as the walk the guard does after the command,
//! so new nested repositories are only found then.
//!
//! The watcher only shortens the time a planted file exists; the guard's
//! final check after the command is what the protection rests on. So a
//! directory that cannot be watched (the per-user watch limit, say) is
//! skipped, and a lost event (`IN_Q_OVERFLOW`) just runs the checks.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use crate::guard::WatchHandle;

const MASK: u32 = libc::IN_CREATE
    | libc::IN_MOVED_TO
    | libc::IN_MOVED_FROM
    | libc::IN_DELETE
    | libc::IN_CLOSE_WRITE
    | libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR
    | libc::IN_DONT_FOLLOW;

/// What the watcher needs from the guard: [`WatchHandle`], or a test double.
pub(super) trait Target: Send + 'static {
    fn dirs(&self) -> Vec<PathBuf>;
    fn relevant(&self, dir: &Path, name: Option<&OsStr>) -> bool;
    fn check(&self);
}

impl Target for WatchHandle {
    fn dirs(&self) -> Vec<PathBuf> {
        WatchHandle::dirs(self)
    }

    fn relevant(&self, dir: &Path, name: Option<&OsStr>) -> bool {
        WatchHandle::relevant(self, dir, name)
    }

    fn check(&self) {
        WatchHandle::check(self);
    }
}

/// A running watcher thread. Stopped by [`stop`](Self::stop) or on drop.
pub(super) struct Watcher {
    stop: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    pub(super) fn start(target: impl Target) -> io::Result<Watcher> {
        // SAFETY: takes only flags; returns a new fd or -1.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just created and nothing else owns it.
        let inotify = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut dirs = HashMap::new();
        for dir in target.dirs() {
            let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
                continue;
            };
            // SAFETY: `path` is NUL-terminated and outlives the call.
            let wd = unsafe { libc::inotify_add_watch(inotify.as_raw_fd(), path.as_ptr(), MASK) };
            if wd >= 0 {
                dirs.insert(wd, dir);
            }
        }
        let (stop_reader, stop) = pipe()?;
        let thread = std::thread::Builder::new()
            .name("harness-git-guard".into())
            .spawn(move || watch(&inotify, &stop_reader, &dirs, &target))?;
        Ok(Watcher {
            stop,
            thread: Some(thread),
        })
    }

    /// Stops the thread and waits for it, so no check runs after this.
    pub(super) fn stop(mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        // SAFETY: writes one byte from a stack array to our own pipe.
        unsafe { libc::write(self.stop.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
        let _ = thread.join();
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shut_down();
    }
}

fn watch(inotify: &OwnedFd, stop: &OwnedFd, dirs: &HashMap<i32, PathBuf>, target: &impl Target) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: inotify.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two `pollfd`s.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if fds[1].revents != 0 {
            return;
        }
        // SAFETY: reads at most `buf.len()` bytes into `buf`.
        let n = unsafe { libc::read(inotify.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        let Ok(n) = usize::try_from(n) else {
            continue;
        };
        let relevant = events(&buf[..n]).into_iter().any(|event| {
            event.mask & libc::IN_Q_OVERFLOW != 0
                || dirs
                    .get(&event.wd)
                    .is_some_and(|dir| target.relevant(dir, event.name))
        });
        if relevant {
            target.check();
        }
    }
}

/// One `struct inotify_event` and its name.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Event<'a> {
    pub(super) wd: i32,
    pub(super) mask: u32,
    pub(super) name: Option<&'a OsStr>,
}

/// The events in `buf`, as `read(2)` on an inotify fd returned them. A
/// truncated last record is ignored.
pub(super) fn events(buf: &[u8]) -> Vec<Event<'_>> {
    const HEADER: usize = std::mem::size_of::<libc::inotify_event>();
    let mut events = Vec::new();
    let mut at = 0;
    while at + HEADER <= buf.len() {
        // SAFETY: at least `HEADER` bytes remain; `read_unaligned` copes with
        // any alignment.
        let header: libc::inotify_event =
            unsafe { std::ptr::read_unaligned(buf[at..].as_ptr().cast()) };
        let start = at + HEADER;
        let Some(end) = start
            .checked_add(header.len as usize)
            .filter(|&e| e <= buf.len())
        else {
            break;
        };
        let raw = &buf[start..end];
        let name = raw.split(|&b| b == 0).next().filter(|n| !n.is_empty());
        events.push(Event {
            wd: header.wd,
            mask: header.mask,
            name: name.map(OsStr::from_bytes),
        });
        at = end;
    }
    events
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `pipe2` fills `fds` with two new descriptors on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned by nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(wd: i32, mask: u32, name: &[u8], padded: usize) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&wd.to_ne_bytes());
        out.extend_from_slice(&mask.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(&(padded as u32).to_ne_bytes());
        let mut name = name.to_vec();
        name.resize(padded, 0);
        out.extend_from_slice(&name);
        out
    }

    #[test]
    fn events_are_parsed_with_their_names() {
        let mut buf = record(1, libc::IN_CREATE, b"commondir", 16);
        buf.extend(record(2, libc::IN_MOVE_SELF, b"", 0));
        assert_eq!(
            events(&buf),
            vec![
                Event {
                    wd: 1,
                    mask: libc::IN_CREATE,
                    name: Some(OsStr::new("commondir"))
                },
                Event {
                    wd: 2,
                    mask: libc::IN_MOVE_SELF,
                    name: None
                },
            ]
        );
    }

    #[test]
    fn a_truncated_record_is_ignored() {
        let buf = record(1, libc::IN_CREATE, b"config", 16);
        assert_eq!(events(&buf[..buf.len() - 1]), vec![]);
        assert_eq!(events(&buf[..10]), vec![]);
    }

    struct Counting {
        dir: PathBuf,
        checks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Target for Counting {
        fn dirs(&self) -> Vec<PathBuf> {
            vec![self.dir.clone()]
        }
        fn relevant(&self, _dir: &Path, name: Option<&OsStr>) -> bool {
            name == Some(OsStr::new("config"))
        }
        fn check(&self) {
            self.checks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn a_relevant_change_runs_a_check_and_others_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let checks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let watcher = Watcher::start(Counting {
            dir: dir.path().to_path_buf(),
            checks: checks.clone(),
        })
        .unwrap();
        std::fs::write(dir.path().join("index.lock"), "x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 0);
        std::fs::write(dir.path().join("config"), "x").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while checks.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "no check within 5s");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        watcher.stop();
    }
}
```

- [ ] **Step 3: Start it with the guard**

In `crates/harness-sandbox/src/linux/mod.rs`:

Replace (1 of 4):

```rust
//! Linux sandbox backend: a Landlock ruleset (filesystem) plus seccomp-BPF
//! programs (network, mounts, namespaces) installed from `pre_exec`, in the
//! forked child, before `execve`; and around every workspace-write
//! command, the git-metadata guard (`crate::guard`).
//!
//! ## Split between parent and child
//!
```

with:

```rust
//! Linux sandbox backend: a Landlock ruleset (filesystem) plus seccomp-BPF
//! programs (network, mounts, namespaces) installed from `pre_exec`, in the
//! forked child, before `execve`; and around every workspace-write
//! command, the git-metadata guard (`crate::guard`) with an inotify watcher
//! (`watch.rs`).
//!
//! ## Split between parent and child
//!
```

Replace (2 of 4):

```rust
mod fs;
mod preexec;
mod seccomp;

use std::io;
use std::path::{Path, PathBuf};
```

with:

```rust
mod fs;
mod preexec;
mod seccomp;
mod watch;

use std::io;
use std::path::{Path, PathBuf};
```

Replace (3 of 4):

```rust
                return Err(e);
            }
        };
        Ok(SandboxedCommand {
            command,
            guard: Some(Box::new(LinuxGuard { guard })),
        })
    }

```

with:

```rust
                return Err(e);
            }
        };
        let watcher = watch::Watcher::start(guard.watch_handle()).ok();
        Ok(SandboxedCommand {
            command,
            guard: Some(Box::new(LinuxGuard { guard, watcher })),
        })
    }

```

Replace (4 of 4):

```rust
    std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
}

/// The guard for one command.
struct LinuxGuard {
    guard: GitGuard,
}

impl CommandGuard for LinuxGuard {
    fn finish(self: Box<Self>) -> Option<GuardReport> {
        self.guard.finish()
    }
}

```

with:

```rust
    std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
}

/// The guard for one command, and its watcher.
struct LinuxGuard {
    guard: GitGuard,
    watcher: Option<watch::Watcher>,
}

impl CommandGuard for LinuxGuard {
    /// Stops the watcher, then runs the guard's final checks.
    fn finish(self: Box<Self>) -> Option<GuardReport> {
        let LinuxGuard { guard, watcher } = *self;
        if let Some(watcher) = watcher {
            watcher.stop();
        }
        guard.finish()
    }
}

```

- [ ] **Step 4: Lint**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && bash target/linux-lint.sh`
Expected: clean. On Linux (CI) `the_watcher_quarantines_a_hook_while_the_command_runs` and the `linux::watch` unit tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/harness-sandbox
git commit -F - <<'EOF'
feat(sandbox): watch git metadata while a Linux command runs

An inotify watcher on each gitdir, its worktrees and modules
directories, the workspace root, and in the basic tier the protected
directories, runs the guard's checks as soon as a protected name
changes, so a planted file is moved within milliseconds instead of when
the command ends.

<trailer lines from the controller>
EOF
```

---

### Task 8: The full tier: read-only mounts where user namespaces work

**Files:**
- Create: `crates/harness-sandbox/src/linux/mountns.rs`, `crates/harness-sandbox/src/linux/mountplan.rs`, `crates/harness-sandbox/src/linux/tier.rs`
- Modify: `crates/harness-sandbox/src/linux/preexec.rs`, `crates/harness-sandbox/src/linux/mod.rs`, `crates/harness-sandbox/src/lib.rs`, `crates/harness-sandbox/tests/linux_git_guard.rs`

**Interfaces:**
- Consumes: `GitIndex` (Task 2); `GuardSession::begin`'s `placeholders` hook and `GitGuard::index` (Task 4); `PreparedSandbox` (Task 5); `LinuxSandbox` (Tasks 6 and 7).
- Produces:
  - `harness_sandbox::linux_git_protection() -> GitProtection`: the probe, run once per process. `LinuxSandbox::new` and so `detect()` use it.
  - Full-tier `LinuxSandbox::prepare`: `begin(workspace, save_all = false, placeholders)`, a mount plan from the guard's index, and the setup pipe. `LinuxSandbox::command` also mounts, without the guard. `is_denial` also treats "device or resource busy" (`EBUSY` from a pinned mount point) as a denial.
  - After a failed mount step: the command has not run, `finish` appends a note (not blocking), and `git_protection()` becomes `Basic { reason: "the full tier's setup failed during the session: …" }`.
  - Crate-internal: `mountns::{MountPlan, MountOp, Step, Failure, enter, openat2, Fd, RESOLVE_*}`, `mountplan::{create_hooks_placeholders, build, probe, setup_pipe, read_failure}`, `PreparedSandbox.mounts: Option<(MountPlan, Option<OwnedFd>)>`.

How the pieces fit: the guard indexes the workspace; `create_hooks_placeholders` gives every gitdir without `hooks/` an empty one before the guard records what exists; `mountplan::build` turns the index into ops (gitdirs pinned read-write, protected entries, gitfiles, `.harness/` and `HEAD` read-only; symlinks, and paths reached through one, left to the guard); `mountns::enter` runs them in the child, between the fd cleanup and `no_new_privs`. A read-only bind of the whole gitdir would break commits, because git creates `index.lock`, `COMMIT_EDITMSG`, `ORIG_HEAD` and others in its root.

- [ ] **Step 1: Write the failing tests**

In `crates/harness-sandbox/tests/linux_git_guard.rs`:

Replace (1 of 6):

```rust
//! Linux git-metadata protection end to end: `LinuxSandbox::prepare` runs real commands with
//! the guard, in the basic tier.
//!
//! Like `linux_sandbox.rs`, a test skips when the sandbox or a tool it needs is missing, unless
//! `HARNESS_REQUIRE_LINUX_SANDBOX=1`.

#![cfg(all(
    target_os = "linux",
```

with:

```rust
//! Linux git-metadata protection end to end: `LinuxSandbox::prepare` runs real commands with
//! the guard, in the basic tier (forced, so it runs on every Linux host) and in the full tier
//! (where user namespaces work).
//!
//! Like `linux_sandbox.rs`, a test skips when the sandbox or a tool it needs is missing, unless
//! `HARNESS_REQUIRE_LINUX_SANDBOX=1`. Full-tier tests skip when the probe picks the basic tier,
//! unless `HARNESS_EXPECT_LINUX_TIER=full` (CI's full-tier job); with
//! `HARNESS_EXPECT_LINUX_TIER=basic` (the stock job) the probe must pick the basic tier.

#![cfg(all(
    target_os = "linux",
```

Replace (2 of 6):

```rust
use std::time::{Duration, Instant};

use harness_core::tool::{CommandSandbox, GitProtection, GuardReport};
use harness_sandbox::{FsAccess, LinuxSandbox, SandboxSettings, linux_sandbox_available};

fn skip_or_require(reason: &str) -> bool {
    assert!(
```

with:

```rust
use std::time::{Duration, Instant};

use harness_core::tool::{CommandSandbox, GitProtection, GuardReport};
use harness_sandbox::{
    FsAccess, LinuxSandbox, SandboxSettings, linux_git_protection, linux_sandbox_available,
};

fn skip_or_require(reason: &str) -> bool {
    assert!(
```

Replace (3 of 6):

```rust
    true
}

/// A git repository under `$HOME` (not `/tmp`, which the sandbox always makes writable), with a
/// commit identity, and a quarantine directory next to it. Both are removed on drop.
struct Env {
```

with:

```rust
    true
}

fn expected_tier() -> Option<String> {
    std::env::var("HARNESS_EXPECT_LINUX_TIER")
        .ok()
        .filter(|tier| !tier.is_empty())
}

/// A git repository under `$HOME` (not `/tmp`, which the sandbox always makes writable), with a
/// commit identity, and a quarantine directory next to it. Both are removed on drop.
struct Env {
```

Replace (4 of 6):

```rust
        )
    }

    /// The one entry the quarantine holds at `rel` (relative to the workspace).
    fn quarantined(&self, rel: &str) -> PathBuf {
        let found: Vec<PathBuf> = std::fs::read_dir(&self.quarantine)
```

with:

```rust
        )
    }

    /// The full tier, or `None` (having skipped) where the probe picks the basic tier.
    fn full(&self) -> Option<LinuxSandbox> {
        match linux_git_protection() {
            GitProtection::Full => Some(LinuxSandbox::with_git_protection(
                self.settings(),
                GitProtection::Full,
            )),
            GitProtection::Basic { reason } => {
                assert!(
                    expected_tier().as_deref() != Some("full"),
                    "HARNESS_EXPECT_LINUX_TIER=full, but the probe picked the basic tier: {reason}"
                );
                eprintln!("skipping: user namespaces are unavailable ({reason})");
                None
            }
        }
    }

    /// The one entry the quarantine holds at `rel` (relative to the workspace).
    fn quarantined(&self, rel: &str) -> PathBuf {
        let found: Vec<PathBuf> = std::fs::read_dir(&self.quarantine)
```

Replace (5 of 6):

```rust
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ---------------------------------------------------------------------------
// The basic tier: undone after the fact
// ---------------------------------------------------------------------------
```

with:

```rust
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ---------------------------------------------------------------------------
// The tier
// ---------------------------------------------------------------------------

#[test]
fn the_probe_picks_the_expected_tier() {
    if !linux_sandbox_available() {
        skip_or_require("linux sandbox unavailable");
        return;
    }
    let tier = linux_git_protection();
    if let GitProtection::Basic { reason } = &tier {
        assert!(!reason.is_empty());
    }
    match expected_tier().as_deref() {
        Some("full") => assert_eq!(tier, GitProtection::Full),
        Some("basic") => assert!(matches!(tier, GitProtection::Basic { .. }), "{tier:?}"),
        _ => eprintln!("probe picked {tier:?}"),
    }
}

// ---------------------------------------------------------------------------
// The basic tier: undone after the fact
// ---------------------------------------------------------------------------
```

Replace (6 of 6):

```rust
    );
    assert!(!env.exists(".git/commondir"));
}
```

with:

```rust
    );
    assert!(!env.exists(".git/commondir"));
}

// ---------------------------------------------------------------------------
// The full tier: writes fail
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_tier_refuses_to_plant_a_hook() {
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let (output, report) = run(&sandbox, &env, "echo 'echo pwned' > .git/hooks/pre-commit").await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Read-only file system"),
        "{}",
        stderr(&output)
    );
    assert!(!env.exists(".git/hooks/pre-commit"));
    assert_eq!(report, None);
}

#[tokio::test]
async fn full_tier_refuses_git_config() {
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let before = std::fs::read(env.ws.join(".git/config")).unwrap();
    let (output, _) = run(&sandbox, &env, "git config user.name evil").await;
    assert!(!output.status.success());
    assert_eq!(std::fs::read(env.ws.join(".git/config")).unwrap(), before);
}

#[tokio::test]
async fn full_tier_pins_dot_git() {
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let (output, _) = run(&sandbox, &env, "mv .git moved").await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Device or resource busy"),
        "{}",
        stderr(&output)
    );
    assert!(env.ws.join(".git").is_dir());
    assert!(sandbox.is_denial(output.status.code(), &stderr(&output)));
}

#[tokio::test]
async fn full_tier_refuses_hard_links_across_its_mounts() {
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let (output, _) = run(&sandbox, &env, "ln .git/config alias").await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Invalid cross-device link"),
        "{}",
        stderr(&output)
    );
    assert!(!env.exists("alias"));
}

#[tokio::test]
async fn full_tier_protects_nested_repositories_harness_and_head() {
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    assert!(env.git(&["init", "-q", "sub"]).status.success());
    std::fs::create_dir(env.ws.join(".harness")).unwrap();
    std::fs::write(env.ws.join(".harness/config.toml"), "mode = \"ask\"\n").unwrap();
    std::fs::write(env.ws.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    for target in ["sub/.git/config", ".harness/config.toml", "HEAD"] {
        let before = std::fs::read(env.ws.join(target)).unwrap();
        let (output, _) = run(&sandbox, &env, &format!("echo evil >> {target}")).await;
        assert!(!output.status.success(), "{target}");
        assert!(
            stderr(&output).contains("Read-only file system"),
            "{target}: {}",
            stderr(&output)
        );
        assert_eq!(
            std::fs::read(env.ws.join(target)).unwrap(),
            before,
            "{target}"
        );
    }
}

#[tokio::test]
async fn full_tier_covers_a_missing_hooks_directory_with_an_empty_one() {
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    std::fs::remove_dir_all(env.ws.join(".git/hooks")).unwrap();
    let (output, report) = run(&sandbox, &env, "echo 'echo pwned' > .git/hooks/pre-commit").await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Read-only file system"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        std::fs::read_dir(env.ws.join(".git/hooks"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(report, None);
}

#[tokio::test]
async fn full_tier_allows_commit_checkout_and_stash() {
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let script = "set -e; git commit -q --allow-empty -m one; git checkout -q -b topic; \
                  echo x > f; git add f; git stash -q; git stash pop -q";
    let (output, report) = run(&sandbox, &env, script).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
    let log = env.git(&["log", "--oneline"]);
    assert!(String::from_utf8_lossy(&log.stdout).contains("one"));
}

#[tokio::test]
async fn full_tier_quarantines_a_new_commondir_while_the_command_runs() {
    // `commondir` cannot have a placeholder (git refuses an empty one), so the guard handles it.
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let prepared = sandbox
        .prepare(
            FsAccess::WorkspaceWrite,
            &env.ws,
            "/bin/sh",
            &["-c", "printf /tmp/elsewhere > .git/commondir; sleep 5"],
        )
        .unwrap();
    let mut cmd = prepared.command;
    cmd.current_dir(&env.ws).stdin(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    env.wait_for_quarantine(".git/commondir").await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "the command should still be running"
    );
    assert!(!env.exists(".git/commondir"));
    let _ = child.kill().await;
    assert!(prepared.guard.unwrap().finish().expect("a report").blocked);
}

#[tokio::test]
async fn a_working_directory_inside_dot_git_still_sees_the_mounts() {
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    let (output, _) = run_in(
        &sandbox,
        &env.ws,
        &env.ws.join(".git"),
        "echo x > hooks/pre-commit",
    )
    .await;
    let output = output.unwrap();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Read-only file system"),
        "{}",
        stderr(&output)
    );
}

#[tokio::test]
async fn full_tier_refuses_umount_and_the_harness_process_root() {
    let Some(env) = Env::new() else { return };
    let Some(sandbox) = env.full() else { return };
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        skip_or_require("python3 not installed");
        return;
    }
    // `exec` makes python the direct child, so `getppid()` is this test process: outside the
    // sandbox, where `/proc/<pid>/root` would show `.git/config` without the mounts.
    let script = format!(
        r#"exec python3 -c '
import ctypes, errno, os, sys
libc = ctypes.CDLL(None, use_errno=True)
if libc.umount2(b".git/config", 0) != -1 or ctypes.get_errno() != errno.EPERM:
    sys.exit("umount2: expected EPERM, got %d" % ctypes.get_errno())
try:
    open("/proc/%d/root{}/.git/config" % os.getppid(), "a")
    sys.exit("opened .git/config through /proc/<pid>/root")
except PermissionError as e:
    if e.errno != errno.EACCES:
        sys.exit("/proc/<pid>/root: expected EACCES, got %d" % e.errno)
'"#,
        env.ws.display()
    );
    let (output, _) = run(&sandbox, &env, &script).await;
    assert!(output.status.success(), "{}", stderr(&output));
}

#[tokio::test]
async fn a_failed_mount_setup_drops_the_session_to_the_basic_tier() {
    let Some(env) = Env::new() else { return };
    if linux_git_protection() == GitProtection::Full {
        eprintln!("skipping: user namespaces work here, so the full tier's setup cannot fail");
        return;
    }
    let sandbox = LinuxSandbox::with_git_protection(env.settings(), GitProtection::Full);
    let (output, report) = run_in(&sandbox, &env.ws, &env.ws, "touch ran").await;
    assert!(
        output.is_err(),
        "the command must not run without its mounts"
    );
    assert!(!env.exists("ran"));
    let report = report.expect("a report");
    assert!(!report.blocked);
    assert!(
        report
            .message
            .contains("could not set up its read-only mounts"),
        "{}",
        report.message
    );
    match sandbox.git_protection() {
        GitProtection::Basic { reason } => {
            assert!(reason.contains("failed during the session"), "{reason}")
        }
        GitProtection::Full => panic!("the session should have dropped to the basic tier"),
    }
    // The next command runs in the basic tier.
    let (output, report) = run(&sandbox, &env, "touch ran").await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(report, None);
    assert!(env.exists("ran"));
}
```

- [ ] **Step 2: Run the lint to make sure it fails**

Run: `bash target/linux-lint.sh`
Expected: FAIL: ``unresolved import `harness_sandbox::linux_git_protection` ``.

- [ ] **Step 3: Add the child's setup**

`crates/harness-sandbox/src/linux/mountns.rs`:

```rust
//! The full tier's mount setup, run in the forked child from `pre_exec`
//! (see `preexec.rs`), before `no_new_privs`, Landlock and seccomp.
//!
//! The child unshares a user and a mount namespace, maps its own uid and gid
//! 1:1 (so the command, which is not uid 0, keeps no capability once it
//! execs), makes every mount private, and then, for each [`MountOp`] in
//! order, self-binds the entry at its path relative to the workspace:
//! read-write for a gitdir (the mount pins it: it can no longer be renamed,
//! removed or replaced) and read-only for a protected entry. Finally it
//! changes into its working directory again, so that a working directory
//! inside a newly covered directory resolves through the new mount.
//!
//! Every step works on file descriptors: entries are opened with `openat2`
//! beneath the workspace without following any symlink, checked against the
//! device and inode the parent saw when it built the plan, cloned with
//! `open_tree`, made read-only with `mount_setattr` (which changes only that
//! flag, unlike a classic read-only remount, which must restate the locked
//! `nosuid`/`nodev`/`noexec` flags inside a user namespace), and attached
//! with `move_mount`. These calls need Linux 5.12; the Landlock ABI 3 floor
//! already requires 6.2.
//!
//! Like the rest of `pre_exec`, this is async-signal-safe: raw syscalls on
//! data the parent prepared, stack buffers, no allocation. A failure returns
//! the raw `errno`, and first writes a [`Failure`] record to the setup pipe,
//! so the parent can say which step failed.

use std::ffi::CStr;
use std::ffi::CString;
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::fd::RawFd;

use libc::c_uint;

// From <linux/mount.h> and <linux/openat2.h>: stable kernel UAPI.
const AT_EMPTY_PATH: c_uint = 0x1000;
const AT_RECURSIVE: c_uint = 0x8000;
const OPEN_TREE_CLONE: c_uint = 1;
const OPEN_TREE_CLOEXEC: c_uint = libc::O_CLOEXEC as c_uint;
const MOVE_MOUNT_F_EMPTY_PATH: c_uint = 0x04;
const MOVE_MOUNT_T_EMPTY_PATH: c_uint = 0x40;
const MOUNT_ATTR_RDONLY: u64 = 0x01;
pub(super) const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
pub(super) const RESOLVE_NO_SYMLINKS: u64 = 0x04;
pub(super) const RESOLVE_BENEATH: u64 = 0x08;

/// `struct open_how`.
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// `struct mount_attr`.
#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

/// Everything the child needs, built by the parent (`mountplan.rs`).
#[derive(Debug)]
pub(super) struct MountPlan {
    /// The canonical workspace.
    pub(super) workspace: CString,
    /// `/proc/self/uid_map` and `gid_map` contents: `"<id> <id> 1\n"`.
    pub(super) uid_map: Vec<u8>,
    pub(super) gid_map: Vec<u8>,
    /// Parents before children.
    pub(super) ops: Vec<MountOp>,
}

/// One self-bind.
#[derive(Debug)]
pub(super) struct MountOp {
    /// Relative to the workspace.
    pub(super) path: CString,
    /// Read-only, or a read-write pin.
    pub(super) read_only: bool,
    /// What the parent saw at `path`; anything else there fails the setup.
    pub(super) dev: u64,
    pub(super) ino: u64,
}

/// A setup step, as reported through the setup pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Step {
    Unshare = 1,
    Setgroups,
    UidMap,
    GidMap,
    Private,
    Workspace,
    Open,
    Identity,
    OpenTree,
    ReadOnly,
    MoveMount,
    Chdir,
}

impl Step {
    const ALL: [Step; 12] = [
        Step::Unshare,
        Step::Setgroups,
        Step::UidMap,
        Step::GidMap,
        Step::Private,
        Step::Workspace,
        Step::Open,
        Step::Identity,
        Step::OpenTree,
        Step::ReadOnly,
        Step::MoveMount,
        Step::Chdir,
    ];

    pub(super) fn describe(self) -> &'static str {
        match self {
            Step::Unshare => "creating a user and mount namespace",
            Step::Setgroups => "writing /proc/self/setgroups",
            Step::UidMap => "writing /proc/self/uid_map",
            Step::GidMap => "writing /proc/self/gid_map",
            Step::Private => "making mounts private",
            Step::Workspace => "opening the workspace",
            Step::Open => "opening",
            Step::Identity => "checking that nothing replaced",
            Step::OpenTree => "cloning a mount of",
            Step::ReadOnly => "making read-only",
            Step::MoveMount => "mounting",
            Step::Chdir => "changing into the working directory again",
        }
    }
}

/// Which step failed, for which op, with which `errno`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Failure {
    pub(super) step: Step,
    pub(super) op: Option<u16>,
    pub(super) errno: i32,
}

impl Failure {
    pub(super) const LEN: usize = 8;

    fn encode(self) -> [u8; Failure::LEN] {
        let op = self.op.unwrap_or(u16::MAX);
        let [o0, o1] = op.to_le_bytes();
        let [e0, e1, e2, e3] = self.errno.to_le_bytes();
        [self.step as u8, 0, o0, o1, e0, e1, e2, e3]
    }

    /// What failed, naming the entry for a per-entry step. `paths` are the
    /// plan's ops' absolute paths.
    pub(super) fn describe(&self, paths: &[std::path::PathBuf]) -> String {
        let err = io::Error::from_raw_os_error(self.errno);
        match self.op.and_then(|op| paths.get(usize::from(op))) {
            Some(path) => format!("{} {} failed: {err}", self.step.describe(), path.display()),
            None => format!("{} failed: {err}", self.step.describe()),
        }
    }

    pub(super) fn decode(bytes: &[u8]) -> Option<Failure> {
        let bytes: &[u8; Failure::LEN] = bytes.try_into().ok()?;
        let step = *Step::ALL.iter().find(|s| **s as u8 == bytes[0])?;
        let op = u16::from_le_bytes([bytes[2], bytes[3]]);
        Some(Failure {
            step,
            op: (op != u16::MAX).then_some(op),
            errno: i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        })
    }
}

/// Sets up the namespace and mounts in the calling process. Must only be
/// called from `pre_exec`. On failure writes a [`Failure`] to `report`, if
/// any, and returns the `errno`.
pub(super) fn enter(plan: &MountPlan, report: Option<RawFd>) -> io::Result<()> {
    setup(plan).map_err(|failure| {
        if let Some(fd) = report {
            let bytes = failure.encode();
            // SAFETY: writes `bytes`, a stack array, to a pipe the parent
            // created; a failed write only loses the diagnosis.
            unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        }
        io::Error::from_raw_os_error(failure.errno)
    })
}

fn setup(plan: &MountPlan) -> Result<(), Failure> {
    let fail = |step: Step| {
        move |errno: i32| Failure {
            step,
            op: None,
            errno,
        }
    };
    // SAFETY: `unshare` takes only flags. The child is single-threaded, as
    // `CLONE_NEWUSER` requires.
    check(unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) }.into())
        .map_err(fail(Step::Unshare))?;
    write_file(c"/proc/self/setgroups", b"deny").map_err(fail(Step::Setgroups))?;
    write_file(c"/proc/self/uid_map", &plan.uid_map).map_err(fail(Step::UidMap))?;
    write_file(c"/proc/self/gid_map", &plan.gid_map).map_err(fail(Step::GidMap))?;
    // SAFETY: null source, type and data are valid for a propagation change.
    check(
        unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        }
        .into(),
    )
    .map_err(fail(Step::Private))?;
    // Opened after `unshare`: the mount calls below only accept mounts in
    // the caller's own namespace.
    let workspace = openat2(
        libc::AT_FDCWD,
        &plan.workspace,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
    )
    .map_err(fail(Step::Workspace))?;
    for (i, op) in plan.ops.iter().enumerate() {
        bind(workspace.0, op).map_err(|(step, errno)| Failure {
            step,
            op: u16::try_from(i).ok(),
            errno,
        })?;
    }
    drop(workspace);
    chdir_again().map_err(fail(Step::Chdir))
}

fn bind(workspace: RawFd, op: &MountOp) -> Result<(), (Step, i32)> {
    let target = openat2(
        workspace,
        &op.path,
        libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
    )
    .map_err(|e| (Step::Open, e))?;
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fstat` fills `stat`, a stack buffer of the right type.
    check(unsafe { libc::fstat(target.0, stat.as_mut_ptr()) }.into())
        .map_err(|e| (Step::Identity, e))?;
    // SAFETY: `fstat` succeeded, so it initialized `stat`.
    let stat = unsafe { stat.assume_init() };
    if stat.st_dev != op.dev || stat.st_ino != op.ino {
        return Err((Step::Identity, libc::ESTALE));
    }
    // SAFETY: `open_tree` on an fd with an empty path; the flags clone the
    // whole subtree into a new, detached mount.
    let tree = check(unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            target.0,
            c"".as_ptr(),
            AT_EMPTY_PATH | OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC | AT_RECURSIVE,
        )
    })
    .map(|fd| Fd(fd as RawFd))
    .map_err(|e| (Step::OpenTree, e))?;
    if op.read_only {
        let attr = MountAttr {
            attr_set: MOUNT_ATTR_RDONLY,
            attr_clr: 0,
            propagation: 0,
            userns_fd: 0,
        };
        // SAFETY: `attr` is a valid `struct mount_attr` of the size passed.
        check(unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                tree.0,
                c"".as_ptr(),
                AT_EMPTY_PATH | AT_RECURSIVE,
                &attr as *const MountAttr,
                size_of::<MountAttr>(),
            )
        })
        .map_err(|e| (Step::ReadOnly, e))?;
    }
    // SAFETY: both paths are empty, so both fds are used as they are.
    check(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            tree.0,
            c"".as_ptr(),
            target.0,
            c"".as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
        )
    })
    .map_err(|e| (Step::MoveMount, e))?;
    Ok(())
}

/// Changes into the current working directory by name, so it resolves
/// through the mounts just made.
fn chdir_again() -> Result<(), i32> {
    let mut path = [0u8; libc::PATH_MAX as usize];
    // SAFETY: `getcwd` writes at most `path.len()` bytes into `path`.
    check(unsafe { libc::syscall(libc::SYS_getcwd, path.as_mut_ptr(), path.len()) })?;
    // SAFETY: `getcwd` succeeded, so `path` holds a NUL-terminated path.
    check(unsafe { libc::chdir(path.as_ptr().cast()) }.into()).map(|_| ())
}

/// A file descriptor closed on drop.
pub(super) struct Fd(pub(super) RawFd);

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: closes an fd this process opened and nothing else owns.
        unsafe { libc::close(self.0) };
    }
}

/// `openat2(dirfd, path, flags, resolve)`.
pub(super) fn openat2(
    dirfd: RawFd,
    path: &CStr,
    flags: libc::c_int,
    resolve: u64,
) -> Result<Fd, i32> {
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve,
    };
    // SAFETY: `how` is a valid `struct open_how` of the size passed.
    check(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dirfd,
            path.as_ptr(),
            &how as *const OpenHow,
            size_of::<OpenHow>(),
        )
    })
    .map(|fd| Fd(fd as RawFd))
}

/// Writes all of `bytes` to the file at `path`.
fn write_file(path: &CStr, bytes: &[u8]) -> Result<(), i32> {
    // SAFETY: `path` is NUL-terminated; `open` allocates nothing.
    let fd = check(unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) }.into())
        .map(|fd| Fd(fd as RawFd))?;
    // SAFETY: writes from `bytes`, which outlives the call.
    let written = check(unsafe { libc::write(fd.0, bytes.as_ptr().cast(), bytes.len()) } as i64)?;
    if written as usize == bytes.len() {
        Ok(())
    } else {
        Err(libc::EIO)
    }
}

/// A syscall's return value, or the `errno` it set.
fn check(rc: i64) -> Result<i64, i32> {
    if rc < 0 {
        Err(io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO))
    } else {
        Ok(rc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_survives_the_pipe() {
        let failure = Failure {
            step: Step::UidMap,
            op: None,
            errno: libc::EPERM,
        };
        assert_eq!(Failure::decode(&failure.encode()), Some(failure));
        let failure = Failure {
            step: Step::MoveMount,
            op: Some(3),
            errno: libc::EINVAL,
        };
        assert_eq!(Failure::decode(&failure.encode()), Some(failure));
        assert_eq!(Failure::decode(&[0; 8]), None);
        assert_eq!(Failure::decode(&[1; 7]), None);
    }

    #[test]
    fn every_step_has_a_distinct_code() {
        for (i, step) in Step::ALL.iter().enumerate() {
            assert_eq!(*step as u8 as usize, i + 1);
        }
    }

    #[test]
    fn uapi_structs_have_the_kernel_sizes() {
        assert_eq!(size_of::<OpenHow>(), 24);
        assert_eq!(size_of::<MountAttr>(), 32);
    }
}
```

- [ ] **Step 4: Add the parent's plan**

`crates/harness-sandbox/src/linux/mountplan.rs`:

```rust
//! Builds the full tier's mount plan in the parent, from the guard's index
//! of the workspace (`crate::gitmeta`).
//!
//! - Every gitdir is pinned with a read-write self-bind: a mount point cannot
//!   be renamed, removed or replaced from inside the namespace (`EBUSY`).
//! - Every protected entry in it that exists (`config`, `config.worktree`,
//!   `commondir`, `hooks/`, `gitweb/`, `pid`) is bound read-only, and so are
//!   every gitfile `.git`, `.harness/` and a top-level `HEAD`.
//! - A gitdir without `hooks/` first gets an empty one, which git treats
//!   exactly like a missing one, so that planting hooks fails too. The
//!   placeholder is left in place: removing it while another command uses it
//!   as a mount point would detach that command's mount.
//!
//! Symlinks cannot be mounted over, so an entry that is a symlink, or that
//! is reached through one, is left to the guard. So is every name that does
//! not exist yet, such as a new `commondir`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::mountns::{
    Fd, MountOp, MountPlan, RESOLVE_BENEATH, RESOLVE_NO_MAGICLINKS, RESOLVE_NO_SYMLINKS, openat2,
};
use crate::gitmeta::{GITDIR_PROTECTED, GitIndex, WORKSPACE_PROTECTED};

/// Creates an empty `hooks/` (mode 0755) in each gitdir in `gitdirs` that has
/// none. A gitdir that cannot be reached from `workspace` without following a
/// symlink is skipped.
pub(super) fn create_hooks_placeholders(workspace: &Path, gitdirs: &BTreeSet<PathBuf>) {
    let Ok(root) = open_dir(libc::AT_FDCWD, workspace, RESOLVE_NO_SYMLINKS) else {
        return;
    };
    for gitdir in gitdirs {
        let Ok(rel) = gitdir.strip_prefix(workspace) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let Ok(dir) = open_dir(root.0, rel, RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS) else {
            continue;
        };
        // SAFETY: creates `hooks` in the directory `dir` refers to; an
        // existing entry of that name makes it fail with `EEXIST`, which is
        // fine.
        unsafe { libc::mkdirat(dir.0, c"hooks".as_ptr(), 0o755) };
    }
}

fn open_dir(dirfd: i32, path: &Path, resolve: u64) -> Result<Fd, i32> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| libc::EINVAL)?;
    openat2(
        dirfd,
        &path,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        resolve | RESOLVE_NO_MAGICLINKS,
    )
}

/// The plan for the canonical `workspace`, and each op's absolute path (for
/// messages). `None` when there is nothing to protect.
pub(super) fn build(workspace: &Path, index: &GitIndex) -> Option<(MountPlan, Vec<PathBuf>)> {
    let mut wanted: BTreeMap<PathBuf, bool> = BTreeMap::new();
    for gitdir in &index.gitdirs {
        if !mountable(gitdir) {
            continue;
        }
        wanted.insert(gitdir.clone(), false);
        for name in GITDIR_PROTECTED {
            let entry = gitdir.join(name);
            if mountable(&entry) {
                wanted.insert(entry, true);
            }
        }
    }
    for dot_git in &index.dot_gits {
        if std::fs::symlink_metadata(dot_git).is_ok_and(|m| m.is_file()) && mountable(dot_git) {
            wanted.insert(dot_git.clone(), true);
        }
    }
    for name in WORKSPACE_PROTECTED {
        let entry = workspace.join(name);
        if mountable(&entry) {
            wanted.insert(entry, true);
        }
    }

    let mut ops = Vec::new();
    let mut paths = Vec::new();
    for (path, read_only) in wanted {
        let Ok(rel) = path.strip_prefix(workspace) else {
            continue;
        };
        let (Ok(meta), Ok(rel)) = (
            std::fs::symlink_metadata(&path),
            CString::new(rel.as_os_str().as_bytes()),
        ) else {
            continue;
        };
        if rel.as_bytes().is_empty() {
            continue;
        }
        ops.push(MountOp {
            path: rel,
            read_only,
            dev: meta.dev(),
            ino: meta.ino(),
        });
        paths.push(path);
    }
    if ops.is_empty() {
        return None;
    }
    let workspace = CString::new(workspace.as_os_str().as_bytes()).ok()?;
    Some((plan(workspace, ops), paths))
}

/// The plan for the probe (`tier.rs`): pin `dir/pin` and make
/// `dir/pin/file` read-only.
pub(super) fn probe(dir: &Path) -> Option<MountPlan> {
    let mut ops = Vec::new();
    for (rel, read_only) in [("pin", false), ("pin/file", true)] {
        let meta = std::fs::symlink_metadata(dir.join(rel)).ok()?;
        ops.push(MountOp {
            path: CString::new(rel).ok()?,
            read_only,
            dev: meta.dev(),
            ino: meta.ino(),
        });
    }
    Some(plan(CString::new(dir.as_os_str().as_bytes()).ok()?, ops))
}

fn plan(workspace: CString, ops: Vec<MountOp>) -> MountPlan {
    // SAFETY: `geteuid` and `getegid` cannot fail.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    MountPlan {
        workspace,
        uid_map: format!("{uid} {uid} 1\n").into_bytes(),
        gid_map: format!("{gid} {gid} 1\n").into_bytes(),
        ops,
    }
}

/// Whether `path` exists, is not a symlink, and is reached without one.
fn mountable(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| !m.file_type().is_symlink())
        && std::fs::canonicalize(path).is_ok_and(|canon| canon == path)
}

/// A pipe for the child's setup failure: the read end for the parent, the
/// write end for the child. Both are close-on-exec and non-blocking.
pub(super) fn setup_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `pipe2` fills `fds` with two new descriptors on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned by nobody else.
    Ok(unsafe {
        use std::os::fd::FromRawFd;
        (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
    })
}

/// The failure the child wrote to `reader`, if it wrote one.
pub(super) fn read_failure(reader: &OwnedFd) -> Option<super::mountns::Failure> {
    let mut bytes = [0u8; super::mountns::Failure::LEN];
    // SAFETY: reads at most `bytes.len()` bytes into `bytes`; the pipe is
    // non-blocking, so this returns at once when it is empty.
    let n = unsafe { libc::read(reader.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
    (n == bytes.len() as isize)
        .then(|| super::mountns::Failure::decode(&bytes))
        .flatten()
}
```

- [ ] **Step 5: Add the probe**

`crates/harness-sandbox/src/linux/tier.rs`:

```rust
//! Chooses the git-protection tier: a throwaway child tries the full tier's
//! exact setup (`mountns.rs`) on a temporary directory.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use harness_core::tool::GitProtection;

use super::{mountns, mountplan};

/// The tier this host supports, probed once per process.
pub fn linux_git_protection() -> GitProtection {
    static TIER: OnceLock<GitProtection> = OnceLock::new();
    TIER.get_or_init(|| match probe() {
        Ok(()) => GitProtection::Full,
        Err(reason) => GitProtection::Basic { reason },
    })
    .clone()
}

/// Runs `/bin/sh` in a child that first sets up a user and mount namespace
/// with a pinned directory and a read-only file in it, then tries to write
/// the file. `Err` says why the full tier is unavailable.
fn probe() -> Result<(), String> {
    let dir =
        ProbeDir::create().map_err(|e| format!("the probe could not create a directory: {e}"))?;
    let plan = mountplan::probe(dir.path())
        .ok_or_else(|| "the probe could not describe its directory".to_string())?;
    let (reader, writer) =
        mountplan::setup_pipe().map_err(|e| format!("the probe could not make a pipe: {e}"))?;
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args(["-c", ": > pin/file 2>/dev/null && exit 10; exit 0"])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `mountns::enter` is async-signal-safe (see its module docs);
    // the closure owns `plan` and the pipe's write end.
    unsafe {
        cmd.pre_exec(move || {
            use std::os::fd::AsRawFd;
            mountns::enter(&plan, Some(writer.as_raw_fd()))
        });
    }
    match cmd.status() {
        Ok(status) if status.code() == Some(0) => Ok(()),
        Ok(status) if status.code() == Some(10) => {
            Err("a read-only bind mount did not stop a write".into())
        }
        Ok(status) => Err(format!("the probe failed ({status})")),
        Err(e) => Err(match mountplan::read_failure(&reader) {
            Some(failure) => {
                failure.describe(&[dir.path().join("pin"), dir.path().join("pin/file")])
            }
            None => format!("the probe could not start: {e}"),
        }),
    }
}

/// `<temp>/harness-userns-probe-<pid>-<n>/pin/file`, removed on drop.
struct ProbeDir(PathBuf);

impl ProbeDir {
    fn create() -> std::io::Result<ProbeDir> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "harness-userns-probe-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("pin"))?;
        let dir = ProbeDir(dir.canonicalize()?);
        std::fs::write(dir.0.join("pin/file"), b"probe")?;
        Ok(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
```

- [ ] **Step 6: Run the setup in pre_exec**

In `crates/harness-sandbox/src/linux/preexec.rs`:

Replace (1 of 4):

```rust
//!   `landlock_restrict_self` and, inside [`fdcleanup`], for `close_range`
//!   and `getdents64`), [`libc::open`], [`libc::fcntl`], [`libc::close`] are
//!   all thin wrappers around a single `syscall(2)` — no heap allocation, no
//!   userspace locking.
//! - [`seccompiler::apply_filter`] builds a `sock_fprog` on the stack that
//!   just points at the already-allocated [`seccompiler::BpfProgram`] slice
//!   (no allocation of its own) and calls `prctl`/`syscall(SYS_seccomp)`
```

with:

```rust
//!   `landlock_restrict_self` and, inside [`fdcleanup`], for `close_range`
//!   and `getdents64`), [`libc::open`], [`libc::fcntl`], [`libc::close`] are
//!   all thin wrappers around a single `syscall(2)` — no heap allocation, no
//!   userspace locking. The full tier's mount setup ([`mountns::enter`]) is
//!   made of the same kind of calls.
//! - [`seccompiler::apply_filter`] builds a `sock_fprog` on the stack that
//!   just points at the already-allocated [`seccompiler::BpfProgram`] slice
//!   (no allocation of its own) and calls `prctl`/`syscall(SYS_seccomp)`
```

Replace (2 of 4):

```rust
use seccompiler::BpfProgram;

use super::fdcleanup;

/// Everything [`apply`] needs, computed in the parent (see `fs.rs` and
/// `seccomp.rs`) before the child is forked.
```

with:

```rust
use seccompiler::BpfProgram;

use super::fdcleanup;
use super::mountns::{self, MountPlan};

/// Everything [`apply`] needs, computed in the parent (see `fs.rs` and
/// `seccomp.rs`) before the child is forked.
```

Replace (3 of 4):

```rust
    /// then `clone3`.
    pub(super) seccomp_program: BpfProgram,
    pub(super) clone3_program: BpfProgram,
}

/// Installs the sandbox in the calling process. Must only be invoked from a
```

with:

```rust
    /// then `clone3`.
    pub(super) seccomp_program: BpfProgram,
    pub(super) clone3_program: BpfProgram,
    /// The full tier's namespace and mounts, and the pipe the child reports
    /// a failed step to.
    pub(super) mounts: Option<(MountPlan, Option<OwnedFd>)>,
}

/// Installs the sandbox in the calling process. Must only be invoked from a
```

Replace (4 of 4):

```rust
    //    could otherwise deny. Fails closed: see `fdcleanup`'s module docs.
    fdcleanup::mark_inherited_fds_close_on_exec()?;

    // 3. Required before `seccomp(2)` will install a filter; applied ahead
    //    of Landlock too so nothing between here and `execve` could regain
    //    privileges (e.g. via a setuid/setgid binary) that the sandbox is
    //    about to remove.
    set_no_new_privs()?;

    // 4. Filesystem restriction. `landlock_ruleset_fd` is always present
    //    (see `PreparedSandbox`'s docs): a kernel that cannot enforce it was
    //    already rejected in the parent, before fork.
    landlock_restrict_self(prepared.landlock_ruleset_fd.as_raw_fd())?;

    // 5. Network, mount and namespace restriction. Installed last so none of
    //    the syscalls above can themselves be filtered.
    seccompiler::apply_filter(&prepared.seccomp_program).map_err(seccomp_apply_error)?;
    seccompiler::apply_filter(&prepared.clone3_program).map_err(seccomp_apply_error)?;
```

with:

```rust
    //    could otherwise deny. Fails closed: see `fdcleanup`'s module docs.
    fdcleanup::mark_inherited_fds_close_on_exec()?;

    // 3. The full tier's user and mount namespace and read-only binds. Must
    //    come before Landlock, which refuses every mount change, and before
    //    seccomp, which refuses `unshare` and the mount calls. Fails closed:
    //    the command does not run.
    if let Some((plan, report)) = &prepared.mounts {
        mountns::enter(plan, report.as_ref().map(AsRawFd::as_raw_fd))?;
    }

    // 4. Required before `seccomp(2)` will install a filter; applied ahead
    //    of Landlock too so nothing between here and `execve` could regain
    //    privileges (e.g. via a setuid/setgid binary) that the sandbox is
    //    about to remove.
    set_no_new_privs()?;

    // 5. Filesystem restriction. `landlock_ruleset_fd` is always present
    //    (see `PreparedSandbox`'s docs): a kernel that cannot enforce it was
    //    already rejected in the parent, before fork.
    landlock_restrict_self(prepared.landlock_ruleset_fd.as_raw_fd())?;

    // 6. Network, mount and namespace restriction. Installed last so none of
    //    the syscalls above can themselves be filtered.
    seccompiler::apply_filter(&prepared.seccomp_program).map_err(seccomp_apply_error)?;
    seccompiler::apply_filter(&prepared.clone3_program).map_err(seccomp_apply_error)?;
```

- [ ] **Step 7: Use the tiers in LinuxSandbox**

Replace `crates/harness-sandbox/src/linux/mod.rs` with:

```rust
//! Linux sandbox backend: a Landlock ruleset (filesystem) plus seccomp-BPF
//! programs (network, mounts, namespaces) installed from `pre_exec`, in the
//! forked child, before `execve`; in the full tier, a user and mount
//! namespace with read-only binds over git metadata first (`mountns.rs`);
//! and around every workspace-write command, the git-metadata guard
//! (`crate::guard`) with an inotify watcher (`watch.rs`).
//!
//! ## Split between parent and child
//!
//! Everything that can allocate, open files, or otherwise take locks (the
//! Landlock ruleset with its `path_beneath` rules, the compiled seccomp-BPF
//! program) is built in the **parent**, by [`fs::build_ruleset_fd`] and
//! [`seccomp::build_deny_filter`]. [`linux_sandbox_command`] hands
//! the results to [`preexec::apply`], which is the only code that runs in
//! the forked child's `pre_exec` closure. That function is restricted to
//! async-signal-safe operations: raw syscalls (`setsid`, `prctl`,
//! `landlock_restrict_self`, `seccomp`) and reads of the already-prepared
//! data. See `preexec.rs` for the full rationale.
//!
//! ## Ordering inside `pre_exec`
//!
//! 1. `setsid()` — the child becomes its own session/process-group leader,
//!    so `killpg(child_pid)` reaches grandchildren too. We deliberately do
//!    *not* also call `setpgid(0, 0)`: once `setsid()` has run, the process
//!    is already its own group leader and a subsequent `setpgid` targeting
//!    it fails with `EPERM`.
//! 2. Mark every inherited fd above stderr close-on-exec, so a writable or
//!    connectable fd cannot leak into the sandboxed program through
//!    inheritance. This runs before Landlock is restricted because it needs
//!    to open `/proc/self/fd`.
//! 3. Full tier only: unshare a user and mount namespace and set up the
//!    read-only binds (`mountns.rs`). Before Landlock and seccomp, which
//!    refuse mount changes and `unshare`.
//! 4. `prctl(PR_SET_NO_NEW_PRIVS)` — required before `seccomp(2)` will
//!    install a filter, and applied before Landlock too so nothing between
//!    here and `execve` could regain privileges via a setuid/setgid binary.
//! 5. `landlock_restrict_self` on the ruleset fd built in the parent.
//! 6. Install the seccomp-BPF programs. Last, so none of the syscalls above
//!    can be filtered by them.

mod detect;
mod fdcleanup;
mod fs;
mod mountns;
mod mountplan;
mod preexec;
mod seccomp;
mod tier;
mod watch;

use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use harness_core::tool::{
    CommandGuard, CommandSandbox, GitProtection, GuardReport, SandboxedCommand,
};
use tokio::process::Command;

use crate::guard::{GitGuard, GuardSession};
use crate::{FsAccess, SandboxPolicy, SandboxSettings};
use mountns::MountPlan;
use preexec::PreparedSandbox;

pub use detect::{landlock_abi, linux_sandbox_available};
pub use tier::linux_git_protection;

/// Builds a [`tokio::process::Command`] for `program`/`args` with `policy`'s
/// Landlock + seccomp sandbox installed via `pre_exec`, without the full
/// tier's mounts or the guard (see [`LinuxSandbox`] for those).
///
/// The Landlock ruleset and the seccomp-BPF programs are all compiled here,
/// in the caller's process, before the child ever exists; `pre_exec` only
/// has to hand already-prepared data to the kernel. See the [`linux`
/// module docs](self) for the full ordering rationale.
///
/// Returns `Err` for setup failures in *this* process (e.g. a seccomp rule
/// that failed to validate) **and** whenever the running kernel cannot
/// fully enforce the Landlock ABI-3 floor this crate requires, including a
/// kernel with no Landlock support at all — see [`fs::build_ruleset_fd`].
/// This function never hands back a command that merely *looks* sandboxed;
/// call [`linux_sandbox_available`] first if the caller wants to know
/// ahead of time whether that floor is met.
pub fn linux_sandbox_command(
    policy: &SandboxPolicy,
    program: &str,
    args: &[&str],
) -> io::Result<Command> {
    sandboxed_command(policy, program, args, None)
}

/// [`linux_sandbox_command`], plus the full tier's namespace and mounts when
/// `mounts` is set.
fn sandboxed_command(
    policy: &SandboxPolicy,
    program: &str,
    args: &[&str],
    mounts: Option<(MountPlan, Option<OwnedFd>)>,
) -> io::Result<Command> {
    let landlock_ruleset_fd = fs::build_ruleset_fd(policy)?;
    let seccomp_program = seccomp::build_deny_filter()?;
    let clone3_program = seccomp::build_clone3_filter()?;

    let prepared = PreparedSandbox {
        landlock_ruleset_fd,
        seccomp_program,
        clone3_program,
        mounts,
    };

    let mut command = Command::new(program);
    command.args(args);

    // A rejected `TMPDIR` (see `fs::tmpdir_override`) is excluded from the
    // Landlock ruleset above, but the child process would otherwise still
    // see the original, now-unwritable value in its environment; override
    // it to `/tmp`, which the ruleset always makes writable, so `mktemp`
    // and friends keep working inside the sandbox.
    if let Some(tmpdir) = fs::tmpdir_override(
        policy.access,
        std::env::var_os("TMPDIR").as_deref(),
        crate::roots::home_dir().as_deref(),
    ) {
        command.env("TMPDIR", tmpdir);
    }

    // SAFETY: `preexec::apply` performs only the async-signal-safe
    // operations documented on it (raw syscalls plus reads of `prepared`,
    // which was fully built above, in the parent, before this closure was
    // constructed). `prepared` is moved into the closure and so stays alive
    // — keeping the Landlock ruleset fd and the setup pipe open — for as
    // long as `command` does, which is at least until `fork()` happens
    // inside `spawn()`.
    unsafe {
        command.pre_exec(move || preexec::apply(&prepared));
    }

    Ok(command)
}

/// [`CommandSandbox`] backed by Landlock + seccomp, with git-metadata
/// protection in one of two tiers ([`GitProtection`]): read-only mounts in a
/// user and mount namespace plus the guard (full), or the guard alone
/// (basic).
#[derive(Debug)]
pub struct LinuxSandbox {
    settings: SandboxSettings,
    quarantine: PathBuf,
    guards: Arc<GuardSession>,
    tier: Arc<Mutex<GitProtection>>,
}

impl LinuxSandbox {
    /// A sandbox in the tier this host supports ([`linux_git_protection`]).
    pub fn new(settings: SandboxSettings) -> Self {
        Self::with_git_protection(settings, linux_git_protection())
    }

    /// A sandbox in the given tier. The full tier on a host that does not
    /// support it fails its first command, then drops to the basic tier.
    pub fn with_git_protection(settings: SandboxSettings, tier: GitProtection) -> Self {
        let quarantine = settings
            .quarantine_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("harness-quarantine"));
        LinuxSandbox {
            guards: GuardSession::new(&quarantine),
            quarantine,
            settings,
            tier: Arc::new(Mutex::new(tier)),
        }
    }

    /// The full tier's command: read-only mounts over what `index` found,
    /// and a pipe the child reports a failed mount step to. Without anything
    /// to protect, just Landlock and seccomp.
    fn mounted_command(
        &self,
        policy: &SandboxPolicy,
        index: &crate::gitmeta::GitIndex,
        program: &str,
        args: &[&str],
    ) -> io::Result<(Command, Option<SetupReport>)> {
        let Some((plan, paths)) = mountplan::build(&policy.workspace, index) else {
            return Ok((sandboxed_command(policy, program, args, None)?, None));
        };
        let (reader, writer) = mountplan::setup_pipe()?;
        let command = sandboxed_command(policy, program, args, Some((plan, Some(writer))))?;
        Ok((command, Some(SetupReport { reader, paths })))
    }
}

impl CommandSandbox for LinuxSandbox {
    fn name(&self) -> &'static str {
        "landlock+seccomp"
    }

    /// Callers must not call [`tokio::process::Command::process_group`] on
    /// the returned command: `pre_exec` calls `setsid()` (see the [`linux`
    /// module docs](self)), which fails with `EPERM` if something has
    /// already changed this process's process-group membership before it
    /// runs. `setsid()` alone already makes the child lead its own process
    /// group, which is what `process_group(0)` would otherwise be for.
    ///
    /// In the full tier this includes the read-only mounts, but not the
    /// guard: use [`prepare`](CommandSandbox::prepare) for that.
    fn command(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> io::Result<Command> {
        if access == FsAccess::ReadOnly || self.git_protection() != GitProtection::Full {
            let policy = self.settings.policy(access, workspace);
            return sandboxed_command(&policy, program, args, None);
        }
        let workspace = canonical(workspace);
        let index = crate::gitmeta::discover(&workspace, Some(&self.quarantine));
        mountplan::create_hooks_placeholders(&workspace, &index.gitdirs);
        let policy = self.settings.policy(access, &workspace);
        self.mounted_command(&policy, &index, program, args)
            .map(|(command, _)| command)
    }

    fn is_denial(&self, exit_code: Option<i32>, output: &str) -> bool {
        if crate::looks_like_sandbox_denial(exit_code, output, true) {
            return true;
        }
        if exit_code == Some(0) {
            return false;
        }
        let output = output.to_lowercase();
        // Landlock's `Refer` right, and a rename or link across one of the
        // full tier's mounts, fail with `EXDEV`; a mount point (a pinned
        // gitdir, a read-only entry) cannot be renamed or removed: `EBUSY`.
        // glibc's `strerror` wording for both is Linux-specific, so the
        // shared keyword list does not cover them.
        output.contains("invalid cross-device link") || output.contains("device or resource busy")
    }

    /// Starts the guard (workspace-write only), then builds the command: in
    /// the full tier with read-only mounts over what the guard's index
    /// found, and a pipe the child reports a failed mount step to.
    fn prepare(
        &self,
        access: FsAccess,
        workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> io::Result<SandboxedCommand> {
        if access == FsAccess::ReadOnly {
            return Ok(SandboxedCommand {
                command: self.command(access, workspace, program, args)?,
                guard: None,
            });
        }
        let full = self.git_protection() == GitProtection::Full;
        let workspace = canonical(workspace);
        let guard = self.guards.begin(&workspace, !full, |index| {
            if full {
                mountplan::create_hooks_placeholders(&workspace, &index.gitdirs);
            }
        });
        let policy = self.settings.policy(access, &workspace);
        let built = if full {
            self.mounted_command(&policy, &guard.index(), program, args)
        } else {
            sandboxed_command(&policy, program, args, None).map(|command| (command, None))
        };
        let (command, setup) = match built {
            Ok(built) => built,
            Err(e) => {
                // Nothing ran; finishing still records where things stand
                // for the next command's check.
                let _ = guard.finish();
                return Err(e);
            }
        };
        let watcher = watch::Watcher::start(guard.watch_handle()).ok();
        Ok(SandboxedCommand {
            command,
            guard: Some(Box::new(LinuxGuard {
                guard,
                watcher,
                setup,
                tier: Arc::clone(&self.tier),
            })),
        })
    }

    fn git_protection(&self) -> GitProtection {
        self.tier
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn canonical(workspace: &Path) -> PathBuf {
    std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
}

/// The read end of the setup pipe, and the plan's ops' paths for messages.
struct SetupReport {
    reader: OwnedFd,
    paths: Vec<PathBuf>,
}

/// The guard for one command, its watcher, and the full tier's setup pipe.
struct LinuxGuard {
    guard: GitGuard,
    watcher: Option<watch::Watcher>,
    setup: Option<SetupReport>,
    tier: Arc<Mutex<GitProtection>>,
}

impl CommandGuard for LinuxGuard {
    /// Stops the watcher, runs the guard's final checks, and, if the child
    /// reported a failed mount step, drops the session to the basic tier.
    fn finish(self: Box<Self>) -> Option<GuardReport> {
        let LinuxGuard {
            guard,
            watcher,
            setup,
            tier,
        } = *self;
        if let Some(watcher) = watcher {
            watcher.stop();
        }
        let mut report = guard.finish();
        let failure = setup.and_then(|setup| {
            mountplan::read_failure(&setup.reader).map(|failure| failure.describe(&setup.paths))
        });
        if let Some(failure) = failure {
            *tier.lock().unwrap_or_else(PoisonError::into_inner) = GitProtection::Basic {
                reason: format!("the full tier's setup failed during the session: {failure}"),
            };
            let note = format!(
                "[the sandbox could not set up its read-only mounts ({failure}), so this command did not run. This session now uses the basic tier, which checks git metadata after each command instead. Run the command again.]\n"
            );
            match &mut report {
                Some(report) => report.message.push_str(&note),
                None => {
                    report = Some(GuardReport {
                        message: note,
                        blocked: false,
                    })
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::tool::CommandSandbox;

    fn sandbox() -> LinuxSandbox {
        LinuxSandbox::with_git_protection(
            crate::SandboxSettings::default(),
            GitProtection::Basic {
                reason: "test".into(),
            },
        )
    }

    #[test]
    fn cross_device_link_message_is_a_denial() {
        assert!(sandbox().is_denial(
            Some(1),
            "mv: cannot move 'a' to 'b': Invalid cross-device link\n"
        ));
    }

    #[test]
    fn success_is_never_a_denial_even_with_the_keyword() {
        assert!(!sandbox().is_denial(Some(0), "Invalid cross-device link\n"));
    }

    #[test]
    fn a_busy_mount_point_is_a_denial() {
        assert!(sandbox().is_denial(
            Some(1),
            "mv: cannot move '.git' to 'g': Device or resource busy\n"
        ));
    }

    #[test]
    fn plain_failure_without_any_keyword_is_not_a_denial() {
        assert!(!sandbox().is_denial(Some(1), "some ordinary error\n"));
    }
}
```

`crates/harness-sandbox/src/lib.rs`:

Replace (1 of 3):

```rust
//! OS sandboxes for shell commands: Seatbelt (`sandbox-exec`) on macOS and Landlock + seccomp on Linux.
//! Both implement [`harness_core::tool::CommandSandbox`]; [`detect`] picks the one this host supports.

mod denial;
pub mod gitmeta;
```

with:

```rust
//! OS sandboxes for shell commands: Seatbelt (`sandbox-exec`) on macOS and Landlock + seccomp on Linux.
//! Both implement [`harness_core::tool::CommandSandbox`]; [`detect`] picks the one this host supports.
//! On Linux, git metadata inside the workspace is protected by read-only mounts where user
//! namespaces work (the full tier) and by the [`guard`] in either tier.

mod denial;
pub mod gitmeta;
```

Replace (2 of 3):

```rust
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub use linux::{LinuxSandbox, landlock_abi, linux_sandbox_available, linux_sandbox_command};
#[cfg(target_os = "macos")]
pub use macos::{Seatbelt, seatbelt_available, seatbelt_command};
pub use policy::{FsAccess, SandboxPolicy};
```

with:

```rust
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub use linux::{
    LinuxSandbox, landlock_abi, linux_git_protection, linux_sandbox_available,
    linux_sandbox_command,
};
#[cfg(target_os = "macos")]
pub use macos::{Seatbelt, seatbelt_available, seatbelt_command};
pub use policy::{FsAccess, SandboxPolicy};
```

Replace (3 of 3):

```rust
}

/// The OS sandbox this host supports, or `None` when none is usable (the caller must then ask before
/// every shell command).
pub fn detect(settings: SandboxSettings) -> Option<Arc<dyn CommandSandbox>> {
    #[cfg(target_os = "macos")]
    if seatbelt_available() {
```

with:

```rust
}

/// The OS sandbox this host supports, or `None` when none is usable (the caller must then ask before
/// every shell command). On Linux this probes the git-protection tier, once per process.
pub fn detect(settings: SandboxSettings) -> Option<Arc<dyn CommandSandbox>> {
    #[cfg(target_os = "macos")]
    if seatbelt_available() {
```

- [ ] **Step 8: Lint and run what runs here**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && bash target/linux-lint.sh && cargo test -p harness-sandbox`
Expected: clean and passing. On Linux (CI): with user namespaces allowed, every `full_tier_*` test, `a_working_directory_inside_dot_git_still_sees_the_mounts` and `the_probe_picks_the_expected_tier` pass; on stock `ubuntu-24.04`, the full-tier tests skip, `the_probe_picks_the_expected_tier` asserts the basic tier, and `a_failed_mount_setup_drops_the_session_to_the_basic_tier` passes. The `mountns` unit tests pass on both.

- [ ] **Step 9: Commit**

```bash
git add crates/harness-sandbox
git commit -F - <<'EOF'
feat(sandbox): mount git metadata read-only where user namespaces work

A probe picks the tier. In the full tier the forked child unshares a
user and mount namespace, pins every gitdir with a read-write self-bind
and binds each protected entry read-only (with the fd-based mount API,
never following symlinks), before no_new_privs, Landlock and seccomp.
A missing hooks directory gets an empty placeholder first. A failed
setup step stops the command, is reported through a pipe, and drops the
session to the basic tier.

<trailer lines from the controller>
EOF
```

---

### Task 9: `sandbox.linux_git_protection`

**Files:**
- Modify: `crates/harness-config/src/config.rs`, `crates/harness-config/tests/config.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `harness_config::config::LinuxGitProtection::{BestEffort (default), Required}` (TOML `"best-effort"`, `"required"`); `SandboxConfig.linux_git_protection: Option<LinuxGitProtection>`; `Config.linux_git_protection: LinuxGitProtection`. A project's `"required"` always applies; a project's `"best-effort"` is the widening item `sandbox.linux_git_protection = "best-effort"` when the global value is `"required"` (so it needs trust and is in the fingerprint), and otherwise changes nothing.

- [ ] **Step 1: Write the failing tests**

Append to `crates/harness-config/tests/config.rs`, after a blank line:

```rust
#[test]
fn linux_git_protection_defaults_to_best_effort_and_reads_both_values() {
    use config::LinuxGitProtection;
    let (cfg, _) = load_project(None, "", false);
    assert_eq!(cfg.linux_git_protection, LinuxGitProtection::BestEffort);
    let (cfg, _) = load_project(
        Some("[sandbox]\nlinux_git_protection = \"required\"\n"),
        "",
        false,
    );
    assert_eq!(cfg.linux_git_protection, LinuxGitProtection::Required);
    let (cfg, _) = load_project(
        Some("[sandbox]\nlinux_git_protection = \"best-effort\"\n"),
        "",
        false,
    );
    assert_eq!(cfg.linux_git_protection, LinuxGitProtection::BestEffort);
}

#[test]
fn an_unknown_linux_git_protection_value_reports_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "[sandbox]\nlinux_git_protection = \"strict\"\n").unwrap();
    let err = config::load(&file, dir.path(), &TrustStore::default())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("config.toml") && err.contains("line 2"),
        "{err}"
    );
    assert!(
        err.contains("best-effort") && err.contains("required"),
        "{err}"
    );
}

#[test]
fn a_project_may_require_linux_git_protection_without_trust() {
    use config::LinuxGitProtection;
    let (cfg, widening) = load_project(
        None,
        "[sandbox]\nlinux_git_protection = \"required\"\n",
        false,
    );
    assert_eq!(cfg.linux_git_protection, LinuxGitProtection::Required);
    assert!(widening.is_none());
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn a_project_relaxing_required_git_protection_needs_trust() {
    use config::LinuxGitProtection;
    let global = "[sandbox]\nlinux_git_protection = \"required\"\n";
    let project = "[sandbox]\nlinux_git_protection = \"best-effort\"\n";
    let (cfg, widening) = load_project(Some(global), project, false);
    assert_eq!(cfg.linux_git_protection, LinuxGitProtection::Required);
    let widening = widening.expect("relaxing the global setting widens");
    assert_eq!(
        widening.items,
        ["sandbox.linux_git_protection = \"best-effort\""]
    );
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);

    // The same value as the global one changes nothing, so it needs no trust.
    let (_, widening) = load_project(None, project, false);
    assert!(widening.is_none());
}

#[test]
fn a_trusted_project_may_relax_required_git_protection() {
    use config::LinuxGitProtection;
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(&global, "[sandbox]\nlinux_git_protection = \"required\"\n").unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    std::fs::write(
        ws.join(".harness/config.toml"),
        "[sandbox]\nlinux_git_protection = \"best-effort\"\n",
    )
    .unwrap();
    let widening = config::project_widening(&global, &ws).unwrap().unwrap();
    let mut trust = TrustStore::load(&dir.path().join("data")).unwrap();
    trust.trust(&ws, &widening.fingerprint).unwrap();
    let cfg = config::load(&global, &ws, &trust).unwrap();
    assert_eq!(cfg.linux_git_protection, LinuxGitProtection::BestEffort);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-config --test config`
Expected: FAIL to compile: ``unresolved import `config::LinuxGitProtection` ``.

- [ ] **Step 3: Add the setting**

In `crates/harness-config/src/config.rs`:

Replace (1 of 8):

```rust
    pub read_dirs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default)]
    pub writable_roots: Vec<String>,
    pub allow_localhost: Option<bool>,
}

/// One `config.toml` file as written by the user.
```

with:

```rust
    pub read_dirs: Vec<String>,
}

/// `sandbox.linux_git_protection`: what to do on Linux when user namespaces are unavailable, so
/// git metadata is protected only after each command (the basic tier).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinuxGitProtection {
    /// Run commands in the basic tier after a startup warning.
    #[default]
    BestEffort,
    /// Treat the basic tier as no sandbox: every shell command asks first.
    Required,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default)]
    pub writable_roots: Vec<String>,
    pub allow_localhost: Option<bool>,
    pub linux_git_protection: Option<LinuxGitProtection>,
}

/// One `config.toml` file as written by the user.
```

Replace (2 of 8):

```rust
    pub read_dirs: Vec<PathBuf>,
    pub writable_roots: Vec<PathBuf>,
    pub allow_localhost: bool,
    pub warnings: Vec<String>,
}

```

with:

```rust
    pub read_dirs: Vec<PathBuf>,
    pub writable_roots: Vec<PathBuf>,
    pub allow_localhost: bool,
    pub linux_git_protection: LinuxGitProtection,
    pub warnings: Vec<String>,
}

```

Replace (3 of 8):

```rust
    pub fingerprint: String,
}

/// The mode and step limit in effect without the project config: the global config's, or the
/// defaults. A project setting that does not go beyond them narrows and needs no trust.
#[derive(Debug, Clone, Copy)]
struct Baseline {
    mode: Mode,
    max_steps: u32,
}

impl Baseline {
```

with:

```rust
    pub fingerprint: String,
}

/// The mode, step limit and Linux git protection in effect without the project config: the global
/// config's, or the defaults. A project setting that does not go beyond them narrows and needs no
/// trust.
#[derive(Debug, Clone, Copy)]
struct Baseline {
    mode: Mode,
    max_steps: u32,
    linux_git_protection: LinuxGitProtection,
}

impl Baseline {
```

Replace (4 of 8):

```rust
            max_steps: global
                .and_then(|g| g.max_steps)
                .unwrap_or(DEFAULT_MAX_STEPS),
        }
    }
}
```

with:

```rust
            max_steps: global
                .and_then(|g| g.max_steps)
                .unwrap_or(DEFAULT_MAX_STEPS),
            linux_git_protection: global
                .and_then(|g| g.sandbox.linux_git_protection)
                .unwrap_or_default(),
        }
    }
}
```

Replace (5 of 8):

```rust
    if let Some(true) = project.sandbox.allow_localhost {
        items.push("sandbox.allow_localhost = true".to_string());
    }
    if items.is_empty() {
        return None;
    }
```

with:

```rust
    if let Some(true) = project.sandbox.allow_localhost {
        items.push("sandbox.allow_localhost = true".to_string());
    }
    if project.sandbox.linux_git_protection == Some(LinuxGitProtection::BestEffort)
        && baseline.linux_git_protection == LinuxGitProtection::Required
    {
        items.push("sandbox.linux_git_protection = \"best-effort\"".to_string());
    }
    if items.is_empty() {
        return None;
    }
```

Replace (6 of 8):

```rust
        cfg.read_dirs = expand_all(&global.permissions.read_dirs, base, home);
        cfg.writable_roots = expand_all(&global.sandbox.writable_roots, base, home);
        cfg.allow_localhost = global.sandbox.allow_localhost.unwrap_or(false);
    }
    let path = project_file(workspace);
    if let Some(project) = parse_file(&path)? {
```

with:

```rust
        cfg.read_dirs = expand_all(&global.permissions.read_dirs, base, home);
        cfg.writable_roots = expand_all(&global.sandbox.writable_roots, base, home);
        cfg.allow_localhost = global.sandbox.allow_localhost.unwrap_or(false);
        cfg.linux_git_protection = global.sandbox.linux_git_protection.unwrap_or_default();
    }
    let path = project_file(workspace);
    if let Some(project) = parse_file(&path)? {
```

Replace (7 of 8):

```rust
        if let Some(false) = project.sandbox.allow_localhost {
            cfg.allow_localhost = false;
        }
        match widening(&project, baseline) {
            None => {}
            Some(w) if trust.is_trusted(workspace, &w.fingerprint) => {
```

with:

```rust
        if let Some(false) = project.sandbox.allow_localhost {
            cfg.allow_localhost = false;
        }
        if let Some(LinuxGitProtection::Required) = project.sandbox.linux_git_protection {
            cfg.linux_git_protection = LinuxGitProtection::Required;
        }
        match widening(&project, baseline) {
            None => {}
            Some(w) if trust.is_trusted(workspace, &w.fingerprint) => {
```

Replace (8 of 8):

```rust
                if let Some(allow) = project.sandbox.allow_localhost {
                    cfg.allow_localhost = allow;
                }
            }
            Some(w) => cfg.warnings.push(format!(
                "{}: ignoring {} setting(s) that widen what the agent may do ({}); run `harness trust` to review and apply them",
```

with:

```rust
                if let Some(allow) = project.sandbox.allow_localhost {
                    cfg.allow_localhost = allow;
                }
                if let Some(protection) = project.sandbox.linux_git_protection {
                    cfg.linux_git_protection = protection;
                }
            }
            Some(w) => cfg.warnings.push(format!(
                "{}: ignoring {} setting(s) that widen what the agent may do ({}); run `harness trust` to review and apply them",
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p harness-config`
Expected: PASS, including the five new tests.

- [ ] **Step 5: Lint**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/harness-config
git commit -F - <<'EOF'
feat(config): add sandbox.linux_git_protection

"best-effort" (the default) runs commands in the Linux basic tier;
"required" makes that tier ask before every command. A project may
always require it; relaxing a global "required" is a widening setting
that needs trust.

<trailer lines from the controller>
EOF
```

---

### Task 10: The CLI: the basic-tier warning, `"required"` and `harness sandbox doctor`

**Files:**
- Create: `crates/harness-cli/src/sandbox.rs`, `crates/harness-cli/src/doctor.rs`
- Modify: `crates/harness-cli/src/main.rs`, `crates/harness-cli/src/ask.rs`, `crates/harness-cli/tests/sandbox_e2e.rs`, `crates/harness-cli/tests/cli_smoke.rs`, `crates/harness-sandbox/src/lib.rs`, `crates/harness-sandbox/src/linux/mod.rs`, `crates/harness-sandbox/tests/linux_git_guard.rs`

**Interfaces:**
- Consumes: `CommandSandbox::git_protection` (Task 3); `LinuxSandbox` (Task 8); `Config.linux_git_protection` (Task 9); `Paths.data_dir`.
- Produces:
  - `SandboxSettings.require_full_git_protection: bool`: `LinuxSandbox::prepare` refuses a workspace-write command in the basic tier (reached only after the session dropped to it).
  - `harness_sandbox::unavailable_reason() -> String`.
  - `crate::sandbox::{Choice { sandbox, warning }, choose(detected, access, required) -> Choice}` in `harness-cli`: in the basic tier with workspace-write access, a warning naming `harness sandbox doctor`, and with `required` no sandbox (every command asks); read-only access is unaffected.
  - `harness sandbox doctor`: `crate::doctor::{run() -> u8, render(&Facts) -> String, Facts, Host}`; exit code 0.

- [ ] **Step 1: Write the failing tests**

In `crates/harness-cli/tests/cli_smoke.rs`:

Replace (1 of 2):

```rust
use assert_cmd::Command;
use predicates::str::contains;

#[test]
```

with:

```rust
use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;

#[test]
```

Replace (2 of 2):

```rust
        .success()
        .stdout(contains("harness 0.1.0"));
}
```

with:

```rust
        .success()
        .stdout(contains("harness 0.1.0"));
}

#[test]
fn help_lists_the_sandbox_doctor() {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("sandbox").and(contains("Inspect the OS sandbox")));
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["sandbox", "--help"])
        .assert()
        .success()
        .stdout(contains("doctor").and(contains(
            "Show which sandbox this system gets, how git metadata is protected, and how to improve it",
        )));
}
```

In `crates/harness-cli/tests/sandbox_e2e.rs` (this also retires the Linux `ignore` on the hook test, which now passes in both tiers):

Replace (1 of 3):

```rust
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::{Value, json};
use tempfile::TempDir;
```

with:

```rust
use std::process::Command as StdCommand;

use assert_cmd::Command;
use harness_core::tool::GitProtection;
use predicates::str::contains;
use serde_json::{Value, json};
use tempfile::TempDir;
```

Replace (2 of 3):

```rust
    ok
}

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
```

with:

```rust
    ok
}

/// On Linux with a sandbox, whether this host gets the basic git-protection tier (`Some(true)`)
/// or the full one (`Some(false)`); `None` elsewhere. With `HARNESS_EXPECT_LINUX_TIER` set (CI
/// sets `basic` or `full`), any other tier fails the test.
fn linux_basic_tier() -> Option<bool> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let protection =
        harness_sandbox::detect(harness_sandbox::SandboxSettings::default())?.git_protection();
    let basic = matches!(protection, GitProtection::Basic { .. });
    let expected = std::env::var("HARNESS_EXPECT_LINUX_TIER").unwrap_or_default();
    if !expected.is_empty() {
        let tier = if basic { "basic" } else { "full" };
        assert_eq!(tier, expected, "{protection:?}");
    }
    Some(basic)
}

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
```

Replace (3 of 3):

```rust
    assert!(String::from_utf8_lossy(&log.stdout).contains("sandboxed"));
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "Landlock cannot protect .git/hooks inside a writable workspace; see README Known limitations"
)]
async fn planting_a_git_hook_fails_in_the_sandbox() {
    if !host_has_sandbox() {
        return;
    }
    let (env, out) = run_bash("echo 'echo pwned' > .git/hooks/pre-commit", "", |_| {}).await;
    assert!(!env.ws.path().join(".git/hooks/pre-commit").exists());
    assert_eq!(out.status.code(), Some(3), "{}", tool_output(&out));
    assert!(
        tool_output(&out).contains("[the sandbox may have blocked"),
        "{}",
        tool_output(&out)
    );
}

#[tokio::test(flavor = "multi_thread")]
```

with:

```rust
    assert!(String::from_utf8_lossy(&log.stdout).contains("sandboxed"));
}

/// Macos and the Linux full tier refuse the write; the Linux basic tier moves the hook to
/// quarantine after the command. Either way the command counts as blocked.
#[tokio::test(flavor = "multi_thread")]
async fn planting_a_git_hook_fails_in_the_sandbox() {
    if !host_has_sandbox() {
        return;
    }
    let basic = linux_basic_tier() == Some(true);
    let (env, out) = run_bash("echo 'echo pwned' > .git/hooks/pre-commit", "", |_| {}).await;
    assert!(!env.ws.path().join(".git/hooks/pre-commit").exists());
    assert_eq!(out.status.code(), Some(3), "{}", tool_output(&out));
    let expected = if basic {
        "- .git/hooks/pre-commit: new in a protected directory; moved to "
    } else {
        "[the sandbox may have blocked"
    };
    assert!(
        tool_output(&out).contains(expected),
        "{}",
        tool_output(&out)
    );
    if basic {
        let quarantine = env.home.path().join("data/quarantine");
        let moved = std::fs::read_dir(&quarantine)
            .unwrap()
            .map(|d| d.unwrap().path().join(".git/hooks/pre-commit"))
            .find(|p| p.exists())
            .expect("the hook is in the quarantine");
        assert_eq!(std::fs::read_to_string(moved).unwrap(), "echo pwned\n");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn git_init_of_a_nested_repository_is_undone() {
    if !host_has_sandbox() {
        return;
    }
    let (env, out) = run_bash("git init -q sub", "", |_| {}).await;
    assert!(!env.ws.path().join("sub/.git").exists());
    assert_eq!(out.status.code(), Some(3), "{}", tool_output(&out));
    if cfg!(target_os = "linux") {
        assert!(
            tool_output(&out).contains("- sub/.git: a new repository; moved to "),
            "{}",
            tool_output(&out)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_linux_basic_tier_warns_at_startup() {
    let Some(basic) = linux_basic_tier() else {
        return;
    };
    let (_env, out) = run_bash("true", "", |_| {}).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.contains("run `harness sandbox doctor`"),
        basic,
        "{stderr}"
    );
    assert_eq!(out.status.code(), Some(0), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn required_git_protection_asks_before_every_command_in_the_basic_tier() {
    let Some(basic) = linux_basic_tier() else {
        return;
    };
    let (env, out) = run_bash(
        "touch made.txt",
        "[sandbox]\nlinux_git_protection = \"required\"\n",
        |_| {},
    )
    .await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if basic {
        assert_eq!(out.status.code(), Some(3), "{stderr}");
        assert!(
            stderr.contains("every shell command will need approval"),
            "{stderr}"
        );
        assert!(!env.ws.path().join("made.txt").exists());
    } else {
        assert_eq!(out.status.code(), Some(0), "{stderr}");
        assert!(env.ws.path().join("made.txt").exists());
    }
}

#[test]
fn sandbox_doctor_reports_the_mechanism_and_tier() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .args(["sandbox", "doctor"])
        .current_dir(ws.path())
        .env("HARNESS_HOME", home.path())
        .env_remove("HARNESS_SANDBOX")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("Sandbox: "), "{stdout}");
    match linux_basic_tier() {
        Some(true) => {
            assert!(
                stdout.contains("Git metadata protection: basic tier\n  Why: "),
                "{stdout}"
            );
            assert!(
                stdout.contains("harness does not change any of these settings itself."),
                "{stdout}"
            );
            let apparmor =
                std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
                    .is_ok_and(|v| v.trim() == "1");
            if apparmor {
                assert!(
                    stdout.contains("sudo apparmor_parser -r /etc/apparmor.d/harness"),
                    "{stdout}"
                );
                assert!(
                    stdout
                        .contains("sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0"),
                    "{stdout}"
                );
            }
        }
        Some(false) => assert!(stdout.contains("full tier"), "{stdout}"),
        None if cfg!(target_os = "macos") => {
            assert!(stdout.starts_with("Sandbox: seatbelt\n"), "{stdout}")
        }
        None => {}
    }
}

#[tokio::test(flavor = "multi_thread")]
```

In `crates/harness-sandbox/tests/linux_git_guard.rs`:

Replace:

```rust
    assert!(!env.exists(".git/commondir"));
}

// ---------------------------------------------------------------------------
// The full tier: writes fail
// ---------------------------------------------------------------------------
```

with:

```rust
    assert!(!env.exists(".git/commondir"));
}

#[tokio::test]
async fn required_protection_refuses_to_run_in_the_basic_tier() {
    let Some(env) = Env::new() else { return };
    let settings = SandboxSettings {
        require_full_git_protection: true,
        ..env.settings()
    };
    let sandbox = LinuxSandbox::with_git_protection(
        settings,
        GitProtection::Basic {
            reason: "forced by the test".into(),
        },
    );
    let err = sandbox
        .prepare(
            FsAccess::WorkspaceWrite,
            &env.ws,
            "/bin/sh",
            &["-c", "touch ran"],
        )
        .err()
        .expect("the basic tier is refused");
    assert!(err.to_string().contains("linux_git_protection"), "{err}");
    // A read-only sandbox protects git metadata completely, so it still runs.
    assert!(
        sandbox
            .prepare(FsAccess::ReadOnly, &env.ws, "/bin/sh", &["-c", "true"])
            .is_ok()
    );
}

// ---------------------------------------------------------------------------
// The full tier: writes fail
// ---------------------------------------------------------------------------
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p harness-cli --test cli_smoke --test sandbox_e2e; bash target/linux-lint.sh`
Expected: `help_lists_the_sandbox_doctor` and `sandbox_doctor_reports_the_mechanism_and_tier` FAIL (`unrecognized subcommand 'sandbox'`); the lint FAILS: ``struct `harness_sandbox::SandboxSettings` has no field named `require_full_git_protection` ``.

- [ ] **Step 3: Add the setting and the reason to harness-sandbox**

`crates/harness-sandbox/src/lib.rs`:

Replace (1 of 3):

```rust
    /// Where the Linux git-metadata guard moves what it takes out of the workspace. The CLI
    /// passes `<data dir>/quarantine`; `None` means `harness-quarantine` in the temp directory.
    pub quarantine_dir: Option<PathBuf>,
}

impl SandboxSettings {
```

with:

```rust
    /// Where the Linux git-metadata guard moves what it takes out of the workspace. The CLI
    /// passes `<data dir>/quarantine`; `None` means `harness-quarantine` in the temp directory.
    pub quarantine_dir: Option<PathBuf>,
    /// `sandbox.linux_git_protection = "required"`: on Linux, refuse to run a workspace-write
    /// command in the basic tier (the CLI then treats the session as having no sandbox).
    pub require_full_git_protection: bool,
}

impl SandboxSettings {
```

Replace (2 of 3):

```rust
    roots::safe_root(workspace, roots::home_dir().as_deref()).is_none()
}

/// The OS sandbox this host supports, or `None` when none is usable (the caller must then ask before
/// every shell command). On Linux this probes the git-protection tier, once per process.
pub fn detect(settings: SandboxSettings) -> Option<Arc<dyn CommandSandbox>> {
```

with:

```rust
    roots::safe_root(workspace, roots::home_dir().as_deref()).is_none()
}

/// Why [`detect`] finds no sandbox on this host, for `harness sandbox doctor`.
pub fn unavailable_reason() -> String {
    #[cfg(target_os = "macos")]
    {
        format!(
            "{} is missing or cannot apply a profile",
            macos::SANDBOX_EXEC_PATH
        )
    }
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        match landlock_abi() {
            None => "Landlock is not enabled in this kernel".to_string(),
            Some(abi) if abi < 3 => format!(
                "this kernel has Landlock ABI {abi}; harness needs ABI 3 or later (Linux 6.2+)"
            ),
            Some(_) => "the seccomp filter could not be built for this system".to_string(),
        }
    }
    #[cfg(not(any(
        target_os = "macos",
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    )))]
    {
        "harness has no sandbox for this system".to_string()
    }
}

/// The OS sandbox this host supports, or `None` when none is usable (the caller must then ask before
/// every shell command). On Linux this probes the git-protection tier, once per process.
pub fn detect(settings: SandboxSettings) -> Option<Arc<dyn CommandSandbox>> {
```

Replace (3 of 3):

```rust
        assert!(!workspace_is_too_broad(dir.path()));
        assert!(workspace_is_too_broad(&dir.path().join("missing")));
    }
}
```

with:

```rust
        assert!(!workspace_is_too_broad(dir.path()));
        assert!(workspace_is_too_broad(&dir.path().join("missing")));
    }

    #[test]
    fn there_is_always_a_reason_to_give_for_having_no_sandbox() {
        let reason = unavailable_reason();
        assert!(!reason.is_empty());
        if cfg!(target_os = "macos") {
            assert!(reason.starts_with("/usr/bin/sandbox-exec "), "{reason}");
        }
    }
}
```

`crates/harness-sandbox/src/linux/mod.rs`:

Replace:

```rust
                guard: None,
            });
        }
        let full = self.git_protection() == GitProtection::Full;
        let workspace = canonical(workspace);
        let guard = self.guards.begin(&workspace, !full, |index| {
            if full {
```

with:

```rust
                guard: None,
            });
        }
        let tier = self.git_protection();
        if let GitProtection::Basic { reason } = &tier
            && self.settings.require_full_git_protection
        {
            return Err(io::Error::other(format!(
                "git metadata protection is required (sandbox.linux_git_protection = \"required\"), but the full tier is unavailable: {reason}; restart harness to have every command ask first"
            )));
        }
        let full = tier == GitProtection::Full;
        let workspace = canonical(workspace);
        let guard = self.guards.begin(&workspace, !full, |index| {
            if full {
```

- [ ] **Step 4: Choose the session's sandbox**

`crates/harness-cli/src/sandbox.rs`:

```rust
//! Which sandbox a session gets once the Linux git-protection tier is known, and what to warn.

use std::sync::Arc;

use harness_core::{
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
}
```

In `crates/harness-cli/src/ask.rs`:

Replace (1 of 3):

```rust
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use harness_config::config;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
```

with:

```rust
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use harness_config::config::{self, LinuxGitProtection};
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
```

Replace (2 of 3):

```rust
use tokio_util::sync::CancellationToken;

use crate::{
    models, prompt, setup,
    term::{terminal_safe, terminal_safe_text},
};

```

with:

```rust
use tokio_util::sync::CancellationToken;

use crate::{
    models, prompt, sandbox, setup,
    term::{terminal_safe, terminal_safe_text},
};

```

Replace (3 of 3):

```rust
    // read-only modes keep their read-only sandbox.
    let workspace_too_broad = mode.fs_access() == FsAccess::WorkspaceWrite
        && harness_sandbox::workspace_is_too_broad(&setup.workspace);
    let sandbox = if mode == Mode::FullAccess || sandbox_disabled_by_env || workspace_too_broad {
        None
    } else {
        harness_sandbox::detect(harness_sandbox::SandboxSettings {
            extra_writable: setup.config.writable_roots.clone(),
            allow_localhost: setup.config.allow_localhost,
            quarantine_dir: Some(setup.paths.data_dir.join("quarantine")),
        })
    };
    let sandboxed = sandbox.is_some();
    if mode != Mode::FullAccess && !sandboxed {
        if sandbox_disabled_by_env {
            eprintln!(
                "warning: the sandbox is disabled by HARNESS_SANDBOX=none; every shell command will need approval"
```

with:

```rust
    // read-only modes keep their read-only sandbox.
    let workspace_too_broad = mode.fs_access() == FsAccess::WorkspaceWrite
        && harness_sandbox::workspace_is_too_broad(&setup.workspace);
    let required = setup.config.linux_git_protection == LinuxGitProtection::Required;
    let detected = if mode == Mode::FullAccess || sandbox_disabled_by_env || workspace_too_broad {
        None
    } else {
        harness_sandbox::detect(harness_sandbox::SandboxSettings {
            extra_writable: setup.config.writable_roots.clone(),
            allow_localhost: setup.config.allow_localhost,
            quarantine_dir: Some(setup.paths.data_dir.join("quarantine")),
            require_full_git_protection: required,
        })
    };
    let choice = sandbox::choose(detected, mode.fs_access(), required);
    if let Some(warning) = &choice.warning {
        eprintln!("warning: {}", terminal_safe(warning));
    }
    let sandbox = choice.sandbox;
    let sandboxed = sandbox.is_some();
    if mode != Mode::FullAccess && !sandboxed && choice.warning.is_none() {
        if sandbox_disabled_by_env {
            eprintln!(
                "warning: the sandbox is disabled by HARNESS_SANDBOX=none; every shell command will need approval"
```

- [ ] **Step 5: Add the doctor**

`crates/harness-cli/src/doctor.rs`:

```rust
//! `harness sandbox doctor`: which sandbox this system gets, how git metadata is protected, and
//! how to get the Linux full tier. It only reads; it never changes system files.

use std::path::{Path, PathBuf};

use harness_config::config::LinuxGitProtection;
use harness_core::tool::GitProtection;

use crate::{setup, term::terminal_safe};

pub fn run() -> u8 {
    let (setting, quarantine) = match setup::load() {
        Ok(setup) => (
            setup.config.linux_git_protection,
            Some(setup.paths.data_dir.join("quarantine")),
        ),
        Err(message) => {
            eprintln!("warning: {}", terminal_safe(&message));
            (LinuxGitProtection::default(), None)
        }
    };
    let detected = harness_sandbox::detect(harness_sandbox::SandboxSettings::default());
    let facts = Facts {
        linux: cfg!(target_os = "linux"),
        mechanism: detected.as_ref().map(|s| s.name()),
        unavailable: detected.is_none().then(harness_sandbox::unavailable_reason),
        protection: detected.as_ref().map(|s| s.git_protection()),
        disabled: std::env::var("HARNESS_SANDBOX").as_deref() == Ok("none"),
        setting,
        quarantine,
        host: Host::read(),
        exe: std::env::current_exe()
            .ok()
            .and_then(|exe| exe.canonicalize().ok()),
    };
    print!("{}", render(&facts));
    0
}

/// What the report is made from.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub linux: bool,
    pub mechanism: Option<&'static str>,
    pub unavailable: Option<String>,
    pub protection: Option<GitProtection>,
    pub disabled: bool,
    pub setting: LinuxGitProtection,
    pub quarantine: Option<PathBuf>,
    pub host: Host,
    pub exe: Option<PathBuf>,
}

/// Kernel settings and container markers that decide whether user namespaces work.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Host {
    /// `kernel.apparmor_restrict_unprivileged_userns` (Ubuntu).
    pub apparmor_restrict: Option<String>,
    /// `kernel.unprivileged_userns_clone` (Debian's older kernels).
    pub userns_clone: Option<String>,
    /// `user.max_user_namespaces`.
    pub max_user_namespaces: Option<String>,
    /// A Docker or Podman container marker exists.
    pub container: bool,
}

impl Host {
    fn read() -> Host {
        let sysctl = |path: &str| {
            std::fs::read_to_string(path)
                .ok()
                .map(|value| value.trim().to_string())
        };
        Host {
            apparmor_restrict: sysctl("/proc/sys/kernel/apparmor_restrict_unprivileged_userns"),
            userns_clone: sysctl("/proc/sys/kernel/unprivileged_userns_clone"),
            max_user_namespaces: sysctl("/proc/sys/user/max_user_namespaces"),
            container: Path::new("/.dockerenv").exists()
                || Path::new("/run/.containerenv").exists(),
        }
    }
}

/// The report. Every value that comes from the system is escaped for the terminal.
pub fn render(facts: &Facts) -> String {
    let mut out = String::new();
    match facts.mechanism {
        Some(name) => out.push_str(&format!("Sandbox: {name}\n")),
        None => out.push_str(&format!(
            "Sandbox: none ({}); every shell command will need approval\n",
            terminal_safe(facts.unavailable.as_deref().unwrap_or("unknown reason"))
        )),
    }
    if facts.disabled {
        out.push_str(
            "HARNESS_SANDBOX=none is set, so harness runs without the sandbox and asks before every shell command.\n",
        );
    }
    let Some(protection) = &facts.protection else {
        return out;
    };
    let reason = match protection {
        GitProtection::Full if facts.linux => {
            out.push_str(
                "Git metadata protection: full tier (read-only mounts in a user namespace)\n",
            );
            return out;
        }
        GitProtection::Full => {
            out.push_str("Git metadata protection: full (writes to git hooks and config fail inside the sandbox)\n");
            return out;
        }
        GitProtection::Basic { reason } => reason,
    };
    out.push_str("Git metadata protection: basic tier\n");
    out.push_str(&format!("  Why: {}\n", terminal_safe(reason)));
    out.push_str(
        "  In this tier harness checks git metadata after each command: it moves new hooks, config and\n  repositories to quarantine and restores changed files. A git process running outside the\n  sandbox, such as an editor's, could still read a planted file in the moment before that.\n",
    );
    if let Some(quarantine) = &facts.quarantine {
        out.push_str(&format!(
            "  Quarantined files go to {}\n",
            terminal_safe(&quarantine.display().to_string())
        ));
    }
    out.push_str(match facts.setting {
        LinuxGitProtection::BestEffort => {
            "  sandbox.linux_git_protection = \"best-effort\": commands run in this tier. Set it to \"required\"\n  to have every shell command ask first instead.\n"
        }
        LinuxGitProtection::Required => {
            "  sandbox.linux_git_protection = \"required\": every shell command asks first in this tier.\n"
        }
    });
    out.push_str("\nTo get the full tier, harness must be able to create user namespaces.\n");
    let mut fixes = 0;
    if facts.host.userns_clone.as_deref() == Some("0") {
        fixes += 1;
        out.push_str(&sysctl_fix(
            "They are turned off (kernel.unprivileged_userns_clone = 0). To turn them on:",
            "kernel.unprivileged_userns_clone",
            "1",
        ));
    }
    if facts.host.max_user_namespaces.as_deref() == Some("0") {
        fixes += 1;
        out.push_str(&sysctl_fix(
            "They are limited to none (user.max_user_namespaces = 0). To allow them:",
            "user.max_user_namespaces",
            "10000",
        ));
    }
    if facts.host.apparmor_restrict.as_deref() == Some("1") {
        fixes += 1;
        out.push_str(&apparmor_fix(facts.exe.as_deref()));
    }
    if facts.host.container {
        fixes += 1;
        out.push_str(
            "\nharness is running in a container, and container runtimes block user namespaces by default\n(Docker's seccomp profile does). The full tier needs the container started with a profile that\nallows them, for example `docker run --security-opt seccomp=unconfined --security-opt\napparmor=unconfined ...`, which lifts those limits for everything in the container.\n",
        );
    }
    if fixes == 0 {
        out.push_str(
            "\nharness could not tell what blocks them here. Check `dmesg` for AppArmor or SELinux denials\naround the step named above, and the sysctls kernel.unprivileged_userns_clone and\nuser.max_user_namespaces.\n",
        );
    }
    out.push_str("\nharness does not change any of these settings itself.\n");
    out
}

fn sysctl_fix(what: &str, key: &str, value: &str) -> String {
    format!(
        "\n{what}\n\n    sudo sysctl -w {key}={value}\n    echo '{key} = {value}' | sudo tee /etc/sysctl.d/60-harness-userns.conf\n"
    )
}

/// Ubuntu's two ways out: an AppArmor profile that lets only this binary create user namespaces,
/// or the sysctl for every program.
fn apparmor_fix(exe: Option<&Path>) -> String {
    let mut out = String::from(
        "\nUbuntu restricts them with AppArmor (kernel.apparmor_restrict_unprivileged_userns = 1). Either:\n\n1. Allow them for this harness binary only (recommended): save this profile and load it.\n\n",
    );
    let path = exe
        .and_then(Path::to_str)
        .filter(|p| !p.contains(['"', '\\']) && !p.chars().any(char::is_control));
    match path {
        Some(path) => out.push_str(&format!(
            "    sudo tee /etc/apparmor.d/harness > /dev/null <<'EOF'\n    abi <abi/4.0>,\n    include <tunables/global>\n\n    profile harness \"{}\" flags=(unconfined) {{\n      userns,\n\n      include if exists <local/harness>\n    }}\n    EOF\n    sudo apparmor_parser -r /etc/apparmor.d/harness\n\n   Do it again if harness moves to another path.\n",
            terminal_safe(path)
        )),
        None => out.push_str(
            "   harness's path cannot be written into an AppArmor profile as it is. Install harness at a\n   plain path such as /usr/local/bin/harness and run this command again.\n",
        ),
    }
    out.push_str(&sysctl_fix(
        "2. Or allow them for every program:",
        "kernel.apparmor_restrict_unprivileged_userns",
        "0",
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(host: Host) -> Facts {
        Facts {
            linux: true,
            mechanism: Some("landlock+seccomp"),
            protection: Some(GitProtection::Basic {
                reason: "writing /proc/self/setgroups failed: Operation not permitted (os error 1)"
                    .into(),
            }),
            quarantine: Some(PathBuf::from("/home/u/.local/share/harness/quarantine")),
            host,
            exe: Some(PathBuf::from("/home/u/.cargo/bin/harness")),
            ..Facts::default()
        }
    }

    #[test]
    fn ubuntu_gets_the_apparmor_profile_and_the_sysctl() {
        let out = render(&basic(Host {
            apparmor_restrict: Some("1".into()),
            max_user_namespaces: Some("63000".into()),
            ..Host::default()
        }));
        assert!(
            out.starts_with("Sandbox: landlock+seccomp\nGit metadata protection: basic tier\n"),
            "{out}"
        );
        assert!(out.contains(
            "  Why: writing /proc/self/setgroups failed: Operation not permitted (os error 1)\n"
        ));
        assert!(out.contains("    profile harness \"/home/u/.cargo/bin/harness\" flags=(unconfined) {\n      userns,\n"), "{out}");
        assert!(out.contains("    sudo apparmor_parser -r /etc/apparmor.d/harness\n"));
        assert!(
            out.contains("    sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0\n")
        );
        assert!(out.contains("    echo 'kernel.apparmor_restrict_unprivileged_userns = 0' | sudo tee /etc/sysctl.d/60-harness-userns.conf\n"));
        assert!(out.contains("Quarantined files go to /home/u/.local/share/harness/quarantine"));
        assert!(out.contains("sandbox.linux_git_protection = \"best-effort\""));
        assert!(out.ends_with("harness does not change any of these settings itself.\n"));
        assert!(!out.contains("could not tell"));
    }

    #[test]
    fn other_causes_get_their_own_fixes() {
        let out = render(&basic(Host {
            userns_clone: Some("0".into()),
            max_user_namespaces: Some("0".into()),
            container: true,
            ..Host::default()
        }));
        assert!(
            out.contains("    sudo sysctl -w kernel.unprivileged_userns_clone=1\n"),
            "{out}"
        );
        assert!(
            out.contains("    sudo sysctl -w user.max_user_namespaces=10000\n"),
            "{out}"
        );
        assert!(out.contains("--security-opt seccomp=unconfined"), "{out}");
        assert!(!out.contains("apparmor_parser"));
    }

    #[test]
    fn an_unknown_cause_says_where_to_look() {
        let out = render(&basic(Host::default()));
        assert!(
            out.contains("could not tell what blocks them here"),
            "{out}"
        );
    }

    #[test]
    fn a_path_that_cannot_go_into_a_profile_is_not_printed_into_one() {
        let mut facts = basic(Host {
            apparmor_restrict: Some("1".into()),
            ..Host::default()
        });
        facts.exe = Some(PathBuf::from("/tmp/evil\"\u{1b}[31m/harness"));
        let out = render(&facts);
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(!out.contains("profile harness \""), "{out}");
        assert!(
            out.contains("cannot be written into an AppArmor profile"),
            "{out}"
        );
    }

    #[test]
    fn a_reason_with_control_characters_is_escaped() {
        let mut facts = basic(Host::default());
        facts.protection = Some(GitProtection::Basic {
            reason: "opening /w/\u{1b}]0;x\u{7} failed".into(),
        });
        let out = render(&facts);
        assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'), "{out:?}");
    }

    #[test]
    fn the_full_tier_and_macos_need_no_fix() {
        let mut facts = basic(Host {
            apparmor_restrict: Some("1".into()),
            ..Host::default()
        });
        facts.protection = Some(GitProtection::Full);
        assert_eq!(
            render(&facts),
            "Sandbox: landlock+seccomp\nGit metadata protection: full tier (read-only mounts in a user namespace)\n"
        );
        facts.linux = false;
        facts.mechanism = Some("seatbelt");
        assert_eq!(
            render(&facts),
            "Sandbox: seatbelt\nGit metadata protection: full (writes to git hooks and config fail inside the sandbox)\n"
        );
    }

    #[test]
    fn no_sandbox_says_why() {
        let facts = Facts {
            linux: true,
            unavailable: Some("Landlock is not enabled in this kernel".into()),
            disabled: true,
            ..Facts::default()
        };
        assert_eq!(
            render(&facts),
            "Sandbox: none (Landlock is not enabled in this kernel); every shell command will need approval\nHARNESS_SANDBOX=none is set, so harness runs without the sandbox and asks before every shell command.\n"
        );
    }

    #[test]
    fn required_protection_is_described() {
        let mut facts = basic(Host::default());
        facts.setting = LinuxGitProtection::Required;
        assert!(
            render(&facts).contains("\"required\": every shell command asks first in this tier.")
        );
    }
}
```

In `crates/harness-cli/src/main.rs`:

Replace (1 of 3):

```rust
mod ask;
mod models;
mod prompt;
mod setup;
mod term;
mod trust;
```

with:

```rust
mod ask;
mod doctor;
mod models;
mod prompt;
mod sandbox;
mod setup;
mod term;
mod trust;
```

Replace (2 of 3):

```rust
        #[arg(long, conflicts_with = "yes")]
        revoke: bool,
    },
}

fn main() -> ExitCode {
```

with:

```rust
        #[arg(long, conflicts_with = "yes")]
        revoke: bool,
    },
    /// Inspect the OS sandbox
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
    },
}

#[derive(Subcommand)]
enum SandboxCommand {
    /// Show which sandbox this system gets, how git metadata is protected, and how to improve it
    Doctor,
}

fn main() -> ExitCode {
```

Replace (3 of 3):

```rust
            }
            Some(Command::Models) => models::run().await,
            Some(Command::Trust { yes, revoke }) => trust::run(yes, revoke),
            None => {
                eprintln!("Interactive mode is not available yet; use `harness ask \"...\"`.");
                2
```

with:

```rust
            }
            Some(Command::Models) => models::run().await,
            Some(Command::Trust { yes, revoke }) => trust::run(yes, revoke),
            Some(Command::Sandbox {
                command: SandboxCommand::Doctor,
            }) => doctor::run(),
            None => {
                eprintln!("Interactive mode is not available yet; use `harness ask \"...\"`.");
                2
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p harness-cli && cargo test -p harness-sandbox`
Expected: PASS, including the `sandbox::tests` and `doctor::tests` unit tests, `help_lists_the_sandbox_doctor`, `sandbox_doctor_reports_the_mechanism_and_tier` and, on macOS, `planting_a_git_hook_fails_in_the_sandbox` and `git_init_of_a_nested_repository_is_undone`. `harness sandbox doctor` prints `Sandbox: seatbelt` and `Git metadata protection: full (writes to git hooks and config fail inside the sandbox)` here.

- [ ] **Step 7: Lint**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && bash target/linux-lint.sh`
Expected: clean. On Linux (CI), the stock job exercises the basic-tier warning, `"required"` making `touch made.txt` exit 3 with nothing created, and the doctor's AppArmor fix; the full-tier job the opposite.

- [ ] **Step 8: Commit**

```bash
git add crates/harness-cli crates/harness-sandbox
git commit -F - <<'EOF'
feat(cli): warn about the Linux basic tier and add harness sandbox doctor

In the basic tier ask and auto warn at startup and point to harness
sandbox doctor; with linux_git_protection = "required" they get no
sandbox, so every command asks. The doctor reports the mechanism, the
tier and why, and the exact commands that would enable the full tier:
an AppArmor profile for this binary or the sysctl. It changes nothing
itself.

<trailer lines from the controller>
EOF
```

---

### Task 11: CI for both tiers, and the README

**Files:**
- Modify: `.github/workflows/ci.yml`, `README.md`

**Interfaces:**
- Consumes: `HARNESS_EXPECT_LINUX_TIER` in the tests (Tasks 8 and 10).
- Produces: a third CI job (`ubuntu-24.04`, `tier: full`) and user-facing docs.

- [ ] **Step 1: Add the full-tier job**

In `.github/workflows/ci.yml`:

Replace:

```yaml
      matrix:
        # ubuntu-24.04: kernel 6.8+ with Landlock ABI ≥ 4 (harness needs ≥ 3); pinned so the sandbox suite keeps running on a known kernel.
        os: [ubuntu-24.04, macos-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - run: rustup toolchain install
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets --locked -- -D warnings
      - uses: taiki-e/install-action@nextest
      - run: cargo nextest run --workspace --locked
        env:
          HARNESS_REQUIRE_LINUX_SANDBOX: ${{ runner.os == 'Linux' && '1' || '0' }}
  deny:
    runs-on: ubuntu-latest
    steps:
```

with:

```yaml
      matrix:
        # ubuntu-24.04: kernel 6.8+ with Landlock ABI ≥ 4 (harness needs ≥ 3); pinned so the sandbox suite keeps running on a known kernel.
        os: [ubuntu-24.04, macos-latest]
        tier: [default]
        include:
          # The same Linux runner with unprivileged user namespaces allowed, so the full tier of
          # Linux git-metadata protection (read-only mounts) is tested too. The default Linux job
          # tests the basic tier: GitHub's Ubuntu runners restrict user namespaces with AppArmor.
          - os: ubuntu-24.04
            tier: full
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - run: rustup toolchain install
      - uses: Swatinem/rust-cache@v2
        with:
          key: ${{ matrix.tier }}
      - name: Allow unprivileged user namespaces
        if: matrix.tier == 'full'
        run: sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
      - run: cargo fmt --all --check
        if: matrix.tier != 'full'
      - run: cargo clippy --workspace --all-targets --locked -- -D warnings
        if: matrix.tier != 'full'
      - uses: taiki-e/install-action@nextest
      - run: cargo nextest run --workspace --locked
        env:
          HARNESS_REQUIRE_LINUX_SANDBOX: ${{ runner.os == 'Linux' && '1' || '0' }}
          HARNESS_EXPECT_LINUX_TIER: ${{ runner.os == 'Linux' && (matrix.tier == 'full' && 'full' || 'basic') || '' }}
  deny:
    runs-on: ubuntu-latest
    steps:
```

- [ ] **Step 2: Check the workflow parses**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))" && echo ok`
Expected: `ok`. (Install PyYAML with `pip3 install pyyaml` if the import fails.)

- [ ] **Step 3: Update the README**

In `README.md`:

Replace (1 of 2):

```markdown
- Approval modes: `plan`, `read-only`, `ask`, `auto`, `full-access`. Default is `auto` inside a git repository and `ask` elsewhere.
- Exit codes for scripting: `0` success, `1` runtime error, `2` invalid usage or no model, `3` an action was blocked for lack of approval, `130` interrupted.

**Sandboxed by default.** In every mode except `full-access`, every shell command runs in an OS sandbox (Seatbelt on macOS, Landlock + seccomp on Linux 6.2+): no network access, and writes only inside the workspace and temp directories (none in `plan`/`read-only`). On macOS, git hooks and `.git` configuration stay read-only inside the sandbox; on Linux they do not yet (see Known limitations). Destructive commands (force-push, `reset --hard`, `rm -rf` of the workspace), commands the analyser cannot fully parse, and re-running a command without the sandbox when the sandbox may have blocked it all need approval; headless runs refuse them and exit `3`. `plan` and `read-only` never offer that re-run. If the system has no usable sandbox, harness warns and asks before every command, and `plan`/`read-only` refuse shell commands. A workspace that is your home directory or one of its parents gets no writable sandbox, since it would cover your dotfiles: harness warns, and `ask` and `auto` ask before every command there.

Set `HARNESS_SANDBOX=none` to turn the sandbox off (every shell command then needs approval, and `plan`/`read-only` refuse them).

```

with:

```markdown
- Approval modes: `plan`, `read-only`, `ask`, `auto`, `full-access`. Default is `auto` inside a git repository and `ask` elsewhere.
- Exit codes for scripting: `0` success, `1` runtime error, `2` invalid usage or no model, `3` an action was blocked for lack of approval, `130` interrupted.

**Sandboxed by default.** In every mode except `full-access`, every shell command runs in an OS sandbox (Seatbelt on macOS, Landlock + seccomp on Linux 6.2+): no network access, and writes only inside the workspace and temp directories (none in `plan`/`read-only`). Git hooks, repository config and `.harness/` stay read-only inside the sandbox on macOS, and on Linux wherever unprivileged user namespaces work. Where they are blocked (stock Ubuntu 24.04 and later, most containers), harness checks them after each command instead, moving anything planted to a quarantine directory and restoring what changed; `harness sandbox doctor` shows how to turn on full protection. Destructive commands (force-push, `reset --hard`, `rm -rf` of the workspace), commands the analyser cannot fully parse, and re-running a command without the sandbox when the sandbox may have blocked it all need approval; headless runs refuse them and exit `3`. `plan` and `read-only` never offer that re-run. If the system has no usable sandbox, harness warns and asks before every command, and `plan`/`read-only` refuse shell commands. A workspace that is your home directory or one of its parents gets no writable sandbox, since it would cover your dotfiles: harness warns, and `ask` and `auto` ask before every command there.

Set `HARNESS_SANDBOX=none` to turn the sandbox off (every shell command then needs approval, and `plan`/`read-only` refuse them).

```

Replace (2 of 2):

````markdown

[sandbox]
writable_roots = ["~/.cargo"]
```

Project-level `.harness/config.toml` settings that widen what the agent may do (allow rules, `read_dirs`, model, providers, sandbox settings, a `mode` wider than your global or default mode, a `max_steps` above your global limit) only apply after `harness trust`.

## Known limitations

- **Linux git metadata:** Landlock can only grant access, so inside a writable workspace it cannot keep `.git/hooks` and `.git/config` read-only. A sandboxed command could plant a git hook that runs on your next `git` command. A stronger Linux backend is planned.
- **Linux kernels older than 6.2** (Landlock ABI < 3) get no sandbox, so harness asks before every command.
- **Linux `/dev/shm`** is writable from the sandbox in `ask` and `auto` modes, because Python's `multiprocessing` needs it.
- **Shell analysis is best effort.** Deny rules match the commands harness can see through wrappers such as `env`, `sudo`, `bash -c` and `xargs`. They do not see inside script files or interpreters (`python -c`, `node -e`), and wrappers such as `busybox` are not unwrapped. On macOS, `/bin/bash` 3.2 reads a here-document inside `$(…)` differently from the analyser, so specially crafted text there can hide a command from deny rules; the command still runs inside the sandbox. The OS sandbox is the security boundary; rules are guardrails.
- **`git -c` asks.** Git configuration on the command line can run programs, so every `git -c` needs approval unless its key is one that cannot: `user.name`, `user.email`, `init.defaultBranch`, `color.*`, `advice.*`, `core.quotepath`, and `commit.gpgsign`/`tag.gpgsign` set to `false`.
- **Sandboxed commands can read any file.** The sandbox limits writes and network access, not reads. `read:` and `write:` rules govern only the file tools, not what a shell command opens, and a `read:` deny rule on a single file does not stop `grep` or `glob` from searching the directory that holds it.
- **Hard links:** on macOS, files with more than one hard link cannot be modified inside the sandbox. On Linux, hard links that already point outside the workspace stay writable.
- **Git inside the sandbox** cannot create repositories or worktrees in the workspace (`git init`, `git clone`, `git worktree add`). A nested repository can still be moved out of the workspace, edited and moved back. `.git/rebase-merge/git-rebase-todo` stays writable, so a sandboxed command could add `exec` lines that run the next time you continue a rebase (`git rebase --continue`). A workspace that is a linked worktree (its `.git` file points into another repository's `.git/worktrees/`) cannot commit inside the sandbox, because that gitdir is outside the workspace. A repository outside the workspace in a writable temp or `writable_roots` directory gets no protection.
- **Path rules** are matched after resolving symlinks. On macOS, write `/private/tmp/...` rather than `/tmp/...` in `allow` rules.

## Roadmap
````

with:

````markdown

[sandbox]
writable_roots = ["~/.cargo"]
# Linux: ask before every command when git metadata can only be checked after the fact
# linux_git_protection = "required"
```

Project-level `.harness/config.toml` settings that widen what the agent may do (allow rules, `read_dirs`, model, providers, sandbox settings, a `mode` wider than your global or default mode, a `max_steps` above your global limit) only apply after `harness trust`.

## Known limitations

- **Linux git metadata without user namespaces:** where unprivileged user namespaces are blocked (stock Ubuntu 24.04 and later, Docker's default profile, GitHub's Ubuntu runners), harness protects git metadata only after the fact. After each sandboxed command it moves planted hooks, config and repositories to the quarantine directory in harness's data directory (`~/.local/share/harness/quarantine/` by default) and restores changed files, so a git process running outside the sandbox at that moment, such as an editor's status poll, could read a planted file first. A change you make yourself to `.git/config` or hooks while a command runs is undone too (it is kept in the quarantine). `harness sandbox doctor` shows the one-time fix; `sandbox.linux_git_protection = "required"` makes harness ask before every command instead.
- **Linux git metadata in either tier:** mounts cannot cover a name that does not exist yet. A new `commondir` in a gitdir, a top-level `HEAD` or `.harness/` is moved to the quarantine milliseconds after it appears rather than refused, a new nested repository only after the command ends, and one inside a git-ignored directory not at all.
- **Linux kernels older than 6.2** (Landlock ABI < 3) get no sandbox, so harness asks before every command.
- **Linux `/dev/shm`** is writable from the sandbox in `ask` and `auto` modes, because Python's `multiprocessing` needs it.
- **No namespaces inside the Linux sandbox:** sandboxed commands cannot create user, mount or other namespaces, so programs that sandbox themselves that way (Chromium and Electron, bubblewrap, rootless Podman) need their no-sandbox mode or an unsandboxed re-run.
- **Shell analysis is best effort.** Deny rules match the commands harness can see through wrappers such as `env`, `sudo`, `bash -c` and `xargs`. They do not see inside script files or interpreters (`python -c`, `node -e`), and wrappers such as `busybox` are not unwrapped. On macOS, `/bin/bash` 3.2 reads a here-document inside `$(…)` differently from the analyser, so specially crafted text there can hide a command from deny rules; the command still runs inside the sandbox. The OS sandbox is the security boundary; rules are guardrails.
- **`git -c` asks.** Git configuration on the command line can run programs, so every `git -c` needs approval unless its key is one that cannot: `user.name`, `user.email`, `init.defaultBranch`, `color.*`, `advice.*`, `core.quotepath`, and `commit.gpgsign`/`tag.gpgsign` set to `false`.
- **Sandboxed commands can read any file.** The sandbox limits writes and network access, not reads. `read:` and `write:` rules govern only the file tools, not what a shell command opens, and a `read:` deny rule on a single file does not stop `grep` or `glob` from searching the directory that holds it.
- **Hard links:** on macOS, files with more than one hard link cannot be modified inside the sandbox. On Linux, hard links that already point outside the workspace stay writable.
- **Git inside the sandbox** cannot create repositories or worktrees in the workspace (`git init`, `git clone`, `git worktree add`); on Linux they are created and then moved to the quarantine. On macOS a nested repository can still be moved out of the workspace, edited and moved back. `.git/rebase-merge/git-rebase-todo` stays writable, so a sandboxed command could add `exec` lines that run the next time you continue a rebase (`git rebase --continue`). A workspace that is a linked worktree (its `.git` file points into another repository's `.git/worktrees/`) cannot commit inside the sandbox, because that gitdir is outside the workspace. A repository outside the workspace in a writable temp or `writable_roots` directory gets no protection.
- **Path rules** are matched after resolving symlinks. On macOS, write `/private/tmp/...` rather than `/tmp/...` in `allow` rules.

## Roadmap
````

- [ ] **Step 4: Run everything**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && bash target/linux-lint.sh`
Expected: clean; `cargo test --workspace` passes (449 tests on macOS).

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml README.md
git commit -F - <<'EOF'
ci: test the Linux full tier; docs: describe Linux git protection

A third job runs the ubuntu-24.04 suite with unprivileged user
namespaces allowed, and HARNESS_EXPECT_LINUX_TIER makes tier-dependent
tests fail instead of skipping on the wrong tier. The README explains
both tiers and their windows, harness sandbox doctor, and that
sandboxed commands cannot create namespaces.

<trailer lines from the controller>
EOF
```

- [ ] **Step 6: Leave tasks.md unticked**

The controller ticks 2.13 in `openspec/changes/add-core-agent/tasks.md` after the pull request's CI is green on all three jobs and the final review is done.

---

## Plan Completion Checklist

- [ ] `cargo test --workspace` is green on macOS, and the pull request's CI is green on `macos-latest`, stock `ubuntu-24.04` (basic tier) and `ubuntu-24.04` with user namespaces allowed (full tier).
- [ ] On an Ubuntu 24.04 machine or VM, `harness sandbox doctor` prints the basic tier and the AppArmor profile; after installing that profile as printed, it prints `Git metadata protection: full tier (read-only mounts in a user namespace)`.
- [ ] In a scratch repository on Linux, `harness --model <local model> ask "run: echo 'echo pwned' > .git/hooks/pre-commit"` exits 3 in both tiers, `.git/hooks/pre-commit` does not exist afterwards, and in the basic tier the hook is under `~/.local/share/harness/quarantine/`.
- [ ] `git commit` through `harness ask` still succeeds in both tiers.
- [ ] Tick 2.13 in `tasks.md` (the controller).
