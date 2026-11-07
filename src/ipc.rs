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

use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    buffer::{Buffer, Cell},
    layout::Rect,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    app::Event,
    runtime::{Palette, lock},
    store::mkdir,
};

pub const VERSION: u32 = 4;
const MAX_MESSAGE: usize = 8 * 1024 * 1024;
/// Clears the screen without scrolling it into the history, as `CSI 2 J`
/// does in terminals such as zellij.
pub const CLEAR: &[u8] = b"\x1b[m\x1b[H\x1b[J";
/// The most of a history passed on with one drawing; a longer one goes on
/// with the next.
const PUSHED: usize = 256 << 10;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Attach {
        version: u32,
        session: Option<String>,
        profile: Option<String>,
        config: PathBuf,
        cwd: Option<String>,
        size: (u16, u16),
        /// The tty of the terminal around: attaching from it again, it has
        /// the history passed on before.
        #[serde(default)]
        tty: Option<String>,
    },
    Input(crossterm::event::Event),
    /// Keyboard flags negotiated by the client; flag 2 enables precise holds.
    Keyboard(u16),
    /// The colors of the terminal around an attached client, for programs asking.
    Colors(Palette),
    Switch {
        session: Option<String>,
        from_session: u64,
        terminal: u64,
        cwd: Option<String>,
    },
    /// A command of decoration, run inside a terminal: for the instance that
    /// typed there last.
    Call {
        name: String,
        from_session: u64,
        terminal: u64,
    },
    List,
    Reload,
    Kill {
        apply: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ready {
        version: u32,
        client: u64,
    },
    /// Bytes for the terminal around the client: a drawing, and what programs
    /// pass on to it, such as a copy to the clipboard.
    Frame(Vec<u8>),
    /// The screen of the terminal around, as the terminal watched shows:
    /// alternate or main, before the drawing on it.
    Screen(bool),
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

/// Drawing in the terminal around a client. Its main screen keeps a history:
/// the rows passed on, which its own scrolling and search reach.
pub struct Render {
    pub terminal: Terminal<CrosstermBackend<FrameWriter>>,
    output: FrameWriter,
    /// The size of the terminal around: rows, columns.
    size: (u16, u16),
    /// The drawing it shows, to scroll along with what goes into its history.
    shown: Buffer,
    /// It shows its alternate screen.
    alternate: bool,
    /// The rows passed on to its history, as far as they are known to stay.
    history: usize,
    /// The bytes taken next go as one synchronized update.
    synchronized: bool,
}

impl Render {
    pub fn new(rows: u16, cols: u16) -> Result<Self, String> {
        Self::open((rows, cols), CLEAR.to_vec())
    }

    /// A fixed viewport, its bytes starting with `bytes`.
    fn open(size: (u16, u16), bytes: Vec<u8>) -> Result<Self, String> {
        let output = FrameWriter(Arc::new(Mutex::new(bytes)));
        let area = area(size.0, size.1);
        let terminal = Terminal::with_options(
            CrosstermBackend::new(output.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            terminal,
            output,
            size,
            shown: Buffer::empty(area),
            alternate: false,
            history: 0,
            synchronized: false,
        })
    }

    /// Draw over all again, beginning with `bytes`, keeping what is known of
    /// the terminal around.
    fn reopen(&mut self, size: (u16, u16), mut bytes: Vec<u8>) -> Result<(), String> {
        bytes.extend(CLEAR);
        let render = Self::open(size, bytes)?;
        *self = Self {
            alternate: self.alternate,
            history: self.history,
            ..render
        };
        Ok(())
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), String> {
        // Terminal::resize on CrosstermBackend queries the *server\'s* TTY
        // while clearing a fixed viewport. There is no server TTY. Rebuild a
        // fixed viewport instead, and send a complete, clean frame.
        // Growing, the terminal around brings rows of its history back onto
        // its screen: scroll them back before drawing over them.
        let mut bytes = Vec::new();
        if !self.alternate {
            let back = usize::from(rows.saturating_sub(self.size.0)).min(self.history);
            if back > 0 {
                bytes.extend(b"\x1b[9999H");
                bytes.resize(bytes.len() + back, b'\n');
            }
        }
        self.reopen((rows, cols), bytes)
    }

    pub fn alternate(&self) -> bool {
        self.alternate
    }

    /// Show the alternate screen or the main one, drawing it all anew.
    pub fn switch(&mut self, alternate: bool) -> Result<(), String> {
        self.alternate = alternate;
        self.reopen(self.size, Vec::new())
    }

    /// The program cleared its history: so does the terminal around.
    pub fn forget(&mut self) {
        lock(&self.output.0).extend_from_slice(b"\x1b[3J");
        self.history = 0;
    }

    /// Pass rows on to the history of the terminal around, as many as fit
    /// one drawing: each is written at the top and scrolled away from the
    /// bottom, the drawing along, so the next one draws only what is new.
    /// Returns how many went.
    pub fn push<'a>(&mut self, rows: impl IntoIterator<Item = &'a str>) -> usize {
        let mut bytes: Vec<u8> = Vec::new();
        let mut count = 0;
        for row in rows {
            if bytes.len() >= PUSHED {
                break;
            }
            // as wide as the screen at most: a wider one would wrap
            let row = vt100::capture::basic_formatted_rows(row, self.size.1.max(1))
                .next()
                .unwrap_or_default();
            bytes.extend(b"\x1b[H\x1b[2K");
            bytes.extend(row.as_bytes());
            bytes.extend(b"\x1b[m\x1b[9999H\n");
            count += 1;
        }
        if count == 0 {
            return 0;
        }
        let mut output = lock(&self.output.0);
        if !std::mem::replace(&mut self.synchronized, true) {
            output.extend(b"\x1b[?2026h");
        }
        output.extend(b"\x1b[?25l\x1b[m");
        output.extend(bytes);
        drop(output);
        self.history = self.history.saturating_add(count);
        self.scroll(count);
        count
    }

    /// The drawing shown went up by `rows`, blank below.
    fn scroll(&mut self, rows: usize) {
        let width = usize::from(self.shown.area.width);
        let content = &mut self.shown.content;
        let gone = rows.saturating_mul(width).min(content.len());
        content.drain(..gone);
        content.resize(content.len() + gone, Cell::EMPTY);
        // what the next drawing changes
        self.terminal
            .current_buffer_mut()
            .content
            .clone_from_slice(content);
        self.terminal.swap_buffers();
    }

    pub fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        let frame = self.terminal.draw(render)?;
        self.shown.clone_from(frame.buffer);
        Ok(())
    }

    pub fn take(&mut self) -> Vec<u8> {
        let mut output = lock(&self.output.0);
        if std::mem::take(&mut self.synchronized) {
            output.extend(b"\x1b[?2026l");
        }
        std::mem::take(&mut *output)
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
    use ratatui::text::Text;

    /// The terminal around: an emulator with a history.
    fn around(rows: u16, cols: u16) -> vt100::Parser {
        let size = |n| std::num::NonZeroU16::new(n).unwrap();
        vt100::Parser::new(size(rows), size(cols), 100)
    }

    fn history(parser: &mut vt100::Parser) -> Vec<String> {
        let rows = usize::from(parser.screen().size().0.get());
        let (mut history, mut offset) = (Vec::new(), parser.screen().scrollback_len());
        while offset > 0 {
            parser.screen_mut().set_scrollback(offset);
            let taken = offset.min(rows);
            history.extend(parser.screen().rows(0, 100).take(taken));
            offset -= taken;
        }
        parser.screen_mut().set_scrollback(0);
        history
    }

    fn draw(render: &mut Render, rows: &[&str]) {
        let text = Text::raw(rows.join("\n"));
        render
            .draw(|frame| frame.render_widget(text, frame.area()))
            .unwrap();
    }

    /// The text bytes write, without their sequences.
    fn printed(bytes: &[u8]) -> String {
        let mut text = String::new();
        let mut bytes = bytes.iter().copied();
        while let Some(byte) = bytes.next() {
            match byte {
                0x1b => {
                    if bytes.next() == Some(b'[') {
                        for byte in bytes.by_ref() {
                            if (0x40..0x7f).contains(&byte) {
                                break;
                            }
                        }
                    }
                }
                byte if byte.is_ascii_graphic() => text.push(byte as char),
                _ => {}
            }
        }
        text
    }

    #[test]
    fn rows_scroll_into_the_history_around_with_the_drawing() {
        let mut outer = around(4, 10);
        let mut render = Render::new(4, 10).unwrap();
        draw(&mut render, &["a", "b", "c", "d"]);
        outer.process(&render.take());
        assert_eq!(render.push(["a", "\x1b[31mb", "wider than the screen"]), 3);
        draw(&mut render, &["d", "e", "f", "g"]);
        let bytes = render.take();
        outer.process(&bytes);
        assert_eq!(history(&mut outer), ["a", "b", "wider than"]);
        assert_eq!(outer.screen().contents(), "d\ne\nf\ng");
        assert_eq!(
            outer.screen().cell(1, 0).unwrap().fgcolor(),
            vt100::Color::Default
        );
        // the drawing went up along, only what is new is drawn
        assert_eq!(printed(&bytes), "abwiderthanefg");
        assert!(bytes.starts_with(b"\x1b[?2026h") && bytes.ends_with(b"\x1b[?2026l"));

        // growing, the terminal around brings history back: it goes back
        outer.set_size(
            std::num::NonZeroU16::new(6).unwrap(),
            std::num::NonZeroU16::new(10).unwrap(),
        );
        render.resize(6, 10).unwrap();
        draw(&mut render, &["g", "h"]);
        outer.process(&render.take());
        assert_eq!(history(&mut outer), ["a", "b", "wider than"]);
        assert_eq!(outer.screen().contents(), "g\nh");

        // the alternate screen draws anew and keeps the history
        render.switch(true).unwrap();
        assert!(render.alternate());
        outer.process(b"\x1b[?1049h");
        draw(&mut render, &["vim"]);
        outer.process(&render.take());
        assert_eq!(outer.screen().contents(), "vim");
        render.switch(false).unwrap();
        outer.process(b"\x1b[?1049l");
        draw(&mut render, &["g", "h", "i"]);
        outer.process(&render.take());
        assert_eq!(outer.screen().contents(), "g\nh\ni");
        assert_eq!(history(&mut outer).len(), 3);

        // a program clearing its history clears it around
        render.forget();
        outer.process(&render.take());
        assert!(history(&mut outer).is_empty());
        assert_eq!(render.push([]), 0);
        assert!(render.take().is_empty());
    }

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
