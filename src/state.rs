//! The Nui state: the single source of truth of the application.
//!
//! Everything shown or reachable in the TUI derives from it: the view, the
//! navigation panel and the processes behind the terminals. Only the content
//! of the terminals lives outside, in the runtime. Changes flow one way: an
//! event makes a new state, and everything else follows it.
//!
//! Indices count from 0 here and from 1 outside: in KDL and in Lua.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::keys;

/// The layout strategy of a container unless set: its children one at a time.
pub const DEFAULT_LAYOUT: &str = "tabs";
/// The key toggling the navigation panel unless set.
pub const DEFAULT_TOGGLE: &str = "ctrl-g";

/// The whole application.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Nui {
    /// The key toggling the navigation panel.
    pub toggle: String,
    /// The visualized session.
    #[serde(with = "one_based")]
    pub attached: usize,
    /// Lua plugins: paths, relative to the state file.
    pub plugins: Vec<String>,
    pub sessions: Vec<Session>,
    /// The navigation panel, while open.
    pub navigation: Option<Navigation>,
    /// A notice for the person: an error, or what a plugin printed.
    pub message: Option<String>,
}

/// The entry point to any activity: it owns terminals and shows them in windows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub name: String,
    /// The layout strategy placing the windows.
    pub layout: String,
    /// The active window.
    #[serde(with = "one_based")]
    pub active: usize,
    /// The processes of the session, shown by panes or detached.
    pub terminals: Vec<Terminal>,
    pub windows: Vec<Window>,
}

/// A grouping of tabs: a marker of one visualization.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Window {
    pub name: Option<String>,
    /// The layout strategy placing the tabs.
    pub layout: String,
    /// The active tab.
    #[serde(with = "one_based")]
    pub active: usize,
    pub tabs: Vec<Tab>,
}

/// A grouping of panes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tab {
    pub name: Option<String>,
    /// The layout strategy placing the panes.
    pub layout: String,
    /// The active pane.
    #[serde(with = "one_based")]
    pub active: usize,
    pub panes: Vec<Pane>,
}

/// What shows a terminal; many panes may show the same one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pane {
    pub name: Option<String>,
    /// The id of a terminal of the session.
    pub terminal: u64,
}

/// A process in a pseudo terminal, owned by its session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Terminal {
    pub id: u64,
    pub command: String,
    pub args: Vec<String>,
    /// Where it runs: the start directory, then the live one.
    pub cwd: Option<String>,
    pub status: Status,
    /// The exit code, once ended on its own.
    pub exit: Option<i32>,
    /// The process, while running.
    pub pid: Option<u32>,
    /// The program in the foreground.
    pub program: Option<String>,
    /// The title the program set.
    pub title: Option<String>,
}

/// Whether a terminal runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Running,
    /// Stopped by the person.
    Stopped,
    /// Ended on its own.
    Exited,
}

/// The navigation panel: a section per level, an item hovered in each.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Navigation {
    /// The focused section.
    pub focus: Level,
    /// The hovered item of every section.
    pub cursor: Location,
    /// The operation waiting for approval.
    pub operation: Option<Operation>,
}

/// An operation on the hovered item of the focused section.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Operation {
    Rename { input: String },
    /// Naming an item just created: an empty input keeps its automatic name,
    /// and approving goes on to the first item inside it.
    Create { input: String },
    Remove,
    Stop,
}

/// A level of the hierarchy, and a section of the navigation panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Session,
    Window,
    Tab,
    Pane,
}

/// The path from a session to a pane: one of the places a terminal is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Location {
    #[serde(with = "one_based")]
    pub session: usize,
    #[serde(with = "one_based")]
    pub window: usize,
    #[serde(with = "one_based")]
    pub tab: usize,
    #[serde(with = "one_based")]
    pub pane: usize,
}

