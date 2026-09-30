//! A panic while the session runs: its message prints with the terminal out of harness's modes.
//! The panic hook is the process's, so this test has a binary of its own.

use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use harness_tui::terminal::{Modes, RawMode};

type Log = Arc<Mutex<Vec<String>>>;

/// Records whether raw mode is on.
struct FakeRaw(Log);

impl RawMode for FakeRaw {
    fn enable(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().push("raw on".into());
        Ok(())
    }
    fn disable(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().push("raw off".into());
        Ok(())
    }
}

/// The terminal: records what is written to it.
struct Screen(Log);

impl Write for Screen {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(buf).into_owned());
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Final review M3: a panic's message printed in raw mode over the live region. Harness's modes
// are left before it prints, raw mode last, as when harness leaves; and set again after, since
// a panic that is caught (a check on another thread, a diff made off the UI's task) leaves the
// session running. While an editor has the terminal, the modes are left as they are.
#[test]
fn a_panic_prints_its_message_out_of_harnesss_modes() {
    let log = Log::default();
    // Prints the message, as the default hook does, which the new one runs; the test's own
    // failures go to the default hook.
    let printed = log.clone();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info.payload().downcast_ref::<&str>() == Some(&"boom") {
            printed.lock().unwrap().push("message".into());
        } else {
            default(info);
        }
    }));
    let mut modes = Modes::enter(Screen(log.clone()), FakeRaw(log.clone()), true).unwrap();
    let (out, raw) = (log.clone(), log.clone());
    modes.leave_on_panic(move || Screen(out.clone()), FakeRaw(raw));
    log.lock().unwrap().clear();
    assert!(std::panic::catch_unwind(|| panic!("boom")).is_err());
    let said = log.lock().unwrap().concat();
    assert_eq!(
        said,
        "\x1b[<1u\x1b[?2004lraw offmessageraw on\x1b[?2004h\x1b[>1u"
    );

    modes.suspend().unwrap();
    log.lock().unwrap().clear();
    assert!(std::panic::catch_unwind(|| panic!("boom")).is_err());
    assert_eq!(log.lock().unwrap().concat(), "message");
}
