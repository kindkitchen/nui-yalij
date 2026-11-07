//! A thin coordinator: catalog/presence, transport and routing. Each session
//! has exactly one SessionServer. All mutations run in this event loop, including
//! edits made while another session is merely being previewed in navigation.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    sync::{atomic::Ordering, mpsc::Sender},
    time::Instant,
};

use ratatui::crossterm::event::{Event as Input, KeyEvent, KeyEventKind};

use crate::{
    decoration::{Decoration, Layer, Library},
    gestures::{Action, Escapes},
    global::{Catalog, Client},
    ipc::{self, Peer, Render, Request, Response},
    keys,
    layout::{self, Container},
    nav,
    plugin::{ClientContext, Plugins},
    runtime::{Effect, Mark, Palette, Screen, Screens, lock},
    session::SessionServer,
    state::{Fact, Level, Location, Nui, Session, StateRepresentation, Status},
    store::{Store, suffixed},
    ui,
};

pub enum Event {
    Request(u64, Request),
    Disconnected(u64),
    Output(u64),
    Exited {
        session: u64,
        id: u64,
        generation: u64,
        code: i32,
    },
}

pub struct App {
    pub catalog: Catalog,
    store: Store,
    owners: HashMap<u64, SessionServer>,
    library: Library,
    peers: HashMap<u64, Peer>,
    views: HashMap<(u64, u64), Plugins>,
    last_input: HashMap<(u64, u64), u64>,
    /// The terminals around connected clients, as far as programs reach them.
    outer: HashMap<u64, Outer>,
    /// What the terminals around detached clients have of the histories, by
    /// tty, for when they attach again.
    seen: HashMap<String, Seen>,
    /// The terminals focused: in the focused pane of an instance whose
    /// terminal has the focus, by session and terminal id.
    focused: HashSet<(u64, u64)>,
    events: Sender<Event>,
    shell: String,
    next_session: u64,
    dirty: bool,
    pub quit: bool,
}

/// The terminal around a client, as far as programs reach it.
#[derive(Default)]
struct Outer {
    escapes: Escapes,
    /// It told it lost the focus.
    unfocused: bool,
    palette: Option<Palette>,
    /// The title it shows.
    title: Option<String>,
    /// Bytes to pass on with the next drawing.
    bytes: Vec<u8>,
    /// Its tty, telling it from others across attachments.
    tty: Option<String>,
    /// What it has of the histories of the terminals it watched.
    seen: Seen,
}

/// How far a terminal around has the history of each terminal, by session
/// and terminal id.
type Seen = HashMap<(u64, u64), Mark>;

struct SessionScreens<'a> {
    order: &'a [u64],
    owners: &'a HashMap<u64, SessionServer>,
}
impl Screens for SessionScreens<'_> {
    fn screen_at(&self, session: usize, terminal: u64) -> Option<&Screen> {
        self.owners
            .get(self.order.get(session)?)?
            .runtime
            .screen(terminal)
    }
}

impl App {
    pub fn open(
        mut store: Store,
        library: Library,
        events: Sender<Event>,
        shell: String,
        legacy: Option<&Path>,
    ) -> Result<App, String> {
        let text = store.load()?;
        let mut seed = HashMap::new();
        let mut catalog = Catalog::default();
        let mut legacy_lock = None;
        let old = if let Some(text) = &text {
            let document: ::kdl::KdlDocument = text
                .parse()
                .map_err(|error| format!("global state: {error}"))?;
            if document
                .nodes()
                .iter()
                .any(|node| node.name().value() == "catalog")
            {
                catalog = Catalog::decode(text)?;
                None
            } else {
                let backup = suffixed(store.path(), ".legacy.kdl");
                if !backup.exists() {
                    fs::copy(store.path(), &backup).map_err(|error| error.to_string())?;
                }
                Some(text.clone())
            }
        } else if let Some(path) = legacy.filter(|path| path.exists()) {
            let mut old = Store::new(path.to_path_buf());
            if !old.lock()? {
                return Err(
                    "the old application still owns state.kdl; stop it before migration".into(),
                );
            }
            let text = old.load()?;
            legacy_lock = Some(old);
            text
        } else {
            None
        };
        if let Some(text) = old {
            let mut nui = crate::kdl::decode(&text)?;
            nui.normalize()?;
            for session in nui.sessions {
                catalog.sessions.push(session.id);
                seed.insert(session.id, session);
            }
        }
        // Presence belongs to live transports, never to a previous daemon run.
        catalog.clients.clear();
        let mut owners = HashMap::new();
        for id in &catalog.sessions {
            owners.insert(
                *id,
                SessionServer::open(
                    store.path(),
                    *id,
                    seed.remove(id),
                    events.clone(),
                    &shell,
                    &library,
                )?,
            );
        }
        let maximum = fs::read_dir(suffixed(store.path(), ".sessions"))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        let next_session = maximum.max(catalog.sessions.iter().copied().max().unwrap_or(0)) + 1;
        let mut app = App {
            catalog,
            store,
            owners,
            library,
            peers: HashMap::new(),
            views: HashMap::new(),
            last_input: HashMap::new(),
            outer: HashMap::new(),
            seen: HashMap::new(),
            focused: HashSet::new(),
            events,
            shell,
            next_session,
            dirty: true,
            quit: false,
        };
        app.projection(None).normalize()?;
        app.cleanup();
        app.persist();
        drop(legacy_lock);
        Ok(app)
    }