/// What the runtime learns about a terminal; it reaches the state only as a fact.
#[derive(Clone, Debug, PartialEq)]
pub enum Fact {
    /// The process runs with this pid, or doesn't run.
    Pid(u64, Option<u32>),
    /// The process ended on its own with this code.
    Exited(u64, i32),
    /// The process could not start.
    Failed(u64, String),
    /// What runs in the terminal now.
    Observed(u64, Observation),
}

/// The foreground program of a terminal, its directory and the title it set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Observation {
    pub program: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
}

impl Default for Nui {
    fn default() -> Self {
        Self {
            toggle: DEFAULT_TOGGLE.into(),
            attached: 0,
            plugins: Vec::new(),
            sessions: Vec::new(),
            navigation: None,
            message: None,
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self {
            name: String::new(),
            layout: DEFAULT_LAYOUT.into(),
            active: 0,
            terminals: Vec::new(),
            windows: Vec::new(),
        }
    }
}

impl Default for Window {
    fn default() -> Self {
        Self { name: None, layout: DEFAULT_LAYOUT.into(), active: 0, tabs: Vec::new() }
    }
}

impl Default for Tab {
    fn default() -> Self {
        Self { name: None, layout: DEFAULT_LAYOUT.into(), active: 0, panes: Vec::new() }
    }
}

impl Operation {
    /// The name typed, in the operations taking one.
    pub fn input_mut(&mut self) -> Option<&mut String> {
        match self {
            Operation::Rename { input } | Operation::Create { input } => Some(input),
            Operation::Remove | Operation::Stop => None,
        }
    }
}

impl Level {
    pub const ALL: [Level; 4] = [Level::Session, Level::Window, Level::Tab, Level::Pane];

    pub fn name(self) -> &'static str {
        match self {
            Level::Session => "session",
            Level::Window => "window",
            Level::Tab => "tab",
            Level::Pane => "pane",
        }
    }

    pub fn parse(name: &str) -> Option<Level> {
        Level::ALL.into_iter().find(|level| level.name() == name)
    }

    /// The level `delta` steps down (positive) or up (negative), staying in bounds.
    pub fn step(self, delta: isize) -> Level {
        let index = (self as isize + delta).clamp(0, Level::ALL.len() as isize - 1);
        Level::ALL[index as usize]
    }
}

impl Location {
    pub fn get(&self, level: Level) -> usize {
        match level {
            Level::Session => self.session,
            Level::Window => self.window,
            Level::Tab => self.tab,
            Level::Pane => self.pane,
        }
    }

    pub fn set(&mut self, level: Level, index: usize) {
        match level {
            Level::Session => self.session = index,
            Level::Window => self.window = index,
            Level::Tab => self.tab = index,
            Level::Pane => self.pane = index,
        }
    }

    /// This location with `index` at `level`.
    pub fn with(mut self, level: Level, index: usize) -> Location {
        self.set(level, index);
        self
    }
}

impl Terminal {
    pub fn new(id: u64, command: &str, cwd: Option<String>) -> Terminal {
        Terminal { id, command: command.into(), cwd, ..Terminal::default() }
    }

    /// The name of what runs: the foreground program, or the command.
    pub fn program_name(&self) -> &str {
        self.program
            .as_deref()
            .unwrap_or_else(|| self.command.rsplit('/').next().unwrap_or_default())
    }

    /// Make it run again, unless it does.
    pub fn start(&mut self) {
        if self.status != Status::Running {
            self.status = Status::Running;
            self.exit = None;
        }
    }
}

impl Session {
    pub fn terminal(&self, id: u64) -> Option<&Terminal> {
        self.terminals.iter().find(|terminal| terminal.id == id)
    }

    /// The terminals some pane shows.
    fn shown(&self) -> HashSet<u64> {
        self.windows
            .iter()
            .flat_map(|window| &window.tabs)
            .flat_map(|tab| &tab.panes)
            .map(|pane| pane.terminal)
            .collect()
    }

