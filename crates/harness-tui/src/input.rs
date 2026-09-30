//! The terminal's events (keys, pastes and resizes) for the session, read on a thread of their
//! own, which is the only one that reads the terminal while the session runs: where the cursor
//! is, asked after a resize, is asked there too, and handed to the session. crossterm parses the
//! events; this reader makes sure it only ever reads what is there. At the terminal's end (a
//! closed terminal, whose input reads as end-of-file or fails, and which says so by hanging up)
//! crossterm would read again and again without end, holding its reader's lock, so that end is
//! found here first: the stream of events then ends, and the session with it. The terminal only
//! ends when it hangs up: a wakeup that finds nothing to read is not its end. While an editor has
//! the terminal, the reader stops reading, so the keys go to the editor.
//!
//! A resize is taken from SIGWINCH as the session next looks at its input, rather than from the
//! reader, so that the next draw already uses the new size.

use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixStream,
    },
    pin::Pin,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use futures::Stream;
use ratatui::{
    crossterm::{
        cursor,
        event::{self, Event},
        terminal,
    },
    layout::Position,
};
use tokio::sync::{mpsc, oneshot};

/// How long the reader waits for the terminal before it looks for a pause or the end of the
/// session again, should nothing wake it.
const WAIT: Duration = Duration::from_millis(100);

/// How long the reader waits before it looks again when the terminal said it had something to
/// read and crossterm found nothing: the reader does not spin meanwhile.
const BACK_OFF: Duration = Duration::from_millis(10);

/// How long a pause waits for the reader to stop reading: a read in the middle of a sequence
/// (a long paste) finishes first.
const PAUSE_WAIT: Duration = Duration::from_secs(1);

/// How often the terminal is looked at for a hangup while it is asked about itself at startup.
const HANGUP_EVERY: Duration = Duration::from_millis(20);

/// How long the terminal has to answer at startup, beyond crossterm's own limits (2 seconds for
/// each question).
const STARTUP_LIMIT: Duration = Duration::from_secs(6);

/// A terminal event, with when it was read: a key counts from when the user typed it, not from
/// when the session got to it, which can be later (while it built a prompt, say).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timed {
    pub event: Event,
    /// When the reader read it.
    pub at: Instant,
}

impl From<Event> for Timed {
    /// `event`, read now.
    fn from(event: Event) -> Timed {
        Timed {
            event,
            at: Instant::now(),
        }
    }
}

/// The terminal's events, as a stream; it ends when the terminal closes or its input fails.
pub struct TerminalInput {
    events: mpsc::UnboundedReceiver<io::Result<Timed>>,
    control: Arc<Control>,
    /// The terminal read.
    fd: RawFd,
    /// SIGWINCH, taken as a resize at once; the reader forwards crossterm's resizes without it.
    resizes: Option<tokio::signal::unix::Signal>,
}

/// Stops [`TerminalInput`]'s reader while another program reads the terminal.
#[derive(Clone)]
pub struct InputPause(Arc<Control>);

/// Until dropped, the reader does not read the terminal.
pub struct Paused(Arc<Control>);

/// Asks the terminal where its cursor is, through [`TerminalInput`]'s reader: its answer comes
/// on the terminal's input, which only the reader reads.
#[derive(Clone)]
pub struct CursorQuery(Arc<Control>);

struct Control {
    state: Mutex<State>,
    changed: Condvar,
    /// Written to wake the reader from its wait on the terminal; read by the reader.
    wake: UnixStream,
    woken: UnixStream,
}

#[derive(Default)]
struct State {
    /// How many pauses hold the reader.
    paused: usize,
    /// The reader may be reading the terminal.
    reading: bool,
    /// The session no longer reads events.
    stopped: bool,
    /// Resizes come from the signal: crossterm's are dropped.
    resizes_elsewhere: bool,
    /// Where to send where the cursor is, once the reader has asked the terminal.
    cursor: Option<oneshot::Sender<io::Result<(u16, u16)>>>,
}

