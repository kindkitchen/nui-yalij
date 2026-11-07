//! Real PTYs and real sockets, with isolated homes and cleanup even on failure.
use std::{
    fs,
    io::{Read, Write},
    num::NonZeroU16,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const BIN: &str = env!("CARGO_BIN_EXE_nui-yalij");
const ROWS: u16 = 30;
const COLS: u16 = 110;
/// The rows the terminal around a client keeps of its history.
const HISTORY: usize = 1000;

struct Home {
    dir: PathBuf,
}
impl Home {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nui-e2e-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Home {
            dir: dir.canonicalize().unwrap(),
        }
    }
    fn global(&self) -> PathBuf {
        self.dir.join("global.kdl")
    }
    fn config(&self) -> PathBuf {
        self.dir.join("config")
    }
    fn session(&self, id: u64) -> PathBuf {
        self.dir.join(format!("global.kdl.sessions/{id}/state.kdl"))
    }
    fn decoration(&self, id: u64) -> PathBuf {
        self.session(id).with_file_name("decoration.kdl")
    }
    fn read(&self, id: u64) -> String {
        fs::read_to_string(self.session(id)).unwrap_or_default()
    }
    fn write(&self, path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("SHELL", "/bin/sh".into()),
            // Never pick up commands belonging to the developer's parent session.
            ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("HOME", self.dir.display().to_string()),
            ("PS1", "$ ".into()),
            ("ENV", String::new()),
            ("BASH_SILENCE_DEPRECATION_WARNING", "1".into()),
            ("TERM", "xterm-256color".into()),
            ("XDG_RUNTIME_DIR", "/tmp".into()),
            (
                "XDG_CONFIG_HOME",
                self.dir.join("xdg-config").display().to_string(),
            ),
            (
                "XDG_STATE_HOME",
                self.dir.join("xdg-state").display().to_string(),
            ),
        ]
    }
    fn command(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args([
                "--state",
                self.global().to_str().unwrap(),
                "--config",
                self.config().to_str().unwrap(),
            ])
            .args(args)
            .env_remove("NUI_YALIJ_STATE")
            .env_remove("NUI_YALIJ_CONFIG")
            .env_remove("NUI_YALIJ_SESSION")
            .env_remove("NUI_YALIJ_TERMINAL")
            .envs(self.env())
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    fn wait(&self, about: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(12);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "timed out: {about}; global: {:?}; session 1: {}; log: {:?}",
                fs::read_to_string(self.global()),
                self.read(1),
                fs::read_to_string(self.dir.join("global.kdl.server.log"))
            );
            thread::sleep(Duration::from_millis(40));
        }
    }
    fn pid(&self, id: u64, terminal: u64) -> Option<u32> {
        let doc: kdl::KdlDocument = self.read(id).parse().ok()?;
        doc.get("session")?
            .children()?
            .nodes()
            .iter()
            .find(|node| {
                node.name().value() == "terminal"
                    && node.get(0).and_then(kdl::KdlValue::as_integer) == Some(terminal as i128)
            })?
            .get("pid")?
            .as_integer()
            .and_then(|pid| u32::try_from(pid).ok())
    }
    fn stop(&self) {
        self.ok(&["kill", "--apply"]);
        self.wait("server shutdown", || !self.read(1).contains("pid="));
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.command(&["kill", "--apply"]);
        // Kill replies before final persistence; don't remove a live owner's directory.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let output = self.command(&["kill"]);
            if String::from_utf8_lossy(&output.stdout).contains("no server") {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct Tui {
    screen: Arc<Mutex<vt100::Parser>>,
    /// All the client wrote, for what is no drawing: answers to pass on.
    raw: Arc<Mutex<Vec<u8>>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
}
impl Tui {
    fn start(home: &Home, args: &[&str]) -> Tui {
        let mut command = CommandBuilder::new(BIN);
        command.args([
            "--state",
            home.global().to_str().unwrap(),
            "--config",
            home.config().to_str().unwrap(),
        ]);
        command.args(args);
        Tui::spawn(home, command)
    }
    /// A shell running `script` in the PTY, `{nui}` in it naming this binary
    /// with the home's state and config.
    fn shell(home: &Home, script: &str) -> Tui {
        let nui = format!(
            "{BIN} --state {} --config {}",
            home.global().display(),
            home.config().display()
        );
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", &script.replace("{nui}", &nui)]);
        Tui::spawn(home, command)
    }
    fn spawn(home: &Home, mut command: CommandBuilder) -> Tui {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        for (key, value) in home.env() {
            command.env(key, value);
        }
        for key in [
            "NUI_YALIJ_STATE",
            "NUI_YALIJ_CONFIG",
            "NUI_YALIJ_SESSION",
            "NUI_YALIJ_TERMINAL",
        ] {
            command.env_remove(key);
        }
        command.cwd(&home.dir);
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let size = |n| NonZeroU16::new(n).unwrap();
        let screen = Arc::new(Mutex::new(vt100::Parser::new(
            size(ROWS),
            size(COLS),
            HISTORY,
        )));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let (output, written) = (screen.clone(), raw.clone());
        thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(count @ 1..) = reader.read(&mut buffer) {
                written.lock().unwrap().extend_from_slice(&buffer[..count]);
                output.lock().unwrap().process(&buffer[..count]);
            }
        });
        Tui {
            screen,
            raw,
            writer: pair.master.take_writer().unwrap(),
            child,
            master: pair.master,
        }
    }
    fn send(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).unwrap();
        self.writer.flush().unwrap();
        thread::sleep(Duration::from_millis(80));
    }
    fn text(&self) -> String {
        self.screen.lock().unwrap().screen().contents()
    }
    fn raw(&self) -> String {
        String::from_utf8_lossy(&self.raw.lock().unwrap()).into_owned()
    }
    fn rows(&self) -> Vec<String> {
        self.screen.lock().unwrap().screen().rows(0, COLS).collect()
    }
    /// The rows the screen around scrolled away, oldest first.
    fn history(&self) -> Vec<String> {
        let mut parser = self.screen.lock().unwrap();
        let (mut history, mut offset) = (Vec::new(), parser.screen().scrollback_len());
        while offset > 0 {
            parser.screen_mut().set_scrollback(offset);
            let taken = offset.min(ROWS as usize);
            history.extend(parser.screen().rows(0, COLS).take(taken));
            offset -= taken;
        }
        parser.screen_mut().set_scrollback(0);
        history
    }
    fn wait(&self, about: &str, condition: impl Fn(&Tui) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(12);
        while !condition(self) {
            assert!(
                Instant::now() < deadline,
                "timed out: {about}; screen:\n{}\nlast written: {:?}",
                self.text(),
                self.raw()
                    .chars()
                    .rev()
                    .take(600)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>()
            );
            thread::sleep(Duration::from_millis(40));
        }
    }
    fn text_is(&self, text: &str) {
        self.wait(text, |tui| tui.text().contains(text));
    }
    fn view(&self) {
        self.wait("the view", |tui| !tui.text().contains(" native "));
    }
    fn prompt(&self) {
        self.wait("a prompt", |tui| tui.rows()[0].starts_with('$'));
    }
    fn exited(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.exit_code(), 0, "{}", self.text());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "client did not exit: {}",
                self.text()
            );
            thread::sleep(Duration::from_millis(40));
        }
    }
    fn detach(&mut self, toggle: &str) {
        self.send(toggle);
        self.text_is(" native ");
        self.send("q");
        self.exited();
    }
    fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        thread::sleep(Duration::from_millis(150));
    }
}
impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn navigation_file_edits_stop_reconnect_and_legacy_migration() {
    let home = Home::new("core");
    // the old application's shape: workspaces were windows of tabs
    home.write(&home.global(), "toggle ctrl-g\nsession main { terminal 1 command=\"/bin/sh\"; window { tab { pane terminal=1 } } }");
    let mut tui = Tui::start(&home, &[]);
    tui.prompt();
    assert!(home.dir.join("global.kdl.legacy.kdl").exists());
    assert!(!home.read(1).contains("toggle"));
    tui.send("echo hello-$((6 * 7))\r");
    tui.text_is("hello-42");
    tui.send("\x14");
    tui.text_is("panes");
    assert!(
        tui.rows()
            .iter()
            .skip(ROWS as usize / 2)
            .any(|row| row.contains("hello-42"))
    );
    tui.send("rlogs\r");
    home.wait("pane name", || home.read(1).contains("pane logs"));
    tui.send("hhhr");
    tui.send("\x7f\x7f\x7f\x7fdev\r");
    home.wait("session name", || home.read(1).contains("session dev"));
    tui.send("\x1b");
    tui.view();

    home.write(
        &home.session(1),
        &home.read(1).replace("pane logs", "pane edited"),
    );
    tui.send("\x14");
    tui.text_is("1 edited");
    home.write(&home.session(1), "session {");
    tui.text_is("session 1:");
    tui.send("j");
    assert_eq!(
        home.read(1),
        "session {",
        "invalid external edits must stay untouched"
    );
    let valid = fs::read_to_string(home.dir.join("global.kdl.legacy.kdl")).unwrap();
    // Restore a valid single-session state (with a stable identity).
    home.write(
        &home.session(1),
        &valid.replace("session main", "session dev id=1"),
    );
    home.wait("valid edit accepted", || home.pid(1, 1).is_some());
    tui.send("s\r");
    tui.text_is("stopped");
    tui.send("\r");
    tui.view();
    tui.send("echo after-restart\r");
    tui.text_is("after-restart");
    let pid = home.pid(1, 1).unwrap();
    tui.detach("\x14");
    assert_eq!(home.pid(1, 1), Some(pid));
    let mut again = Tui::start(&home, &["attach", "dev"]);
    again.text_is("after-restart");
    assert_eq!(home.pid(1, 1), Some(pid));
    assert!(
        home.ok(&["kill"])
            .contains("would stop 1 session server(s), 1 connected")
    );
    assert_eq!(home.pid(1, 1), Some(pid));
    again.detach("\x14");
    assert!(
        home.ok(&["list"])
            .contains("dev  id: 1  workspaces: 1  windows: 1  panes: 1")
    );
    home.stop();
}

