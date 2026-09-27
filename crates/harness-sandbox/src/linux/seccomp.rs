//! Compiles the seccomp-BPF programs that deny network access, mount
//! changes and new namespaces, in the parent process.
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
//! from Python — x32's own number for `socket`, `0x4000_0000 | 41` — would
//! otherwise create an `AF_INET` socket despite the `socket`/`connect`/...
//! rules below.
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
//!
//! ## The splice point is verified, not assumed
//!
//! Splicing at a fixed index is only correct if that index really is
//! "right after the prologue, before the first rule". [`verify_prologue`]
//! checks `compile_rules()`'s first four instructions byte-for-byte against
//! seccompiler 0.5.0's actual output before [`deny_x32_syscalls`] ever runs;
//! see its doc for the literals and where they come from. Because
//! `seccompiler` is pinned to exactly `0.5.0` in the workspace manifest (a
//! `=` requirement, not `^`), that shape cannot change out from under this
//! module without a deliberate version bump; the check exists anyway as a
//! second, independent line of defense — and because `program[..4]` is
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

#[cfg(target_arch = "x86_64")]
use seccompiler::sock_filter;
use seccompiler::{
    BpfProgram, Error, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
    SeccompFilter, SeccompRule, TargetArch,
};

/// `open_tree_attr` (Linux 6.15), which `libc` does not name yet. New
/// syscalls share one number on the architectures harness supports.
const SYS_OPEN_TREE_ATTR: i64 = 467;

/// Syscalls denied unconditionally (regardless of arguments): the rest of
/// the socket lifecycle beyond creation; `io_uring`, which can perform
/// network I/O (including creating `AF_VSOCK`/`AF_INET` sockets under the
/// hood on newer kernels) without ever calling `socket(2)` itself; and every
/// call that changes mounts or enters a namespace (see the module docs).
const DENY_UNCONDITIONALLY: &[i64] = &[
    libc::SYS_connect,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept,
    libc::SYS_accept4,
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
/// On x86_64, also refuses the entire x32 ABI; see the module docs. Returns
/// `Err` (in the parent, before any child exists, so the spawn simply fails
/// rather than running unfiltered) if seccompiler's compiled output does
/// not have the exact shape [`deny_x32_syscalls`] depends on.
pub fn build_deny_filter() -> io::Result<BpfProgram> {
    #[allow(unused_mut)]
    let mut program = compile_rules().map_err(io::Error::other)?;
    #[cfg(target_arch = "x86_64")]
    {
        verify_prologue(&program)?;
        deny_x32_syscalls(&mut program);
    }
    Ok(program)
}

/// The rule-based part of [`build_deny_filter`], without the x32
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
    // A syscall's rules match when any one of them does: one rule per flag.
    let namespace_rules = CLONE_NAMESPACE_FLAGS
        .iter()
        .map(|&flag| has_flag(0, flag))
        .collect::<Result<Vec<_>, Error>>()?;
    rules.insert(libc::SYS_clone, namespace_rules);

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,                     // default: allow
        SeccompAction::Errno(libc::EPERM as u32), // on match: deny with EPERM
        target_arch(),
    )?;

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
    Ok(SeccompRule::new(vec![SeccompCondition::new(
        arg_index,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Ne,
        libc::AF_UNIX as u64,
    )?])?)
}

/// The two architectures this crate builds seccomp filters for. `lib.rs`
/// only compiles the whole `linux` module (see its `mod linux` gate) on
/// `x86_64`/`aarch64`, so these two `cfg` arms are exhaustive: this function
/// can never actually run on another architecture, and there is nothing
/// left to fall back to if it somehow did.
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

/// Offset of `seccomp_data.arch` (the second field of the struct, right
/// after the `int nr`): always 4. Mirrors seccompiler's own private
/// `SECCOMP_DATA_ARCH_OFFSET`.
#[cfg(target_arch = "x86_64")]
const ARCH_OFFSET: u32 = 4;

/// `AUDIT_ARCH_X86_64` from `<linux/audit.h>`: `EM_X86_64 (62) |
/// __AUDIT_ARCH_64BIT | __AUDIT_ARCH_LE`. Reproduces seccompiler's own
/// private constant of the same name (`backend::bpf::AUDIT_ARCH_X86_64`) by
/// the same formula, so this evaluates to the same `0xC000_003E` it does.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_X86_64: u32 = 62 | 0x8000_0000 | 0x4000_0000;

/// Number of instructions in seccompiler 0.5.0's arch-check prologue
/// (`backend::bpf::build_arch_validation_sequence`, private to that crate):
/// `LD [arch]`; `JEQ <target arch>, jt=1, jf=0`; `RET KILL_PROCESS`. Every
/// compiled [`BpfProgram`] this crate produces starts with exactly these
/// three instructions (`backend::filter::TryFrom<SeccompFilter>` always
/// calls `build_arch_validation_sequence` first, unconditionally), followed
/// immediately by the rule section's own first instruction, `LD [nr]` (see
/// [`expected_prologue`] for the full four-instruction shape and
/// [`verify_prologue`] for where this gets checked against reality rather
/// than assumed).
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
    /// `BPF_JMP (0x05) | BPF_JEQ (0x10) | BPF_K (0x00)`: jump if the
    /// accumulator is `== k`.
    pub(super) const JMP_JEQ_K: u16 = 0x15;
    /// `BPF_JMP (0x05) | BPF_JGE (0x30) | BPF_K (0x00)`: jump if the
    /// accumulator is `>= k`.
    pub(super) const JMP_JGE_K: u16 = 0x35;
    /// `BPF_RET (0x06) | BPF_K (0x00)`: return the immediate value `k`.
    pub(super) const RET_K: u16 = 0x06;
}