impl Control {
    fn new() -> io::Result<Control> {
        let (wake, woken) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        woken.set_nonblocking(true)?;
        Ok(Control {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            wake,
            woken,
        })
    }

    /// Wakes the reader: from waiting while paused, and from its wait on the terminal.
    fn wake(&self) {
        self.changed.notify_all();
        // A full pipe wakes the reader as well.
        let _ = (&self.wake).write(&[0]);
    }

    /// Takes what woke the reader.
    fn drain_wakes(&self) {
        let mut buf = [0u8; 64];
        while matches!((&self.woken).read(&mut buf), Ok(n) if n > 0) {}
    }
}

impl TerminalInput {
    /// Starts reading this process's terminal (its standard input, which interactive mode
    /// needs to be one). Within a tokio runtime, resizes are taken from SIGWINCH at once.
    pub fn start() -> io::Result<TerminalInput> {
        let control = Arc::new(Control::new()?);
        let resizes = tokio::runtime::Handle::try_current().ok().and_then(|_| {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok()
        });
        lock(&control).resizes_elsewhere = resizes.is_some();
        let (sender, events) = mpsc::unbounded_channel();
        let reader = control.clone();
        std::thread::Builder::new()
            .name("harness-input".into())
            .spawn(move || reader_thread(libc::STDIN_FILENO, &reader, sender))?;
        Ok(TerminalInput {
            events,
            control,
            fd: libc::STDIN_FILENO,
            resizes,
        })
    }

    /// Stops the reader while another program reads the terminal.
    pub fn pauser(&self) -> InputPause {
        InputPause(self.control.clone())
    }

    /// Asks the terminal where its cursor is, through the reader.
    pub fn cursor_query(&self) -> CursorQuery {
        CursorQuery(self.control.clone())
    }
}

impl Stream for TerminalInput {
    type Item = io::Result<Timed>;

    /// The next event: a resize first, as soon as the terminal signals it; the end once the
    /// terminal hung up, even should crossterm be reading its end on the reader's thread (it
    /// closed in the middle of an escape sequence, which crossterm reads on for). The session
    /// looks here at least every few hundred milliseconds.
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(resizes) = &mut self.resizes
            && let Poll::Ready(Some(())) = resizes.poll_recv(cx)
        {
            let (columns, rows) = terminal::size().unwrap_or_default();
            return Poll::Ready(Some(Ok(Event::Resize(columns, rows).into())));
        }
        match self.events.poll_recv(cx) {
            Poll::Pending if hung_up(self.fd) => Poll::Ready(None),
            polled => polled,
        }
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        lock(&self.control).stopped = true;
        self.control.wake();
    }
}

impl InputPause {
    /// Stops the reader, once it has finished what it was reading, until the result is dropped.
    pub fn pause(&self) -> Paused {
        let mut state = lock(&self.0);
        state.paused += 1;
        self.0.wake();
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
        drop(state);
        self.0.wake();
    }
}

impl CursorQuery {
    /// Where the terminal's cursor is, as its answer says, within `limit`: `None` when it did
    /// not answer by then, or its input ended. An answer to an earlier question that came too
    /// late is not taken for this one.
    pub async fn position(&self, limit: Duration) -> Option<Position> {
        let (answer, answered) = oneshot::channel();
        lock(&self.0).cursor = Some(answer);
        self.0.wake();
        match tokio::time::timeout(limit, answered).await {
            Ok(Ok(Ok((column, row)))) => Some(Position::new(column, row)),
            _ => None,
        }
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
    /// Readable, with nothing to read: not the end, which a terminal says by hanging up.
    Empty,
    /// Its end: it hung up (its input reads as end-of-file, or fails), or cannot be polled.
    Closed,
}

/// What the terminal has, from what `poll` said of it (`revents`) and how many bytes wait to be
/// read (`waiting`, `None` when that could not be asked).
fn classify(revents: libc::c_short, waiting: Option<libc::c_int>) -> Terminal {
    if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        Terminal::Closed
    } else if revents & libc::POLLIN == 0 {
        Terminal::Quiet
    } else if waiting.is_some_and(|waiting| waiting > 0) {
        Terminal::Ready
    } else {
        Terminal::Empty
    }
}