#[test]
fn fs_arranges_terminals_by_folders_and_points_where_native_does() {
    let home = Home::new("fs");
    let dir = home.dir.display().to_string();
    let mut tui = Tui::start(&home, &[]);
    tui.prompt();
    tui.send("mkdir -p one two && cd one\r");
    home.wait("the shell in one", || {
        home.read(1).contains(&format!("{dir}/one"))
    });
    // a second terminal where the first is, entered; it goes to two
    tui.send("\x14");
    tui.text_is("panes");
    tui.send("rfirst\r");
    home.wait("first named", || home.read(1).contains("pane first"));
    tui.send("asecond\r\r");
    tui.wait("the second shell", |tui| tui.rows()[0].trim_end() == "$");
    tui.send("cd ../two\r");
    home.wait("the shell in two", || {
        home.read(1).contains(&format!("{dir}/two"))
    });
    let top = |tui: &Tui| tui.rows()[..ROWS as usize / 2].join("\n");
    let bottom = |tui: &Tui| tui.rows()[ROWS as usize / 2..].join("\n");
    // the hovered pane's terminal previewed, over the one navigation is opened from
    tui.send("\x14");
    tui.wait("second previewed", |tui| {
        top(tui).contains(" workspaces") && top(tui).contains("cd ../two")
    });
    tui.wait("where it was opened", |tui| {
        bottom(tui).contains(" main › second › second › second ")
            && bottom(tui).contains("cd ../two")
    });
    // the fs representation points to the same terminal, in its folder
    tui.send("]");
    tui.wait("fs at second", |tui| {
        !top(tui).contains(" workspaces")
            && top(tui).contains(" ~/two")
            && top(tui).contains("cd ../two")
    });
    // back out, and to the folder next to it: its items; into it, the screen
    // of its terminal
    tui.send("hk");
    tui.wait("one/ hovered", |tui| {
        top(tui).contains(" ~/one") && !top(tui).contains("mkdir")
    });
    tui.send("l");
    tui.wait("first previewed", |tui| {
        top(tui).contains("mkdir -p one two")
    });
    assert!(bottom(&tui).contains("cd ../two") && !bottom(&tui).contains("mkdir"));
    // the native one points to it as well, and fs again
    tui.send("[");
    tui.wait("native at first", |tui| {
        top(tui).contains(" workspaces") && top(tui).contains("mkdir -p one two")
    });
    tui.send("]");
    tui.wait("fs at first", |tui| {
        !top(tui).contains(" workspaces") && top(tui).contains("mkdir -p one two")
    });
    // only to move in: nothing to remove
    tui.send("x");
    assert!(!tui.text().contains("Remove?"));
    tui.send("\r");
    tui.view();
    home.wait("first entered", || !home.read(1).contains("active=2"));
    tui.send("echo in-$((40 + 2))\r");
    tui.text_is("in-42");
    // the panel opens again on the tab it showed last
    tui.send("\x14");
    tui.wait("fs again", |tui| {
        top(tui).contains(" ~/one") && !top(tui).contains(" workspaces")
    });
    tui.send("\x1b");
    tui.view();
    tui.detach("\x14");
    home.stop();
}

