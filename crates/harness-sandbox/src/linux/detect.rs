//! Landlock ABI detection.
//!
//! The `landlock` crate does not expose a standalone "what ABI does the
//! kernel support" query (it folds detection into `Ruleset::create`), so we
//! reimplement the documented probe idiom directly: call
//! `landlock_create_ruleset(2)` with a null `attr` pointer, `size` 0, and the
//! `LANDLOCK_CREATE_RULESET_VERSION` flag. With that flag the syscall does
//! not create anything; it returns the kernel's supported ABI version
//! instead of a ruleset fd, or fails with `ENOSYS` (not built in) /
//! `EOPNOTSUPP` (built in but disabled, e.g. via `CONFIG_LSM`).

/// `LANDLOCK_CREATE_RULESET_VERSION` from `linux/landlock.h`. Not exported by
/// the `landlock` crate (it is an internal `uapi` constant there), so it is
/// redefined here; its value is part of the stable kernel UAPI.
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;

/// Landlock ABI 1 cannot correctly express cross-directory rename/link
/// restrictions, and ABI 2 has no `LANDLOCK_ACCESS_FS_TRUNCATE` at all — a
/// program denied write access could still `truncate(2)` or
/// `open(O_TRUNC)` any file it can merely open (see landlock(7), "Kernel
/// compatibility" / ABI history). This crate requires ABI >= 3 (kernel
/// 6.2+), the first version that can restrict truncation.
const MIN_SUPPORTED_ABI: i32 = 3;

/// Probes the running kernel's Landlock ABI version.
///
/// Returns `None` when Landlock is not implemented or not enabled by the
/// running kernel. Returns `Some(abi)` with `abi >= 1` otherwise, even if
/// this crate only requires/uses a lower ABI for its own rules — callers
/// that want the availability check this crate uses internally should call
/// [`linux_sandbox_available`] instead of comparing this value themselves.
pub fn landlock_abi() -> Option<i32> {
    // SAFETY: `landlock_create_ruleset(2)` with a null `attr` pointer, size
    // 0, and the VERSION flag is the documented no-op probe form: the
    // syscall never dereferences `attr` in this mode.
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if version < 0 {
        None
    } else {
        i32::try_from(version).ok()
    }
}

/// Whether this process can rely on the Linux sandbox backend: the Landlock
/// half requires kernel ABI >= 3, and the seccomp half requires the
/// network-deny filter to actually build (see [`super::seccomp::build_network_deny_filter`],
/// which includes its own prologue-shape check). The architecture check
/// that used to live here is now a compile-time gate instead: the whole
/// `linux` module (see `lib.rs`) only builds on `x86_64`/`aarch64`, the two
/// architectures the seccomp filter knows how to target, so by the time
/// this function can even be called the architecture is already known-good.
///
/// Building the filter here — the same filter [`super::linux_sandbox_command`]
/// builds again, independently, at actual spawn time — is deliberately
/// redundant: the point of this function is that callers use it to decide
/// *whether* to offer the sandbox at all, before ever trying to spawn
/// anything. If the filter can't build (e.g. a future `seccompiler` upgrade
/// changed the shape [`super::seccomp::verify_prologue`] checks, without
/// this crate's version pin being bumped deliberately — see the workspace
/// manifest), this must report unavailable, not available-but-broken: with
/// no sandbox detected, every `bash` call asks for approval instead; if this
/// reported available anyway, every sandboxed spawn would instead fail
/// outright.
pub fn linux_sandbox_available() -> bool {
    landlock_abi().is_some_and(|abi| abi >= MIN_SUPPORTED_ABI)
        && super::seccomp::build_network_deny_filter().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_supported_abi_rejects_abi_below_3() {
        const { assert!(MIN_SUPPORTED_ABI >= 3) };
    }

    #[test]
    fn network_deny_filter_builds_on_this_host_architecture() {
        // Independent of Landlock support: `linux_sandbox_available`'s
        // seccomp half should always succeed on x86_64/aarch64, the only
        // architectures this module compiles for at all.
        assert!(
            crate::linux::seccomp::build_network_deny_filter().is_ok(),
            "the seccomp filter should always build on this architecture"
        );
    }
}