/// Waits up to `wait` for the terminal on `fd` to have something, or for `woken` to be written
/// to, and says what the terminal has, and whether the reader was woken.
fn terminal(fd: RawFd, woken: Option<RawFd>, wait: Duration) -> (Terminal, bool) {
    let mut polls = [
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: woken.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let millis = libc::c_int::try_from(wait.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: valid `pollfd`s, and their count; a negative descriptor is ignored.
    let ready = unsafe { libc::poll(polls.as_mut_ptr(), 2, millis) };
    if ready < 0 {
        return if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            (Terminal::Quiet, false)
        } else {
            (Terminal::Closed, false)
        };
    }
    let woke = polls[1].revents & libc::POLLIN != 0;
    let mut waiting: libc::c_int = 0;
    let waiting = if polls[0].revents & libc::POLLIN != 0 {
        // SAFETY: `FIONREAD` writes one `c_int` through the pointer given.
        let asked = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut waiting) };
        (asked >= 0).then_some(waiting)
    } else {
        None
    };
    (classify(polls[0].revents, waiting), woke)
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
fn reader_thread(fd: RawFd, control: &Control, events: mpsc::UnboundedSender<io::Result<Timed>>) {
    read(fd, control, &events);
    let mut state = lock(control);
    state.reading = false;
    control.changed.notify_all();
}

/// The reader's loop: sends the terminal's events until the terminal closes, its input fails,
/// or the session stops reading them; asks where the cursor is when the session wants to know.
fn read(fd: RawFd, control: &Control, events: &mpsc::UnboundedSender<io::Result<Timed>>) {
    let woken = control.woken.as_raw_fd();
    // The terminal said it had bytes to read at the last wait.
    let mut ready = false;
    loop {
        let (asked, resizes_elsewhere) = {
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
            (state.cursor.take(), state.resizes_elsewhere)
        };
        // crossterm is asked only while the terminal is not at its end.
        if hung_up(fd) {
            return;
        }
        // Events crossterm set aside while it waited for the answer were read meanwhile: they
        // are timed from when it started to wait, never later than they were read.
        let mut read_from = None;
        if let Some(answer) = asked {
            read_from = Some(Instant::now());
            let _ = answer.send(cursor::position());
        }
        // What crossterm has now: what it parsed already (keys typed while harness started, or
        // while it waited for an answer), and what the terminal has.
        let Some(got) = forward(fd, events, read_from, resizes_elsewhere) else {
            return;
        };
        // Bytes waited, and crossterm, which is told of new input only as it comes, took none:
        // it reads them once more comes. Not spinning meanwhile.
        if ready && got == 0 {
            std::thread::sleep(BACK_OFF);
        }
        let (terminal, woke) = terminal(fd, Some(woken), WAIT);
        if woke {
            control.drain_wakes();
        }
        ready = false;
        match terminal {
            Terminal::Closed => return,
            Terminal::Ready => ready = true,
            Terminal::Empty => std::thread::sleep(BACK_OFF),
            Terminal::Quiet => {}
        }
    }
}

