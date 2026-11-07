//! A thin terminal client: raw input in, rendered diffs out. No local session
//! state, plugins or PTY children, so disconnecting cannot stop a session.
//!
//! It asks the terminal around it for what programs inside may ask of their
//! terminal: keys told apart (kitty's escape codes, or xterm's
//! modifyOtherKeys), focus, the colors and their scheme.

use std::{
    fs::OpenOptions,
    io::{self, Write},
    os::{
        fd::RawFd,
        unix::{fs::OpenOptionsExt, net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};

use crate::{
    cli::Options,
    input::{Decoder, Input},
    ipc::{self, Request, Response},
    runtime::Palette,
    store::{mkdir, suffixed},
};

/// Asked of the terminal around on attach: save its title; keys told apart,
/// with the keys of the base layout; reports of focus and the color scheme.
const SETUP: &[u8] = b"\x1b[22;2t\x1b[>7u\x1b[?1004h\x1b[?2031h";
/// Taken back on exit, modifyOtherKeys too when it was the way.
const TEARDOWN: &[u8] = b"\x1b[<u\x1b[?1004l\x1b[?2031l\x1b[23;2t";
const MODIFY_OTHER_KEYS: &[u8] = b"\x1b[>4;2m";
const MODIFY_OTHER_KEYS_OFF: &[u8] = b"\x1b[>4;0m";
/// How long an escape waits to be the start of a sequence.
const ESCAPE: Duration = Duration::from_millis(25);

/// Whether modifyOtherKeys was asked for, the terminal around lacking kitty's.
static MODIFYING: AtomicBool = AtomicBool::new(false);
/// Where the handler of resizes writes, waking the reader of input.
static RESIZED: AtomicI32 = AtomicI32::new(-1);

extern "C" fn resized(_: libc::c_int) {
    let fd = RESIZED.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: write is async-signal-safe; the pipe never blocks.
        unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
    }
}

/// The questions about colors: foreground, background, and the 16 of the palette.
fn colors() -> Vec<u8> {
    let mut questions = b"\x1b]10;?\x07\x1b]11;?\x07".to_vec();
    for index in 0..16 {
        questions.extend(format!("\x1b]4;{index};?\x07").into_bytes());
    }
    // the device attributes come last: all asked before is answered by then
    questions.extend(b"\x1b[c");
    questions
}

fn tell(bytes: &[u8]) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(bytes).and_then(|()| out.flush());
}

pub fn connect(state: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(ipc::socket_path(state))
}

pub fn start(options: &Options) -> Result<UnixStream, String> {
    if let Ok(stream) = connect(&options.state) {
        return Ok(stream);
    }
    mkdir(options.state.parent().ok_or("state has no parent")?)?;
    let log_path = suffixed(&options.state, ".server.log");
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|error| error.to_string())?;
    let mut command = Command::new(std::env::current_exe().map_err(|error| error.to_string())?);
    command.args([
        "--state",
        options.state.to_str().ok_or("non-UTF8 state path")?,
        "--config",
        options.config.to_str().ok_or("non-UTF8 config path")?,
        "server",
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; detach from the launching terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(stream) = connect(&options.state) {
            return Ok(stream);
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            // A simultaneous attach may have won the lock. Give its socket a chance.
            for _ in 0..25 {
                if let Ok(stream) = connect(&options.state) {
                    return Ok(stream);
                }
                thread::sleep(Duration::from_millis(40));
            }
            return Err(format!(
                "server exited ({status}); see {}",
                log_path.display()
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!("server did not start; see {}", log_path.display()));
        }
        thread::sleep(Duration::from_millis(40));
    }
}

pub fn control(mut stream: UnixStream, request: Request) -> Result<String, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    ipc::send(&mut stream, &request).map_err(|error| error.to_string())?;
    match ipc::receive(&mut stream).map_err(|error| error.to_string())? {
        Response::Result(result) => result,
        _ => Err("unexpected server response".into()),
    }
}

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        terminal::enable_raw_mode().map_err(|error| error.to_string())?;
        let guard = TerminalGuard;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
            .map_err(|error| error.to_string())?;
        // on the alternate screen, so leaving it gives the keys back too
        let mut questions = SETUP.to_vec();
        questions.extend(b"\x1b[?u\x1b[?996n");
        questions.extend(colors());
        tell(&questions);
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        tell(TEARDOWN);
        if MODIFYING.load(Ordering::Relaxed) {
            tell(MODIFY_OTHER_KEYS_OFF);
        }
        let _ = execute!(
            io::stdout(),
            Show,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

/// A pipe the handler of SIGWINCH writes to; its end to read.
fn resizes() -> Result<RawFd, String> {
    let mut fds = [0; 2];
    // SAFETY: pipe fills two descriptors; fcntl and signal take valid ones.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        RESIZED.store(fds[1], Ordering::Relaxed);
        libc::signal(libc::SIGWINCH, resized as *const () as libc::sighandler_t);
    }
    Ok(fds[0])
}

