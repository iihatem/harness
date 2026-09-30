//! The terminal's events (keys, pastes and resizes) for the session, read on a thread of their
//! own. crossterm parses them; this reader makes sure it only ever reads what is there. At the
//! terminal's end (a closed terminal, whose input reads as empty forever) crossterm would read
//! again and again without end, holding its reader's lock, so that end is found here first: the
//! stream of events then ends, and the session with it. While an editor has the terminal, the
//! reader stops reading, so the keys go to the editor.

use std::{
    io,
    os::fd::RawFd,
    pin::Pin,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use futures::Stream;
use ratatui::crossterm::event::{self, Event};
use tokio::sync::mpsc;

/// How long the reader waits for the terminal before it looks for a resize, a pause or the end
/// of the session again.
const WAIT: Duration = Duration::from_millis(100);

/// How long a pause waits for the reader to stop reading: a read in the middle of a sequence
/// (a long paste) finishes first.
const PAUSE_WAIT: Duration = Duration::from_secs(1);

/// The terminal's events, as a stream; it ends when the terminal closes or its input fails.
pub struct TerminalInput {
    events: mpsc::UnboundedReceiver<io::Result<Event>>,
    control: Arc<Control>,
    /// The terminal read.
    fd: RawFd,
}

/// Stops [`TerminalInput`]'s reader while another program reads the terminal.
#[derive(Clone)]
pub struct InputPause(Arc<Control>);

/// Until dropped, the reader does not read the terminal.
pub struct Paused(Arc<Control>);

#[derive(Default)]
struct Control {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// How many pauses hold the reader.
    paused: usize,
    /// The reader may be reading the terminal.
    reading: bool,
    /// The session no longer reads events.
    stopped: bool,
}

impl TerminalInput {
    /// Starts reading this process's terminal (its standard input, which interactive mode
    /// needs to be one).
    pub fn start() -> TerminalInput {
        let control = Arc::new(Control::default());
        let (sender, events) = mpsc::unbounded_channel();
        let reader = control.clone();
        std::thread::Builder::new()
            .name("harness-input".into())
            .spawn(move || reader_thread(libc::STDIN_FILENO, &reader, sender))
            .expect("failed to start the terminal reader");
        TerminalInput {
            events,
            control,
            fd: libc::STDIN_FILENO,
        }
    }

    /// Stops the reader while another program reads the terminal.
    pub fn pauser(&self) -> InputPause {
        InputPause(self.control.clone())
    }
}

impl Stream for TerminalInput {
    type Item = io::Result<Event>;

    /// The next event; the end once the terminal hung up, even should crossterm be reading its
    /// end on the reader's thread (it closed in the middle of an escape sequence, which crossterm
    /// reads on for). The session looks here at least every few hundred milliseconds.
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.events.poll_recv(cx) {
            Poll::Pending if hung_up(self.fd) => Poll::Ready(None),
            polled => polled,
        }
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        let mut state = lock(&self.control);
        state.stopped = true;
        self.control.changed.notify_all();
    }
}

impl InputPause {
    /// Stops the reader, once it has finished what it was reading, until the result is dropped.
    pub fn pause(&self) -> Paused {
        let mut state = lock(&self.0);
        state.paused += 1;
        let (_state, _timed_out) = self
            .0
            .changed
            .wait_timeout_while(state, PAUSE_WAIT, |state| state.reading)
            .unwrap_or_else(|e| e.into_inner());
        Paused(self.0.clone())
    }
}

impl Drop for Paused {
    fn drop(&mut self) {
        let mut state = lock(&self.0);
        state.paused = state.paused.saturating_sub(1);
        self.0.changed.notify_all();
    }
}

fn lock(control: &Control) -> std::sync::MutexGuard<'_, State> {
    control.state.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the terminal on `fd` has for the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Terminal {
    /// Nothing yet.
    Quiet,
    /// Bytes to read.
    Ready,
    /// Its end: it closed, or cannot be read.
    Closed,
}

/// Waits up to `wait` for the terminal on `fd` to have something, and says what.
fn terminal(fd: RawFd, wait: Duration) -> Terminal {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = libc::c_int::try_from(wait.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: one valid `pollfd`, and its count.
    let ready = unsafe { libc::poll(&mut poll, 1, millis) };
    if ready < 0 {
        return if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            Terminal::Quiet
        } else {
            Terminal::Closed
        };
    }
    if ready == 0 {
        return Terminal::Quiet;
    }
    if poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Terminal::Closed;
    }
    // Readable with nothing to read is the terminal's end.
    let mut waiting: libc::c_int = 0;
    // SAFETY: `FIONREAD` writes one `c_int` through the pointer given.
    let asked = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut waiting) };
    if asked < 0 || waiting <= 0 {
        return Terminal::Closed;
    }
    Terminal::Ready
}