#[test]
fn same_and_different_sessions_share_only_session_state() {
    let home = Home::new("multi");
    // Simultaneous startup must reuse the one coordinator/session owner.
    let mut a = Tui::start(&home, &["attach", "main"]);
    let mut b = Tui::start(&home, &["attach", "main"]);
    a.prompt();
    b.prompt();
    let pid = home.pid(1, 1).unwrap();
    a.send("echo shared-output\r");
    b.text_is("shared-output");
    assert_eq!(home.pid(1, 1), Some(pid));
    let mut c = Tui::start(&home, &["attach", "other"]);
    c.prompt();
    assert_ne!(home.pid(2, 1), Some(pid));
    assert!(!c.text().contains("shared-output"));

    // Edit a foreign session without entering it, then return to the original.
    a.send("\x14");
    a.text_is("panes");
    a.send("hhhjlllrforeign\r");
    home.wait("edit reaches the other owner", || {
        home.read(2).contains("pane foreign")
    });
    a.send("\x1b");
    a.view();
    a.text_is("shared-output");
    assert_eq!(home.pid(1, 1), Some(pid));
    c.send("\x14");
    c.text_is("1 foreign");
    c.send("\x1b");
    c.view();

    // Both clients add against the latest authoritative state: no lost update.
    a.send("\x14");
    b.send("\x14");
    a.text_is("panes");
    b.text_is("panes");
    a.send("aone\r");
    b.send("atwo\r");
    home.wait("both additions kept", || {
        home.read(1).matches("pane ").count() == 3
    });
    a.send("\r");
    a.view();
    b.send("\x1b");
    b.view();
    a.send("echo focused-together\r");
    b.text_is("focused-together");

    // A global presence edit moves only that connection, not either session's state.
    let global = fs::read_to_string(home.global()).unwrap();
    let doc: kdl::KdlDocument = global.parse().unwrap();
    let root = doc.get("catalog").unwrap();
    let first = root
        .children()
        .unwrap()
        .nodes()
        .iter()
        .find(|node| node.name().value() == "client")
        .unwrap();
    let client = first.get(0).unwrap().as_integer().unwrap();
    let edited = global.replace(
        &format!("client {client} session=1"),
        &format!("client {client} session=2"),
    );
    home.write(&home.global(), &edited);
    // Startup races can assign a/b ids in either order.
    home.wait("the moved client", || {
        a.text().trim_end() == "$" || b.text().trim_end() == "$"
    });
    c.send("echo shared-other\r");
    home.wait("the moved client sees other", || {
        a.text().contains("shared-other") || b.text().contains("shared-other")
    });
    home.stop();
    a.exited();
    b.exited();
    c.exited();
}

#[test]
fn per_instance_profiles_static_reload_and_live_session_override() {
    let home = Home::new("decoration");
    home.write(
        &home.config().join("decoration.kdl"),
        "keys { panel ctrl-a }\ntheme accent=red",
    );
    let plugin = home.config().join("profiles/work.lua");
    home.write(
        &plugin,
        r#"return {
        layouts = { tabs = function(c) return { areas = { [c.active] = c.area }, draw = {
            {x=c.area.x,y=c.area.y,text="WORK",style={fg=c.theme.accent}}
        } } end },
        update = function(s) error("a profile must never run automatic state hooks") end
    }"#,
    );
    home.write(
        &home.config().join("profiles/work.kdl"),
        "keys { panel ctrl-b }\ntheme accent=green\nplugin work.lua",
    );
    let mut plain = Tui::start(&home, &["attach", "main"]);
    plain.prompt();
    let pid = home.pid(1, 1).unwrap();
    let mut work = Tui::start(&home, &["--profile", "work", "attach", "main"]);
    work.text_is("WORK");
    assert_eq!(home.pid(1, 1), Some(pid));
    assert!(!plain.text().contains("WORK"));
    plain.send("\x01");
    plain.text_is("panes");
    work.send("\x02");
    work.text_is("panes");
    assert!(!work.text().contains("automatic state hooks"));
    plain.send("\x1b");
    plain.view();
    work.send("\x1b");
    work.view();

    // Global edits remain static until reload; a per-instance override still wins.
    home.write(
        &home.config().join("decoration.kdl"),
        "keys { panel ctrl-c }\ntheme accent=blue",
    );
    thread::sleep(Duration::from_millis(350));
    plain.send("\x01");
    plain.text_is("panes");
    plain.send("\x1b");
    plain.view();
    home.ok(&["reload"]);
    plain.send("\x01");
    assert!(!plain.text().contains(" native "));
    plain.send("\x03");
    plain.text_is("panes");
    plain.send("\x1b");
    plain.view();
    work.send("\x02");
    work.text_is("panes");
    work.send("\x1b");
    work.view();

    // Session overrides apply live, above BOTH viewers' chosen profiles.
    home.write(
        &home.decoration(1),
        "keys { panel ctrl-d }\ntheme accent=yellow",
    );
    thread::sleep(Duration::from_millis(450));
    plain.send("\x04");
    plain.text_is("panes");
    plain.send("\x1b");
    plain.view();
    work.send("\x04");
    work.text_is("panes");
    work.send("\x1b");
    work.view();
    let code = fs::read_to_string(&plugin)
        .unwrap()
        .replace("WORK", "RELOADED");
    home.write(&plugin, &code);
    assert!(!work.text().contains("RELOADED"));
    home.ok(&["reload"]);
    work.text_is("RELOADED");
    home.write(&home.decoration(1), "theme typo=red");
    thread::sleep(Duration::from_millis(350));
    plain.send("\x04");
    plain.text_is("panes");
    plain.send("\x1b");
    plain.view();
    assert_eq!(
        fs::read_to_string(home.decoration(1)).unwrap(),
        "theme typo=red"
    );
    home.write(&home.config().join("decoration.kdl"), "theme typo=red");
    assert!(!home.command(&["reload"]).status.success());
    plain.send("\x04");
    plain.text_is("panes");
    plain.send("\x1b");
    plain.view();
    assert_eq!(home.pid(1, 1), Some(pid));
    // Removing a live override restores the previous successful static snapshot.
    fs::remove_file(home.decoration(1)).unwrap();
    thread::sleep(Duration::from_millis(450));
    plain.detach("\x03");
    work.detach("\x02");
    assert_eq!(home.pid(1, 1), Some(pid));
}

