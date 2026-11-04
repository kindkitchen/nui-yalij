//! The application: events come in, a new state comes out, and everything
//! else follows the state: the processes, the file and the drawing.

use std::{collections::HashSet, sync::atomic::Ordering, sync::mpsc::Sender};

use ratatui::{
    Frame,
    crossterm::event::{Event as Input, KeyCode, KeyEvent, KeyEventKind},
};

use crate::layout::{self, Container};
use crate::plugin::Plugins;
use crate::runtime::{Runtime, lock};
use crate::state::{Fact, Level, Location, Nui, Status};
use crate::store::Store;
use crate::{keys, nav, ui};

/// What happens to the application.
pub enum Event {
    /// The person typed, pasted or resized the terminal.
    Input(Input),
    /// Terminals have new output to show.
    Output,
    /// The process of a terminal ended.
    Exited { id: u64, generation: u64, code: i32 },
}

pub struct App {
    nui: Nui,
    store: Store,
    runtime: Runtime,
    plugins: Plugins,
    /// The command of new terminals.
    shell: String,
    pub quit: bool,
}

impl App {
    /// Start from a state: its plugins load and its terminals run.
    pub fn new(nui: Nui, store: Store, events: Sender<Event>, size: (u16, u16), shell: String) -> App {
        let runtime = Runtime::new(events, size);
        let plugins = Plugins::new(shell.clone());
        let mut app = App { nui: Nui::default(), store, runtime, plugins, shell, quit: false };
        app.commit(nui);
        app
    }

    /// Make `next` the state: checked, and through the hooks of the plugins.
    /// The processes follow it, then the file keeps it.
    pub fn commit(&mut self, mut next: Nui) {
        if let Some(notice) = self.plugins.take_notices().pop() {
            next.message = Some(notice);
        }
        // the processes report facts on the way, which are changes as well
        for _ in 0..4 {
            if next == self.nui {
                break;
            }
            match self.settle(next) {
                Ok(settled) => self.nui = settled,
                Err(error) => {
                    self.nui.message = Some(error);
                    break;
                }
            }
            let facts = self.runtime.reconcile(&self.nui);
            if facts.is_empty() {
                break;
            }
            next = self.nui.clone();
            for fact in facts {
                next.apply(fact);
            }
        }
        if let Err(error) = self.store.save(&self.nui) {
            self.nui.message = Some(error);
        }
        self.quit |= self.nui.sessions.is_empty();
    }

