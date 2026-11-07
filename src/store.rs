//! Atomic, single-writer KDL files. Codecs belong to the state they describe.
//! An unseen or invalid external edit is never overwritten.

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions, TryLockError},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub struct Store {
    path: PathBuf,
    known: Option<String>,
    held: bool,
    lock: Option<File>,
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
        let text = self
            .read()
            .map_err(|error| format!("{}: {error}", self.path.display()))?;
        self.known = text.clone();
        Ok(text)
    }

    fn read(&self) -> std::io::Result<Option<String>> {
        match fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
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
        if self.held || self.known.as_deref() == Some(text) {
            return Ok(());
        }
        // Equal length and mtime do not imply equal contents, particularly for
        // rapid edits on Linux or editors preserving file timestamps.
        if self
            .read()
            .map_err(|error| format!("{}: {error}", self.path.display()))?
            != self.known
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
            // An edit may arrive while the replacement is being written/synced.
            if self.read()? != self.known {
                return Ok(false);
            }
            fs::rename(&aside, &self.path)?;
            Ok::<_, std::io::Error>(true)
        })();
        if !matches!(result, Ok(true)) {
            let _ = fs::remove_file(&aside);
        }
        if result.map_err(|error| format!("{}: {error}", self.path.display()))? {
            self.known = Some(text.to_owned());
        }
        Ok(())
    }

    pub fn poll(&mut self) -> Option<Result<String, String>> {
        match self.read() {
            Ok(text) if text == self.known => None,
            Ok(text) => {
                self.known = text.clone();
                self.held = false;
                text.map(Ok)
            }
            Err(error) => {
                self.held = true;
                Some(Err(format!("{}: {error}", self.path.display())))
            }
        }
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
    fn equal_length_edits_with_unchanged_mtime_are_detected_and_preserved() {
        let dir = std::env::temp_dir().join(format!("nui-store-collision-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut store = Store::new(dir.join("state.kdl"));
        store.save("session main\n").unwrap();
        let path = store.path().to_path_buf();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let external = |text: &str| {
            assert_eq!(text.len(), "session main\n".len());
            fs::write(&path, text).unwrap();
            File::open(&path).unwrap().set_modified(modified).unwrap();
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        };
        external("session work\n");
        store.save("message mine\n").unwrap();
        assert_eq!(fs::read_to_string(store.path()).unwrap(), "session work\n");
        assert_eq!(store.poll().unwrap().unwrap(), "session work\n");
        // Invalid edits and repairs may have the same length AND timestamp too.
        external("session {   \n");
        assert_eq!(store.poll().unwrap().unwrap(), "session {   \n");
        store.reject();
        store.save("session main\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "session {   \n");
        external("session safe\n");
        assert_eq!(store.poll().unwrap().unwrap(), "session safe\n");
        store.save("session main\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "session main\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unreadable_content_and_unseen_deletion_are_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("nui-store-unreadable-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut store = Store::new(dir.join("state.kdl"));
        store.save("session main\n").unwrap();
        fs::write(store.path(), [0xff]).unwrap();
        assert!(store.save("session other\n").is_err());
        assert_eq!(fs::read(store.path()).unwrap(), [0xff]);
        assert!(store.poll().unwrap().is_err());
        fs::write(store.path(), "session fixed\n").unwrap();
        assert_eq!(store.poll().unwrap().unwrap(), "session fixed\n");
        fs::remove_file(store.path()).unwrap();
        store.save("session other\n").unwrap();
        assert!(!store.path().exists());
        assert!(store.poll().is_none());
        store.save("session other\n").unwrap();
        assert_eq!(fs::read_to_string(store.path()).unwrap(), "session other\n");
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