/// Whether the terminal on `fd` hung up (its other side closed) or cannot be polled. Unlike
/// [`terminal`], this reads nothing about what waits to be read, which the reader's thread may be
/// reading at the same moment.
fn hung_up(fd: RawFd) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid `pollfd`, and its count.
    let ready = unsafe { libc::poll(&mut poll, 1, 0) };
    ready > 0 && poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

/// The reader's thread: reads until done, then says so, so that a pause does not wait for it.
/// The stream of events ends as `events` drops.
fn reader_thread(fd: RawFd, control: &Control, events: mpsc::UnboundedSender<io::Result<Event>>) {
    read(fd, control, &events);
    let mut state = lock(control);
    state.reading = false;
    control.changed.notify_all();
}

/// The reader's loop: sends the terminal's events until the terminal closes, its input fails,
/// or the session stops reading them.
fn read(fd: RawFd, control: &Control, events: &mpsc::UnboundedSender<io::Result<Event>>) {
    loop {
        {
            let mut state = lock(control);
            state.reading = false;
            control.changed.notify_all();
            while state.paused > 0 && !state.stopped {
                state = control
                    .changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
            if state.stopped || events.is_closed() {
                return;
            }
            state.reading = true;
        }
        if terminal(fd, WAIT) == Terminal::Closed {
            return;
        }
        // What crossterm has now: what it parsed already, a resize, and what the terminal has.
        // It is asked only while the terminal is not at its end.
        loop {
            match event::poll(Duration::ZERO) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        if events.send(Ok(event)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = events.send(Err(e));
                        return;
                    }
                },
                Ok(false) => break,
                Err(e) => {
                    let _ = events.send(Err(e));
                    return;
                }
            }
            if terminal(fd, Duration::ZERO) == Terminal::Closed {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        os::fd::AsRawFd,
        time::Instant,
    };

    use super::*;

    // Review C I1: the terminal's end is what makes crossterm read forever, so it is found here:
    // readable with nothing to read, or hung up.
    #[test]
    fn the_end_of_the_input_is_told_from_bytes_and_from_nothing_yet() {
        let (mut reader, mut writer) = std::io::pipe().unwrap();
        let fd = reader.as_raw_fd();
        assert_eq!(terminal(fd, Duration::ZERO), Terminal::Quiet);
        writer.write_all(b"x").unwrap();
        assert_eq!(terminal(fd, Duration::ZERO), Terminal::Ready);
        reader.read_exact(&mut [0]).unwrap();
        assert_eq!(terminal(fd, Duration::ZERO), Terminal::Quiet);
        drop(writer);
        // Waits a while: on macOS, a process another test starts meanwhile can hold a copy of
        // the pipe's end for a moment.
        assert_eq!(terminal(fd, Duration::from_secs(5)), Terminal::Closed);
    }

    // Should crossterm still read the terminal's end on the reader's thread (it closed in the
    // middle of an escape sequence, which crossterm reads on for), the stream ends anyway.
    #[tokio::test]
    async fn the_stream_ends_at_the_terminals_end_whatever_the_reader_does() {
        let (reader, writer) = std::io::pipe().unwrap();
        let (sender, events) = mpsc::unbounded_channel();
        let mut input = TerminalInput {
            events,
            control: Arc::new(Control::default()),
            fd: reader.as_raw_fd(),
        };
        sender.send(Ok(Event::FocusGained)).unwrap();
        drop(writer);
        use futures::{FutureExt, StreamExt};
        assert!(matches!(input.next().await, Some(Ok(Event::FocusGained))));
        // As the session does, looking again every so often: on macOS, a process another test
        // starts meanwhile can hold a copy of the pipe's end for a moment.
        let mut end = None;
        for _ in 0..250 {
            if let Some(next) = input.next().now_or_never() {
                end = Some(next);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(matches!(end, Some(None)), "the stream went on");
        drop(sender);
    }

    // The terminal's end ends the stream of events, and the reader: a pause no longer waits
    // for it.
    #[test]
    fn at_the_terminals_end_the_events_end_and_the_reader_stops() {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(writer);
        let control = Control::default();
        let (sender, mut events) = mpsc::unbounded_channel();
        reader_thread(reader.as_raw_fd(), &control, sender);
        assert!(!lock(&control).reading);
        assert!(events.try_recv().is_err() && events.is_closed());
    }

    // A pause waits for a read under way to finish, so the editor gets every key after it.
    #[test]
    fn a_pause_waits_for_the_read_under_way() {
        let control = Arc::new(Control::default());
        lock(&control).reading = true;
        let reader = control.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let mut state = lock(&reader);
            state.reading = false;
            reader.changed.notify_all();
        });
        let started = Instant::now();
        let paused = InputPause(control.clone()).pause();
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert_eq!(lock(&control).paused, 1);
        drop(paused);
        assert_eq!(lock(&control).paused, 0);
    }
}