#[test]
fn plugins_and_inside_attach_route_to_the_typing_instance() {
    let home = Home::new("plugin");
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/example.lua");
    home.write(
        &home.config().join("decoration.kdl"),
        &format!("plugin \"{}\"", example.display()),
    );
    home.write(&home.global(), "session main { terminal 1 command=\"/bin/sh\"; terminal 2 command=\"/bin/sh\"; workspace { window layout=columns { pane terminal=1; pane terminal=2 } } }");
    let mut tui = Tui::start(&home, &[]);
    tui.wait("two columns", |tui| {
        tui.rows()[0].starts_with('$') && tui.rows()[0].contains("│$")
    });
    tui.send("\x14");
    tui.text_is("panes");
    tui.send("n");
    home.wait("third terminal", || home.read(1).contains("terminal 3"));
    tui.send("\r");
    tui.wait("three columns", |tui| {
        tui.rows()[0].matches('$').count() == 3
    });
    tui.send("exit\r");
    home.wait("successful exit closes pane", || {
        !home.read(1).contains("terminal 3")
    });

    let mut watcher = Tui::start(&home, &["attach", "main"]);
    watcher.wait("two columns", |tui| tui.rows()[0].contains("│$"));
    tui.send(&format!("{BIN} attach other\r"));
    home.wait("the other session", || {
        home.read(2).contains("session other")
    });
    tui.wait("switched to other", |tui| tui.rows()[0].trim_end() == "$");
    assert!(
        watcher.rows()[0].contains("│$"),
        "inside attach must not drag other viewers along"
    );
    watcher.detach("\x14");
    tui.detach("\x14");
}

#[test]
fn programs_talk_to_the_terminal_around_through_the_client() {
    let home = Home::new("terminal");
    let mut tui = Tui::start(&home, &[]);
    tui.prompt();
    // the client asks the terminal around it; answer as Ghostty would
    tui.wait("questions", |tui| {
        tui.raw().contains("\x1b]11;?\x07\x1b]4;0;?")
    });
    assert!(tui.raw().contains("\x1b[>7u"));
    tui.send("\x1b[?5u\x1b]11;rgb:1111/2222/3333\x1b\\\x1b[?62;22c");

    // shift with enter, in kitty's escape codes, reaches a program asking for them...
    tui.send("printf '\\033[>1u'; stty raw -echo; printf ready-$((40+1)); k=$(dd bs=1 count=7 2>/dev/null); stty sane; printf '\\033[<u'; printf 'got-1:%s\\n' \"$k\" | tr '\\033' E\r");
    tui.text_is("ready-41");
    tui.send("\x1b[13;2u");
    tui.text_is("got-1:E[13;2u");
    // ...and as enter to one that doesn't
    tui.send("stty raw -echo; printf ready-$((40+2)); k=$(dd bs=1 count=1 2>/dev/null); stty sane; printf 'got-2:%s\\n' \"$k\" | tr '\\r' R\r");
    tui.text_is("ready-42");
    tui.send("\x1b[13;2u");
    tui.text_is("got-2:R");

    // focus, to a program asking for it
    tui.send("printf '\\033[?1004h'; stty raw -echo; printf ready-$((40+3)); k=$(dd bs=1 count=3 2>/dev/null); stty sane; printf '\\033[?1004l'; printf 'got-3:%s\\n' \"$k\" | tr '\\033' E\r");
    tui.text_is("ready-43");
    tui.send("\x1b[O");
    tui.text_is("got-3:E[O");
    tui.send("\x1b[I");

    // the colors of the terminal around answer the programs inside
    tui.send("stty raw -echo; printf '\\033]11;?\\007'; k=$(dd bs=1 count=24 2>/dev/null); stty sane; printf 'got-4:%s\\n' \"$k\" | tr '\\033\\007' EB\r");
    tui.text_is("got-4:E]11;rgb:1111/2222/3333B");

    // copies to the clipboard, titles and progress pass on to it
    tui.send("printf '\\033]52;c;aGVsbG8=\\007\\033]2;title-%s\\007\\033]9;4;3\\007' $((40+5))\r");
    tui.wait("passed on", |tui| {
        let raw = tui.raw();
        raw.contains("\x1b]52;c;aGVsbG8=\x07")
            && raw.contains("\x1b]2;title-45\x07")
            && raw.contains("\x1b]9;4;3;0\x07")
    });
    // ctrl with the key of t on a Ukrainian layout is ctrl-t; progress shows
    tui.send("\x1b[1077::116;5u");
    tui.text_is("panes");
    tui.text_is("working");
    tui.send("\x1b[27u");
    tui.view();

    // a terminal around without kitty's escape codes gets xterm's modifyOtherKeys
    let mut plain = Tui::start(&home, &["attach", "main"]);
    plain.prompt();
    plain.wait("questions", |tui| tui.raw().contains("\x1b[c"));
    plain.send("\x1b[?62;22c");
    plain.wait("modifyOtherKeys", |tui| tui.raw().contains("\x1b[>4;2m"));
    plain.detach("\x14");
    plain.wait("taken back", |tui| tui.raw().contains("\x1b[>4;0m"));
    tui.detach("\x14");
    tui.wait("taken back", |tui| tui.raw().contains("\x1b[<u"));
}

