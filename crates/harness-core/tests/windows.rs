//! 1.7: windows are labelled by their length, and a snapshot goes stale after 15 minutes.

use harness_core::meter::{Window, WindowSnapshot, WindowSource};

fn window(minutes: Option<u64>, used: Option<f64>) -> Window {
    Window {
        window_minutes: minutes,
        used_percent: used,
        resets_at: None,
        source: WindowSource::Header,
    }
}

#[test]
fn a_window_is_labelled_by_its_length_never_by_its_position() {
    for (minutes, label) in [
        (300, "5h"),
        (10_080, "7d"),
        (1_440, "1d"),
        (43_200, "30d"),
        (60, "1h"),
        (90, "90m"),
        (45, "45m"),
    ] {
        assert_eq!(window(Some(minutes), None).label(), label, "{minutes}");
    }
    assert_eq!(window(None, Some(10.0)).label(), "window");
}

#[test]
fn a_snapshot_older_than_15_minutes_is_stale() {
    let snapshot = WindowSnapshot {
        windows: vec![window(Some(300), Some(42.0))],
        observed_at: 1_000_000,
    };
    assert!(!snapshot.is_stale(1_000_000 + 15 * 60));
    assert!(snapshot.is_stale(1_000_000 + 15 * 60 + 1));
    // 20 minutes ago.
    assert!(snapshot.is_stale(1_000_000 + 20 * 60));
}

#[test]
fn the_most_used_window_is_the_one_the_status_line_shows() {
    let snapshot = WindowSnapshot {
        windows: vec![
            window(Some(300), Some(42.0)),
            window(Some(10_080), Some(12.0)),
            window(Some(1_440), None),
        ],
        observed_at: 0,
    };
    assert_eq!(snapshot.most_used().unwrap().label(), "5h");
    let none = WindowSnapshot {
        windows: vec![window(Some(300), None)],
        observed_at: 0,
    };
    assert!(
        none.most_used().is_none(),
        "a window with no percentage is unknown, not 0%"
    );
}

#[test]
fn a_snapshot_has_a_stable_id_that_follows_its_content() {
    let a = WindowSnapshot {
        windows: vec![window(Some(300), Some(42.0))],
        observed_at: 1_790_000_000,
    };
    let mut b = a.clone();
    assert_eq!(a.id(), b.id());
    b.windows[0].used_percent = Some(43.0);
    assert_ne!(a.id(), b.id());
    assert!(a.id().starts_with('w'), "{}", a.id());
}

// Final review, Minor 1: the time it was seen is not part of what a snapshot is, so one that
// repeats unchanged is the same snapshot and is written once.
#[test]
fn the_id_of_a_snapshot_does_not_depend_on_when_it_was_seen() {
    let a = WindowSnapshot {
        windows: vec![window(Some(300), Some(42.0))],
        observed_at: 1_790_000_000,
    };
    let b = WindowSnapshot {
        observed_at: 1_790_000_001,
        ..a.clone()
    };
    assert_eq!(a.id(), b.id());
}
