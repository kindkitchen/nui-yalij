//! Lifecycle tests use fake Cargo and isolated homes/runtime roots: no network,
//! real installation, or other user's session can be affected.
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_nui-yalij");

struct Home {
    dir: PathBuf,
    runtime: PathBuf,
    children: Vec<Child>,
}
impl Home {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nui-lifecycle-{name}-{}", std::process::id()));
        let runtime = PathBuf::from(format!("/tmp/nui-life-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&runtime);
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(dir.join("tools")).unwrap();
        let home = Self {
            dir: dir.canonicalize().unwrap(),
            runtime,
            children: Vec::new(),
        };
        home.executable("tools/cargo", r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/cargo-calls"
[ "$FAIL_CARGO" = 1 ] && exit 7
case "$1" in
install)
    mkdir -p "$CARGO_INSTALL_ROOT/bin"
    cp "$HOME/new-binary" "$CARGO_INSTALL_ROOT/bin/nui-yalij"
    printf '{"installs":{"nui-yalij 0.2.0 (registry+test)":{}}}' > "$CARGO_INSTALL_ROOT/.crates2.json"
    ;;
uninstall)
    rm -f "$CARGO_INSTALL_ROOT/bin/nui-yalij" "$CARGO_INSTALL_ROOT/.crates2.json"
    ;;
*) exit 8 ;;
esac
"#);
        home.executable("new-binary", "#!/bin/sh\nprintf 'nui-yalij 0.2.0\\n'\n");
        home
    }
    fn write(&self, relative: &str, text: &str) {
        let path = self.dir.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn executable(&self, relative: &str, text: &str) {
        self.write(relative, text);
        fs::set_permissions(self.dir.join(relative), fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .envs([
                ("HOME", self.dir.clone()),
                ("CARGO_HOME", self.dir.join("cargo")),
                ("CARGO_INSTALL_ROOT", self.dir.join("cargo")),
                ("XDG_STATE_HOME", self.dir.join("state")),
                ("XDG_CONFIG_HOME", self.dir.join("config")),
                ("XDG_CACHE_HOME", self.dir.join("cache")),
                ("XDG_RUNTIME_DIR", self.runtime.clone()),
                ("TMPDIR", self.runtime.clone()),
                ("SHELL", PathBuf::from("/bin/sh")),
            ])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.dir.join("tools").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env_remove("NUI_YALIJ_STATE")
            .env_remove("FAIL_CARGO");
        command
    }
    fn ok(&self, args: &[&str]) -> String {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    fn failed(&self, output: Output) -> String {
        assert!(!output.status.success());
        String::from_utf8(output.stderr).unwrap()
    }
    fn installed(&self) {
        self.executable(
            "cargo/bin/nui-yalij",
            "#!/bin/sh\nprintf 'nui-yalij 0.1.0\\n'\n",
        );
        self.write(
            "cargo/.crates2.json",
            "{\"installs\":{\"nui-yalij 0.1.0 (registry+test)\":{}}}",
        );
    }
    fn server(&mut self, relative: &str) {
        self.write(relative, "session main { terminal 1 command=\"/bin/sh\"; workspace { window { pane terminal=1 } } }");
        let state = self.dir.join(relative);
        let session = PathBuf::from(format!("{}.sessions/1/state.kdl", state.display()));
        let child = self
            .command(&["--state", state.to_str().unwrap(), "server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(self.dir.join("server-errors")).unwrap())
            .spawn()
            .unwrap();
        self.children.push(child);
        let deadline = Instant::now() + Duration::from_secs(12);
        while !fs::read_to_string(&session)
            .unwrap_or_default()
            .contains("pid=")
        {
            assert!(
                Instant::now() < deadline,
                "server did not start: {}; {:?}; {}; state: {}",
                state.display(),
                self.children.last_mut().unwrap().try_wait(),
                fs::read_to_string(self.dir.join("server-errors")).unwrap_or_default(),
                fs::read_to_string(&session).unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(40));
        }
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.dir);
        let _ = fs::remove_dir_all(&self.runtime);
    }
}

#[test]
fn lifecycle_observes_without_installing_stopping_or_deleting() {
    let home = Home::new("observe");
    assert!(home.ok(&["install"]).contains("Observed only"));
    home.installed();
    home.write("config/nui-yalij/decoration.kdl", "theme accent=green");
    for command in ["update", "uninstall", "remove"] {
        assert!(home.ok(&[command]).contains("Observed only"));
    }
    assert!(home.dir.join("cargo/bin/nui-yalij").exists());
    assert!(home.dir.join("config/nui-yalij/decoration.kdl").exists());
    assert!(!home.dir.join("cargo-calls").exists());
    assert!(
        home.failed(home.command(&["install", "--apply"]).output().unwrap())
            .contains("already exists")
    );
}

#[test]
fn install_and_update_really_replace_the_binary_and_preserve_data() {
    let mut home = Home::new("update");
    assert!(
        home.failed(home.command(&["update", "--apply"]).output().unwrap())
            .contains("not installed")
    );
    home.ok(&["install", "0.2.0", "--apply"]);
    home.write("config/nui-yalij/decoration.kdl", "theme accent=green");
    home.server("state/nui-yalij/global-state.kdl");
    let state = home.dir.join("state/nui-yalij/global-state.kdl");
    let failed = home
        .command(&["update", "--apply"])
        .env("FAIL_CARGO", "1")
        .output()
        .unwrap();
    assert!(home.failed(failed).contains("failed"));
    assert!(
        home.children[0].try_wait().unwrap().is_none(),
        "failed installation must not stop servers"
    );
    home.ok(&["update", "0.2.0", "--apply"]);
    assert!(home.children[0].wait().unwrap().success());
    let session = PathBuf::from(format!("{}.sessions/1/state.kdl", state.display()));
    assert!(!fs::read_to_string(session).unwrap().contains("pid="));
    assert_eq!(
        fs::read_to_string(home.dir.join("config/nui-yalij/decoration.kdl")).unwrap(),
        "theme accent=green"
    );
    let calls = fs::read_to_string(home.dir.join("cargo-calls")).unwrap();
    assert!(calls.contains("--force") && calls.contains("--version 0.2.0"));
}

#[test]
fn uninstall_wipes_managed_files_and_all_servers_but_not_source_or_shared_cargo_files() {
    let mut home = Home::new("wipe");
    home.installed();
    home.server("state/nui-yalij/global-state.kdl");
    home.server("custom/global.kdl");
    for file in [
        "config/nui-yalij/decoration.kdl",
        "custom/config/profiles/work.kdl",
        "cache/nui-yalij/cache",
        ".cache/nui-yalij/old",
        ".config/nui-yalij/old",
        ".local/state/nui-yalij/old",
        "custom/global.kdl.123.tmp",
    ] {
        home.write(file, "app-owned");
    }
    for file in [
        "source/.git/config",
        "source/src/main.rs",
        "cargo/registry/shared",
        "custom/unrelated.txt",
    ] {
        home.write(file, "keep");
    }
    let state = home.dir.join("custom/global.kdl");
    let config = home.dir.join("custom/config");
    home.ok(&[
        "--state",
        state.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "uninstall",
        "--apply",
    ]);
    for child in &mut home.children {
        assert!(child.wait().unwrap().success());
    }
    for file in [
        "cargo/bin/nui-yalij",
        "cargo/.crates2.json",
        "state/nui-yalij",
        "config/nui-yalij",
        "cache/nui-yalij",
        ".cache/nui-yalij",
        ".config/nui-yalij",
        ".local/state/nui-yalij",
        "custom/config",
        "custom/global.kdl",
        "custom/global.kdl.sessions",
        "custom/global.kdl.lock",
        "custom/global.kdl.legacy.kdl",
        "custom/global.kdl.123.tmp",
    ] {
        assert!(!home.dir.join(file).exists(), "left behind: {file}");
    }
    for file in [
        "source/.git/config",
        "source/src/main.rs",
        "cargo/registry/shared",
        "custom/unrelated.txt",
    ] {
        assert_eq!(fs::read_to_string(home.dir.join(file)).unwrap(), "keep");
    }
    assert!(fs::read_dir(&home.runtime).unwrap().next().is_none());
    assert!(
        fs::read_to_string(home.dir.join("cargo-calls"))
            .unwrap()
            .contains("uninstall nui-yalij")
    );
    assert!(
        Path::new(BIN).exists(),
        "never delete the development binary"
    );
    home.ok(&["remove", "--apply"]); // Repeating a completed wipe is harmless.
}

#[test]
fn uninstall_does_not_follow_symlinks_or_wipe_home_or_repositories() {
    let home = Home::new("boundaries");
    home.write("outside/keep", "keep");
    home.write("outside/.git/config", "keep");
    fs::create_dir_all(home.dir.join("config")).unwrap();
    symlink(home.dir.join("outside"), home.dir.join("config/nui-yalij")).unwrap();
    home.ok(&["uninstall", "--apply"]);
    assert!(!home.dir.join("config/nui-yalij").exists());
    assert_eq!(
        fs::read_to_string(home.dir.join("outside/keep")).unwrap(),
        "keep"
    );
    home.write("source/.git/config", "keep");
    for config in [&home.dir, &home.dir.join("source")] {
        let output = home
            .command(&["--config", config.to_str().unwrap(), "uninstall", "--apply"])
            .output()
            .unwrap();
        assert!(home.failed(output).contains("protected directory"));
    }
    let output = home
        .command(&["uninstall", "--apply"])
        .env("NUI_YALIJ_STATE", "/inside")
        .output()
        .unwrap();
    assert!(home.failed(output).contains("outside nui-yalij panes"));
    assert!(home.dir.join("source/.git/config").exists());
}