#[test]
fn a_command_inside_a_pane_toggles_navigation_of_the_instance_typing() {
    let home = Home::new("command");
    home.write(
        &home.config().join("profiles/work.kdl"),
        "commands { panel navi }",
    );
    let mut tui = Tui::start(&home, &[]);
    tui.prompt();
    // first on the PATH of the terminals of the session, and nowhere else
    tui.send("command -v t\r");
    tui.text_is("global.kdl.sessions/1/bin/t");
    // the path reads as the panel
    tui.send("clear\r");
    tui.view();
    tui.send("t\r");
    tui.text_is("panes");
    tui.send("\x1b");
    tui.view();
    // another profile's command is there too, but not this instance's
    tui.send("navi\r");
    tui.text_is("navi is not a command of this instance");

    // decoration renames it, or unbinds it; reload rewrites them
    home.write(
        &home.config().join("decoration.kdl"),
        "commands { panel nav }",
    );
    home.ok(&["reload"]);
    tui.send("clear; t; nav\r");
    tui.text_is("not found");
    tui.text_is("panes");
    tui.send("\x1b");
    tui.view();
    // a session's decoration wins, live
    home.write(&home.decoration(1), "commands { panel s }");
    thread::sleep(Duration::from_millis(450));
    tui.send("clear; nav; s\r");
    tui.text_is("not found");
    tui.text_is("panes");
    tui.send("q");
    tui.exited();
}

#[test]
fn last_typing_client_owns_shared_terminal_size() {
    let home = Home::new("resize");
    let mut a = Tui::start(&home, &[]);
    a.prompt();
    let mut b = Tui::start(&home, &[]);
    b.prompt();
    a.resize(20, 60);
    a.send("stty size\r");
    a.text_is("20 60");
    b.text_is("20 60");
    b.resize(25, 90);
    b.send("stty size\r");
    a.text_is("25 90");
    b.text_is("25 90");
    assert!(
        !fs::read_to_string(home.global())
            .unwrap()
            .contains("message"),
        "resize must not query a nonexistent server TTY"
    );
    a.detach("\x14");
    b.detach("\x14");
}

#[test]
fn the_terminal_around_keeps_the_history_of_the_terminal_watched() {
    let home = Home::new("history");
    // attaching twice from the same terminal around
    // a shell not interactive drops PS1 for what it runs
    let mut tui = Tui::shell(
        &home,
        "export PS1='$ '; {nui} attach main; {nui} attach main",
    );
    tui.prompt();
    let numbers = |history: &[String], from: u32| -> Vec<u32> {
        history
            .iter()
            .filter_map(|row| row.trim().parse().ok())
            .filter(|n| (from..from + 1000).contains(n))
            .collect()
    };
    // what scrolls away goes into its history, in order, once; the rows the
    // screen still shows are not history yet
    tui.send("seq 1000 1099\r");
    tui.wait("the history", |tui| tui.history().len() == 72);
    let history = tui.history();
    assert_eq!(history[0].trim_end(), "$ seq 1000 1099");
    assert_eq!(numbers(&history, 1000), (1000..=1070).collect::<Vec<_>>());
    assert!(tui.rows()[0].starts_with("1071") && tui.rows()[29].starts_with('$'));

    // a program on the alternate screen gets the alternate screen around,
    // keys told apart there too, and leaves the history as it was
    tui.send("printf '\\033[?1049halt-%s' $((6 * 7)); sleep 1; printf '\\033[?1049l'\r");
    tui.text_is("alt-42");
    assert!(tui.raw().contains("\x1b[?1049h\x1b[>7u"));
    tui.wait("the main screen", |tui| {
        tui.raw().contains("\x1b[<u\x1b[?1049l") && !tui.text().contains("alt-42")
    });
    // the line typed pushed the top row away
    assert_eq!(
        numbers(&tui.history(), 1000),
        (1000..=1071).collect::<Vec<_>>()
    );

    // a program clearing its history clears it around
    tui.send("printf '\\033[3J'\r");
    tui.wait("the history cleared", |tui| tui.history().is_empty());
    tui.send("seq 2000 2099\r");
    tui.wait("the history", |tui| {
        numbers(&tui.history(), 2000).len() == 71
    });
    assert_eq!(
        numbers(&tui.history(), 1000),
        (1073..=1099).collect::<Vec<_>>()
    );

    // another terminal around gets the history it never had
    let mut other = Tui::start(&home, &["attach", "main"]);
    other.wait("the history", |tui| {
        numbers(&tui.history(), 2000) == (2000..=2070).collect::<Vec<_>>()
    });
    other.detach("\x14");

    // attaching again from the same one: what it has stays once
    tui.send("\x14");
    tui.text_is(" native ");
    tui.send("q");
    tui.wait("attached again", |tui| {
        tui.raw().matches("\x1b[>7u").count() >= 2
    });
    tui.text_is("2099");
    assert_eq!(
        numbers(&tui.history(), 2000),
        (2000..=2070).collect::<Vec<_>>()
    );
    tui.detach("\x14");
}

