//! Compiles the seccomp-BPF program that denies network access, in the
//! parent process.
//!
//! Like [`super::fs::build_ruleset_fd`], this runs entirely before `fork()`.
//! `seccompiler::SeccompFilter::try_into::<BpfProgram>()` allocates the
//! `Vec<sock_filter>` that gets carried into `pre_exec`; installing that
//! already-compiled program with `seccompiler::apply_filter` is the only
//! part that happens in the child (see `preexec.rs`).
//!
//! ## The x32 ABI is refused outright, on x86_64
//!
//! On x86_64, a syscall made through the x32 ABI (a 32-bit-pointer calling
//! convention that still runs on the 64-bit kernel entry path) reports the
//! *same* `seccomp_data.arch` as a native x86_64 syscall — `AUDIT_ARCH_X86_64`
//! — with its syscall number OR'd with `__X32_SYSCALL_BIT` (`0x4000_0000`).
//! seccompiler's own arch-check prologue only compares `arch`, so it cannot
//! tell the two apart; and every rule below is keyed by a *native* syscall
//! number (e.g. `libc::SYS_connect`), so an x32-tagged number never equals
//! any of them and falls through to the filter's default action (`Allow`).
//! Concretely: `ctypes.CDLL(None).syscall(0x4000_0029, AF_INET, SOCK_STREAM, 0)`
//! from Python would otherwise create an `AF_INET` socket despite the
//! `socket`/`connect`/... rules below.
//!
//! [`deny_x32_syscalls`] closes this by post-processing the compiled
//! program: it splices in a fixed 3-instruction block, immediately after
//! seccompiler's 3-instruction arch-check prologue, that returns `EPERM` for
//! *any* syscall number `>= 0x4000_0000` — refusing the entire x32 ABI
//! rather than trying to enumerate x32-tagged equivalents of every rule
//! below. aarch64 has no equivalent compat ABI reachable from a native
//! aarch64 seccomp filter (32-bit ARM compat processes report a different
//! `arch` value entirely, already rejected by the prologue itself), so this
//! is x86_64-only.

use std::collections::BTreeMap;

#[cfg(target_arch = "x86_64")]
use seccompiler::sock_filter;
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
///
/// On x86_64, also refuses the entire x32 ABI; see the module docs.
pub fn build_network_deny_filter() -> Result<BpfProgram, Error> {
    #[allow(unused_mut)]
    let mut program = compile_rules()?;
    #[cfg(target_arch = "x86_64")]
    deny_x32_syscalls(&mut program);
    Ok(program)
}