/// Sends every event crossterm has, timed as read now or from `read_from`, and crossterm's
/// resizes unless they come from elsewhere; how many, or `None` when the session is done.
fn forward(
    fd: RawFd,
    events: &mpsc::UnboundedSender<io::Result<Timed>>,
    read_from: Option<Instant>,
    resizes_elsewhere: bool,
) -> Option<usize> {
    let mut got = 0;
    loop {
        match event::poll(Duration::ZERO) {
            Ok(true) => match event::read() {
                Ok(Event::Resize(..)) if resizes_elsewhere => {}
                Ok(event) => {
                    let at = read_from.unwrap_or_else(Instant::now);
                    events.send(Ok(Timed { event, at })).ok()?;
                    got += 1;
                }
                Err(e) => {
                    let _ = events.send(Err(e));
                    return None;
                }
            },
            Ok(false) => return Some(got),
            Err(e) => {
                let _ = events.send(Err(e));
                return None;
            }
        }
        if hung_up(fd) {
            return None;
        }
    }
}

/// What the terminal says about itself as the session starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Startup {
    /// It reports disambiguated keys (the kitty keyboard protocol).
    pub keyboard: bool,
    /// Where its cursor is, (column, row), when it says.
    pub cursor: Option<(u16, u16)>,
}

/// How asking the terminal about itself at startup went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unanswered {
    /// It hung up while asked.
    HungUp,
    /// It was still being read long after it should have answered or been given up on.
    Stuck,
}

