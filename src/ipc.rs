//! Local transport. Clients send input, never replacement states. Slow clients
//! disconnect instead of blocking session mutation or losing rendering diffs.

use std::{
    fs,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::{fs::MetadataExt, net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Sender, SyncSender},
    },
    thread,
    time::Duration,
};

use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, layout::Rect};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{app::Event, runtime::lock, store::mkdir};

pub const VERSION: u32 = 1;
const MAX_MESSAGE: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Attach {
        version: u32,
        session: Option<String>,
        profile: Option<String>,
        config: PathBuf,
        cwd: Option<String>,
        size: (u16, u16),
    },
    Input(crossterm::event::Event),
    Switch {
        session: Option<String>,
        from_session: u64,
        terminal: u64,
        cwd: Option<String>,
    },
    List,
    Reload,
    Kill {
        apply: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ready { version: u32, client: u64 },
    Frame(Vec<u8>),
    Result(Result<String, String>),
    Closed,
}

pub fn send<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_MESSAGE {
        return Err(io::Error::other("message too large"));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

pub fn receive<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_MESSAGE {
        return Err(io::Error::other("message too large"));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

pub fn socket_path(state: &Path) -> PathBuf {
    // Stable across runs and independent of Rust's randomized hash maps.
    let hash = state
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ *byte as u64).wrapping_mul(0x100000001b3)
        });
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    // SAFETY: geteuid has no preconditions.
    base.join(format!("nui-yalij-{}", unsafe { libc::geteuid() }))
        .join(format!("{hash:016x}.sock"))
}

pub fn prepare_socket(path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("socket has no parent")?;
    mkdir(parent)?;
    let metadata = fs::symlink_metadata(parent).map_err(|error| error.to_string())?;
    // Don't follow someone else's directory or accept a shared writable socket directory.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(format!(
            "{} must be a private directory owned by this user",
            parent.display()
        ));
    }
    if path.as_os_str().as_encoded_bytes().len() >= 104 {
        return Err("socket path too long; use a shorter XDG_RUNTIME_DIR".into());
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[derive(Clone, Default)]
pub struct FrameWriter(Arc<Mutex<Vec<u8>>>);
impl Write for FrameWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        lock(&self.0).extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct Render {
    pub terminal: Terminal<CrosstermBackend<FrameWriter>>,
    output: FrameWriter,
}

impl Render {
    pub fn new(rows: u16, cols: u16) -> Result<Self, String> {
        let output = FrameWriter::default();
        lock(&output.0).extend_from_slice(b"\x1b[2J\x1b[H");
        let terminal = Terminal::with_options(
            CrosstermBackend::new(output.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area(rows, cols)),
            },
        )
        .map_err(|error| error.to_string())?;
        Ok(Self { terminal, output })
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), String> {
        // Terminal::resize on CrosstermBackend queries the *server\'s* TTY
        // while clearing a fixed viewport. There is no server TTY. Rebuild a
        // fixed viewport instead, and send a complete, clean frame.
        *self = Self::new(rows, cols)?;
        Ok(())
    }

    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut *lock(&self.output.0))
    }
}

fn area(rows: u16, cols: u16) -> Rect {
    Rect::new(0, 0, cols.clamp(1, 500), rows.clamp(1, 250))
}

pub struct Peer {
    outgoing: SyncSender<Response>,
    socket: UnixStream,
    pub render: Option<Render>,
    writer: Option<thread::JoinHandle<()>>,
}

impl Peer {
    pub fn spawn(id: u64, stream: UnixStream, events: Sender<Event>) -> Result<Peer, String> {
        // On macOS accepted sockets inherit the listener\'s nonblocking flag.
        stream
            .set_nonblocking(false)
            .map_err(|error| error.to_string())?;
        let mut reader = stream.try_clone().map_err(|error| error.to_string())?;
        let mut writer = stream.try_clone().map_err(|error| error.to_string())?;
        writer
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| error.to_string())?;
        let (outgoing, replies) = mpsc::sync_channel(8);
        let input = events.clone();
        thread::spawn(move || {
            while let Ok(request) = receive(&mut reader) {
                if input.send(Event::Request(id, request)).is_err() {
                    break;
                }
            }
            let _ = input.send(Event::Disconnected(id));
        });
        let writer_thread = thread::spawn(move || {
            for response in replies {
                let closing = matches!(response, Response::Result(_) | Response::Closed);
                if send(&mut writer, &response).is_err() || closing {
                    break;
                }
            }
            let _ = writer.shutdown(Shutdown::Both);
            let _ = events.send(Event::Disconnected(id));
        });
        Ok(Peer {
            outgoing,
            socket: stream,
            render: None,
            writer: Some(writer_thread),
        })
    }

    pub fn send(&self, response: Response) -> bool {
        self.outgoing.try_send(response).is_ok()
    }

    pub fn finish(&mut self) {
        if !self.send(Response::Closed) {
            let _ = self.socket.shutdown(Shutdown::Both);
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_is_framed_and_bounded() {
        let mut bytes = Vec::new();
        send(&mut bytes, &Request::Kill { apply: false }).unwrap();
        assert!(matches!(
            receive::<Request>(&mut bytes.as_slice()).unwrap(),
            Request::Kill { apply: false }
        ));
        let bytes = u32::MAX.to_be_bytes();
        assert!(receive::<Request>(&mut bytes.as_slice()).is_err());
    }
}