/// The exact four instructions [`verify_prologue`] requires
/// `compile_rules()`'s output to start with: seccompiler's own 3-instruction
/// arch-check prologue, plus the rule section's first instruction (the
/// unconditional `LD [nr]` that `backend::filter::TryFrom<SeccompFilter>`
/// emits before any rule, whenever `filter.rules` is non-empty — which it
/// always is here; see [`DENY_UNCONDITIONALLY`]). Values confirmed directly
/// against seccompiler 0.5.0's source
/// (`~/.cargo/registry/src/*/seccompiler-0.5.0/src/backend/{bpf,filter}.rs`).
#[cfg(target_arch = "x86_64")]
fn expected_prologue() -> [sock_filter; 4] {
    use bpf_opcode::{JMP_JEQ_K, LD_W_ABS, RET_K};
    [
        // LD [arch]
        sock_filter {
            code: LD_W_ABS,
            jt: 0,
            jf: 0,
            k: ARCH_OFFSET,
        },
        // JEQ AUDIT_ARCH_X86_64, jt=1 (matched: skip the KILL below), jf=0
        // (mismatched: fall into it).
        sock_filter {
            code: JMP_JEQ_K,
            jt: 1,
            jf: 0,
            k: AUDIT_ARCH_X86_64,
        },
        // RET KILL_PROCESS
        sock_filter {
            code: RET_K,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_KILL_PROCESS,
        },
        // LD [nr] — the rule section's own first instruction.
        sock_filter {
            code: LD_W_ABS,
            jt: 0,
            jf: 0,
            k: NR_OFFSET,
        },
    ]
}

/// Checks `program`'s first four instructions against [`expected_prologue`]
/// byte-for-byte. Returns `Err` on any mismatch — a different seccompiler
/// version producing a differently-shaped prologue, most plausibly — rather
/// than let [`deny_x32_syscalls`] splice at an index that no longer means
/// what it assumed. Runs in the parent, before any child exists, so a
/// mismatch here fails the spawn instead of installing a filter whose x32
/// denial block landed somewhere unintended (or corrupted the prologue's own
/// jump target).
#[cfg(target_arch = "x86_64")]
fn verify_prologue(program: &BpfProgram) -> io::Result<()> {
    let expected = expected_prologue();
    if program.len() < expected.len() || program[..expected.len()] != expected {
        return Err(io::Error::other(format!(
            "seccomp filter prologue does not match the shape this crate's x32 denial \
             splice depends on (seccompiler version mismatch?); refusing to install a \
             filter that might not actually deny x32 syscalls. Got: {:?}",
            &program[..program.len().min(expected.len())]
        )));
    }
    Ok(())
}

/// Splices a 3-instruction block in at index [`ARCH_PROLOGUE_LEN`] — right
/// after seccompiler's arch-check prologue, before the first rule's own
/// `LD [nr]` — that returns `EPERM` for any syscall number
/// `>= 0x4000_0000` (`__X32_SYSCALL_BIT`). See the module docs for why. Only
/// called after [`verify_prologue`] has confirmed that index means what
/// this function assumes.
///
/// Correctness of the splice point: BPF conditional jumps encode a relative
/// instruction count (how many instructions to skip), not an absolute
/// index, and every jump this crate or seccompiler emits only ever jumps
/// *forward*. The prologue's own "arch matched" jump already skips exactly
/// past its 3rd instruction, landing here regardless of what follows; and
/// nothing after this point ever jumps to a target before it. Inserting a
/// fixed-size, self-contained block at this one fixed offset therefore
/// cannot invalidate any jump target already computed elsewhere in the
/// program. The tests below don't inspect jump resolution directly — that
/// argument is the reasoning above, not something they execute — but they
/// do check the two facts that argument depends on:
/// [`x32_denial_block_lands_exactly_after_the_prologue`] checks the spliced
/// block's own instructions and their index, and
/// [`rules_after_the_x32_block_are_unchanged_and_still_reachable`] checks
/// that everything after it is byte-identical to the unpatched program,
/// merely shifted by exactly 3.
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
        // Spelled out again directly, independently of `expected_prologue`,
        // so a mistake in that function's own construction would not also
        // hide itself from this test.
        let expected = [
            sock_filter {
                code: 0x20, // BPF_LD | BPF_W | BPF_ABS
                jt: 0,
                jf: 0,
                k: 4, // SECCOMP_DATA_ARCH_OFFSET
            },
            sock_filter {
                code: 0x15, // BPF_JMP | BPF_JEQ | BPF_K
                jt: 1,
                jf: 0,
                k: 0xC000_003E, // AUDIT_ARCH_X86_64
            },
            sock_filter {
                code: 0x06, // BPF_RET | BPF_K
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_KILL_PROCESS,
            },
            sock_filter {
                code: 0x20, // BPF_LD | BPF_W | BPF_ABS
                jt: 0,
                jf: 0,
                k: 0, // SECCOMP_DATA_NR_OFFSET
            },
        ];

        let program = compile_rules().expect("filter should compile");
        assert_eq!(&program[..4], &expected[..]);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x32_denial_block_lands_exactly_after_the_prologue() {
        let program = build_deny_filter().expect("filter should compile");
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
        let patched = build_deny_filter().expect("filter should compile");

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