    pub fn accept(&mut self, id: u64, peer: Peer) {
        self.peers.insert(id, peer);
    }

    fn projection(&self, client: Option<u64>) -> Nui {
        let mut nui = Nui {
            sessions: self
                .catalog
                .sessions
                .iter()
                .filter_map(|id| self.owners.get(id).map(|owner| owner.state.clone()))
                .collect(),
            ..Nui::default()
        };
        if let Some(client) = client.and_then(|id| self.catalog.client(id)) {
            nui.attached = self
                .catalog
                .sessions
                .iter()
                .position(|id| *id == client.session)
                .unwrap_or(0);
            nui.navigation = client.navigation.clone();
            nui.representation = client.representation;
            nui.message = client.message.clone();
        }
        nui
    }

    fn decoration(&self, client: u64) -> Result<(u64, Decoration), String> {
        let client = self
            .catalog
            .client(client)
            .ok_or("client is not attached")?;
        let session = client
            .navigation
            .as_ref()
            .and_then(|nav| self.catalog.sessions.get(nav.cursor.session))
            .copied()
            .unwrap_or(client.session);
        let owner = self.owners.get(&session).ok_or("session is gone")?;
        Ok((
            session,
            self.library
                .effective(client.profile.as_deref(), &owner.layer)?,
        ))
    }

    fn ensure_session(&mut self, name: Option<&str>, cwd: Option<String>) -> Result<u64, String> {
        if let Some(owner) = self
            .catalog
            .sessions
            .iter()
            .filter_map(|id| self.owners.get(id))
            .find(|owner| name.is_none_or(|name| owner.state.name == name))
        {
            return Ok(owner.state.id);
        }
        let name = name.unwrap_or("main");
        if name.trim().is_empty() {
            return Err("a session needs a name".into());
        }
        let mut nui = Nui::default();
        nui.add_session(name, &self.shell, cwd);
        let mut state = nui.sessions.remove(0);
        state.id = self.next_session;
        self.next_session += 1;
        let owner = SessionServer::open(
            self.store.path(),
            state.id,
            Some(state),
            self.events.clone(),
            &self.shell,
            &self.library,
        )?;
        let id = owner.state.id;
        self.catalog.sessions.push(id);
        self.owners.insert(id, owner);
        Ok(id)
    }

    fn reply(&mut self, id: u64, reply: Response) {
        if !self.peers.get(&id).is_some_and(|peer| peer.send(reply)) {
            self.disconnect(id);
        }
    }

    pub fn disconnect(&mut self, id: u64) {
        self.peers.remove(&id);
        self.catalog.clients.retain(|client| client.id != id);
        self.views.retain(|(client, _), _| *client != id);
        self.last_input.retain(|_, client| *client != id);
        if let Some(outer) = self.outer.remove(&id)
            && let Some(tty) = outer.tty
        {
            self.seen.insert(tty, outer.seen);
        }
        self.dirty = true;
        self.persist();
    }

    /// The colors of an instance's terminal answer the programs of its session.
    fn paint(&self, client: u64) {
        if let Some(palette) = self
            .outer
            .get(&client)
            .and_then(|outer| outer.palette.as_ref())
            && let Some(owner) = self
                .catalog
                .client(client)
                .and_then(|client| self.owners.get(&client.session))
        {
            owner.runtime.paint(palette);
        }
    }

    /// Pass on what the programs of a session asked of the terminal around
    /// the person: a copy to the clipboard to the instance that typed there,
    /// the rest to all attached to the session. A bell nobody sees marks its
    /// terminal.
    fn effects(&mut self, session: u64) {
        let Some(owner) = self.owners.get(&session) else {
            return;
        };
        let effects = owner.runtime.take_effects();
        if effects.is_empty() {
            return;
        }
        let viewers: Vec<u64> = self
            .catalog
            .clients
            .iter()
            .filter(|client| client.session == session)
            .map(|client| client.id)
            .collect();
        let mut bell = false;
        for (terminal, effect) in effects {
            let (targets, bytes) = match effect {
                Effect::Bell => {
                    if !self.focused.contains(&(session, terminal)) {
                        owner.runtime.ring(terminal);
                    }
                    bell = true;
                    continue;
                }
                Effect::Copy(bytes) => {
                    let typist = self.typist(session, terminal);
                    (typist.map_or_else(|| viewers.clone(), |id| vec![id]), bytes)
                }
                Effect::Notify(bytes) => (viewers.clone(), bytes),
            };
            for id in targets {
                self.outer.entry(id).or_default().bytes.extend(&bytes);
            }
        }
        if bell {
            for id in &viewers {
                self.outer.entry(*id).or_default().bytes.push(0x07);
            }
        }
        self.dirty = true;
    }