    /// Remove the node at `level` of `at` and the containers it leaves empty,
    /// as every container holds at least one child. False when that empties
    /// the session itself.
    fn remove(&mut self, level: Level, at: Location) -> bool {
        if level == Level::Session {
            return false;
        }
        let Some(window) = self.windows.get_mut(at.window) else { return true };
        if level > Level::Window {
            let Some(tab) = window.tabs.get_mut(at.tab) else { return true };
            if level == Level::Pane {
                if at.pane >= tab.panes.len() {
                    return true;
                }
                tab.panes.remove(at.pane);
                shift(&mut tab.active, at.pane, tab.panes.len());
                if !tab.panes.is_empty() {
                    return true;
                }
            }
            window.tabs.remove(at.tab);
            shift(&mut window.active, at.tab, window.tabs.len());
            if !window.tabs.is_empty() {
                return true;
            }
        }
        self.windows.remove(at.window);
        shift(&mut self.active, at.window, self.windows.len());
        !self.windows.is_empty()
    }
}

impl Nui {
    /// The location of the person: the active pane of the attached session.
    pub fn location(&self) -> Option<Location> {
        self.active_location(self.attached)
    }

    /// The active pane of a session, through its active window and tab.
    pub fn active_location(&self, session: usize) -> Option<Location> {
        let at = self.descend(Level::Session, Location { session, ..Location::default() });
        self.pane(at).map(|_| at)
    }

    /// `at` with the levels below `level` moved to the active children.
    pub fn descend(&self, level: Level, mut at: Location) -> Location {
        if level < Level::Window {
            at.window = self.sessions.get(at.session).map_or(0, |session| session.active);
        }
        if level < Level::Tab {
            at.tab = self.window(at).map_or(0, |window| window.active);
        }
        if level < Level::Pane {
            at.pane = self.tab(at).map_or(0, |tab| tab.active);
        }
        at
    }

    pub fn window(&self, at: Location) -> Option<&Window> {
        self.sessions.get(at.session)?.windows.get(at.window)
    }

    pub fn tab(&self, at: Location) -> Option<&Tab> {
        self.window(at)?.tabs.get(at.tab)
    }

    pub fn pane(&self, at: Location) -> Option<&Pane> {
        self.tab(at)?.panes.get(at.pane)
    }

    /// The terminal the pane at `at` shows.
    pub fn shown(&self, at: Location) -> Option<&Terminal> {
        self.sessions.get(at.session)?.terminal(self.pane(at)?.terminal)
    }

    pub fn terminals(&self) -> impl Iterator<Item = &Terminal> {
        self.sessions.iter().flat_map(|session| &session.terminals)
    }

    fn terminal_mut(&mut self, id: u64) -> Option<&mut Terminal> {
        self.sessions
            .iter_mut()
            .flat_map(|session| &mut session.terminals)
            .find(|terminal| terminal.id == id)
    }

    /// An id no terminal has.
    pub fn next_terminal_id(&self) -> u64 {
        self.terminals().map(|terminal| terminal.id).max().map_or(1, |id| id + 1)
    }

    /// How many items the section `level` lists under the path `at`.
    pub fn count(&self, level: Level, at: Location) -> usize {
        match level {
            Level::Session => self.sessions.len(),
            Level::Window => self.sessions.get(at.session).map_or(0, |session| session.windows.len()),
            Level::Tab => self.window(at).map_or(0, |window| window.tabs.len()),
            Level::Pane => self.tab(at).map_or(0, |tab| tab.panes.len()),
        }
    }

    /// Whether the node at `level` of `at` is the one its parent shows; for a
    /// session, whether it is attached.
    pub fn is_active(&self, level: Level, at: Location) -> bool {
        match level {
            Level::Session => at.session == self.attached,
            Level::Window => self.sessions.get(at.session).is_some_and(|session| session.active == at.window),
            Level::Tab => self.window(at).is_some_and(|window| window.active == at.tab),
            Level::Pane => self.tab(at).is_some_and(|tab| tab.active == at.pane),
        }
    }

