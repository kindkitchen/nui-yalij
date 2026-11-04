//! nui-yalij: a terminal multiplexer derived from one state, after yazi and zellij.
//!
//! The state (`state::Nui`, kept as KDL) is the single source of truth: the
//! view, the navigation panel and the processes all follow it, and nothing
//! flows the other way but as new states.

mod app;
mod kdl;
mod keys;
mod layout;
mod nav;
mod plugin;
mod runtime;
mod state;
mod store;
mod ui;

use std::{
    env,
    io::stdout,
    path::PathBuf,
    process::ExitCode,
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use ratatui::{
    DefaultTerminal,
    crossterm::{
        event::{self, DisableBracketedPaste, EnableBracketedPaste},
        execute,
    },
};

use crate::{
    app::{App, Event},
    state::Nui,
    store::Store,
};

const USAGE: &str = "\
nui-yalij: a terminal multiplexer derived from one state

usage: nui-yalij [--state <file>] [command]

commands:
  attach [<session>]  attach a session, created when missing; the default command.
                      Inside a running nui-yalij, it switches that one to the session.
  list                list the sessions of the state
  help                show this help

The state file is $XDG_STATE_HOME/nui-yalij/state.kdl (~/.local/state/...),
or <file>. Edit it while nui-yalij runs: it follows. ctrl-g toggles the
navigation panel unless the state sets another `toggle`.
";

/// How often the state file and the programs in the terminals are looked at.
const TICK: Duration = Duration::from_millis(250);

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nui-yalij: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut path = None;
    let mut words = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state" => path = Some(PathBuf::from(args.next().ok_or("--state needs a file")?)),
            "-h" | "--help" | "help" => {
                print!("{USAGE}");
                return Ok(());
            }
            _ => words.push(arg),
        }
    }
    let mut store = Store::new(path.unwrap_or_else(default_path));
    let nui = store.load()?.unwrap_or_default();
    match words.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["attach"] => attach(store, nui, None),
        ["attach", name] => attach(store, nui, Some(name)),
        ["list"] => {
            list(&nui);
            Ok(())
        }
        _ => Err(format!("unknown command: {}\n\n{USAGE}", words.join(" "))),
    }
}

fn default_path() -> PathBuf {
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    base.join("nui-yalij").join("state.kdl")
}

fn shell() -> String {
    env::var("SHELL").ok().filter(|shell| !shell.is_empty()).unwrap_or_else(|| "/bin/sh".into())
}

/// Attach a session: in a new instance, or in the running one, which follows
/// the state file.
fn attach(mut store: Store, mut nui: Nui, name: Option<&str>) -> Result<(), String> {
    let cwd = env::current_dir().ok().map(|dir| dir.display().to_string());
    match name {
        Some(name) => nui.attach(name, &shell(), cwd),
        None if nui.sessions.is_empty() => nui.attach("main", &shell(), cwd),
        None => {}
    }
    if store.lock()? {
        return tui(store, nui);
    }
    let Some(name) = name else {
        return Err(format!("already running on {}: open its navigation to switch", store.path().display()));
    };
    store.save(&nui)?;
    println!("nui-yalij: the running instance attaches {name}");
    Ok(())
}

fn list(nui: &Nui) {
    for (index, session) in nui.sessions.iter().enumerate() {
        let tabs = session.windows.iter().flat_map(|window| &window.tabs);
        let panes: usize = tabs.clone().map(|tab| tab.panes.len()).sum();
        let attached = if index == nui.attached { "  (attached)" } else { "" };
        println!(
            "{}  windows: {}  tabs: {}  panes: {}  terminals: {}{attached}",
            session.name,
            session.windows.len(),
            tabs.count(),
            panes,
            session.terminals.len()
        );
    }
}

fn tui(store: Store, nui: Nui) -> Result<(), String> {
    let mut terminal = ratatui::try_init().map_err(|error| error.to_string())?;
    let _ = execute!(stdout(), EnableBracketedPaste);
    let (events, received) = mpsc::channel();
    let input = events.clone();
    thread::spawn(move || {
        while let Ok(read) = event::read() {
            if input.send(Event::Input(read)).is_err() {
                break;
            }
        }
    });
    let size = terminal.size().map_err(|error| error.to_string())?;
    let mut app = App::new(nui, store, events, (size.height, size.width), shell());
    let result = run_loop(&mut terminal, &mut app, &received);
    app.shutdown();
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

/// Draw, wait for events, handle them all; tick in between.
fn run_loop(terminal: &mut DefaultTerminal, app: &mut App, events: &Receiver<Event>) -> Result<(), String> {
    let mut tick = Instant::now() + TICK;
    while !app.quit {
        terminal.draw(|frame| app.draw(frame)).map_err(|error| error.to_string())?;
        app.drawn();
        match events.recv_timeout(tick.saturating_duration_since(Instant::now())) {
            Ok(event) => {
                app.handle(event);
                while let Ok(event) = events.try_recv() {
                    app.handle(event);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if Instant::now() >= tick {
            app.tick();
            tick = Instant::now() + TICK;
        }
    }
    Ok(())
}