    /// Tell the programs asking when their terminal gains or loses the focus.
    fn focus(&mut self) {
        let focused: HashSet<(u64, u64)> = self
            .catalog
            .clients
            .iter()
            .filter(|client| {
                client.navigation.is_none()
                    && !self
                        .outer
                        .get(&client.id)
                        .is_some_and(|outer| outer.unfocused)
            })
            .filter_map(|client| {
                let session = &self.owners.get(&client.session)?.state;
                Some((client.session, session.active_terminal()?))
            })
            .collect();
        for (gained, list) in [(true, &focused), (false, &self.focused)] {
            let other = if gained { &self.focused } else { &focused };
            for (session, terminal) in list.difference(other) {
                if let Some(owner) = self.owners.get(session) {
                    owner.runtime.focus(*terminal, gained);
                }
            }
        }
        self.focused = focused;
    }

    fn notice(&mut self, client: u64, message: impl Into<String>) {
        let message = message.into();
        if let Some(client) = self.catalog.client_mut(client)
            && client.message.as_ref() != Some(&message)
        {
            client.message = Some(message);
            self.dirty = true;
        }
    }

    fn broadcast(&mut self, message: impl Into<String>) {
        let message = message.into();
        for client in &mut self.catalog.clients {
            if client.message.as_ref() != Some(&message) {
                client.message = Some(message.clone());
                self.dirty = true;
            }
        }
    }

    fn persist(&mut self) {
        if let Err(error) = self.store.save(&self.catalog.encode()) {
            self.broadcast(error);
        }
    }

    /// Route changed sessions by stable identity, not by the client's current
    /// attachment. Looking elsewhere and coming back cannot undo those edits.
    fn commit(&mut self, client: u64, mut next: Nui) -> Result<(), String> {
        next.normalize()?;
        let before = self.catalog.sessions.clone();
        for session in &mut next.sessions {
            if !self.owners.contains_key(&session.id) {
                session.id = self.next_session;
                self.next_session += 1;
            }
        }
        for session in &next.sessions {
            match self.owners.get_mut(&session.id) {
                Some(owner) if owner.state != *session => {
                    owner.commit(session.clone(), &self.library)?
                }
                Some(_) => {}
                None => {
                    let owner = SessionServer::open(
                        self.store.path(),
                        session.id,
                        Some(session.clone()),
                        self.events.clone(),
                        &self.shell,
                        &self.library,
                    )?;
                    self.owners.insert(session.id, owner);
                }
            }
        }
        let wanted: HashSet<u64> = next.sessions.iter().map(|session| session.id).collect();
        self.owners.retain(|id, _| wanted.contains(id));
        self.catalog.sessions = next.sessions.iter().map(|session| session.id).collect();
        self.remap_presence(&before);
        if let Some(presence) = self.catalog.client_mut(client) {
            if let Some(session) = next.sessions.get(next.attached) {
                presence.session = session.id;
            }
            presence.navigation = next.navigation;
            presence.representation = next.representation;
            presence.message = next.message;
        }
        self.cleanup();
        self.persist();
        self.dirty = true;
        Ok(())
    }

    fn remap_presence(&mut self, before: &[u64]) {
        for client in &mut self.catalog.clients {
            if let Some(nav) = &mut client.navigation {
                nav.cursor.session = before
                    .get(nav.cursor.session)
                    .and_then(|id| self.catalog.sessions.iter().position(|known| known == id))
                    .unwrap_or(0);
            }
        }
    }

    fn cleanup(&mut self) {
        let before = self.catalog.sessions.clone();
        self.catalog
            .sessions
            .retain(|id| self.owners.get(id).is_some_and(|owner| !owner.removed));
        self.owners
            .retain(|id, _| self.catalog.sessions.contains(id));
        self.remap_presence(&before);
        let ids: Vec<u64> = self
            .catalog
            .clients
            .iter()
            .map(|client| client.id)
            .collect();
        for id in ids {
            if self.catalog.sessions.is_empty() {
                self.disconnect(id);
                continue;
            }
            let fallback = self
                .catalog
                .client(id)
                .filter(|client| !self.catalog.sessions.contains(&client.session))
                .map(|_| self.catalog.sessions[0]);
            if let Some(session) = fallback {
                let client = self.catalog.client_mut(id).unwrap();
                client.session = session;
                client.navigation = None;
            }
            let mut nui = self.projection(Some(id));
            if nui.normalize().is_ok() {
                self.catalog.client_mut(id).unwrap().navigation = nui.navigation;
            }
        }
        self.views.retain(|(client, session), _| {
            self.catalog.client(*client).is_some() && self.owners.contains_key(session)
        });
        let notices: Vec<String> = self
            .owners
            .values_mut()
            .filter_map(|owner| owner.notice.take())
            .collect();
        for notice in notices {
            self.broadcast(notice);
        }
    }