    /// The name shown for the node at `level` of `at`: its own, or the name of
    /// what it shows.
    pub fn label(&self, level: Level, at: Location) -> String {
        let label = match level {
            Level::Session => self.sessions.get(at.session).map(|session| session.name.clone()),
            Level::Window => self.window(at).map(|window| {
                window.name.clone().unwrap_or_else(|| self.label(Level::Tab, at.with(Level::Tab, window.active)))
            }),
            Level::Tab => self.tab(at).map(|tab| {
                tab.name.clone().unwrap_or_else(|| self.label(Level::Pane, at.with(Level::Pane, tab.active)))
            }),
            Level::Pane => self.pane(at).map(|pane| {
                pane.name.clone().unwrap_or_else(|| {
                    self.shown(at).map_or_else(|| "?".into(), |terminal| terminal.program_name().into())
                })
            }),
        };
        label.unwrap_or_default()
    }

    /// The terminals inside the node at `level` of `at`: the ones its panes
    /// show, or all of a session's.
    pub fn terminals_in(&self, level: Level, at: Location) -> Vec<u64> {
        let Some(session) = self.sessions.get(at.session) else { return Vec::new() };
        let panes: Vec<&Pane> = match level {
            Level::Session => return session.terminals.iter().map(|terminal| terminal.id).collect(),
            Level::Window => self.window(at).into_iter().flat_map(|window| &window.tabs).flat_map(|tab| &tab.panes).collect(),
            Level::Tab => self.tab(at).into_iter().flat_map(|tab| &tab.panes).collect(),
            Level::Pane => self.pane(at).into_iter().collect(),
        };
        let mut ids: Vec<u64> = panes.iter().map(|pane| pane.terminal).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Attach the session `name`; a missing one is created, running `command` in `cwd`.
    pub fn attach(&mut self, name: &str, command: &str, cwd: Option<String>) {
        self.attached = match self.sessions.iter().position(|session| session.name == name) {
            Some(index) => index,
            None => self.add_session(name, command, cwd),
        };
        self.navigation = None;
    }

    /// Add a session of one window, tab and pane showing a new terminal; returns its index.
    pub fn add_session(&mut self, name: &str, command: &str, cwd: Option<String>) -> usize {
        let id = self.next_terminal_id();
        let tab = Tab { panes: vec![Pane { name: None, terminal: id }], ..Tab::default() };
        let window = Window { tabs: vec![tab], ..Window::default() };
        self.sessions.push(Session {
            name: name.into(),
            terminals: vec![Terminal::new(id, command, cwd)],
            windows: vec![window],
            ..Session::default()
        });
        self.sessions.len() - 1
    }

    /// Create a node at `level` after the one at `at`, with one of every level
    /// below it: the last a pane showing a new terminal, running `command` in
    /// `cwd`. Each is named automatically. Returns where the new node is.
    pub fn create(&mut self, level: Level, at: Location, command: &str, cwd: Option<String>) -> Option<Location> {
        let id = self.next_terminal_id();
        let first = |level| Some(automatic(level, &[]));
        let pane = Pane { name: first(Level::Pane), terminal: id };
        let tab = Tab { name: first(Level::Tab), panes: vec![pane.clone()], ..Tab::default() };
        let window = Window { name: first(Level::Window), tabs: vec![tab.clone()], ..Window::default() };
        let terminal = Terminal::new(id, command, cwd);
        if level == Level::Session {
            let names: Vec<_> = self.sessions.iter().map(|session| Some(session.name.as_str())).collect();
            let name = automatic(level, &names);
            let session = Session { name, terminals: vec![terminal], windows: vec![window], ..Session::default() };
            let index = insert(&mut self.sessions, &mut self.attached, at.session + 1, session);
            return Some(Location { session: index, ..Location::default() });
        }
        let session = self.sessions.get_mut(at.session)?;
        let new = match level {
            Level::Window => {
                let names: Vec<_> = session.windows.iter().map(|window| window.name.as_deref()).collect();
                let window = Window { name: Some(automatic(level, &names)), ..window };
                let index = insert(&mut session.windows, &mut session.active, at.window + 1, window);
                Location { session: at.session, window: index, ..Location::default() }
            }
            Level::Tab => {
                let parent = session.windows.get_mut(at.window)?;
                let names: Vec<_> = parent.tabs.iter().map(|tab| tab.name.as_deref()).collect();
                let tab = Tab { name: Some(automatic(level, &names)), ..tab };
                Location { tab: insert(&mut parent.tabs, &mut parent.active, at.tab + 1, tab), pane: 0, ..at }
            }
            _ => {
                let parent = session.windows.get_mut(at.window)?.tabs.get_mut(at.tab)?;
                let names: Vec<_> = parent.panes.iter().map(|pane| pane.name.as_deref()).collect();
                let pane = Pane { name: Some(automatic(level, &names)), ..pane };
                Location { pane: insert(&mut parent.panes, &mut parent.active, at.pane + 1, pane), ..at }
            }
        };
        session.terminals.push(terminal);
        Some(new)
    }

    /// Rename the node at `level` of `at`. An empty name gives a window, tab or
    /// pane back the name of what it shows; a session needs a unique one.
    pub fn rename(&mut self, level: Level, at: Location, name: &str) -> Result<(), String> {
        let name = name.trim();
        let missing = || format!("no such {}", level.name());
        if level == Level::Session {
            if name.is_empty() {
                return Err("a session needs a name".into());
            }
            if self.sessions.iter().enumerate().any(|(index, session)| index != at.session && session.name == name) {
                return Err(format!("session {name} already exists"));
            }
            self.sessions.get_mut(at.session).ok_or_else(missing)?.name = name.into();
            return Ok(());
        }
        let name = (!name.is_empty()).then(|| name.to_string());
        let session = self.sessions.get_mut(at.session).ok_or_else(missing)?;
        let window = session.windows.get_mut(at.window).ok_or_else(missing)?;
        if level == Level::Window {
            window.name = name;
            return Ok(());
        }
        let tab = window.tabs.get_mut(at.tab).ok_or_else(missing)?;
        if level == Level::Tab {
            tab.name = name;
            return Ok(());
        }
        tab.panes.get_mut(at.pane).ok_or_else(missing)?.name = name;
        Ok(())
    }

    /// Remove the node at `level` of `at` with everything inside it. A container
    /// left empty goes too; so do the terminals no pane shows anymore.
    pub fn remove(&mut self, level: Level, at: Location) {
        let inside = self.terminals_in(level, at);
        let Some(session) = self.sessions.get_mut(at.session) else { return };
        if !session.remove(level, at) {
            self.sessions.remove(at.session);
            shift(&mut self.attached, at.session, self.sessions.len());
            return;
        }
        let shown = session.shown();
        session.terminals.retain(|terminal| !inside.contains(&terminal.id) || shown.contains(&terminal.id));
    }

    /// Stop the terminals inside the node at `level` of `at`.
    pub fn stop(&mut self, level: Level, at: Location) {
        let inside = self.terminals_in(level, at);
        let Some(session) = self.sessions.get_mut(at.session) else { return };
        for terminal in &mut session.terminals {
            if inside.contains(&terminal.id) && terminal.status == Status::Running {
                terminal.status = Status::Stopped;
            }
        }
    }

    /// Go into the pane at `at`: it becomes the location of the person, and its
    /// terminal runs.
    pub fn enter(&mut self, at: Location) {
        let Some(id) = self.pane(at).map(|pane| pane.terminal) else { return };
        self.attached = at.session;
        let session = &mut self.sessions[at.session];
        session.active = at.window;
        let window = &mut session.windows[at.window];
        window.active = at.tab;
        window.tabs[at.tab].active = at.pane;
        if let Some(terminal) = session.terminals.iter_mut().find(|terminal| terminal.id == id) {
            terminal.start();
        }
        self.navigation = None;
    }

    /// Take in what the runtime learned.
    pub fn apply(&mut self, fact: Fact) {
        match fact {
            Fact::Pid(id, pid) => {
                if let Some(terminal) = self.terminal_mut(id) {
                    terminal.pid = pid;
                }
            }
            Fact::Exited(id, code) => {
                if let Some(terminal) = self.terminal_mut(id) {
                    terminal.pid = None;
                    if terminal.status == Status::Running {
                        terminal.status = Status::Exited;
                        terminal.exit = Some(code);
                    }
                }
            }
            Fact::Failed(id, error) => {
                if let Some(terminal) = self.terminal_mut(id) {
                    terminal.pid = None;
                    terminal.status = Status::Exited;
                    terminal.exit = None;
                }
                self.message = Some(error);
            }
            Fact::Observed(id, seen) => {
                if let Some(terminal) = self.terminal_mut(id) {
                    terminal.program = seen.program.or(terminal.program.take());
                    terminal.cwd = seen.cwd.or(terminal.cwd.take());
                    terminal.title = seen.title.or(terminal.title.take());
                }
            }
        }
    }

    /// Check the invariants and settle the indices: states from outside, the
    /// file and plugins, pass here.
    pub fn normalize(&mut self) -> Result<(), String> {
        if keys::canonical(&self.toggle).is_none() {
            return Err(format!("toggle: {} is not a key", self.toggle));
        }
        let mut names = HashSet::new();
        let mut ids = HashSet::new();
        for session in &mut self.sessions {
            let name = session.name.clone();
            if name.trim().is_empty() {
                return Err("a session needs a name".into());
            }
            if !names.insert(name.clone()) {
                return Err(format!("session {name} is there twice"));
            }
            for terminal in &session.terminals {
                if !ids.insert(terminal.id) {
                    return Err(format!("terminal {} is there twice", terminal.id));
                }
            }
            let known: HashSet<u64> = session.terminals.iter().map(|terminal| terminal.id).collect();
            if session.windows.is_empty() {
                return Err(format!("session {name} has no windows"));
            }
            clamp(&mut session.active, session.windows.len());
            for (w, window) in session.windows.iter_mut().enumerate() {
                if window.tabs.is_empty() {
                    return Err(format!("session {name}, window {}: no tabs", w + 1));
                }
                clamp(&mut window.active, window.tabs.len());
                for (t, tab) in window.tabs.iter_mut().enumerate() {
                    if tab.panes.is_empty() {
                        return Err(format!("session {name}, window {}, tab {}: no panes", w + 1, t + 1));
                    }
                    clamp(&mut tab.active, tab.panes.len());
                    if let Some(pane) = tab.panes.iter().find(|pane| !known.contains(&pane.terminal)) {
                        return Err(format!("session {name}: a pane shows terminal {}, not in the session", pane.terminal));
                    }
                }
            }
        }
        clamp(&mut self.attached, self.sessions.len());
        if self.sessions.is_empty() {
            self.navigation = None;
        }
        let cursor = self.navigation.as_ref().map(|navigation| self.settle(navigation.cursor));
        if let (Some(navigation), Some(cursor)) = (self.navigation.as_mut(), cursor) {
            navigation.cursor = cursor;
        }
        Ok(())
    }

    /// `at` with every index in bounds.
    fn settle(&self, mut at: Location) -> Location {
        for level in Level::ALL {
            at.set(level, at.get(level).min(self.count(level, at).saturating_sub(1)));
        }
        at
    }
}

/// Keep `index` in a list of `len`.
fn clamp(index: &mut usize, len: usize) {
    *index = (*index).min(len.saturating_sub(1));
}

/// The automatic name of a node at `level` joining siblings of these names:
/// `window-3` as the third window, or the next one no sibling has.
fn automatic(level: Level, siblings: &[Option<&str>]) -> String {
    (siblings.len() + 1..)
        .map(|n| format!("{}-{n}", level.name()))
        .find(|name| !siblings.contains(&Some(name.as_str())))
        .unwrap_or_default()
}

/// Insert `item` at `index`, or last when beyond, keeping `active` on the item
/// it was on. Returns where the item went.
fn insert<T>(list: &mut Vec<T>, active: &mut usize, index: usize, item: T) -> usize {
    let index = index.min(list.len());
    if *active >= index && !list.is_empty() {
        *active += 1;
    }
    list.insert(index, item);
    index
}

/// Follow the removal of the item at `removed`: the next one takes its place.
fn shift(index: &mut usize, removed: usize, len: usize) {
    if *index > removed {
        *index -= 1;
    }
    clamp(index, len);
}

/// Serde of indices as people and Lua count them: from 1.
mod one_based {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(index: &usize, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(*index as u64 + 1)
    }

