//! One serialized owner per session: its KDL state, PTYs and automatic hooks.
//! Owners share a coordinator process/transport, not state or PTY runtimes.

use std::{
    collections::BTreeSet, fs, os::unix::fs::PermissionsExt, path::Path, sync::mpsc::Sender,
};

use crate::{
    app::Event,
    decoration::{Layer, Library},
    global, kdl,
    plugin::Plugins,
    runtime::Runtime,
    state::{Fact, Nui, Session, Status},
    store::{Store, mkdir},
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
        runtime.config = Some(library.root.clone());
        runtime.bin = Some(store.dir().join("bin"));
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
        server.write_commands(library);
        server.commit(state, library)?;
        Ok(server)
    }

    /// Write the commands of decoration where the terminals of the session
    /// find them first, on their PATH: each a script calling this program.
    pub fn write_commands(&mut self, library: &Library) {
        let dir = self.store.dir().join("bin");
        if let Err(error) = commands(&dir, &library.commands(&self.layer)) {
            self.notice = Some(format!("commands: {error}"));
        }
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

/// The scripts of the commands `names` in `dir`, and nothing else.
fn commands(dir: &Path, names: &BTreeSet<String>) -> Result<(), String> {
    let program = std::env::current_exe().map_err(|error| error.to_string())?;
    let program = program
        .to_str()
        .ok_or("non-UTF8 program path")?
        .replace('\'', r"'\''");
    mkdir(dir)?;
    let failed = |path: &Path, error: std::io::Error| format!("{}: {error}", path.display());
    for entry in fs::read_dir(dir).map_err(|error| failed(dir, error))? {
        let path = entry.map_err(|error| failed(dir, error))?.path();
        let known = path.file_name().and_then(|name| name.to_str());
        if !known.is_some_and(|name| names.contains(name)) {
            fs::remove_file(&path).map_err(|error| failed(&path, error))?;
        }
    }
    for name in names {
        let path = dir.join(name);
        // the names are plain: no quote to escape
        let script = format!(
            "#!/bin/sh\n# nui-yalij: a command of decoration, inside the terminals of a session\nexec '{program}' call '{name}'\n"
        );
        if fs::read_to_string(&path).is_ok_and(|known| known == script) {
            continue;
        }
        fs::write(&path, script)
            .and_then(|()| fs::set_permissions(&path, fs::Permissions::from_mode(0o700)))
            .map_err(|error| failed(&path, error))?;
    }
    Ok(())
}

impl Drop for SessionServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}
