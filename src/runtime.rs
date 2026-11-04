//! The runtime: the processes behind the terminals of the state, in PTYs.
//!
//! It follows the state (`reconcile`) and never changes it: what it learns
//! goes back as facts, which become state like any other change.

use std::{
    collections::HashMap,
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

use crate::app::Event;
use crate::state::{Fact, Nui, Observation, Status, Terminal};

/// The screen of a terminal: an emulator fed with the output of its process.
pub type Screen = Arc<Mutex<vt100::Parser<Replies>>>;

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
    /// Children already hung up but not yet reaped. A daemon must not exit
    /// before their delayed force-kill has had a chance to run.
    retiring: Vec<Arc<AtomicBool>>,
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
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
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
            retiring: Vec::new(),
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
        let writer = Arc::new(Mutex::new(
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
            Replies::default(),
        )));
        let (output, events, pending) = (screen.clone(), self.events.clone(), self.pending.clone());
        let replies = writer.clone();
        let owner = self.owner;
        thread::spawn(move || read(reader, output, replies, events, pending, owner));
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

/// Feed a screen with the output of its process, answering the queries the
/// program makes of its terminal.
fn read(
    mut reader: Box<dyn Read + Send>,
    screen: Screen,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    events: Sender<Event>,
    pending: Arc<AtomicBool>,
    owner: u64,
) {
    let mut buffer = [0; 16 * 1024];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let replies = {
            let mut parser = lock(&screen);
            parser.process(&buffer[..count]);
            std::mem::take(&mut parser.callbacks_mut().replies)
        };
        if !replies.is_empty() {
            let mut writer = lock(&writer);
            let _ = writer.write_all(&replies).and_then(|()| writer.flush());
        }
        if !pending.swap(true, Ordering::AcqRel) && events.send(Event::Output(owner)).is_err() {
            break;
        }
    }
}

/// The emulator's side of a conversation with a program: what to answer it,
/// and what it set.
#[derive(Default)]
pub struct Replies {
    /// Answers to write back to the program.
    pub replies: Vec<u8>,
    pub title: Option<String>,
}

impl vt100::Callbacks for Replies {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        let title = String::from_utf8_lossy(title).trim().to_string();
        self.title = (!title.is_empty()).then_some(title);
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        intermediate: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params
            .first()
            .and_then(|param| param.first())
            .copied()
            .unwrap_or(0);
        match (intermediate, c, first) {
            // the cursor position, which shells such as nushell wait for
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .extend(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
            }
            (None, 'n', 5) => self.replies.extend(b"\x1b[0n"),
            // primary device attributes: a VT220 with colors
            (None, 'c', 0) => self.replies.extend(b"\x1b[?62;22c"),
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