    /// Lua numbers come as integers or floats.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<usize, D::Error> {
        let number = f64::deserialize(deserializer)?;
        if number >= 1.0 && number.fract() == 0.0 {
            Ok(number as usize - 1)
        } else {
            Err(D::Error::custom(format!("{number} is no index: they count from 1")))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A session `main` of two windows: the first with tabs of 2 and 1 panes,
    /// the second with one pane; the second pane shows the first terminal again.
    pub(crate) fn sample() -> Nui {
        let mut nui = Nui::default();
        nui.add_session("main", "sh", None);
        let session = &mut nui.sessions[0];
        session.terminals.extend([Terminal::new(2, "top", None), Terminal::new(3, "vim", None)]);
        let pane = |terminal| Pane { name: None, terminal };
        session.windows[0].tabs[0].panes.push(pane(1));
        session.windows[0].tabs.push(Tab { panes: vec![pane(2)], ..Tab::default() });
        session.windows.push(Window { tabs: vec![Tab { panes: vec![pane(3)], ..Tab::default() }], ..Window::default() });
        nui
    }

    fn at(session: usize, window: usize, tab: usize, pane: usize) -> Location {
        Location { session, window, tab, pane }
    }

    #[test]
    fn labels_derive_from_what_is_shown() {
        let mut nui = sample();
        assert_eq!(nui.label(Level::Window, at(0, 1, 0, 0)), "vim");
        nui.rename(Level::Pane, at(0, 1, 0, 0), "editor").unwrap();
        assert_eq!(nui.label(Level::Window, at(0, 1, 0, 0)), "editor");
        nui.rename(Level::Pane, at(0, 1, 0, 0), "  ").unwrap();
        assert_eq!(nui.label(Level::Tab, at(0, 1, 0, 0)), "vim");
    }

    #[test]
    fn sessions_need_unique_names() {
        let mut nui = sample();
        nui.add_session("other", "sh", None);
        assert!(nui.rename(Level::Session, at(1, 0, 0, 0), "main").is_err());
        assert!(nui.rename(Level::Session, at(1, 0, 0, 0), "").is_err());
        assert!(nui.rename(Level::Session, at(1, 0, 0, 0), "work").is_ok());
        assert_eq!(nui.sessions[1].name, "work");
    }

    #[test]
    fn removal_keeps_terminals_still_shown() {
        let mut nui = sample();
        nui.remove(Level::Pane, at(0, 0, 0, 0));
        // terminal 1 is still shown by the other pane of the tab
        assert_eq!(nui.sessions[0].terminals.len(), 3);
        nui.remove(Level::Pane, at(0, 0, 0, 0));
        // the tab went with its last pane, and terminal 1 with it
        assert_eq!(nui.sessions[0].windows[0].tabs.len(), 1);
        assert!(nui.sessions[0].terminal(1).is_none());
    }

    #[test]
    fn removal_cascades_up_to_the_session() {
        let mut nui = sample();
        nui.add_session("other", "sh", None);
        nui.attached = 1;
        nui.remove(Level::Window, at(0, 1, 0, 0));
        nui.remove(Level::Tab, at(0, 0, 1, 0));
        assert_eq!(nui.sessions[0].windows.len(), 1);
        nui.remove(Level::Pane, at(0, 0, 0, 0));
        nui.remove(Level::Pane, at(0, 0, 0, 0));
        assert_eq!(nui.sessions.len(), 1);
        assert_eq!(nui.sessions[0].name, "other");
        assert_eq!(nui.attached, 0);
    }

    #[test]
    fn creation_goes_after_the_hovered_with_automatic_names() {
        let mut nui = sample();
        nui.sessions[0].active = 1;
        // a window after the first: the active second one stays active, as third
        let new = nui.create(Level::Window, at(0, 0, 1, 0), "sh", None).unwrap();
        assert_eq!(new, at(0, 1, 0, 0));
        assert_eq!(nui.sessions[0].active, 2);
        let window = &nui.sessions[0].windows[1];
        assert_eq!(window.name.as_deref(), Some("window-3"));
        assert_eq!((window.tabs[0].name.as_deref(), window.tabs[0].panes[0].name.as_deref()), (Some("tab-1"), Some("pane-1")));
        assert_eq!(nui.sessions[0].terminal(4).map(|terminal| terminal.command.as_str()), Some("sh"));
        // a taken name is skipped
        nui.rename(Level::Pane, at(0, 0, 0, 0), "pane-3").unwrap();
        let new = nui.create(Level::Pane, at(0, 0, 0, 1), "sh", None).unwrap();
        assert_eq!(nui.pane(new).unwrap().name.as_deref(), Some("pane-4"));
        assert_eq!(nui.create(Level::Session, at(0, 0, 0, 0), "sh", None), Some(at(1, 0, 0, 0)));
        assert_eq!(nui.sessions[1].name, "session-2");
        nui.normalize().unwrap();
    }

    #[test]
    fn stop_and_enter() {
        let mut nui = sample();
        nui.stop(Level::Window, at(0, 0, 0, 0));
        let status = |nui: &Nui, id| nui.sessions[0].terminal(id).unwrap().status;
        assert_eq!(status(&nui, 1), Status::Stopped);
        assert_eq!(status(&nui, 2), Status::Stopped);
        assert_eq!(status(&nui, 3), Status::Running);
        nui.enter(at(0, 0, 1, 0));
        assert_eq!(nui.location(), Some(at(0, 0, 1, 0)));
        assert_eq!(status(&nui, 2), Status::Running);
    }

    #[test]
    fn facts_from_the_runtime() {
        let mut nui = sample();
        nui.apply(Fact::Pid(2, Some(42)));
        nui.apply(Fact::Observed(2, Observation { program: Some("htop".into()), ..Observation::default() }));
        assert_eq!(nui.sessions[0].terminal(2).unwrap().program_name(), "htop");
        nui.apply(Fact::Exited(2, 3));
        let terminal = nui.sessions[0].terminal(2).unwrap();
        assert_eq!((terminal.status, terminal.exit, terminal.pid), (Status::Exited, Some(3), None));
    }

    #[test]
    fn normalize_checks_invariants() {
        let mut nui = sample();
        nui.sessions[0].windows[1].active = 7;
        nui.normalize().unwrap();
        assert_eq!(nui.sessions[0].windows[1].active, 0);
        nui.sessions[0].windows[1].tabs[0].panes.clear();
        assert!(nui.normalize().is_err());
        let mut nui = sample();
        nui.sessions[0].windows[0].tabs[0].panes[0].terminal = 9;
        assert!(nui.normalize().is_err());
    }
}
