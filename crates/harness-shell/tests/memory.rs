//! Heap use of the analysis on long commands, measured with a counting allocator. This is
//! its own test binary so that no other test allocates while one is measured.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use harness_shell::{Rules, Verdict, evaluate};

/// Counts the bytes currently allocated and the highest count since the last reset.
struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = CURRENT.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(now, Relaxed);
}

// SAFETY: every call is forwarded unchanged to the system allocator; only counters change.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        CURRENT.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, size) };
        if !q.is_null() {
            if size > layout.size() {
                grew(size - layout.size());
            } else {
                CURRENT.fetch_sub(layout.size() - size, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Keeps the tests of this binary from measuring each other.
static SERIAL: Mutex<()> = Mutex::new(());

/// The most heap `f` holds at once beyond what was allocated before it started.
fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Relaxed);
    PEAK.store(base, Relaxed);
    let out = f();
    (out, PEAK.load(Relaxed).saturating_sub(base))
}

/// Longer commands are not scanned at all.
const MAX_SCAN_CHARS: usize = 262_144;
const MIB: usize = 1 << 20;

fn rules() -> Rules {
    Rules {
        allow: vec!["echo*".into()],
        deny: vec!["curl*".into()],
        confirm: Vec::new(),
    }
}

/// `prefix` followed by `unit` repeated, `len` characters in all.
fn shape(prefix: &str, unit: &str, len: usize) -> String {
    let mut s = String::with_capacity(len + unit.len());
    s.push_str(prefix);
    while s.len() < len {
        s.push_str(unit);
    }
    s.truncate(len);
    s
}

#[test]
fn rough_scan_memory_is_bounded() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // The rough scan cannot tell where this body ends, so it splits what follows several
    // ways (see `fallback.rs`).
    let untracked = "cat <<\\EOF\nit's\nEOF\n";
    let mut failures = Vec::new();
    for (prefix, unit) in [
        (untracked, "$("),
        (untracked, "a\n"),
        ("", "a\n"),
        ("", ";"),
    ] {
        let cmd = shape(prefix, unit, MAX_SCAN_CHARS);
        let (verdict, peak) = peak_during(|| evaluate(&cmd, &rules(), Path::new("/work/proj")));
        let mib = peak as f64 / MIB as f64;
        if !matches!(verdict, Verdict::Ask { .. }) || peak >= 32 * MIB {
            failures.push(format!(
                "{prefix:?} + {unit:?}: peak {mib:.1} MiB, {verdict:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn commands_over_the_scan_limit_ask() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let cmd = shape("curl x; echo ", "a", MIB);
    let start = Instant::now();
    let (verdict, peak) = peak_during(|| evaluate(&cmd, &rules(), Path::new("/work/proj")));
    let took = start.elapsed();
    // Not scanned, so the denied `curl x` is not found; but the command could hide a
    // denied one, so full-access still prompts.
    assert!(
        matches!(verdict, Verdict::Ask { may_deny: true, ref reason, .. } if reason.contains("262144")),
        "{verdict:?}"
    );
    assert!(took < Duration::from_millis(100), "{took:?}");
    assert!(peak < MIB, "peak {peak} bytes");
    // Without deny rules there is nothing to hide.
    let no_deny = Rules {
        deny: Vec::new(),
        ..rules()
    };
    let verdict = evaluate(&cmd, &no_deny, Path::new("/work/proj"));
    assert!(
        matches!(
            verdict,
            Verdict::Ask {
                may_deny: false,
                ..
            }
        ),
        "{verdict:?}"
    );
}
