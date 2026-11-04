//! End to end: nui-yalij in a pseudo terminal, driven by keys and read from
//! its screen and its state file.

use std::{
    fs,
    io::{Read, Write},
    num::NonZeroU16,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const BIN: &str = env!("CARGO_BIN_EXE_nui-yalij");
const ROWS: u16 = 30;
const COLS: u16 = 100;

struct Tui {
    screen: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
}

impl Tui {
    fn start(dir: &Path) -> Tui {
        let pair = native_pty_system()
            .openpty(PtySize { rows: ROWS, cols: COLS, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut command = CommandBuilder::new(BIN);
        command.args(["--state", state(dir).to_str().unwrap()]);
        for (key, value) in env(dir) {
            command.env(key, value);
        }
        command.cwd(dir);
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let size = |n| NonZeroU16::new(n).unwrap();
        let screen = Arc::new(Mutex::new(vt100::Parser::new(size(ROWS), size(COLS), 0)));
        let output = screen.clone();
        thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(count @ 1..) = reader.read(&mut buffer) {
                output.lock().unwrap().process(&buffer[..count]);
            }
        });
        Tui { screen, writer: pair.master.take_writer().unwrap(), child, _master: pair.master }
    }

    fn send(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).unwrap();
        self.writer.flush().unwrap();
        // keys apart, as typed: a lone escape is a key of its own
        thread::sleep(Duration::from_millis(80));
    }

    fn text(&self) -> String {
        self.screen.lock().unwrap().screen().contents()
    }

    /// The screen row by row, for checks on where things are.
    fn rows(&self) -> Vec<String> {
        let screen = self.screen.lock().unwrap();
        screen.screen().rows(0, COLS).collect()
    }

    fn wait(&self, about: &str, done: impl Fn(&Tui) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if done(self) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {about}; the screen:\n{}", self.text());
    }

    fn wait_text(&self, text: &str) {
        self.wait(text, |tui| tui.text().contains(text));
    }

    fn quit(mut self) {
        self.send("\x07");
        self.wait("the panel", |tui| tui.text().contains("sessions"));
        self.send("q");
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "nui-yalij did not quit:\n{}", self.text());
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// A fresh directory for a test, its home and state.
fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nui-yalij-e2e-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn state(dir: &Path) -> PathBuf {
    dir.join("state.kdl")
}

/// A plain shell with a plain prompt, whatever the person running the tests uses.
fn env(dir: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("SHELL", "/bin/sh".into()),
        ("HOME", dir.display().to_string()),
        ("PS1", "$ ".into()),
        ("ENV", String::new()),
        ("BASH_SILENCE_DEPRECATION_WARNING", "1".into()),
        ("TERM", "xterm-256color".into()),
    ]
}

fn read_state(dir: &Path) -> String {
    fs::read_to_string(state(dir)).unwrap()
}

fn wait_state(dir: &Path, about: &str, done: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(&read_state(dir)) {
        assert!(Instant::now() < deadline, "timed out waiting for {about}; the state:\n{}", read_state(dir));
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn navigate_rename_stop_and_follow_the_file() {
    let dir = dir("core");
    let mut tui = Tui::start(&dir);
    tui.wait("a prompt", |tui| tui.rows()[0].starts_with('$'));
    tui.send("echo hello-$((6 * 7))\r");
    tui.wait_text("hello-42");

    // the panel: four sections over the preview of the hovered pane
    tui.send("\x07");
    tui.wait("the panel", |tui| ["sessions", "windows", "tabs", "panes", "hello-42"].iter().all(|t| tui.text().contains(t)));
    let rows = tui.rows();
    assert!(rows[0].contains("sessions") && rows[0].contains("panes"), "{rows:#?}");
    assert!(rows.iter().skip(ROWS as usize / 2).any(|row| row.contains("hello-42")), "the preview: {rows:#?}");

    // rename the pane, then the session
    tui.send("r");
    tui.wait_text("Rename:");
    tui.send("logs\r");
    tui.wait_text("1 logs");
    wait_state(&dir, "the pane name", |state| state.contains("pane logs") || state.contains("pane \"logs\""));
    tui.send("hhhr");
    tui.send("\x7f\x7f\x7f\x7fdev\r");
    wait_state(&dir, "the session name", |state| state.contains("session dev") || state.contains("session \"dev\""));

    // esc goes back, where the person was
    tui.send("\x1b");
    tui.wait("the view", |tui| !tui.text().contains("sessions"));
    tui.wait_text("hello-42");

    // the state file is the source of truth: an edit shows
    let edited = read_state(&dir).replace("pane logs", "pane edited").replace("pane \"logs\"", "pane edited");
    fs::write(state(&dir), edited).unwrap();
    thread::sleep(Duration::from_millis(600));
    tui.send("\x07");
    tui.wait_text("1 edited");

    // stop the terminal of the pane, then enter it: it runs again
    tui.send("s");
    tui.wait_text("Stop?");
    tui.send("\r");
    tui.wait_text("stopped");
    wait_state(&dir, "a stopped terminal", |state| state.contains("stopped"));
    tui.send("\r");
    tui.wait("the view", |tui| !tui.text().contains("sessions"));
    wait_state(&dir, "a running terminal", |state| !state.contains("status=") && state.contains("pid="));
    tui.send("echo again\r");
    tui.wait_text("again");

    tui.quit();
    let state = read_state(&dir);
    assert!(!state.contains("navigation") && !state.contains("pid="), "{state}");
    let list = Command::new(BIN).args(["--state", self::state(&dir).to_str().unwrap(), "list"]).output().unwrap();
    let list = String::from_utf8(list.stdout).unwrap();
    assert!(list.starts_with("dev  windows: 1  tabs: 1  panes: 1  terminals: 1  (attached)"), "{list}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn plugins_layouts_and_attach_from_inside() {
    let dir = dir("plugin");
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/example.lua");
    let text = format!(
        "plugin \"{}\"\nsession main {{\n  terminal 1 command=\"/bin/sh\"\n  terminal 2 command=\"/bin/sh\"\n  window {{ tab layout=columns {{ pane terminal=1; pane terminal=2 }} }}\n}}\n",
        example.display()
    );
    fs::write(state(&dir), text).unwrap();
    let mut tui = Tui::start(&dir);
    // two shells side by side, a rail apart
    tui.wait("two columns", |tui| tui.rows()[0].starts_with('$') && tui.rows()[0].contains("│$"));

    // the plugin's key: a new pane after the hovered one
    tui.send("\x07");
    tui.wait_text("panes");
    tui.send("n");
    // the label is the program: macOS runs sh as bash
    tui.wait("a third pane", |tui| ["3 sh", "3 bash"].iter().any(|label| tui.text().contains(label)));
    wait_state(&dir, "a third terminal", |state| state.contains("terminal 3"));
    tui.send("\r");
    tui.wait("three columns", |tui| tui.rows()[0].matches('$').count() == 3);

    // a shell ending well closes its pane: the plugin's update hook
    tui.send("exit\r");
    wait_state(&dir, "two panes", |state| state.matches("pane terminal").count() == 2 && !state.contains("terminal 3"));

    // attach from a shell inside: the running instance follows the file
    let command = format!("{BIN} --state {} attach other\r", state(&dir).display());
    tui.send(&command);
    wait_state(&dir, "the other session", |state| state.contains("session other") && state.contains("attached 2"));
    tui.wait("a fresh shell", |tui| tui.rows()[0].trim_end() == "$");

    tui.quit();
    let _ = fs::remove_dir_all(&dir);
}
