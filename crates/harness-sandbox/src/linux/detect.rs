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
/// restrictions (see landlock(7), "Kernel compatibility" / ABI history), so
/// this crate treats it as unusable and requires ABI >= 2.
const MIN_SUPPORTED_ABI: i32 = 2;

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

/// Whether this process can rely on the Linux sandbox backend: the seccomp
/// half requires a BPF-supported architecture we know how to target, and the
/// Landlock half requires kernel ABI >= 2.
pub fn linux_sandbox_available() -> bool {
    supported_arch() && landlock_abi().is_some_and(|abi| abi >= MIN_SUPPORTED_ABI)
}

fn supported_arch() -> bool {
    cfg!(target_arch = "x86_64") || cfg!(target_arch = "aarch64")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_supported_abi_rejects_abi_1() {
        const { assert!(MIN_SUPPORTED_ABI > 1) };
    }
}
