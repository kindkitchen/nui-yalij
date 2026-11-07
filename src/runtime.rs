//! The runtime: the processes behind the terminals of the state, in PTYs.
//!
//! It follows the state (`reconcile`) and never changes it: what it learns
//! goes back as facts, which become state like any other change.

use std::{
    collections::{BTreeMap, HashMap},
    io::{ErrorKind, Read, Write},
    num::NonZeroU16,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};

use crate::app::Event;
use crate::keys;
use crate::state::{Fact, Nui, Observation, Status, Terminal};

/// The screen of a terminal: an emulator fed with the output of its process.
pub type Screen = Arc<Mutex<vt100::Parser<Replies>>>;

/// What programs asked of the terminal around the person, by terminal id.
type Effects = Arc<Mutex<Vec<(u64, Effect)>>>;

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// Rendering can look into any session without taking ownership of its PTYs.
pub trait Screens {
    fn screen_at(&self, session: usize, terminal: u64) -> Option<&Screen>;
}

impl Screens for Runtime {
    fn screen_at(&self, _: usize, terminal: u64) -> Option<&Screen> {
        self.screen(terminal)
    }
}

/// How long a stopped process may take before it is killed.
const GRACE: Duration = Duration::from_secs(2);
/// How long a synchronized update may hold its output back, and how much.
const SYNC: Duration = Duration::from_millis(200);
const HOLD: usize = 4 << 20;
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
/// How long progress shows without news of it, as terminals forget it.
pub const PROGRESS: Duration = Duration::from_secs(15);
/// The longest text passed on: a notification, a copy to the clipboard.
const MAX_TEXT: usize = 1024;
const MAX_COPY: usize = 1 << 20;

/// The colors of the terminal around the person, as it told them, for
/// programs asking: `10` foreground, `11` background, `4;n` the palette.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Palette {
    /// The color scheme: 1 dark, 2 light.
    pub scheme: Option<u8>,
    pub colors: BTreeMap<String, String>,
}

/// What a program asked of the terminal around the person, beyond its screen.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Bell,
    /// A copy to the clipboard, OSC 52.
    Copy(Vec<u8>),
    /// A notification or progress, as the sequence to pass on.
    Notify(Vec<u8>),
}

pub struct Runtime {
    processes: HashMap<u64, Process>,
    events: Sender<Event>,
    /// Set while an output event waits to be handled, so bursts make one.
    pub pending: Arc<AtomicBool>,
    /// The size of terminals before a layout gives them one: the whole screen.
    pub size: (u16, u16),
    generation: u64,
    pub owner: u64,
    pub global: Option<PathBuf>,
    pub config: Option<PathBuf>,
    /// The commands of decoration, first on the PATH of programs.
    pub bin: Option<PathBuf>,
    /// Children already hung up but not yet reaped. A daemon must not exit
    /// before their delayed force-kill has had a chance to run.
    retiring: Vec<Arc<AtomicBool>>,
    palette: Arc<Mutex<Palette>>,
    effects: Effects,
}

/// A terminal of the state: its screen, and its process while it runs.
struct Process {
    screen: Screen,
    size: (u16, u16),
    live: Option<Live>,
}