/// Asks the terminal on standard input whether it reports disambiguated keys and where its
/// cursor is, on a thread of its own, before the session's reader starts. A terminal that closes
/// meanwhile, which crossterm would read forever, is told from its hangup and left to that
/// thread. However this ends, dropped before the answers came included, the terminal is left
/// out of the raw mode crossterm puts it in to ask.
pub async fn ask_at_startup() -> Result<Startup, Unanswered> {
    /// Once dropped, nothing more is asked, and the terminal's modes are put back.
    struct Asking(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for Asking {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            // Nothing to undo once the questions were answered.
            let _ = terminal::disable_raw_mode();
        }
    }

    let asking = Asking(Arc::default());
    let given_up = asking.0.clone();
    let (answer, answered) = oneshot::channel();
    let asked = std::thread::Builder::new()
        .name("harness-queries".into())
        .spawn(move || {
            let keyboard = terminal::supports_keyboard_enhancement().unwrap_or(false);
            if given_up.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let cursor = cursor::position().ok();
            let _ = answer.send(Startup { keyboard, cursor });
        });
    if asked.is_err() {
        return Ok(Startup {
            keyboard: false,
            cursor: None,
        });
    }
    let hung_up = async {
        while !hung_up(libc::STDIN_FILENO) {
            tokio::time::sleep(HANGUP_EVERY).await;
        }
    };
    let asked = tokio::select! {
        answered = answered => Ok(answered.unwrap_or(Startup { keyboard: false, cursor: None })),
        () = hung_up => Err(Unanswered::HungUp),
        () = tokio::time::sleep(STARTUP_LIMIT) => Err(Unanswered::Stuck),
    };
    drop(asking);
    asked
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        os::fd::AsRawFd,
        time::Instant,
    };

    use super::*;

    // Review C I1: the terminal's end is what makes crossterm read forever, so it is found here,
    // from its hangup.
    #[test]
    fn the_end_of_the_input_is_told_from_bytes_and_from_nothing_yet() {
        let (mut reader, mut writer) = std::io::pipe().unwrap();
        let fd = reader.as_raw_fd();
        assert_eq!(terminal(fd, None, Duration::ZERO).0, Terminal::Quiet);
        writer.write_all(b"x").unwrap();
        assert_eq!(terminal(fd, None, Duration::ZERO).0, Terminal::Ready);
        reader.read_exact(&mut [0]).unwrap();
        assert_eq!(terminal(fd, None, Duration::ZERO).0, Terminal::Quiet);
        drop(writer);
        // Waits a while: on macOS, a process another test starts meanwhile can hold a copy of
        // the pipe's end for a moment.
        assert_eq!(
            terminal(fd, None, Duration::from_secs(5)).0,
            Terminal::Closed
        );
    }

    // Review A N1: a wakeup that finds nothing to read (another reader took the bytes, as the
    // cursor query at a resize once did) is not the terminal's end; only a hangup is.
    #[test]
    fn readable_with_nothing_to_read_is_not_the_end() {
        assert_eq!(classify(libc::POLLIN, Some(0)), Terminal::Empty);
        assert_eq!(classify(libc::POLLIN, None), Terminal::Empty);
        assert_eq!(classify(libc::POLLIN, Some(3)), Terminal::Ready);
        assert_eq!(classify(0, None), Terminal::Quiet);
        for end in [libc::POLLHUP, libc::POLLERR, libc::POLLNVAL] {
            assert_eq!(classify(libc::POLLIN | end, Some(0)), Terminal::Closed);
            assert_eq!(classify(end, None), Terminal::Closed);
        }
    }

    // A question for the reader wakes it at once, rather than after its wait on the terminal.
    #[test]
    fn the_reader_is_woken_from_its_wait() {
        let (reader, _writer) = std::io::pipe().unwrap();
        let control = Arc::new(Control::new().unwrap());
        let waker = control.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            waker.wake();
        });
        let started = Instant::now();
        let (terminal, woke) = terminal(
            reader.as_raw_fd(),
            Some(control.woken.as_raw_fd()),
            Duration::from_secs(5),
        );
        assert!(woke);
        assert_eq!(terminal, Terminal::Quiet);
        assert!(started.elapsed() < Duration::from_secs(1));
        control.drain_wakes();
        let (_, woke) = super::terminal(
            reader.as_raw_fd(),
            Some(control.woken.as_raw_fd()),
            Duration::ZERO,
        );
        assert!(!woke);
    }

    // Should crossterm still read the terminal's end on the reader's thread (it closed in the
    // middle of an escape sequence, which crossterm reads on for), the stream ends anyway.
    #[tokio::test]
    async fn the_stream_ends_at_the_terminals_end_whatever_the_reader_does() {
        let (reader, writer) = std::io::pipe().unwrap();
        let (sender, events) = mpsc::unbounded_channel();
        let mut input = TerminalInput {
            events,
            control: Arc::new(Control::new().unwrap()),
            fd: reader.as_raw_fd(),
            resizes: None,
        };
        sender.send(Ok(Event::FocusGained.into())).unwrap();
        drop(writer);
        use futures::{FutureExt, StreamExt};
        assert!(matches!(
            input.next().await,
            Some(Ok(Timed {
                event: Event::FocusGained,
                ..
            }))
        ));
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
        let control = Control::new().unwrap();
        let (sender, mut events) = mpsc::unbounded_channel();
        reader_thread(reader.as_raw_fd(), &control, sender);
        assert!(!lock(&control).reading);
        assert!(events.try_recv().is_err() && events.is_closed());
    }

    // A pause waits for a read under way to finish, so the editor gets every key after it.
    #[test]
    fn a_pause_waits_for_the_read_under_way() {
        let control = Arc::new(Control::new().unwrap());
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

    // Review A N2: a resize the terminal signals reaches the session at once, ahead of what the
    // reader has not sent yet.
    #[tokio::test]
    async fn a_resize_is_taken_from_the_signal_at_once() {
        use futures::StreamExt;
        let (reader, _writer) = std::io::pipe().unwrap();
        let (_sender, events) = mpsc::unbounded_channel();
        let mut input = TerminalInput {
            events,
            control: Arc::new(Control::new().unwrap()),
            fd: reader.as_raw_fd(),
            resizes: Some(
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                    .unwrap(),
            ),
        };
        // SAFETY: sends this process a signal it handles.
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGWINCH) }, 0);
        let next = tokio::time::timeout(Duration::from_secs(5), input.next())
            .await
            .expect("the resize came");
        assert!(matches!(
            next,
            Some(Ok(Timed {
                event: Event::Resize(..),
                ..
            }))
        ));
    }
}