    pub fn handle(&mut self, event: Event) {
        let result = match event {
            Event::Disconnected(id) => {
                self.disconnect(id);
                Ok(())
            }
            Event::Output(session) => {
                if let Some(owner) = self.owners.get(&session) {
                    owner.runtime.pending.store(false, Ordering::Release);
                }
                self.effects(session);
                self.dirty = true;
                Ok(())
            }
            Event::Exited {
                session,
                id,
                generation,
                code,
            } => {
                self.poll_files();
                if let Some(owner) = self.owners.get_mut(&session) {
                    if owner.runtime.exited(id, generation) {
                        self.dirty = true;
                        let result = owner.fact(Fact::Exited(id, code), &self.library);
                        if owner.state.active_terminal() == Some(id)
                            && owner
                                .state
                                .terminal(id)
                                .is_some_and(|terminal| terminal.status == Status::Exited)
                            && let Some(attached) = self
                                .catalog
                                .sessions
                                .iter()
                                .position(|known| *known == session)
                        {
                            let mut nui = self.projection(None);
                            nui.attached = attached;
                            nav::open(&mut nui);
                            for client in &mut self.catalog.clients {
                                if client.session == session && client.navigation.is_none() {
                                    client.navigation = nui.navigation.clone();
                                }
                            }
                        }
                        result
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                }
            }
            Event::Request(id, request) => {
                if !self.peers.contains_key(&id) {
                    return;
                }
                self.poll_files();
                let result = self.request(id, request);
                if let Err(error) = result {
                    if self.catalog.client(id).is_some() {
                        self.notice(id, error);
                    } else {
                        self.reply(id, Response::Result(Err(error)));
                    }
                }
                Ok(())
            }
        };
        if let Err(error) = result {
            self.broadcast(error);
        }
        self.cleanup();
        self.persist();
    }

    fn request(&mut self, id: u64, request: Request) -> Result<(), String> {
        match request {
            Request::Attach {
                version,
                session,
                profile,
                config,
                cwd,
                size,
                tty,
            } => {
                if self.catalog.client(id).is_some() {
                    return Err("already attached".into());
                }
                if version != ipc::VERSION {
                    return Err("client/server protocol mismatch; stop the old server".into());
                }
                if config != self.library.root {
                    return Err("this catalog's server uses another --config directory".into());
                }
                self.library
                    .effective(profile.as_deref(), &Layer::default())?;
                let render = Render::new(size.0, size.1)?;
                let session = self.ensure_session(session.as_deref(), cwd)?;
                self.catalog.clients.push(Client {
                    id,
                    session,
                    profile,
                    navigation: None,
                    representation: StateRepresentation::Native,
                    message: None,
                });
                self.peers.get_mut(&id).unwrap().render = Some(render);
                let outer = self.outer.entry(id).or_default();
                outer.seen = tty
                    .as_ref()
                    .and_then(|tty| self.seen.remove(tty))
                    .unwrap_or_default();
                outer.tty = tty;
                self.reply(
                    id,
                    Response::Ready {
                        version: ipc::VERSION,
                        client: id,
                    },
                );
                self.dirty = true;
            }
            Request::Input(input) if self.catalog.client(id).is_some() => match input {
                Input::Key(key) => self.key(id, key)?,
                Input::Paste(text) => self.paste(id, &text)?,
                Input::Resize(cols, rows) => {
                    if let Some(render) = self
                        .peers
                        .get_mut(&id)
                        .and_then(|peer| peer.render.as_mut())
                    {
                        render.resize(rows, cols)?;
                    }
                    self.dirty = true;
                    self.draw();
                }
                Input::FocusGained | Input::FocusLost => {
                    let outer = self.outer.entry(id).or_default();
                    outer.unfocused = input == Input::FocusLost;
                    if outer.unfocused {
                        outer.escapes.reset();
                    }
                    self.dirty = true;
                }
                _ => {}
            },
            Request::Input(_) => return Err("attach before sending input".into()),
            Request::Keyboard(flags) if self.catalog.client(id).is_some() => {
                let escapes = &mut self.outer.entry(id).or_default().escapes;
                escapes.reset();
                escapes.events = flags & 2 != 0;
            }
            Request::Keyboard(_) => return Err("attach before telling keyboard flags".into()),
            Request::Colors(palette) if self.catalog.client(id).is_some() => {
                self.outer.entry(id).or_default().palette = Some(palette);
                self.paint(id);
            }
            Request::Colors(_) => return Err("attach before telling colors".into()),
            Request::Switch {
                session,
                from_session,
                terminal,
                cwd,
            } => {
                let client = self.typist(from_session, terminal).ok_or(
                    "no connected instance typed into this terminal; use attach from outside",
                )?;
                let target = match session {
                    Some(name) => self.ensure_session(Some(&name), cwd)?,
                    None => from_session,
                };
                let presence = self
                    .catalog
                    .client_mut(client)
                    .ok_or("instance disconnected")?;
                presence.session = target;
                presence.navigation = None;
                self.reply(id, Response::Result(Ok("instance switched".into())));
                self.dirty = true;
            }
            Request::Call {
                name,
                from_session,
                terminal,
            } => {
                let client = self
                    .typist(from_session, terminal)
                    .ok_or("no connected instance typed into this terminal")?;
                let (_, decoration) = self.decoration(client)?;
                if !decoration.calls("panel", &name) {
                    return Err(format!(
                        "{name} is not a command of this instance; see commands in decoration"
                    ));
                }
                let mut next = self.projection(Some(client));
                if next.navigation.is_some() {
                    nav::close(&mut next);
                } else {
                    nav::open(&mut next);
                }
                self.commit(client, next)?;
                self.reply(id, Response::Result(Ok(String::new())));
            }
            Request::List => self.reply(id, Response::Result(Ok(self.list()))),
            Request::Reload => {
                // Parse everything first; a broken reload keeps the previous snapshot.
                let library = Library::load(self.library.root.clone())?;
                for client in &self.catalog.clients {
                    library.effective(
                        client.profile.as_deref(),
                        &self.owners[&client.session].layer,
                    )?;
                }
                self.library = library;
                self.views.clear();
                for owner in self.owners.values_mut() {
                    owner.reload();
                    owner.write_commands(&self.library);
                    owner.commit(owner.state.clone(), &self.library)?;
                }
                self.reply(id, Response::Result(Ok("decoration reloaded".into())));
                self.dirty = true;
            }
            Request::Kill { apply } => {
                let text = if apply {
                    "stopping this catalog's servers and all its clients".into()
                } else {
                    format!(
                        "would stop {} session server(s), {} connected instance(s); add --apply",
                        self.owners.len(),
                        self.catalog.clients.len()
                    )
                };
                self.reply(id, Response::Result(Ok(text)));
                self.quit |= apply;
            }
        }
        Ok(())
    }

    /// The connected instance that typed into a terminal last.
    fn typist(&self, session: u64, terminal: u64) -> Option<u64> {
        self.last_input
            .get(&(session, terminal))
            .copied()
            .filter(|id| self.catalog.client(*id).is_some())
    }

    fn input_owner(&mut self, session: u64, terminal: u64, client: u64) {
        if self.last_input.insert((session, terminal), client) != Some(client) {
            // The colors and size of the instance typing are the ones of the
            // session. Fit the PTY BEFORE forwarding this client's first bytes:
            // otherwise a command such as `stty size` can race the next draw.
            self.paint(client);
            self.dirty = true;
            self.draw();
        }
    }

    fn key(&mut self, client: u64, key: KeyEvent) -> Result<(), String> {
        let (session, decoration) = self.decoration(client)?;
        let mut next = self.projection(Some(client));
        match self.outer.entry(client).or_default().escapes.key(
            &key,
            next.navigation.is_none(),
            decoration.gestures,
            Instant::now(),
        ) {
            Action::Open => {
                nav::open(&mut next);
                return self.commit(client, next);
            }
            Action::Consume => return Ok(()),
            Action::Pass => {}
        }
        if key.kind == KeyEventKind::Release {
            return Ok(());
        }
        let Some(notation) = keys::notation(&key) else {
            return Ok(());
        };
        if decoration.binds("panel", &notation) {
            if next.navigation.is_some() {
                nav::close(&mut next);
            } else {
                nav::open(&mut next);
            }
            return self.commit(client, next);
        }
        // plugin keys of navigation act on the native representation
        if next.navigation.as_ref().is_none_or(|nav| {
            nav.operation.is_none()
                && nav.search.is_none()
                && next.representation == StateRepresentation::Native
        }) {
            let plugins = self
                .views
                .entry((client, session))
                .or_insert_with(|| Plugins::new(self.shell.clone()));
            plugins.sync(&decoration.plugins, self.library.root.as_path());
            plugins.client = Some(ClientContext {
                id: client,
                session,
                profile: self
                    .catalog
                    .client(client)
                    .and_then(|client| client.profile.clone()),
            });
            let mode = if next.navigation.is_some() {
                "navigate"
            } else {
                "view"
            };
            if let Some(result) = plugins.key(mode, &notation, &next) {
                return self.commit(client, result?);
            }
        }
        if let Some(navigation) = &next.navigation {
            let translated = if navigation.operation.is_some()
                || navigation
                    .search
                    .as_ref()
                    .is_some_and(|search| search.editing)
            {
                Some(key)
            } else {
                decoration.navigation_key(&notation)
            };
            if let Some(key) = translated {
                match nav::key(&mut next, &key, &self.shell) {
                    nav::Outcome::Quit => {
                        self.disconnect(client);
                        return Ok(());
                    }
                    nav::Outcome::Shutdown => {
                        self.quit = true;
                        return Ok(());
                    }
                    nav::Outcome::Stay => {}
                }
                self.commit(client, next)?;
            }
        } else if let Some(at) = next.location()
            && let Some(terminal) = next.shown(at)
        {
            let session = next.sessions[at.session].id;
            self.input_owner(session, terminal.id, client);
            if terminal.status == Status::Running {
                let runtime = &self.owners[&session].runtime;
                let mode = runtime.keys(terminal.id);
                runtime.write(terminal.id, &keys::encode(&key, mode));
                self.dirty = true;
            } else if decoration.binds("restart", &notation) {
                next.enter(at);
                self.commit(client, next)?;
            }
        }
        Ok(())
    }

    fn paste(&mut self, client: u64, text: &str) -> Result<(), String> {
        self.outer.entry(client).or_default().escapes.reset();
        let mut next = self.projection(Some(client));
        if next.navigation.is_some() {
            nav::paste(&mut next, text);
            return self.commit(client, next);
        }
        let Some(at) = next.location() else {
            return Ok(());
        };
        let Some(terminal) = next
            .shown(at)
            .filter(|terminal| terminal.status == Status::Running)
        else {
            return Ok(());
        };
        let session = next.sessions[at.session].id;
        self.input_owner(session, terminal.id, client);
        let owner = &self.owners[&session];
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = owner
            .runtime
            .screen(terminal.id)
            .is_some_and(|screen| lock(screen).screen().bracketed_paste());
        let text = if bracketed {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text
        };
        owner.runtime.write(terminal.id, text.as_bytes());
        self.dirty = true;
        Ok(())
    }

    fn read_catalog(&mut self, text: &str) -> Result<(), String> {
        let mut catalog = Catalog::decode(text)?;
        if catalog
            .clients
            .iter()
            .any(|client| self.catalog.client(client.id).is_none())
        {
            return Err(
                "client nodes are created by connections, not by adding them to the file".into(),
            );
        }
        let mut added = HashMap::new();
        for id in &catalog.sessions {
            if !self.owners.contains_key(id) {
                added.insert(
                    *id,
                    SessionServer::open(
                        self.store.path(),
                        *id,
                        None,
                        self.events.clone(),
                        &self.shell,
                        &self.library,
                    )?,
                );
            }
        }
        let mut nui = Nui {
            sessions: catalog
                .sessions
                .iter()
                .map(|id| {
                    self.owners
                        .get(id)
                        .or_else(|| added.get(id))
                        .unwrap()
                        .state
                        .clone()
                })
                .collect(),
            ..Nui::default()
        };
        nui.normalize()?;
        for client in &catalog.clients {
            let owner = self
                .owners
                .get(&client.session)
                .or_else(|| added.get(&client.session))
                .unwrap();
            self.library
                .effective(client.profile.as_deref(), &owner.layer)?;
        }
        let deleted: Vec<u64> = self
            .catalog
            .clients
            .iter()
            .filter(|client| catalog.client(client.id).is_none())
            .map(|client| client.id)
            .collect();
        for id in deleted {
            self.disconnect(id);
        }
        catalog
            .clients
            .retain(|client| self.peers.contains_key(&client.id));
        self.owners.extend(added);
        self.owners.retain(|id, _| catalog.sessions.contains(id));
        self.next_session = self
            .next_session
            .max(catalog.sessions.iter().copied().max().unwrap_or(0) + 1);
        self.catalog = catalog;
        self.dirty = true;
        Ok(())
    }

    /// Always consume file edits before applying the next interactive mutation.
    fn poll_files(&mut self) {
        if let Some(read) = self.store.poll()
            && let Err(error) = read.and_then(|text| self.read_catalog(&text))
        {
            self.store.reject();
            self.broadcast(format!("global state: {error}"));
        }
        let ids = self.catalog.sessions.clone();
        for id in ids {
            let read = self.owners.get_mut(&id).and_then(SessionServer::poll_state);
            if let Some(read) = read {
                let result = read.and_then(|state| {
                    let mut next = self.projection(None);
                    if let Some(session) = next.sessions.iter_mut().find(|session| session.id == id)
                    {
                        *session = state.clone();
                    }
                    next.normalize()?;
                    self.owners
                        .get_mut(&id)
                        .unwrap()
                        .commit(state, &self.library)
                });
                if let Err(error) = result {
                    self.owners.get_mut(&id).unwrap().store.reject();
                    self.broadcast(format!("session {id}: {error}"));
                }
                self.dirty = true;
            }
            if self
                .owners
                .get_mut(&id)
                .is_some_and(SessionServer::poll_decoration)
            {
                self.views.retain(|(_, session), _| *session != id);
                if let Some(owner) = self.owners.get_mut(&id) {
                    owner.write_commands(&self.library);
                    if let Err(error) = owner.commit(owner.state.clone(), &self.library) {
                        self.broadcast(error);
                    }
                }
                self.dirty = true;
            }
        }
        self.cleanup();
    }

    /// Hold timers run at the input-loop cadence, not the slower file/PTY tick.
    pub fn poll_gestures(&mut self) {
        let now = Instant::now();
        let pending: Vec<u64> = self
            .outer
            .iter()
            .filter(|(_, outer)| outer.escapes.waiting())
            .map(|(id, _)| *id)
            .collect();
        for id in pending {
            if self
                .catalog
                .client(id)
                .is_none_or(|client| client.navigation.is_some())
            {
                continue;
            }
            let Ok((_, decoration)) = self.decoration(id) else {
                continue;
            };
            if self
                .outer
                .get_mut(&id)
                .unwrap()
                .escapes
                .poll(decoration.gestures, now)
            {
                let mut next = self.projection(Some(id));
                nav::open(&mut next);
                if let Err(error) = self.commit(id, next) {
                    self.notice(id, error);
                }
            }
        }
    }

    pub fn tick(&mut self) {
        self.poll_files();
        let mut errors = Vec::new();
        let mut released = Vec::new();
        for (id, owner) in &mut self.owners {
            match owner.observe(&self.library) {
                Ok(changed) => self.dirty |= changed,
                Err(error) => errors.push(error),
            }
            if owner.runtime.release() {
                released.push(*id);
            }
        }
        for session in released {
            self.effects(session);
            self.dirty = true;
        }
        for error in errors {
            self.broadcast(error);
        }
        self.cleanup();
        self.persist();
    }

    pub fn draw(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.focus();
        let mut waiting = false;
        let clients: Vec<u64> = self
            .catalog
            .clients
            .iter()
            .map(|client| client.id)
            .collect();
        for client in clients {
            let Some(mut render) = self
                .peers
                .get_mut(&client)
                .and_then(|peer| peer.render.take())
            else {
                continue;
            };
            let nui = self.projection(Some(client));
            let Ok((session, decoration)) = self.decoration(client) else {
                continue;
            };
            let (switched, more) = match self.watch(client, &nui, &mut render) {
                Ok(watched) => watched,
                Err(error) => {
                    eprintln!("client {client}: render: {error}");
                    self.disconnect(client);
                    continue;
                }
            };
            waiting |= more;
            let plugins = self
                .views
                .entry((client, session))
                .or_insert_with(|| Plugins::new(self.shell.clone()));
            plugins.sync(&decoration.plugins, self.library.root.as_path());
            plugins.theme = decoration.lua_theme();
            let mut place = |name: &str, container: &Container| {
                plugins
                    .layout(name, container)
                    .unwrap_or_else(|| layout::tabs(container))
            };
            let result = if nui.navigation.is_some() {
                let screens = SessionScreens {
                    order: &self.catalog.sessions,
                    owners: &self.owners,
                };
                render.draw(|frame| ui::navigation(frame, &nui, &screens, &decoration))
            } else {
                let root = Location {
                    session: nui.attached,
                    ..Location::default()
                };
                let area = render.terminal.get_frame().area();
                let scene = layout::scene(&nui, Level::Session, root, area, &mut place);
                let mut sized = HashSet::new();
                for shown in scene
                    .panes
                    .iter()
                    .filter(|shown| Some(shown.at) == nui.location())
                    .chain(&scene.panes)
                {
                    if !sized.insert(shown.terminal) {
                        continue;
                    }
                    let session = nui.sessions[shown.at.session].id;
                    let owner = self
                        .last_input
                        .entry((session, shown.terminal))
                        .or_insert(client);
                    if *owner == client {
                        self.owners.get_mut(&session).unwrap().runtime.resize(
                            shown.terminal,
                            shown.area.height,
                            shown.area.width,
                        );
                    }
                }
                let screens = SessionScreens {
                    order: &self.catalog.sessions,
                    owners: &self.owners,
                };
                render.draw(|frame| ui::view(frame, &nui, &screens, &scene))
            };
            let notice = plugins.take_notices().pop();
            if let Some(notice) = notice {
                self.notice(client, notice);
            }
            if let Err(error) = result {
                eprintln!("client {client}: render: {error}");
                self.disconnect(client);
                continue;
            }
            let title = self.title(&nui);
            let outer = self.outer.entry(client).or_default();
            let mut bytes = std::mem::take(&mut outer.bytes);
            if let Some(title) = title.filter(|title| outer.title.as_ref() != Some(title)) {
                bytes.extend(format!("\x1b]2;{title}\x07").into_bytes());
                outer.title = Some(title);
            }
            bytes.extend(render.take());
            if let Some(peer) = self.peers.get_mut(&client) {
                peer.render = Some(render);
            }
            if let Some(alternate) = switched {
                self.reply(client, Response::Screen(alternate));
            }
            if !bytes.is_empty() {
                self.reply(client, Response::Frame(bytes));
            }
        }
        // the rest of a long history goes with the next drawing
        self.dirty |= waiting;
        self.persist();
    }

    /// Keep the terminal around an instance as the terminal it watches would
    /// be there: on the screen it shows, with what its main screen scrolled
    /// away in the history, so the scrolling and search of the terminal
    /// around reach it. Returns the screen switched to, and whether more of
    /// the history waits.
    fn watch(
        &mut self,
        client: u64,
        nui: &Nui,
        render: &mut Render,
    ) -> Result<(Option<bool>, bool), String> {
        let watched = nui
            .location()
            .and_then(|at| Some((nui.sessions.get(at.session)?.id, nui.pane(at)?.terminal)));
        let parser = watched
            .and_then(|(session, terminal)| self.owners.get(&session)?.runtime.screen(terminal))
            .map(|screen| lock(screen));
        let alternate = parser
            .as_ref()
            .is_some_and(|parser| parser.screen().alternate_screen());
        let switched = (render.alternate() != alternate).then_some(alternate);
        if switched.is_some() {
            render.switch(alternate)?;
        }
        // the alternate screen keeps no history: the rest waits for the main one
        let (Some(parser), Some(watched), false) = (parser, watched, alternate) else {
            return Ok((switched, false));
        };
        let history = &parser.callbacks().history;
        let seen = &mut self.outer.entry(client).or_default().seen;
        let (cleared, after, rows) = history.since(seen.get(&watched).copied());
        if cleared {
            render.forget();
        }
        let count = after + render.push(rows) as u64;
        seen.insert(watched, history.mark(count));
        Ok((switched, count < history.count()))
    }

    /// The title for the terminal around an instance: the one the program of
    /// its focused pane set, or the name of the pane. None in navigation.
    fn title(&self, nui: &Nui) -> Option<String> {
        let at = nui.location().filter(|_| nui.navigation.is_none())?;
        let id = nui.pane(at)?.terminal;
        let set = self
            .catalog
            .sessions
            .get(at.session)
            .and_then(|session| self.owners.get(session)?.runtime.screen(id))
            .and_then(|screen| lock(screen).callbacks().title.clone());
        let title = set.unwrap_or_else(|| nui.label(Level::Pane, at));
        Some(
            title
                .chars()
                .filter(|c| !c.is_control())
                .take(256)
                .collect(),
        )
    }

    pub fn list(&self) -> String {
        describe(&self.projection(None).sessions, &self.catalog.clients)
    }

    pub fn shutdown(&mut self) {
        for owner in self.owners.values_mut() {
            owner.shutdown();
        }
        self.catalog.clients.clear();
        self.persist();
        for peer in self.peers.values_mut() {
            peer.finish();
        }
    }
}

pub fn describe(sessions: &[Session], clients: &[Client]) -> String {
    sessions.iter().map(|session| {
        let windows = session.windows().count();
        let panes: usize = session.windows().map(|window| window.panes.len()).sum();
        let connected = clients.iter().filter(|client| client.session == session.id).count();
        format!("{}  id: {}  workspaces: {}  windows: {windows}  panes: {panes}  terminals: {}  clients: {connected}\n", session.name, session.id, session.workspaces.len(), session.terminals.len())
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn default_legacy_import_respects_its_lock_and_leaves_the_original_untouched() {
        let dir =
            std::env::temp_dir().join(format!("nui-default-migration-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("state.kdl");
        let global = dir.join("global-state.kdl");
        // the shape of the old application: workspaces were windows of tabs
        let original = "toggle ctrl-a\nplugin missing.lua\nsession work { terminal 1 command=\"/bin/sh\"; window { tab { pane terminal=1 } } }";
        fs::write(&legacy, original).unwrap();
        let mut old = Store::new(legacy.clone());
        assert!(old.lock().unwrap());
        let open = || {
            let mut store = Store::new(global.clone());
            assert!(store.lock().unwrap());
            let (events, _) = mpsc::channel();
            App::open(
                store,
                Library::load(dir.join("config")).unwrap(),
                events,
                "/bin/sh".into(),
                Some(&legacy),
            )
        };
        assert!(open().err().unwrap().contains("old application"));
        assert!(!global.exists());
        drop(old);
        let mut app = open().unwrap();
        assert_eq!(app.catalog.sessions, [1]);
        assert_eq!(app.projection(None).sessions[0].name, "work");
        assert!(app.catalog.clients.is_empty());
        app.shutdown();
        drop(app);
        assert_eq!(fs::read_to_string(&legacy).unwrap(), original);
        let session = fs::read_to_string(crate::global::session_path(&global, 1)).unwrap();
        assert!(
            !session.contains("toggle") && !session.contains("plugin") && !session.contains("pid=")
        );
        assert!(
            session.contains("workspace {") && !session.contains("tab"),
            "{session}"
        );
        let _ = fs::remove_dir_all(dir);
    }
}