#[test]
fn repl_completes_then_requires_explicit_yes_even_with_apply() {
    let home = Home::new("repl");
    let mut cancelled = Tui::start(&home, &["--repl"]);
    cancelled.text_is("Command");
    cancelled.send("\r");
    cancelled.text_is("Session");
    cancelled.send("\x1b");
    cancelled.text_is("Command");
    cancelled.send("\r");
    cancelled.text_is("Session");
    cancelled.send("\r");
    cancelled.text_is("Session name:");
    cancelled.send("draft\r");
    cancelled.text_is("Apply? (N/y)");
    cancelled.send("\r");
    cancelled.exited();
    assert!(!home.global().exists());

    let mut accepted = Tui::start(&home, &["--repl", "attach", "draft"]);
    accepted.text_is("Apply? (N/y)");
    assert!(!home.global().exists());
    accepted.send("y\r");
    accepted.prompt();
    home.wait("session created", || home.read(1).contains("session draft"));
    accepted.detach("\x14");
    let pid = home.pid(1, 1).unwrap();

    let mut no = Tui::start(&home, &["kill", "--repl", "--apply"]);
    no.text_is("Apply? (N/y)");
    no.send("n\r");
    no.exited();
    assert_eq!(home.pid(1, 1), Some(pid));
    let mut yes = Tui::start(&home, &["kill", "--repl"]);
    yes.text_is("Apply? (N/y)");
    yes.send("y\r");
    yes.exited();
    home.wait("server stopped", || !home.read(1).contains("pid="));
}

#[test]
fn terminal_exits_open_navigation_but_nested_shells_and_commands_do_not() {
    let home = Home::new("exit-navigation");
    let mut tui = Tui::start(&home, &[]);
    tui.prompt();
    let original = home.pid(1, 1).unwrap();

    tui.send("PS1='nested> ' /bin/sh\r");
    tui.wait("nested shell", |tui| {
        tui.rows().iter().any(|row| row.starts_with("nested>"))
    });
    tui.send("exit\r");
    tui.wait("outer shell", |tui| tui.text().trim_end().ends_with('$'));
    tui.view();
    assert_eq!(home.pid(1, 1), Some(original));
    tui.send("/bin/sh -c 'exit 9'; printf 'command-returned\\n'\r");
    tui.wait("command returned", |tui| {
        tui.rows()
            .iter()
            .any(|row| row.trim_end() == "command-returned")
    });
    tui.view();
    assert_eq!(home.pid(1, 1), Some(original));

    for (ending, code) in [
        ("exit\r", Some(0)),
        ("\x04", Some(0)),
        ("exit 5\r", Some(5)),
        ("kill -KILL $$\r", None),
    ] {
        let pid = home.pid(1, 1).unwrap();
        tui.send("printf 'retained-output\\n'\r");
        tui.wait("output before exit", |tui| {
            tui.rows()
                .iter()
                .any(|row| row.trim_end() == "retained-output")
        });
        tui.send(ending);
        tui.text_is(" native ");
        home.wait("terminal exit persisted", || {
            let state = home.read(1);
            state.contains("status=exited")
                && home.pid(1, 1).is_none()
                && code.is_none_or(|code| state.contains(&format!("exit={code}")))
        });
        tui.wait("exited terminal kept below navigation", |tui| {
            tui.rows()
                .iter()
                .skip(ROWS as usize / 2 + 1)
                .any(|row| row.contains("retained-output"))
        });
        assert!(!tui.text().contains(": restart"), "{}", tui.text());
        tui.send("\r");
        tui.view();
        tui.prompt();
        home.wait("enter restarts the terminal", || {
            home.pid(1, 1).is_some_and(|next| next != pid)
        });
    }
    tui.detach("\x14");
}

#[test]
fn terminal_exits_only_move_focused_viewers_and_leave_navigation_alone() {
    let home = Home::new("exit-viewers");
    home.write(&home.global(), "session main { terminal 1 command=\"/bin/sh\"; terminal 2 command=\"/bin/sh\"; workspace { window { pane one terminal=1; pane two terminal=2 } } }");
    let mut a = Tui::start(&home, &[]);
    a.prompt();
    let mut b = Tui::start(&home, &["attach", "main"]);
    b.prompt();
    b.send("\x14]");
    b.text_is(" native ");
    b.send("\x1b");
    b.view();
    let mut browser = Tui::start(&home, &["attach", "main"]);
    browser.prompt();
    browser.send("\x14jr");
    browser.text_is("Rename:");
    browser.send("draft");
    browser.text_is("twodraft");
    let mut other = Tui::start(&home, &["attach", "other"]);
    other.prompt();
    let other_pid = home.pid(2, 1).unwrap();

    // An inactive pane ending must not pull viewers out of the running pane.
    let background = home.pid(1, 2).unwrap();
    assert_eq!(unsafe { libc::kill(background as i32, libc::SIGKILL) }, 0);
    home.wait("inactive terminal exited", || home.pid(1, 2).is_none());
    a.view();
    b.view();
    browser.text_is("twodraft");

    a.send("exit\r");
    a.text_is(" native ");
    b.text_is(" native ");
    // The automatically opened panel keeps each viewer's representation.
    home.wait("fs representation retained", || {
        fs::read_to_string(home.global())
            .unwrap()
            .contains("representation=fs")
    });
    browser.text_is("Rename:");
    browser.text_is("twodraft");
    other.view();
    assert_eq!(home.pid(2, 1), Some(other_pid));
    other.send("printf 'still-running\\n'\r");
    other.wait("other session still interactive", |tui| {
        tui.rows()
            .iter()
            .any(|row| row.trim_end() == "still-running")
    });

    a.send("q");
    a.exited();
    b.send("q");
    b.exited();
    browser.send("\x1b");
    browser.send("q");
    browser.exited();
    other.detach("\x14");
}