    /// A new state as it will be: valid, after the hooks. A failing hook
    /// leaves the state as it came, with the error.
    fn settle(&mut self, mut next: Nui) -> Result<Nui, String> {
        next.normalize()?;
        self.plugins.sync(&next.plugins, self.store.dir());
        let hooked = self.plugins.update(next.clone()).and_then(|mut hooked| hooked.normalize().map(|()| hooked));
        let mut settled = hooked.unwrap_or_else(|error| {
            next.message = Some(error);
            next
        });
        if let Some(notice) = self.plugins.take_notices().pop() {
            settled.message = Some(notice);
        }
        Ok(settled)
    }

    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Input(Input::Key(key)) if key.kind != KeyEventKind::Release => self.key(key),
            Event::Input(Input::Paste(text)) => self.paste(&text),
            // a resize needs a draw only, which follows every event
            Event::Input(_) => {}
            Event::Output => self.runtime.pending.store(false, Ordering::Release),
            Event::Exited { id, generation, code } => {
                if self.runtime.exited(id, generation) {
                    let mut next = self.nui.clone();
                    next.apply(Fact::Exited(id, code));
                    self.commit(next);
                }
            }
        }
    }

    /// The toggle key opens and closes the panel. Other keys go to the
    /// plugins first, then to the panel or the focused terminal.
    fn key(&mut self, key: KeyEvent) {
        let Some(notation) = keys::notation(&key) else { return };
        if keys::canonical(&self.nui.toggle).is_some_and(|toggle| toggle == notation) {
            let mut next = self.nui.clone();
            match next.navigation {
                Some(_) => nav::close(&mut next),
                None => nav::open(&mut next),
            }
            return self.commit(next);
        }
        let navigating = self.nui.navigation.as_ref();
        // a pending operation takes all the keys: typing a name binds nothing
        if navigating.is_none_or(|navigation| navigation.operation.is_none()) {
            let mode = if navigating.is_some() { "navigate" } else { "view" };
            if let Some(result) = self.plugins.key(mode, &notation, &self.nui) {
                let next = result.unwrap_or_else(|error| Nui { message: Some(error), ..self.nui.clone() });
                return self.commit(next);
            }
        }
        if self.nui.navigation.is_some() {
            let mut next = self.nui.clone();
            self.quit |= nav::key(&mut next, &key, &self.shell) == nav::Outcome::Quit;
            self.commit(next);
        } else {
            self.type_in(&key);
        }
    }

    /// A key of the view goes to the focused terminal; enter starts it when
    /// it doesn't run.
    fn type_in(&mut self, key: &KeyEvent) {
        let Some(at) = self.nui.location() else { return };
        let Some(terminal) = self.nui.shown(at) else { return };
        if terminal.status == Status::Running {
            let screen = self.runtime.screen(terminal.id);
            let application_cursor = screen.is_some_and(|screen| lock(screen).screen().application_cursor());
            self.runtime.write(terminal.id, &keys::encode(key, application_cursor));
        } else if key.code == KeyCode::Enter {
            let mut next = self.nui.clone();
            next.enter(at);
            self.commit(next);
        }
    }

    fn paste(&mut self, text: &str) {
        if self.nui.navigation.is_some() {
            let mut next = self.nui.clone();
            nav::paste(&mut next, text);
            return self.commit(next);
        }
        let at = self.nui.location();
        let Some(terminal) = at.and_then(|at| self.nui.shown(at)).filter(|t| t.status == Status::Running) else {
            return;
        };
        let screen = self.runtime.screen(terminal.id);
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = screen.is_some_and(|screen| lock(screen).screen().bracketed_paste());
        let bytes = if bracketed { format!("\x1b[200~{text}\x1b[201~") } else { text };
        self.runtime.write(terminal.id, bytes.as_bytes());
    }

    /// Every now and then: the state file, which someone else may have
    /// changed, and what runs in the terminals.
    pub fn tick(&mut self) {
        let mut next = match self.store.poll() {
            Some(Ok(read)) => read,
            Some(Err(error)) => Nui { message: Some(format!("state file: {error}")), ..self.nui.clone() },
            None => self.nui.clone(),
        };
        let running: Vec<u64> = next.terminals().filter(|t| t.status == Status::Running).map(|t| t.id).collect();
        for id in running {
            if let Some(seen) = self.runtime.observe(id) {
                next.apply(Fact::Observed(id, seen));
            }
        }
        self.commit(next);
    }

    /// Draw the state. In the view, terminals take the size of the panes
    /// showing them: of the focused one first.
    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.runtime.size = (area.height, area.width);
        let plugins = &self.plugins;
        let mut place = |name: &str, container: &Container| {
            plugins.layout(name, container).unwrap_or_else(|| layout::tabs(container))
        };
        if self.nui.navigation.is_some() {
            return ui::navigation(frame, &self.nui, &self.runtime, &mut place);
        }
        let root = Location { session: self.nui.attached, ..Location::default() };
        let scene = layout::scene(&self.nui, Level::Session, root, area, &mut place);
        let here = self.nui.location();
        let mut sized = HashSet::new();
        for shown in scene.panes.iter().filter(|shown| Some(shown.at) == here).chain(&scene.panes) {
            if sized.insert(shown.terminal) {
                self.runtime.resize(shown.terminal, shown.area.height, shown.area.width);
            }
        }
        ui::view(frame, &self.nui, &self.runtime, &scene);
    }

    /// After a draw: what the layouts reported reaches the state.
    pub fn drawn(&mut self) {
        let notice = self.plugins.take_notices().pop();
        if notice.is_some() && notice != self.nui.message {
            let next = Nui { message: notice, ..self.nui.clone() };
            self.commit(next);
        }
    }

    /// Quit: every process stops. The state keeps what should run, for the
    /// next start; it is written as is, as nothing follows it anymore.
    pub fn shutdown(&mut self) {
        self.runtime.shutdown();
        for terminal in self.nui.sessions.iter_mut().flat_map(|session| &mut session.terminals) {
            terminal.pid = None;
        }
        let _ = self.store.save(&self.nui);
    }
}
