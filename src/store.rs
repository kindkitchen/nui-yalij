//! Atomic, single-writer KDL files. Codecs belong to the state they describe.
//! An unseen or invalid external edit is never overwritten.

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions, TryLockError},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::SystemTime,
};

pub struct Store {
    path: PathBuf,
    known: Option<(Stamp, String)>,
    held: bool,
    lock: Option<File>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Stamp {
    modified: SystemTime,
    len: u64,
}

pub fn mkdir(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("{}: {error}", path.display()))
}

pub fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    name.into()
}

impl Store {
    pub fn new(path: PathBuf) -> Store {
        Store {
            path,
            known: None,
            held: false,
            lock: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    pub fn load(&mut self) -> Result<Option<String>, String> {
        match fs::read_to_string(&self.path) {
            Ok(text) => {
                self.known = self.stamp().map(|stamp| (stamp, text.clone()));
                Ok(Some(text))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{}: {error}", self.path.display())),
        }
    }

    /// Kept for the lifetime of the owner, not for a client attachment.
    pub fn lock(&mut self) -> Result<bool, String> {
        mkdir(self.dir())?;
        let path = suffixed(&self.path, ".lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {
                self.lock = Some(file);
                Ok(true)
            }
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(error)) => Err(error.to_string()),
        }
    }

    /// A codec rejected the last read. Keep it until someone fixes the file.
    pub fn reject(&mut self) {
        self.held = true;
    }

    pub fn save(&mut self, text: &str) -> Result<(), String> {
        let known = self.known.as_ref();
        if self.held
            || known.is_some_and(|(_, known)| known == text)
            || self.stamp() != known.map(|(stamp, _)| *stamp)
        {
            return Ok(());
        }
        mkdir(self.dir())?;
        let aside = suffixed(&self.path, &format!(".{}.tmp", std::process::id()));
        let result = (|| {
            use std::io::Write;
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&aside)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(&aside, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&aside);
        }
        result.map_err(|error| format!("{}: {error}", self.path.display()))?;
        self.known = self.stamp().map(|stamp| (stamp, text.to_owned()));
        Ok(())
    }

    pub fn poll(&mut self) -> Option<Result<String, String>> {
        if self.stamp() == self.known.as_ref().map(|(stamp, _)| *stamp) {
            return None;
        }
        let previous = self.known.as_ref().map(|(_, text)| text.clone());
        match self.load() {
            Ok(None) => {
                self.known = None;
                self.held = false;
                None
            }
            Ok(Some(text)) if previous.as_ref() == Some(&text) => None,
            Ok(Some(text)) => {
                self.held = false;
                Some(Ok(text))
            }
            Err(error) => {
                self.held = true;
                Some(Err(error))
            }
        }
    }

    fn stamp(&self) -> Option<Stamp> {
        let metadata = fs::metadata(&self.path).ok()?;
        Some(Stamp {
            modified: metadata.modified().ok()?,
            len: metadata.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unseen_and_invalid_edits_are_preserved() {
        let dir = std::env::temp_dir().join(format!("nui-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut store = Store::new(dir.join("state.kdl"));
        assert_eq!(store.load(), Ok(None));
        store.save("session main\n").unwrap();
        assert!(store.poll().is_none());
        fs::write(store.path(), "session work\n").unwrap();
        store.save("message mine\n").unwrap();
        assert_eq!(store.poll().unwrap().unwrap(), "session work\n");
        fs::write(store.path(), "session {").unwrap();
        store.poll().unwrap().unwrap();
        store.reject();
        store.save("session main\n").unwrap();
        assert_eq!(fs::read_to_string(store.path()).unwrap(), "session {");
        fs::write(store.path(), "session fixed\n").unwrap();
        assert_eq!(store.poll().unwrap().unwrap(), "session fixed\n");
        store.save("session main\n").unwrap();
        assert_eq!(fs::read_to_string(store.path()).unwrap(), "session main\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn only_one_owner_even_before_the_first_save() {
        let dir = std::env::temp_dir().join(format!("nui-lock-{}", std::process::id()));
        let mut first = Store::new(dir.join("state.kdl"));
        let mut second = Store::new(dir.join("state.kdl"));
        assert_eq!(first.lock(), Ok(true));
        assert_eq!(second.lock(), Ok(false));
        drop(first);
        assert_eq!(second.lock(), Ok(true));
        let _ = fs::remove_dir_all(dir);
    }
}
