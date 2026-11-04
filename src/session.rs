//! One serialized owner per session: its KDL state, PTYs and automatic hooks.
//! Owners share a coordinator process/transport, not state or PTY runtimes.

use std::{path::Path, sync::mpsc::Sender};

use crate::{
    app::Event,
    decoration::{Layer, Library},
    global, kdl,
    plugin::Plugins,
    runtime::Runtime,
    state::{Fact, Nui, Session, Status},
    store::Store,
};

pub struct SessionServer {
    pub state: Session,
    pub runtime: Runtime,
    pub layer: Layer,
    pub removed: bool,
    pub notice: Option<String>,
    pub store: Store,
    decoration: Store,
    hooks: Plugins,
}

pub fn decode(text: &str, id: u64) -> Result<Session, String> {
    let mut nui = kdl::decode(text)?;
    nui.normalize()?;
    if nui.sessions.len() != 1 || nui.sessions[0].id != id {
        return Err(format!("expected exactly session id {id}"));
    }
    Ok(nui.sessions.remove(0))
}

impl SessionServer {
    pub fn open(
        global: &Path,
        id: u64,
        initial: Option<Session>,
        events: Sender<Event>,
        shell: &str,
        library: &Library,
    ) -> Result<Self, String> {
        let mut store = Store::new(global::session_path(global, id));
        if !store.lock()? {
            return Err(format!("session {id} already has a server owner"));
        }
        let state = match store.load()? {
            Some(text) => decode(&text, id)?,
            None => initial
                .ok_or_else(|| format!("session {id}: {} is missing", store.path().display()))?,
        };
        let mut decoration = Store::new(store.dir().join("decoration.kdl"));
        let layer = decoration
            .load()?
            .map(|text| Layer::decode(&text, decoration.dir()))
            .transpose()?
            .unwrap_or_default();
        let mut runtime = Runtime::new(events, (24, 80));
        runtime.owner = id;
        runtime.global = Some(global.to_path_buf());
        let mut server = Self {
            state: state.clone(),
            runtime,
            layer,
            removed: false,
            notice: None,
            store,
            decoration,
            hooks: Plugins::new(shell.into()),
        };
        server.commit(state, library)?;
        Ok(server)
    }

    /// Called only by the coordinator's event loop. No client writes state files.
    pub fn commit(&mut self, next: Session, library: &Library) -> Result<(), String> {
        let id = self.state.id;
        if next.id != id {
            return Err("a session's identity cannot change".into());
        }
        let mut nui = Nui {
            sessions: vec![next],
            ..Nui::default()
        };
        for _ in 0..4 {
            nui.normalize()?;
            self.hooks
                .sync(&library.hooks(&self.layer), self.store.dir());
            let hooked = self
                .hooks
                .update(nui.clone())
                .and_then(|mut hooked| hooked.normalize().map(|()| hooked));
            match hooked {
                Ok(next) if next.sessions.is_empty() => {
                    self.removed = true;
                    self.runtime.shutdown();
                    return Ok(());
                }
                Ok(next) if next.sessions.len() == 1 && next.sessions[0].id == id => nui = next,
                Ok(_) => {
                    self.notice = Some("update hooks may only change their own session".into())
                }
                Err(error) => self.notice = Some(error),
            }
            self.notice = self
                .hooks
                .take_notices()
                .pop()
                .or(nui.message.take())
                .or(self.notice.take());
            self.state = nui.sessions[0].clone();
            let facts = self.runtime.reconcile(&nui);
            if facts.is_empty() {
                break;
            }
            for fact in facts {
                nui.apply(fact);
            }
            self.state = nui.sessions[0].clone();
        }
        self.store.save(&kdl::encode_session(&self.state))
    }

    pub fn fact(&mut self, fact: Fact, library: &Library) -> Result<(), String> {
        let mut nui = Nui {
            sessions: vec![self.state.clone()],
            ..Nui::default()
        };
        nui.apply(fact);
        if nui.sessions[0] != self.state {
            self.commit(nui.sessions.remove(0), library)?;
        }
        Ok(())
    }

    pub fn poll_state(&mut self) -> Option<Result<Session, String>> {
        let result = self
            .store
            .poll()?
            .and_then(|text| decode(&text, self.state.id));
        if result.is_err() {
            self.store.reject();
        }
        Some(result)
    }

    pub fn poll_decoration(&mut self) -> bool {
        // Deleting a session override restores the inherited decoration.
        let deleted = !self.decoration.path().exists();
        let read = self.decoration.poll();
        let next = if deleted {
            Some(Ok(Layer::default()))
        } else {
            read.map(|result| result.and_then(|text| Layer::decode(&text, self.decoration.dir())))
        };
        match next {
            Some(Ok(layer)) if layer != self.layer => {
                self.layer = layer;
                self.hooks.reload();
                true
            }
            Some(Err(error)) => {
                self.decoration.reject();
                self.notice = Some(format!("session decoration: {error}"));
                false
            }
            _ => false,
        }
    }

    pub fn reload(&mut self) {
        self.hooks.reload();
    }

    pub fn observe(&mut self, library: &Library) -> Result<bool, String> {
        let ids: Vec<u64> = self
            .state
            .terminals
            .iter()
            .filter(|terminal| terminal.status == Status::Running)
            .map(|terminal| terminal.id)
            .collect();
        let mut nui = Nui {
            sessions: vec![self.state.clone()],
            ..Nui::default()
        };
        for id in ids {
            if let Some(seen) = self.runtime.observe(id) {
                nui.apply(Fact::Observed(id, seen));
            }
        }
        let changed = nui.sessions[0] != self.state;
        if changed {
            self.commit(nui.sessions.remove(0), library)?;
        }
        Ok(changed)
    }

    pub fn shutdown(&mut self) {
        self.runtime.shutdown();
        for terminal in &mut self.state.terminals {
            terminal.pid = None;
        }
        if !self.removed {
            let _ = self.store.save(&kdl::encode_session(&self.state));
        }
    }
}

impl Drop for SessionServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}