/// The rule-based part of [`build_network_deny_filter`], without the x32
/// post-processing — factored out so the x32 unit tests below can compare
/// against the program as seccompiler itself compiled it.
fn compile_rules() -> Result<BpfProgram, Error> {
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

// ---------------------------------------------------------------------------
// x32 (x86_64 only)
// ---------------------------------------------------------------------------

/// `__X32_SYSCALL_BIT` from `<asm/unistd.h>`: OR'd into the syscall number
/// (not the reported `arch`) for every syscall made through the x32 ABI.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// Offset of `seccomp_data.nr` (the first field of the struct): always 0.
/// Mirrors seccompiler's own private `SECCOMP_DATA_NR_OFFSET`.
#[cfg(target_arch = "x86_64")]
const NR_OFFSET: u32 = 0;

/// Number of instructions in seccompiler 0.5.0's arch-check prologue
/// (`backend::bpf::build_arch_validation_sequence`, private to that crate):
/// `LD [arch]`; `JEQ <target arch>, jt=1, jf=0`; `RET KILL_PROCESS`. Every
/// compiled [`BpfProgram`] this crate produces starts with exactly these
/// three instructions (`backend::filter::TryFrom<SeccompFilter>` always
/// calls `build_arch_validation_sequence` first, unconditionally). Verified
/// against seccompiler 0.5.0's source
/// (`~/.cargo/registry/src/*/seccompiler-0.5.0/src/backend/bpf.rs`); the
/// tests below would fail loudly if a future seccompiler version changed
/// this shape.
#[cfg(target_arch = "x86_64")]
const ARCH_PROLOGUE_LEN: usize = 3;

/// BPF classic opcodes and field combinations from `<linux/bpf_common.h>` /
/// `<linux/filter.h>`. Not exported by `seccompiler` (its equivalent
/// `backend::bpf` constants are private to that crate), so they are
/// redefined here; their values are part of the stable BPF UAPI.
#[cfg(target_arch = "x86_64")]
mod bpf_opcode {
    /// `BPF_LD (0x00) | BPF_W (0x00) | BPF_ABS (0x20)`: load a 32-bit word
    /// from the fixed data area (`seccomp_data`) at offset `k`.
    pub(super) const LD_W_ABS: u16 = 0x20;
    /// `BPF_JMP (0x05) | BPF_JGE (0x30) | BPF_K (0x00)`: jump if the
    /// accumulator is `>= k`.
    pub(super) const JMP_JGE_K: u16 = 0x35;
    /// `BPF_RET (0x06) | BPF_K (0x00)`: return the immediate value `k`.
    pub(super) const RET_K: u16 = 0x06;
}

/// Splices a 3-instruction block in at index [`ARCH_PROLOGUE_LEN`] — right
/// after seccompiler's arch-check prologue, before the first rule's own
/// `LD [nr]` — that returns `EPERM` for any syscall number
/// `>= 0x4000_0000` (`__X32_SYSCALL_BIT`). See the module docs for why.
///
/// Correctness of the splice point: BPF conditional jumps encode a relative
/// instruction count (how many instructions to skip), not an absolute
/// index, and every jump this crate or seccompiler emits only ever jumps
/// *forward*. The prologue's own "arch matched" jump already skips exactly
/// past its 3rd instruction, landing here regardless of what follows; and
/// nothing after this point ever jumps to a target before it. Inserting a
/// fixed-size, self-contained block at this one fixed offset therefore
/// cannot invalidate any jump target already computed elsewhere in the
/// program — this is checked directly by
/// [`x32_denial_block_lands_exactly_after_the_prologue`] and
/// [`rules_after_the_x32_block_are_unchanged_and_still_reachable`] below.
#[cfg(target_arch = "x86_64")]
fn deny_x32_syscalls(program: &mut BpfProgram) {
    use bpf_opcode::{JMP_JGE_K, LD_W_ABS, RET_K};

    let deny_with_eperm = u32::from(SeccompAction::Errno(libc::EPERM as u32));
    let block = [
        // Load the (possibly x32-tagged) syscall number.
        sock_filter {
            code: LD_W_ABS,
            jt: 0,
            jf: 0,
            k: NR_OFFSET,
        },
        // nr >= X32_SYSCALL_BIT: fall through (jt=0) into the RET EPERM
        // immediately below; otherwise (jf=1), skip over it, into the
        // first real rule.
        sock_filter {
            code: JMP_JGE_K,
            jt: 0,
            jf: 1,
            k: X32_SYSCALL_BIT,
        },
        sock_filter {
            code: RET_K,
            jt: 0,
            jf: 0,
            k: deny_with_eperm,
        },
    ];
    program.splice(ARCH_PROLOGUE_LEN..ARCH_PROLOGUE_LEN, block);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_deny_filter_compiles_to_a_non_empty_program() {
        let program = build_network_deny_filter().expect("filter should compile");
        assert!(!program.is_empty());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x32_denial_block_lands_exactly_after_the_prologue() {
        let program = build_network_deny_filter().expect("filter should compile");
        let inserted = &program[ARCH_PROLOGUE_LEN..ARCH_PROLOGUE_LEN + 3];

        assert_eq!(inserted[0].code, bpf_opcode::LD_W_ABS);
        assert_eq!(inserted[0].k, NR_OFFSET);

        assert_eq!(inserted[1].code, bpf_opcode::JMP_JGE_K);
        assert_eq!(inserted[1].k, X32_SYSCALL_BIT);
        assert_eq!(
            inserted[1].jt, 0,
            "match (nr >= bit) must fall into RET EPERM next"
        );
        assert_eq!(inserted[1].jf, 1, "no match must skip over RET EPERM");

        assert_eq!(inserted[2].code, bpf_opcode::RET_K);
        assert_eq!(
            inserted[2].k,
            u32::from(SeccompAction::Errno(libc::EPERM as u32))
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn rules_after_the_x32_block_are_unchanged_and_still_reachable() {
        let plain = compile_rules().expect("filter should compile");
        let patched = build_network_deny_filter().expect("filter should compile");

        assert_eq!(patched.len(), plain.len() + 3);
        assert_eq!(
            &patched[..ARCH_PROLOGUE_LEN],
            &plain[..ARCH_PROLOGUE_LEN],
            "the prologue itself must be untouched"
        );
        assert_eq!(
            &patched[ARCH_PROLOGUE_LEN + 3..],
            &plain[ARCH_PROLOGUE_LEN..],
            "every rule instruction must be shifted by exactly 3, unmodified"
        );
    }
}
