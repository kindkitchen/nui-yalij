//! Cargo owns installation bookkeeping; nui owns its processes and data.
//! Lifecycle operations observe by default and never delete a source checkout.

use std::{
    env, fs, io,
    os::unix::{fs::MetadataExt, net::UnixStream},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use crate::{cli, client, ipc, store::suffixed};

const NAME: &str = "nui-yalij";

/// Resolve the parent, but not a final symlink: uninstall removes the link,
/// never a directory it happens to point to.
pub fn path(path: &Path) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .ok_or("a lifecycle path needs a file name")?;
    Ok(cli::absolute(path.parent().unwrap_or(Path::new(".")))?.join(name))
}

fn home() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is required for lifecycle operations".into())
}

fn base(variable: &str, fallback: PathBuf) -> PathBuf {
    env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(fallback)
}

fn root(home: &Path) -> Result<PathBuf, String> {
    cli::absolute(&base(
        "CARGO_INSTALL_ROOT",
        base("CARGO_HOME", home.join(".cargo")),
    ))
}

fn runtime_dirs() -> Result<Vec<PathBuf>, String> {
    // The current XDG location and the fallback used without XDG_RUNTIME_DIR.
    let suffix = format!("{NAME}-{}", unsafe { libc::geteuid() });
    let fallback = env::temp_dir().join(&suffix);
    let current = base("XDG_RUNTIME_DIR", env::temp_dir()).join(suffix);
    let mut dirs = vec![path(&current)?, path(&fallback)?];
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

/// Stop every coordinator in this user's private runtime directories and wait
/// for shutdown/persistence, not just the acknowledgement of the kill request.
fn stop(dirs: &[PathBuf]) -> Result<(), String> {
    let mut waiting = Vec::new();
    for dir in dirs {
        let metadata = match fs::symlink_metadata(dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(format!(
                "{} is not a private runtime directory",
                dir.display()
            ));
        }
        for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
            let socket = entry.map_err(|error| error.to_string())?.path();
            if socket
                .extension()
                .is_none_or(|extension| extension != "sock")
            {
                continue;
            }
            match UnixStream::connect(&socket) {
                Ok(stream) => {
                    client::control(stream, ipc::Request::Kill { apply: true })?;
                    waiting.push(socket);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) => {}
                Err(error) => return Err(format!("{}: {error}", socket.display())),
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while waiting.iter().any(|socket| socket.exists()) {
        if Instant::now() >= deadline {
            return Err("servers did not finish shutting down; data was not deleted".into());
        }
        thread::sleep(Duration::from_millis(40));
    }
    Ok(())
}

fn registered(root: &Path) -> bool {
    [".crates.toml", ".crates2.json"].iter().any(|name| {
        fs::read_to_string(root.join(name)).is_ok_and(|text| text.contains("\"nui-yalij "))
    })
}

fn cargo(args: &[&str], root: &Path) -> Result<(), String> {
    let status = Command::new("cargo")
        .args(args)
        .arg("--root")
        .arg(root)
        .status()
        .map_err(|error| format!("cannot run cargo: {error}"))?;
    if !status.success() {
        return Err(format!("cargo {} failed ({status})", args[0]));
    }
    Ok(())
}

fn state_files(state: &Path, paths: &mut Vec<PathBuf>) {
    paths.push(state.to_path_buf());
    for suffix in [".sessions", ".lock", ".server.log", ".legacy.kdl"] {
        paths.push(suffixed(state, suffix));
    }
    if let (Some(parent), Some(name)) = (state.parent(), state.file_name()) {
        let prefix = format!("{}.", name.to_string_lossy());
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(pid) = name
                    .strip_prefix(&prefix)
                    .and_then(|s| s.strip_suffix(".tmp"))
                    && !pid.is_empty()
                    && pid.bytes().all(|byte| byte.is_ascii_digit())
                {
                    paths.push(entry.path());
                }
            }
        }
    }
}

fn wipe_paths(options: &cli::Options, home: &Path, root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = vec![
        home.join(".local/state").join(NAME),
        base("XDG_STATE_HOME", home.join(".local/state")).join(NAME),
        home.join(".config").join(NAME),
        base("XDG_CONFIG_HOME", home.join(".config")).join(NAME),
        home.join(".cache").join(NAME),
        base("XDG_CACHE_HOME", home.join(".cache")).join(NAME),
        options.config.clone(),
    ];
    state_files(&options.state, &mut paths);
    if let Some(legacy) = options.legacy() {
        state_files(&legacy, &mut paths);
    }
    paths.extend(runtime_dirs()?);
    let protected = [
        cli::absolute(home)?,
        cli::absolute(&env::current_dir().map_err(|error| error.to_string())?)?,
        root.join("bin"),
    ];
    let mut safe: Vec<PathBuf> = Vec::new();
    for candidate in paths {
        let candidate = path(&candidate)?;
        if protected.iter().any(|keep| keep.starts_with(&candidate))
            || (fs::symlink_metadata(&candidate).is_ok_and(|metadata| metadata.is_dir())
                && candidate.join(".git").exists())
        {
            return Err(format!(
                "refusing to wipe protected directory {}",
                candidate.display()
            ));
        }
        safe.push(candidate);
    }
    safe.sort_by_key(|path| path.components().count());
    let mut unique: Vec<PathBuf> = Vec::new();
    for candidate in safe {
        if !unique.iter().any(|parent| candidate.starts_with(parent)) {
            unique.push(candidate);
        }
    }
    Ok(unique)
}

fn remove(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let result = if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|error| format!("{}: {error}", path.display()))
}

