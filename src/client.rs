//! A thin terminal client: raw input in, rendered diffs out. No local session
//! state, plugins or PTY children, so disconnecting cannot stop a session.

use std::{
    fs::OpenOptions,
    io::{self, Write},
    os::unix::{fs::OpenOptionsExt, net::UnixStream, process::CommandExt},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    cursor::Show,
    event::{self, DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};

use crate::{
    cli::Options,
    ipc::{self, Request, Response},
    store::{mkdir, suffixed},
};

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
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            Show,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
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
    let mut input = stream.try_clone().map_err(|error| error.to_string())?;
    thread::spawn(move || {
        while let Ok(event) = event::read() {
            if ipc::send(&mut input, &Request::Input(event)).is_err() {
                break;
            }
        }
    });
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
