//! The state file: written after every change, read again when someone else
//! changes it. The file wins: a change from outside is taken before writing.

use std::{
    ffi::OsString,
    fs::{self, File, TryLockError},
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{kdl, state::Nui};

pub struct Store {
    path: PathBuf,
    /// The file as last written or read: its stamp and text.
    known: Option<(Stamp, String)>,
    /// The file holds a broken edit from outside: it isn't overwritten.
    held: bool,
    /// The lock of the running instance, held while it lives.
    lock: Option<File>,
}

/// What tells a changed file.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Stamp {
    modified: SystemTime,
    len: u64,
}

impl Store {
    pub fn new(path: PathBuf) -> Store {
        Store { path, known: None, held: false, lock: None }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where relative paths of the state start.
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// The state in the file; none when there is no file yet.
    pub fn load(&mut self) -> Result<Option<Nui>, String> {
        let Some(stamp) = self.stamp() else { return Ok(None) };
        let text = fs::read_to_string(&self.path).map_err(|error| format!("{}: {error}", self.path.display()))?;
        let mut nui = kdl::decode(&text).map_err(|error| format!("{}: {error}", self.path.display()))?;
        nui.normalize().map_err(|error| format!("{}: {error}", self.path.display()))?;
        self.known = Some((stamp, text));
        Ok(Some(nui))
    }

    /// Hold the state for this instance: false when another one holds it.
    pub fn lock(&mut self) -> Result<bool, String> {
        let mut path = OsString::from(self.path.as_os_str());
        path.push(".lock");
        let error = |error: std::io::Error| format!("{}: {error}", Path::new(&path).display());
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(error)?;
        }
        let file = File::create(&path).map_err(error)?;
        match file.try_lock() {
            Ok(()) => {
                self.lock = Some(file);
                Ok(true)
            }
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(error)) => Err(error.to_string()),
        }
    }

    /// Write the state, unless the file holds it already, or holds a change
    /// from outside not read yet.
    pub fn save(&mut self, nui: &Nui) -> Result<(), String> {
        let text = kdl::encode(nui);
        let known = self.known.as_ref();
        if self.held || known.is_some_and(|(_, known)| *known == text) || self.stamp() != known.map(|(stamp, _)| *stamp) {
            return Ok(());
        }
        let error = |error: std::io::Error| format!("{}: {error}", self.path.display());
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(error)?;
        }
        // write aside, then swap: a reader never sees half a state
        let mut aside = OsString::from(self.path.as_os_str());
        aside.push(".tmp");
        fs::write(&aside, &text).map_err(error)?;
        fs::rename(&aside, &self.path).map_err(error)?;
        self.known = self.stamp().map(|stamp| (stamp, text));
        Ok(())
    }

    /// The state from the file, when someone else changed it; an error when
    /// that change is broken, and the file is kept until fixed.
    pub fn poll(&mut self) -> Option<Result<Nui, String>> {
        let stamp = self.stamp();
        if stamp == self.known.as_ref().map(|(stamp, _)| *stamp) {
            return None;
        }
        let Some(stamp) = stamp else {
            // gone: written anew with the next change
            self.known = None;
            self.held = false;
            return None;
        };
        let text = fs::read_to_string(&self.path).ok()?;
        let unchanged = self.known.as_ref().is_some_and(|(_, known)| *known == text);
        self.known = Some((stamp, text.clone()));
        if unchanged {
            return None;
        }
        let result = kdl::decode(&text).and_then(|mut nui| nui.normalize().map(|()| nui));
        self.held = result.is_err();
        Some(result)
    }

    fn stamp(&self) -> Option<Stamp> {
        let metadata = fs::metadata(&self.path).ok()?;
        Some(Stamp { modified: metadata.modified().ok()?, len: metadata.len() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::sample;

    #[test]
    fn the_file_wins() {
        let dir = std::env::temp_dir().join(format!("nui-yalij-store-{}", std::process::id()));
        let path = dir.join("state.kdl");
        let _ = fs::remove_dir_all(&dir);
        let mut store = Store::new(path.clone());
        assert_eq!(store.load(), Ok(None));
        let mut nui = sample();
        store.save(&nui).unwrap();
        assert!(store.poll().is_none());
        // someone else renames the session
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, fs::read_to_string(&path).unwrap().replace("\"main\"", "\"work\"").replace("main", "work")).unwrap();
        // not overwritten before it is read
        nui.message = Some("mine".into());
        store.save(&nui).unwrap();
        let read = store.poll().unwrap().unwrap();
        assert_eq!(read.sessions[0].name, "work");
        // a broken edit is held, not overwritten
        fs::write(&path, "session {").unwrap();
        assert!(store.poll().unwrap().is_err());
        store.save(&nui).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "session {");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_instance_holds_the_lock() {
        let dir = std::env::temp_dir().join(format!("nui-yalij-lock-{}", std::process::id()));
        let mut first = Store::new(dir.join("state.kdl"));
        let mut second = Store::new(dir.join("state.kdl"));
        assert_eq!(first.lock(), Ok(true));
        assert_eq!(second.lock(), Ok(false));
        drop(first);
        assert_eq!(second.lock(), Ok(true));
        let _ = fs::remove_dir_all(&dir);
    }
}