pub fn run(command: &str, options: &cli::Options) -> Result<(), String> {
    let home = home()?;
    let root = root(&home)?;
    let binary = root.join("bin").join(NAME);
    let uninstall = matches!(command, "uninstall" | "remove");
    let dirs = runtime_dirs()?;
    let paths = if uninstall {
        wipe_paths(options, &home, &root)?
    } else {
        Vec::new()
    };
    let version = options.words.get(1);
    if command == "update" && !binary.is_file() {
        return Err(format!(
            "{} is not installed; use install first",
            binary.display()
        ));
    }
    if command == "install" && binary.exists() {
        return Err(format!("{} already exists; use update", binary.display()));
    }
    if uninstall {
        println!(
            "Stop all nui-yalij servers; remove {} and its Cargo registration.",
            binary.display()
        );
        for path in &paths {
            println!("Wipe {}", path.display());
        }
    } else {
        println!(
            "{command} {} in {} (cargo install --locked).",
            version.map_or("latest", String::as_str),
            root.display()
        );
        if command == "update" {
            println!(
                "Stop old servers after successful replacement; preserve all configuration and state."
            );
        }
    }
    if !options.apply {
        println!("Observed only; add --apply to make these changes.");
        return Ok(());
    }
    if env::var_os("NUI_YALIJ_STATE").is_some() {
        return Err("run lifecycle operations outside nui-yalij panes".into());
    }
    if uninstall {
        stop(&dirs)?;
        if registered(&root) {
            cargo(&["uninstall", NAME], &root)?;
        } else {
            // A manually copied binary in the managed bin directory has no
            // Cargo record. Never remove the currently running development build.
            remove(&binary)?;
        }
        for path in paths {
            remove(&path)?;
        }
        println!("Uninstalled: managed binary, app data and runtime files removed.");
    } else {
        let mut args = vec!["install", "--locked", NAME];
        if command == "update" {
            args.push("--force");
        }
        if let Some(version) = version {
            args.extend(["--version", version]);
        }
        cargo(&args, &root)?;
        let output = Command::new(&binary)
            .arg("--version")
            .output()
            .map_err(|error| format!("installed binary cannot run: {error}"))?;
        let actual = String::from_utf8_lossy(&output.stdout);
        if !output.status.success()
            || !actual.starts_with("nui-yalij ")
            || version.is_some_and(|version| actual.trim() != format!("nui-yalij {version}"))
        {
            return Err("installer did not produce the requested runnable nui-yalij binary".into());
        }
        if command == "update" {
            stop(&dirs)?;
        }
        println!("{} installed at {}", actual.trim(), binary.display());
    }
    Ok(())
}