struct Live {
    /// Tells this run from the earlier ones of the same terminal.
    generation: u64,
    pid: Option<u32>,
    /// Set once the process is gone, waited for.
    ended: Arc<AtomicBool>,
    master: Box<dyn MasterPty + Send>,
    writer: Writer,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl Runtime {
    pub fn new(events: Sender<Event>, size: (u16, u16)) -> Runtime {
        Runtime {
            processes: HashMap::new(),
            events,
            pending: Arc::default(),
            size,
            generation: 0,
            owner: 0,
            global: None,
            config: None,
            bin: None,
            retiring: Vec::new(),
            palette: Arc::default(),
            effects: Arc::default(),
        }
    }

    /// Make the processes match the state: start the terminals that should
    /// run, stop the others, drop the ones gone. Returns what it learned.
    pub fn reconcile(&mut self, nui: &Nui) -> Vec<Fact> {
        let mut terminals: Vec<&Terminal> = nui.terminals().collect();
        terminals.sort_by_key(|terminal| terminal.id);
        self.retiring.retain(|ended| !ended.load(Ordering::Acquire));
        let retiring = &mut self.retiring;
        self.processes.retain(|id, process| {
            let kept = terminals.iter().any(|terminal| terminal.id == *id);
            if !kept && let Some(ended) = process.stop() {
                retiring.push(ended);
            }
            kept
        });
        let mut facts = Vec::new();
        for terminal in terminals {
            let running = self.running(terminal.id);
            match terminal.status {
                Status::Running if !running => {
                    if let Err(error) = self.spawn(terminal) {
                        facts.push(Fact::Failed(terminal.id, error));
                        continue;
                    }
                }
                Status::Stopped | Status::Exited if running => {
                    if let Some(process) = self.processes.get_mut(&terminal.id)
                        && let Some(ended) = process.stop()
                    {
                        self.retiring.push(ended);
                    }
                }
                _ => {}
            }
            let pid = self.live(terminal.id).and_then(|live| live.pid);
            if terminal.pid != pid {
                facts.push(Fact::Pid(terminal.id, pid));
            }
        }
        facts
    }

    fn spawn(&mut self, terminal: &Terminal) -> Result<(), String> {
        let (rows, cols) = self
            .processes
            .get(&terminal.id)
            .map_or(self.size, |process| process.size);
        let pair = native_pty_system()
            .openpty(pty_size(rows, cols))
            .map_err(|error| error.to_string())?;
        let mut command = CommandBuilder::new(&terminal.command);
        command.args(&terminal.args);
        match terminal
            .cwd
            .as_deref()
            .filter(|cwd| Path::new(cwd).is_dir())
        {
            Some(cwd) => command.cwd(cwd),
            None => {
                if let Some(home) = std::env::var_os("HOME") {
                    command.cwd(home);
                }
            }
        }
        // programs inside see this emulator, not the terminal around it
        command.env("TERM", "xterm-256color");
        command.env("TERM_PROGRAM", "nui-yalij");
        command.env_remove("TERM_PROGRAM_VERSION");
        if let Some(global) = &self.global {
            command.env("NUI_YALIJ_STATE", global);
            command.env("NUI_YALIJ_SESSION", self.owner.to_string());
            command.env("NUI_YALIJ_TERMINAL", terminal.id.to_string());
        }
        if let Some(config) = &self.config {
            command.env("NUI_YALIJ_CONFIG", config);
        }
        if let Some(bin) = &self.bin {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let paths = std::iter::once(bin.clone()).chain(std::env::split_paths(&path));
            if let Ok(path) = std::env::join_paths(paths) {
                command.env("PATH", path);
            }
        }
        let mut child = pair.slave.spawn_command(command).map_err(|error| {
            let error = error.to_string();
            let reason = if error.contains("No viable candidates") {
                "not found"
            } else {
                error.lines().next().unwrap_or_default()
            };
            format!("{}: {reason}", terminal.command)
        })?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| error.to_string())?;
        let writer: Writer = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .map_err(|error| error.to_string())?,
        ));
        self.generation += 1;
        let generation = self.generation;
        let screen: Screen = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            dimension(rows),
            dimension(cols),
            0,
            Replies::new(self.palette.clone()),
        )));
        let output = Output {
            screen: screen.clone(),
            writer: writer.clone(),
            effects: self.effects.clone(),
            terminal: terminal.id,
        };
        let (events, pending, owner) = (self.events.clone(), self.pending.clone(), self.owner);
        thread::spawn(move || read(reader, output, events, pending, owner));
        let (events, id, pid, killer) = (
            self.events.clone(),
            terminal.id,
            child.process_id(),
            child.clone_killer(),
        );
        let ended = Arc::new(AtomicBool::new(false));
        let waited = ended.clone();
        thread::spawn(move || {
            let code = child.wait().map_or(-1, |status| status.exit_code() as i32);
            waited.store(true, Ordering::Release);
            let _ = events.send(Event::Exited {
                session: owner,
                id,
                generation,
                code,
            });
        });
        let live = Live {
            generation,
            pid,
            ended,
            master: pair.master,
            writer,
            killer,
        };
        self.processes.insert(
            terminal.id,
            Process {
                screen,
                size: (rows, cols),
                live: Some(live),
            },
        );
        Ok(())
    }

    /// A process ended: true when it was the current run of its terminal.
    pub fn exited(&mut self, id: u64, generation: u64) -> bool {
        let Some(process) = self.processes.get_mut(&id) else {
            return false;
        };
        if process
            .live
            .as_ref()
            .is_none_or(|live| live.generation != generation)
        {
            return false;
        }
        process.live = None;
        true
    }

    fn live(&self, id: u64) -> Option<&Live> {
        self.processes.get(&id)?.live.as_ref()
    }

    pub fn running(&self, id: u64) -> bool {
        self.live(id).is_some()
    }

    pub fn screen(&self, id: u64) -> Option<&Screen> {
        self.processes.get(&id).map(|process| &process.screen)
    }

    /// Send input to the process of a terminal.
    pub fn write(&self, id: u64, bytes: &[u8]) {
        if let Some(live) = self.live(id) {
            let mut writer = lock(&live.writer);
            let _ = writer.write_all(bytes).and_then(|()| writer.flush());
        }
    }

    /// How the program of a terminal wants its keys.
    pub fn keys(&self, id: u64) -> keys::Mode {
        self.screen(id).map_or_else(keys::Mode::default, |screen| {
            let parser = lock(screen);
            parser.callbacks().keys(parser.screen())
        })
    }

    /// Fit a terminal to the pane showing it.
    pub fn resize(&mut self, id: u64, rows: u16, cols: u16) {
        let size = (rows.max(1), cols.max(1));
        let Some(process) = self.processes.get_mut(&id) else {
            return;
        };
        if process.size == size {
            return;
        }
        process.size = size;
        lock(&process.screen)
            .screen_mut()
            .set_size(dimension(size.0), dimension(size.1));
        if let Some(live) = &process.live {
            let _ = live.master.resize(pty_size(size.0, size.1));
        }
    }

    /// The colors of the terminal around the person. Programs asking for
    /// reports of the color scheme learn when it changes.
    pub fn paint(&self, palette: &Palette) {
        let before = std::mem::replace(&mut *lock(&self.palette), palette.clone());
        let Some(scheme) = palette.scheme.filter(|_| before.scheme.is_some()) else {
            return;
        };
        if before.scheme == Some(scheme) {
            return;
        }
        for (id, process) in &self.processes {
            if lock(&process.screen).callbacks().scheme_reports {
                self.write(*id, format!("\x1b[?997;{scheme}n").as_bytes());
            }
        }
    }

    /// A terminal gains or loses the focus. Its program learns it when it
    /// asked to; a bell is seen once its terminal is focused.
    pub fn focus(&self, id: u64, focused: bool) {
        let Some(screen) = self.screen(id) else {
            return;
        };
        let reports = {
            let mut parser = lock(screen);
            let callbacks = parser.callbacks_mut();
            callbacks.bell &= !focused;
            callbacks.focus_reports
        };
        if reports {
            self.write(id, if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    /// A bell rang where nobody looks: its terminal shows it until focused.
    pub fn ring(&self, id: u64) {
        if let Some(screen) = self.screen(id) {
            lock(screen).callbacks_mut().bell = true;
        }
    }

    /// What programs asked of the terminal around the person since last time.
    pub fn take_effects(&self) -> Vec<(u64, Effect)> {
        std::mem::take(&mut *lock(&self.effects))
    }

    /// Show the synchronized updates that take too long to end; true when
    /// some did.
    pub fn release(&self) -> bool {
        let mut released = false;
        for (id, process) in &self.processes {
            let Some(live) = &process.live else {
                continue;
            };
            let due = lock(&process.screen)
                .callbacks()
                .held_since
                .is_some_and(|since| since.elapsed() >= SYNC);
            if due {
                let output = Output {
                    screen: process.screen.clone(),
                    writer: live.writer.clone(),
                    effects: self.effects.clone(),
                    terminal: *id,
                };
                released |= output.emulate(&[], false);
            }
        }
        released
    }

    /// What runs in a terminal now: the foreground program, its directory and
    /// the title it set.
    pub fn observe(&self, id: u64) -> Option<Observation> {
        let live = self.live(id)?;
        let pid = live
            .master
            .process_group_leader()
            .filter(|&pid| pid > 0)
            .or(live.pid.map(|pid| pid as i32))?;
        let title = self
            .screen(id)
            .and_then(|screen| lock(screen).callbacks().title.clone());
        Some(Observation {
            program: process::name(pid),
            cwd: process::cwd(pid),
            title,
        })
    }

    /// Stop AND reap children before the hosting daemon can exit, including
    /// ones stopped earlier whose grace period is still running.
    pub fn shutdown(&mut self) {
        for process in self.processes.values_mut() {
            if let Some(ended) = process.stop() {
                self.retiring.push(ended);
            }
        }
        let deadline = Instant::now() + GRACE + Duration::from_secs(1);
        while self
            .retiring
            .iter()
            .any(|ended| !ended.load(Ordering::Acquire))
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        self.retiring.retain(|ended| !ended.load(Ordering::Acquire));
    }
}

impl Process {
    /// Hang up the process, and kill it when it ignores that; its screen
    /// stays, as it was left.
    fn stop(&mut self) -> Option<Arc<AtomicBool>> {
        let mut live = self.live.take()?;
        let _ = live.killer.kill();
        let waited = live.ended.clone();
        if let Some(pid) = live.pid {
            let ended = waited.clone();
            thread::spawn(move || {
                thread::sleep(GRACE);
                if !ended.load(Ordering::Acquire) {
                    // SAFETY: a plain signal to our child, not yet reported as reaped.
                    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                }
            });
        }
        Some(waited)
    }
}

/// Where the output of a process goes: its screen, its answers back to it,
/// and what it asks beyond the screen.
struct Output {
    screen: Screen,
    writer: Writer,
    effects: Effects,
    terminal: u64,
}

impl Output {
    /// Feed the screen; `end` when the program is gone. False when nothing
    /// shows yet, held back in a synchronized update.
    fn emulate(&self, bytes: &[u8], end: bool) -> bool {
        let (shown, replies, effects) = {
            let mut parser = lock(&self.screen);
            let shown = feed(&mut parser, bytes, end);
            let callbacks = parser.callbacks_mut();
            (
                shown,
                std::mem::take(&mut callbacks.replies),
                std::mem::take(&mut callbacks.effects),
            )
        };
        if !replies.is_empty() {
            let mut writer = lock(&self.writer);
            let _ = writer.write_all(&replies).and_then(|()| writer.flush());
        }
        if !effects.is_empty() {
            let id = self.terminal;
            lock(&self.effects).extend(effects.into_iter().map(|effect| (id, effect)));
        }
        shown
    }
}

/// Feed a screen with the output of its process, answering the queries the
/// program makes of its terminal.
fn read(
    mut reader: Box<dyn Read + Send>,
    output: Output,
    events: Sender<Event>,
    pending: Arc<AtomicBool>,
    owner: u64,
) {
    // false once nobody listens anymore
    let notify =
        || pending.swap(true, Ordering::AcqRel) || events.send(Event::Output(owner)).is_ok();
    let mut buffer = [0; 16 * 1024];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if output.emulate(&buffer[..count], false) && !notify() {
            return;
        }
    }
    // the program is gone: whatever it held back shows as it was left
    if output.emulate(&[], true) {
        notify();
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Process output, holding a synchronized update back until it ends, so a
/// frame never shows half drawn; `end` shows what is held. True when the
/// screen took something.
fn feed(parser: &mut vt100::Parser<Replies>, bytes: &[u8], end: bool) -> bool {
    let callbacks = parser.callbacks_mut();
    let mut input = std::mem::take(&mut callbacks.held);
    input.extend_from_slice(bytes);
    let mut since = callbacks.held_since.take();
    let mut at = 0;
    let mut shown = false;
    while at < input.len() {
        let rest = &input[at..];
        let taken = match since {
            Some(began) => match find(rest, SYNC_END) {
                Some(found) => {
                    since = None;
                    found + SYNC_END.len()
                }
                None if end || began.elapsed() >= SYNC || rest.len() > HOLD => {
                    since = None;
                    rest.len()
                }
                None => break,
            },
            None => match find(rest, SYNC_BEGIN) {
                Some(found) => {
                    since = Some(Instant::now());
                    found + SYNC_BEGIN.len()
                }
                None => rest.len(),
            },
        };
        process(parser, &rest[..taken]);
        at += taken;
        shown = true;
    }
    // a timeout ends an update that made nothing more since
    if since.is_some_and(|began| at == input.len() && (end || began.elapsed() >= SYNC)) {
        since = None;
    }
    let callbacks = parser.callbacks_mut();
    callbacks.held = input.split_off(at);
    callbacks.held_since = since;
    shown
}

/// Process bytes; a full reset (RIS) resets what the emulator keeps beyond
/// the screen too.
fn process(parser: &mut vt100::Parser<Replies>, bytes: &[u8]) {
    let mut rest = bytes;
    if parser.callbacks().escaped && rest.first() == Some(&b'c') {
        parser.process(&rest[..1]);
        parser.callbacks_mut().reset();
        rest = &rest[1..];
    }
    while let Some(found) = find(rest, b"\x1bc") {
        parser.process(&rest[..found + 2]);
        parser.callbacks_mut().reset();
        rest = &rest[found + 2..];
    }
    parser.process(rest);
    let escaped = bytes.last() == Some(&0x1b);
    parser.callbacks_mut().escaped = escaped;
}

/// The emulator's side of a conversation with a program: what to answer it,
/// what it set, and what it asks of the terminal around the person.
#[derive(Default)]
pub struct Replies {
    /// Answers to write back to the program.
    pub replies: Vec<u8>,
    pub title: Option<String>,
    /// The kitty keyboard flags pushed, on the main and the alternate screen.
    keyboard: [Vec<u8>; 2],
    /// The level of xterm's modifyOtherKeys.
    modify_other_keys: u8,
    /// DEC 1004: the program wants to know when it gains or loses focus.
    pub focus_reports: bool,
    /// DEC 2031: the program wants to know when the color scheme changes.
    pub scheme_reports: bool,
    /// Output held back while a synchronized update goes, since when.
    held: Vec<u8>,
    held_since: Option<Instant>,
    /// The last output ended with an escape, maybe of a split reset.
    escaped: bool,
    /// A bell rang that nobody saw yet.
    pub bell: bool,
    /// Progress the program reports, OSC 9;4: its state, percent and when.
    pub progress: Option<(u8, u8, Instant)>,
    /// What goes beyond the screen, for the server to pass on.
    effects: Vec<Effect>,
    palette: Arc<Mutex<Palette>>,
}

/// The kitty keyboard flags a terminal keeps at most, per screen.
const KEYBOARD_STACK: usize = 16;

impl Replies {
    fn new(palette: Arc<Mutex<Palette>>) -> Replies {
        Replies {
            palette,
            ..Replies::default()
        }
    }

    /// How the program wants its keys.
    pub fn keys(&self, screen: &vt100::Screen) -> keys::Mode {
        keys::Mode {
            application_cursor: screen.application_cursor(),
            kitty: self.flags(screen.alternate_screen()),
            modify_other_keys: self.modify_other_keys,
        }
    }

    fn flags(&self, alternate: bool) -> u8 {
        self.keyboard[alternate as usize]
            .last()
            .copied()
            .unwrap_or(0)
    }

    /// Progress reported lately: its state and percent.
    pub fn progress(&self) -> Option<(u8, u8)> {
        self.progress
            .filter(|(_, _, at)| at.elapsed() < PROGRESS)
            .map(|(state, percent, _)| (state, percent))
    }

    /// After a full reset, as a terminal starts.
    fn reset(&mut self) {
        self.keyboard = Default::default();
        self.modify_other_keys = 0;
        self.focus_reports = false;
        self.scheme_reports = false;
        self.progress = None;
    }

    /// Kitty's progressive enhancement: CSI > flags u pushes, CSI < n u
    /// pops, CSI = flags ; mode u sets, on the stack of the screen in use.
    fn keyboard(&mut self, screen: &vt100::Screen, operation: u8, first: u16, second: u16) {
        let alternate = screen.alternate_screen();
        if !alternate {
            // the alternate screen starts afresh
            self.keyboard[1].clear();
        }
        let stack = &mut self.keyboard[alternate as usize];
        let flags = first as u8 & keys::DISAMBIGUATE;
        match operation {
            b'>' => {
                if stack.len() == KEYBOARD_STACK {
                    stack.remove(0);
                }
                stack.push(flags);
            }
            b'<' => {
                let count = (first as usize).max(1);
                stack.truncate(stack.len().saturating_sub(count));
            }
            _ => {
                if stack.is_empty() {
                    stack.push(0);
                }
                let top = stack.last_mut().unwrap();
                *top = match second {
                    2 => *top | flags,
                    3 => *top & !flags,
                    _ => flags,
                };
            }
        }
    }

    /// The answer of DECRQM about a private mode: 1 set, 2 reset, 0 unknown.
    fn mode(&self, screen: &vt100::Screen, mode: u16) -> u8 {
        let set = match mode {
            1 => screen.application_cursor(),
            25 => !screen.hide_cursor(),
            47 | 1047 | 1049 => screen.alternate_screen(),
            1004 => self.focus_reports,
            2004 => screen.bracketed_paste(),
            2026 => self.held_since.is_some(),
            2031 => self.scheme_reports,
            _ => return 0,
        };
        if set { 1 } else { 2 }
    }

    /// Answer a question about a color, when the terminal around told it.
    fn color(&mut self, name: &str) {
        if let Some(value) = lock(&self.palette).colors.get(name) {
            self.replies
                .extend(format!("\x1b]{name};{value}\x07").into_bytes());
        }
    }
}

/// Text to pass on: printable, and not too long.
fn plain(parts: &[&[u8]]) -> String {
    let text = parts
        .iter()
        .map(|part| String::from_utf8_lossy(part))
        .collect::<Vec<_>>()
        .join(";");
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect()
}

impl vt100::Callbacks for Replies {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.effects.push(Effect::Bell);
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        let title = String::from_utf8_lossy(title).trim().to_string();
        self.title = (!title.is_empty()).then_some(title);
    }

    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        if data.len() <= MAX_COPY {
            let mut copy = b"\x1b]52;".to_vec();
            copy.extend_from_slice(ty);
            copy.push(b';');
            copy.extend_from_slice(data);
            copy.push(0x07);
            self.effects.push(Effect::Copy(copy));
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        intermediate: Option<u8>,
        next: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let param = |i: usize| {
            params
                .get(i)
                .and_then(|param| param.first())
                .copied()
                .unwrap_or(0)
        };
        let first = param(0);
        match (intermediate, next, c) {
            // the cursor position, which shells such as nushell wait for
            (None, None, 'n') if first == 6 => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .extend(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
            }
            (None, None, 'n') if first == 5 => self.replies.extend(b"\x1b[0n"),
            // primary device attributes: a VT220 with colors
            (None, None, 'c') if first == 0 => self.replies.extend(b"\x1b[?62;22c"),
            (Some(b'?'), None, 'u') => {
                let flags = self.flags(screen.alternate_screen());
                self.replies.extend(format!("\x1b[?{flags}u").into_bytes());
            }
            (Some(operation @ (b'>' | b'<' | b'=')), None, 'u') => {
                self.keyboard(screen, operation, first, param(1))
            }
            // modifyOtherKeys: CSI > 4 ; level m, or CSI > 4 m to reset it
            (Some(b'>'), None, 'm') if first == 4 || params.len() <= 1 && first == 0 => {
                self.modify_other_keys = (param(1) as u8).min(2);
            }
            (Some(b'?'), None, 'h' | 'l') => {
                for mode in params.iter().filter_map(|param| param.first()) {
                    match mode {
                        1004 => self.focus_reports = c == 'h',
                        2031 => self.scheme_reports = c == 'h',
                        _ => {}
                    }
                }
            }
            (Some(b'?'), None, 'n') if first == 996 => {
                if let Some(scheme) = lock(&self.palette).scheme {
                    self.replies
                        .extend(format!("\x1b[?997;{scheme}n").into_bytes());
                }
            }
            (Some(b'?'), Some(b'$'), 'p') => {
                let value = self.mode(screen, first);
                self.replies
                    .extend(format!("\x1b[?{first};{value}$y").into_bytes());
            }
            _ => {}
        }
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        match params {
            // OSC 10;? and 11;?, also both at once as 10;?;?
            [number @ (b"10" | b"11"), asked @ ..] => {
                let first = if *number == b"10" { 10 } else { 11 };
                for (i, _) in asked
                    .iter()
                    .enumerate()
                    .filter(|(_, value)| **value == b"?")
                {
                    if first + i <= 11 {
                        self.color(&(first + i).to_string());
                    }
                }
            }
            [b"4", pairs @ ..] => {
                for pair in pairs.chunks(2) {
                    if let [index, b"?"] = pair
                        && let Ok(index) = std::str::from_utf8(index)
                        && index.parse::<u8>().is_ok()
                    {
                        self.color(&format!("4;{index}"));
                    }
                }
            }
            [b"9", b"4", rest @ ..] => {
                let number = |i: usize| {
                    rest.get(i)
                        .and_then(|n| std::str::from_utf8(n).ok()?.parse::<u8>().ok())
                        .unwrap_or(0)
                };
                let (state, percent) = (number(0), number(1).min(100));
                self.progress = (state != 0).then(|| (state, percent, Instant::now()));
                self.effects.push(Effect::Notify(
                    format!("\x1b]9;4;{state};{percent}\x07").into_bytes(),
                ));
            }
            [b"9", text @ ..] if !text.is_empty() => {
                let text = plain(text);
                self.effects
                    .push(Effect::Notify(format!("\x1b]9;{text}\x07").into_bytes()));
            }
            [b"777", b"notify", text @ ..] => {
                let text = plain(text);
                self.effects.push(Effect::Notify(
                    format!("\x1b]777;notify;{text}\x07").into_bytes(),
                ));
            }
            _ => {}
        }
    }
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn dimension(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

/// Lock, even after a panic of another holder: screens stay drawable.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the system tells about a process.
mod process {
    #[cfg(target_os = "macos")]
    pub fn name(pid: i32) -> Option<String> {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer is as large as told.
        let length =
            unsafe { libc::proc_name(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        (length > 0).then(|| String::from_utf8_lossy(&buffer[..length as usize]).into_owned())
    }

    #[cfg(target_os = "macos")]
    pub fn cwd(pid: i32) -> Option<String> {
        let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // SAFETY: the buffer has the size of the structure asked for.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written != size {
            return None;
        }
        // SAFETY: proc_pidinfo filled all of it.
        let info = unsafe { info.assume_init() };
        let path: Vec<u8> = info
            .pvi_cdir
            .vip_path
            .as_flattened()
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        (!path.is_empty()).then(|| String::from_utf8_lossy(&path).into_owned())
    }

    #[cfg(target_os = "linux")]
    pub fn name(pid: i32) -> Option<String> {
        let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        Some(name.trim_end().to_string())
    }

    #[cfg(target_os = "linux")]
    pub fn cwd(pid: i32) -> Option<String> {
        let path = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
        Some(path.to_string_lossy().into_owned())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub fn name(_: i32) -> Option<String> {
        None
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub fn cwd(_: i32) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Instant};

    use super::*;

    fn emulator() -> (vt100::Parser<Replies>, Arc<Mutex<Palette>>) {
        let palette = Arc::new(Mutex::new(Palette::default()));
        let size = NonZeroU16::new(5).unwrap();
        let parser = vt100::Parser::new_with_callbacks(
            size,
            size.saturating_mul(size),
            0,
            Replies::new(palette.clone()),
        );
        (parser, palette)
    }

    /// What the emulator answers to `bytes`.
    fn answer(parser: &mut vt100::Parser<Replies>, bytes: &str) -> String {
        feed(parser, bytes.as_bytes(), false);
        String::from_utf8(std::mem::take(&mut parser.callbacks_mut().replies)).unwrap()
    }

    #[test]
    fn programs_negotiate_their_keys() {
        let (mut parser, _) = emulator();
        let keys = |parser: &vt100::Parser<Replies>| parser.callbacks().keys(parser.screen());
        // pushing more than emulated gets what is, and tells it
        assert_eq!(answer(&mut parser, "\x1b[>7u\x1b[?u"), "\x1b[?1u");
        assert_eq!(keys(&parser).kitty, keys::DISAMBIGUATE);
        // the alternate screen has a stack of its own
        answer(&mut parser, "\x1b[?1049h");
        assert_eq!(keys(&parser).kitty, 0);
        answer(&mut parser, "\x1b[>1u\x1b[?1049l");
        assert_eq!(keys(&parser).kitty, 1);
        assert_eq!(answer(&mut parser, "\x1b[<u\x1b[?u"), "\x1b[?0u");
        answer(&mut parser, "\x1b[=1;1u");
        assert_eq!(keys(&parser).kitty, 1);
        answer(&mut parser, "\x1b[=1;3u");
        assert_eq!(keys(&parser).kitty, 0);
        answer(&mut parser, "\x1b[>4;2m");
        assert_eq!(keys(&parser).modify_other_keys, 2);
        answer(&mut parser, "\x1b[>4m");
        assert_eq!(keys(&parser).modify_other_keys, 0);
        // a full reset forgets them, also split between reads
        answer(&mut parser, "\x1b[>1u\x1b[>4;1m\x1b");
        answer(&mut parser, "c");
        assert_eq!(keys(&parser), keys::Mode::default());
    }

    #[test]
    fn modes_colors_and_effects() {
        let (mut parser, palette) = emulator();
        assert_eq!(
            answer(&mut parser, "\x1b[?2026$p\x1b[?1004$p\x1b[?9999$p"),
            "\x1b[?2026;2$y\x1b[?1004;2$y\x1b[?9999;0$y"
        );
        answer(&mut parser, "\x1b[?1004;2031h");
        assert!(parser.callbacks().focus_reports && parser.callbacks().scheme_reports);
        // colors, once the terminal around told them
        assert_eq!(answer(&mut parser, "\x1b]11;?\x07\x1b[?996n"), "");
        *lock(&palette) = Palette {
            scheme: Some(1),
            colors: BTreeMap::from([
                ("11".into(), "rgb:00/00/00".into()),
                ("4;1".into(), "rgb:ff/00/00".into()),
            ]),
        };
        assert_eq!(
            answer(&mut parser, "\x1b]11;?\x07\x1b]4;1;?;2;?\x07\x1b[?996n"),
            "\x1b]11;rgb:00/00/00\x07\x1b]4;1;rgb:ff/00/00\x07\x1b[?997;1n"
        );
        answer(
            &mut parser,
            "\x07\x1b]52;c;aGk=\x07\x1b]9;4;1;42\x07\x1b]777;notify;Pi;done\x07",
        );
        assert_eq!(parser.callbacks().progress(), Some((1, 42)));
        assert_eq!(
            std::mem::take(&mut parser.callbacks_mut().effects),
            [
                Effect::Bell,
                Effect::Copy(b"\x1b]52;c;aGk=\x07".to_vec()),
                Effect::Notify(b"\x1b]9;4;1;42\x07".to_vec()),
                Effect::Notify(b"\x1b]777;notify;Pi;done\x07".to_vec()),
            ]
        );
    }

    #[test]
    fn synchronized_updates_show_whole() {
        let (mut parser, _) = emulator();
        let row = |parser: &vt100::Parser<Replies>| parser.screen().rows(0, 25).next().unwrap();
        assert!(feed(&mut parser, b"old\x1b[?2026h\x1b[Hne", false));
        assert_eq!(row(&parser), "old");
        assert!(!feed(&mut parser, b"w", false));
        assert_eq!(row(&parser), "old");
        assert!(feed(&mut parser, b"\x1b[?2026l", false));
        assert_eq!(row(&parser), "new");
        // an update that never ends shows once it took too long
        feed(&mut parser, b"\x1b[?2026h\x1b[Hlate", false);
        assert_eq!(row(&parser), "new");
        parser.callbacks_mut().held_since = Some(Instant::now() - SYNC);
        assert!(feed(&mut parser, b"", false));
        assert_eq!(row(&parser), "late");
        assert!(parser.callbacks().held_since.is_none());
    }

    #[test]
    fn shutdown_waits_for_previously_stopped_children() {
        let (events, _) = mpsc::channel();
        let mut runtime = Runtime::new(events, (24, 80));
        let mut nui = Nui::default();
        nui.add_session("main", "/bin/sh", None);
        nui.sessions[0].terminals[0].args =
            vec!["-c".into(), "trap \'\' HUP; exec sleep 30".into()];
        runtime.reconcile(&nui);
        let pid = runtime.live(1).unwrap().pid.unwrap();
        thread::sleep(Duration::from_millis(150));
        nui.sessions[0].terminals[0].status = Status::Stopped;
        runtime.reconcile(&nui);
        runtime.shutdown();
        assert!(
            runtime.retiring.is_empty(),
            "shutdown left a child waiting for its force-kill"
        );
        // SAFETY: signal 0 only probes the child this test spawned.
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }

    #[test]
    fn stop_kills_what_ignores_the_hang_up() {
        let (events, received) = mpsc::channel();
        let mut runtime = Runtime::new(events, (24, 80));
        let mut nui = Nui::default();
        nui.add_session("main", "/bin/sh", None);
        nui.sessions[0].terminals[0].args = vec!["-c".into(), "trap '' HUP; exec sleep 30".into()];
        assert!(matches!(
            runtime.reconcile(&nui).as_slice(),
            [Fact::Pid(1, Some(_))]
        ));
        thread::sleep(Duration::from_millis(300));
        nui.sessions[0].terminals[0].status = Status::Stopped;
        runtime.reconcile(&nui);
        assert!(!runtime.running(1));
        let deadline = Instant::now() + GRACE + Duration::from_secs(3);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match received.recv_timeout(left) {
                Ok(Event::Exited { id: 1, .. }) => break,
                Ok(_) => continue,
                Err(_) => panic!("the process still runs"),
            }
        }
    }
}