#[test]
fn shutdown_restarts_from_independent_session_states() {
    let home = Home::new("restart");
    let mut a = Tui::start(&home, &["attach", "main"]);
    a.prompt();
    let mut b = Tui::start(&home, &["attach", "other"]);
    b.prompt();
    let first = home.pid(1, 1).unwrap();
    let second = home.pid(2, 1).unwrap();
    assert!(
        !home.command(&["server"]).status.success(),
        "a second coordinator must not replace the owner"
    );
    // Each session file has a held owner lock, not one lock per client.
    let lock = fs::File::open(home.session(1).with_file_name("state.kdl.lock")).unwrap();
    assert!(matches!(lock.try_lock(), Err(fs::TryLockError::WouldBlock)));
    a.send("exit 5\r");
    a.text_is("exited"); // Long program labels may truncate the numeric status.
    home.wait("exit persisted", || {
        let state = home.read(1);
        state.contains("status=exited") && state.contains("exit=5")
    });
    home.stop();
    a.exited();
    b.exited();
    home.wait("socket removed", || {
        String::from_utf8_lossy(&home.command(&["kill"]).stdout).contains("no server")
    });
    assert_eq!(unsafe { libc::kill(first as i32, 0) }, -1);
    assert_eq!(unsafe { libc::kill(second as i32, 0) }, -1);

    let mut again = Tui::start(&home, &["attach", "main"]);
    again.wait("client ready", |tui| tui.raw().contains("\x1b[>7u"));
    again.send("\x14");
    again.text_is("exited");
    assert!(home.read(1).contains("exit=5"));
    let mut other = Tui::start(&home, &["attach", "other"]);
    other.prompt();
    assert_ne!(home.pid(2, 1), Some(second));
    again.send("\r");
    again.prompt();
    assert_ne!(home.pid(1, 1), Some(first));
    again.send("\x14");
    again.text_is("panes");
    again.send("Q");
    again.exited();
    other.exited();
    home.wait("Q stops every owner", || {
        !home.read(1).contains("pid=") && !home.read(2).contains("pid=")
    });
}

#[test]
fn kill_reaps_a_process_that_ignores_hangup() {
    let home = Home::new("force-kill");
    home.write(
        &home.global(),
        r#"session main {
        terminal 1 command="/bin/sh" { args "-c" "trap '' HUP; printf READY; exec sleep 30" }
        workspace { window { pane terminal=1 } }
    }"#,
    );
    let mut tui = Tui::start(&home, &[]);
    tui.text_is("READY");
    let pid = home.pid(1, 1).unwrap();
    home.stop();
    tui.exited();
    assert_eq!(
        unsafe { libc::kill(pid as i32, 0) },
        -1,
        "kill left the session's process running"
    );
}

#[test]
fn deleting_one_client_node_disconnects_only_that_instance() {
    let home = Home::new("presence");
    let mut a = Tui::start(&home, &[]);
    a.prompt();
    let mut b = Tui::start(&home, &[]);
    b.prompt();
    let pid = home.pid(1, 1).unwrap();
    let text = fs::read_to_string(home.global()).unwrap();
    let mut doc: kdl::KdlDocument = text.parse().unwrap();
    let clients = doc
        .get_mut("catalog")
        .unwrap()
        .children_mut()
        .as_mut()
        .unwrap()
        .nodes_mut();
    let index = clients
        .iter()
        .position(|node| node.name().value() == "client")
        .unwrap();
    clients.remove(index);
    home.write(&home.global(), &doc.to_string());
    a.exited();
    b.send("echo still-here\r");
    b.text_is("still-here");
    assert_eq!(home.pid(1, 1), Some(pid));
    b.detach("\x14");
}

#[test]
fn configuration_and_skill_commands_are_local_explicit_and_never_overwrite() {
    let home = Home::new("config-cli");
    let config = home.config().join("decoration.kdl");
    assert_eq!(home.ok(&["config"]).trim(), config.to_str().unwrap());
    let template = home.ok(&["config", "show"]);
    assert!(template.contains("double-escape-ms=300") && template.contains("search \"/\""));
    assert!(home.ok(&["config", "init"]).contains("--apply"));
    home.ok(&["config", "check"]);
    assert!(!home.config().exists());
    assert_eq!(
        home.ok(&["skill"]),
        include_str!("../skills/nui-yalij/SKILL.md")
    );
    assert!(home.ok(&["help"]).contains("skill"));
    assert!(!home.global().exists());
    assert!(!home.dir.join("global.kdl.server.log").exists());
    home.ok(&["config", "init", "--apply"]);
    assert_eq!(fs::read_to_string(&config).unwrap(), template);
    assert!(
        !home
            .command(&["config", "init", "--apply"])
            .status
            .success()
    );
    assert_eq!(home.ok(&["config", "show"]), template);
    let profile = home.config().join("profiles/work.kdl");
    assert_eq!(
        home.ok(&["--profile", "work", "config", "path"]).trim(),
        profile.to_str().unwrap()
    );
    home.ok(&["--profile", "work", "config", "init", "--apply"]);
    home.ok(&["--profile", "work", "config", "check"]);
    home.write(&config, "gestures hold-escape-ms=-1");
    assert!(!home.command(&["config", "check"]).status.success());
    assert_eq!(home.ok(&["config", "show"]), "gestures hold-escape-ms=-1");
    assert!(!home.global().exists());
}

