//! Compiles the seccomp-BPF program that denies network access, in the
//! parent process.
//!
//! Like [`super::fs::build_ruleset_fd`], this runs entirely before `fork()`.
//! `seccompiler::SeccompFilter::try_into::<BpfProgram>()` allocates the
//! `Vec<sock_filter>` that gets carried into `pre_exec`; installing that
//! already-compiled program with `seccompiler::apply_filter` is the only
//! part that happens in the child (see `preexec.rs`).

use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, Error, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
    SeccompFilter, SeccompRule, TargetArch,
};

/// Syscalls denied unconditionally (regardless of arguments): the rest of
/// the socket lifecycle beyond creation, plus `io_uring`, which can perform
/// network I/O (including creating `AF_VSOCK`/`AF_INET` sockets under the
/// hood on newer kernels) without ever calling `socket(2)` itself.
const DENY_UNCONDITIONALLY: &[i64] = &[
    libc::SYS_connect,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept,
    libc::SYS_accept4,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
];

/// Builds the network-denying seccomp-BPF program.
///
/// Default action is `Allow` (every syscall not mentioned below runs
/// normally); the on-match action is `Errno(EPERM)`, so a denied call fails
/// the way it would against a firewall rather than killing the process
/// (`SIGSYS`), which is easier for shell scripts and CLI tools to handle
/// gracefully.
///
/// `socket`/`socketpair` are allowed only for `AF_UNIX` (arg0 ==
/// `AF_UNIX`), so tools that use Unix-domain sockets or socketpairs for
/// local IPC (many process-supervisor / build-tool patterns, including
/// `cargo`'s own jobserver) keep working, while `AF_INET`, `AF_INET6`,
/// `AF_VSOCK`, `AF_NETLINK`, etc. are denied at the point of creation.
pub fn build_network_deny_filter() -> Result<BpfProgram, Error> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    for &nr in DENY_UNCONDITIONALLY {
        // An empty rule vector matches the syscall unconditionally.
        rules.insert(nr, vec![]);
    }

    rules.insert(libc::SYS_socket, vec![not_af_unix(0)?]);
    rules.insert(libc::SYS_socketpair, vec![not_af_unix(0)?]);

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,                     // default: allow
        SeccompAction::Errno(libc::EPERM as u32), // on match: deny with EPERM
        target_arch(),
    )?;

    Ok(filter.try_into()?)
}

/// A rule matching "argument `arg_index` (the socket domain) is not
/// `AF_UNIX`", i.e. exactly the sockets we want to deny.
fn not_af_unix(arg_index: u8) -> Result<SeccompRule, Error> {
    Ok(SeccompRule::new(vec![SeccompCondition::new(
        arg_index,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Ne,
        libc::AF_UNIX as u64,
    )?])?)
}

/// The two architectures this crate builds seccomp filters for. Any other
/// Linux architecture is a compile error here rather than a runtime one:
/// [`super::linux_sandbox_available`] already reports the sandbox as
/// unavailable off x86_64/aarch64, so in practice this module is simply
/// never reached, but keeping it a hard compile error avoids silently
/// shipping a filter compiled for the wrong architecture.
fn target_arch() -> TargetArch {
    #[cfg(target_arch = "x86_64")]
    {
        TargetArch::x86_64
    }
    #[cfg(target_arch = "aarch64")]
    {
        TargetArch::aarch64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_deny_filter_compiles_to_a_non_empty_program() {
        let program = build_network_deny_filter().expect("filter should compile");
        assert!(!program.is_empty());
    }
}