/// The conversation with the terminal around: what it answered so far.
struct Talk {
    /// The device attributes asked for, and not answered yet.
    asked: usize,
    kitty: bool,
    palette: Palette,
    sent: Option<Palette>,
}

impl Talk {
    /// Take an answer; returns what to tell the server.
    fn answer(&mut self, input: Input) -> Option<Request> {
        match input {
            Input::Event(event) => return Some(Request::Input(event)),
            Input::Keyboard(flags) => {
                self.kitty = flags != 0;
                return Some(Request::Keyboard(flags));
            }
            Input::Color(name, value) => {
                self.palette.colors.insert(name, value);
            }
            Input::Scheme(scheme) => {
                self.palette.scheme = Some(scheme);
                // a change, unasked: the colors changed with it
                if self.asked == 0 {
                    self.asked += 1;
                    tell(&colors());
                }
            }
            Input::Attributes => {
                self.asked = self.asked.saturating_sub(1);
                if self.asked == 0 {
                    if !self.kitty && !MODIFYING.swap(true, Ordering::Relaxed) {
                        tell(MODIFY_OTHER_KEYS);
                    }
                    // nothing told is nothing to answer programs with
                    let told = self.palette.scheme.is_some() || !self.palette.colors.is_empty();
                    if told && self.sent.as_ref() != Some(&self.palette) {
                        self.sent = Some(self.palette.clone());
                        return Some(Request::Colors(self.palette.clone()));
                    }
                }
            }
        }
        None
    }
}

/// Read the terminal around: events go to the server, answers settle how to
/// talk to the terminal; a lone escape is a key once nothing follows in time.
fn listen(mut stream: UnixStream, resized: RawFd) {
    let mut decoder = Decoder::default();
    let mut talk = Talk {
        asked: 1,
        kitty: false,
        palette: Palette::default(),
        sent: None,
    };
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let timeout = if decoder.pending() {
            ESCAPE.as_millis() as libc::c_int
        } else {
            -1
        };
        let mut fds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: resized,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two valid pollfd structures.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if ready < 0 {
            continue;
        }
        let mut inputs = Vec::new();
        if fds[1].revents != 0 {
            // SAFETY: reading into a buffer as large as told.
            while unsafe { libc::read(resized, buffer.as_mut_ptr().cast(), buffer.len()) } > 0 {}
            if let Ok((cols, rows)) = terminal::size() {
                inputs.push(Input::Event(Event::Resize(cols, rows)));
            }
        }
        if fds[0].revents != 0 {
            // SAFETY: reading into a buffer as large as told.
            let count =
                unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
            match count {
                0 => return,
                count if count > 0 => inputs.extend(decoder.feed(&buffer[..count as usize])),
                _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                _ => return,
            }
        } else if ready == 0 {
            inputs.extend(decoder.flush());
        }
        for input in inputs {
            if let Some(request) = talk.answer(input)
                && ipc::send(&mut stream, &request).is_err()
            {
                return;
            }
        }
    }
}

pub fn attach(
    mut stream: UnixStream,
    options: &Options,
    session: Option<String>,
) -> Result<(), String> {
    let (cols, rows) = terminal::size().map_err(|error| error.to_string())?;
    let request = Request::Attach {
        version: ipc::VERSION,
        session,
        profile: options.profile.clone(),
        config: options.config.clone(),
        cwd: std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string()),
        size: (rows, cols),
    };
    ipc::send(&mut stream, &request).map_err(|error| error.to_string())?;
    match ipc::receive(&mut stream).map_err(|error| error.to_string())? {
        Response::Ready { version, .. } if version == ipc::VERSION => {}
        Response::Result(Err(error)) => return Err(error),
        _ => return Err("unexpected attach response (old server?)".into()),
    }
    let _guard = TerminalGuard::enter()?;
    crate::server::signals();
    let resized = resizes()?;
    let input = stream.try_clone().map_err(|error| error.to_string())?;
    thread::spawn(move || listen(input, resized));
    let (sent, received) = mpsc::sync_channel(8);
    thread::spawn(move || {
        while let Ok(response) = ipc::receive::<Response>(&mut stream) {
            if sent.send(response).is_err() {
                break;
            }
        }
    });
    loop {
        if crate::server::stopped() {
            return Ok(());
        }
        match received.recv_timeout(Duration::from_millis(50)) {
            Ok(Response::Frame(bytes)) => {
                let mut out = io::stdout().lock();
                out.write_all(&bytes)
                    .and_then(|()| out.flush())
                    .map_err(|error| error.to_string())?;
            }
            Ok(Response::Closed) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Ok(Response::Result(Err(error))) => return Err(error),
            _ => {}
        }
    }
}