#[test]
fn escape_gestures_are_per_instance_use_reports_or_repeats_and_can_be_disabled() {
    let home = Home::new("gestures");
    home.write(
        &home.config().join("profiles/quiet.kdl"),
        "gestures double-escape-ms=0 hold-escape-ms=0",
    );
    home.write(
        &home.config().join("profiles/repeat.kdl"),
        "gestures double-escape-ms=0",
    );
    let mut tui = Tui::start(&home, &[]);
    tui.prompt();
    // Check routing before Escape gestures leave input in the shell's editor.
    tui.send(&format!("'{BIN}' config path\r"));
    tui.text_is(home.config().join("decoration.kdl").to_str().unwrap());
    let mut quiet = Tui::start(&home, &["--profile", "quiet"]);
    quiet.prompt();
    tui.send("\x1b\x1b");
    tui.wait("legacy double Escape opens navigation", |tui| {
        tui.text().contains(" native ")
    });
    assert!(!quiet.text().contains(" native "));
    tui.send("\x14");
    tui.view();
    quiet.send("\x1b\x1b");
    thread::sleep(Duration::from_millis(750));
    assert!(!quiet.text().contains(" native "));
    // A lone Escape on a terminal without releases is never a guessed hold.
    tui.send("\x1b");
    thread::sleep(Duration::from_millis(750));
    assert!(!tui.text().contains(" native "));

    // Negotiate kitty event types. A short press/release cancels the hold timer.
    tui.send("\x1b[?7u\x1b[?62;22c");
    tui.send("\x1b[27u\x1b[27;1:3u");
    thread::sleep(Duration::from_millis(750));
    assert!(!tui.text().contains(" native "));
    tui.send("\x1b[27u\x1b[27;1:3u\x1b[27u\x1b[27;1:3u");
    tui.wait("reported double Escape opens navigation", |tui| {
        tui.text().contains(" native ")
    });
    tui.send("\x14");
    tui.view();
    tui.send("\x1b[27u");
    tui.wait(
        "reported hold opens navigation without repeat events",
        |tui| tui.text().contains(" native "),
    );
    tui.send("\x1b[27;1:2u\x1b[27;1:2u");
    assert!(
        tui.text().contains(" native "),
        "repeats must not close the panel"
    );
    tui.send("\x1b[27;1:3u\x1b[27u\x1b[27;1:3u");
    tui.view();
    tui.send("\x1b[27u\x1b[O");
    thread::sleep(Duration::from_millis(750));
    assert!(
        !tui.text().contains(" native "),
        "focus loss cancels a pending hold"
    );
    tui.send("\x1b[I\x1b[27;1:3u");

    let mut repeat = Tui::start(&home, &["--profile", "repeat"]);
    repeat.prompt();
    repeat.send("\x1b");
    thread::sleep(Duration::from_millis(400));
    // A held key repeats until release, not just twice. Keep sending until
    // the panel opens so a single busy-runner scheduling gap cannot end the
    // simulated hold before its threshold.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !repeat.text().contains(" native ") && Instant::now() < deadline {
        repeat.send("\x1b");
    }
    repeat.wait("legacy hold opens navigation during auto-repeat", |tui| {
        tui.text().contains(" native ")
    });
    repeat.send("\x14");
    repeat.view();

    // Live session decoration overrides all profiles, including precise holds.
    home.write(
        &home.decoration(1),
        "gestures double-escape-ms=0 hold-escape-ms=0",
    );
    tui.send("\x1b[27u\x1b[27;1:3u\x1b[27u\x1b[27;1:3u");
    assert!(!tui.text().contains(" native "));
    tui.send("\x1b[27u");
    thread::sleep(Duration::from_millis(750));
    assert!(!tui.text().contains(" native "));
    tui.send("\x1b[27;1:3u");
    tui.detach("\x14");
    quiet.detach("\x14");
    repeat.detach("\x14");
}

#[test]
fn slash_search_is_live_then_frozen_across_sessions_without_affecting_other_clients() {
    let home = Home::new("search");
    home.write(&home.global(), r#"
        session alpha id=1 {
            terminal 1 command="/bin/sh"
            terminal 2 command="/bin/sh"
            workspace local { window editor { pane console terminal=1; pane notes terminal=2 } }
        }
        session beta id=2 {
            terminal 1 command="/bin/sh"
            terminal 2 command="/bin/sh"
            workspace project { window target { pane remote-one terminal=1; pane remote-two terminal=2 } }
        }
    "#);
    let mut tui = Tui::start(&home, &["attach", "alpha"]);
    tui.prompt();
    let mut watcher = Tui::start(&home, &["attach", "alpha"]);
    watcher.prompt();
    tui.send("echo original-$((40+2))\r");
    tui.text_is("original-42");
    tui.send("\x14/");
    tui.text_is("1/4 panes");
    assert!(
        tui.rows()
            .iter()
            .skip(ROWS as usize / 2)
            .any(|row| row.contains("original-42"))
    );
    tui.send("remote");
    tui.text_is("1/2 panes");
    tui.send("z");
    tui.text_is("No matching panes");
    tui.send("\x7f");
    tui.text_is("1/2 panes");
    assert!(!watcher.text().contains(" native ") && !watcher.text().contains("Search /"));
    tui.send("\r");
    tui.text_is("Search / remote");
    let old = home.read(2);
    assert!(old.contains("pane remote-two terminal=2"), "{old}");
    home.write(
        &home.session(2),
        &old.replace(
            "pane remote-two terminal=2",
            "pane remote-two terminal=2\n            pane remote-new terminal=1",
        ),
    );
    home.wait("new matching pane", || home.read(2).contains("remote-new"));
    thread::sleep(Duration::from_millis(350));
    assert!(
        tui.text().contains("1/2 panes"),
        "a frozen list must not acquire a new match"
    );
    tui.send("j");
    tui.text_is("2/2 panes");
    tui.send("\r");
    tui.prompt();
    home.wait("search enters foreign session", || {
        fs::read_to_string(home.global())
            .unwrap()
            .contains("client 1 session=2")
    });
    assert!(!watcher.text().contains(" native "));
    assert!(home.read(2).contains("active=2"));
    tui.send("\x14r/");
    tui.text_is("Rename:");
    assert!(!tui.text().contains("Search /"));
    tui.send("\x1b");
    tui.text_is(" native ");
    tui.send("]/");
    tui.text_is("1/5 panes");
    // Query paste is literal, not a navigation operation, in fs too.
    tui.send("\x1b[200~remote-two\x1b[201~");
    tui.text_is("1/1 panes");
    tui.send("\x1b");
    tui.text_is(" native ");
    tui.send("\x1b");
    tui.prompt();
    tui.detach("\x14");
    watcher.detach("\x14");
}

#[test]
fn listing_and_invalid_options_do_not_create_state() {
    let home = Home::new("listing");
    assert_eq!(
        home.ok(&["--version"]).trim(),
        format!("nui-yalij {}", env!("CARGO_PKG_VERSION"))
    );
    assert!(!home.global().exists());
    assert!(!home.config().exists());
    assert!(home.ok(&["list"]).is_empty());
    assert!(!home.global().exists());
    assert!(!home.command(&["--unknown"]).status.success());
}
