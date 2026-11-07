//! Local configuration helpers. They never attach or start a coordinator.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use crate::{
    cli::Options,
    decoration::{Layer, Library},
    store::mkdir,
};

pub const ACTIONS: [&str; 4] = ["path", "show", "init", "check"];

/// A valid, editable starting point; omitted settings inherit their defaults.
pub const TEMPLATE: &str = r#"// nui-yalij configuration (KDL). Keep only the overrides you need.
// Apply global/profile edits with: nui-yalij reload
// Layers: global -> profile (--profile NAME) -> session decoration.kdl.
keys {
    panel ctrl-t
    search "/"
    left h left
    right l right
    next j down
    previous k up
    previous-tab "["
    next-tab "]"
    add a
    rename r
    remove x
    stop s
    enter enter
    back esc
    detach q
    quit Q
    restart enter
}
// Milliseconds, 0 disables that gesture. Both OPEN navigation, never toggle it.
gestures double-escape-ms=300 hold-escape-ms=600
commands {
    panel t
}
theme accent=blue active=green muted=darkgray notice=yellow error=red
// Use plain if your terminal's font has no Nerd Font icons.
icons nerd
// plugin "~/path/to/plugin.lua"
"#;

pub fn path(options: &Options) -> PathBuf {
    match &options.profile {
        Some(name) => options.config.join("profiles").join(format!("{name}.kdl")),
        None => options.config.join("decoration.kdl"),
    }
}

pub fn run(action: &str, options: &Options) -> Result<(), String> {
    let path = path(options);
    match action {
        "path" => println!("{}", path.display()),
        "show" => match fs::read_to_string(&path) {
            Ok(text) => print!("{text}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => print!("{TEMPLATE}"),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        },
        "init" => {
            if fs::symlink_metadata(&path).is_ok() {
                return Err(format!(
                    "{} already exists; edit it instead",
                    path.display()
                ));
            }
            if !options.apply {
                println!(
                    "Create {} with default settings; use --apply to write it.",
                    path.display()
                );
                return Ok(());
            }
            mkdir(path.parent().ok_or("configuration has no parent")?)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            file.write_all(TEMPLATE.as_bytes())
                .map_err(|error| error.to_string())?;
            println!(
                "Created {}. Edit it, then run nui-yalij reload.",
                path.display()
            );
        }
        "check" => {
            Library::load(options.config.clone())?
                .effective(options.profile.as_deref(), &Layer::default())?;
            println!(
                "Configuration is valid (global and profiles; session overrides are checked live)."
            );
        }
        _ => {
            return Err(format!(
                "unknown config action {action}; use path, show, init or check"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn starter_configuration_is_valid_and_matches_defaults() {
        let layer = Layer::decode(TEMPLATE, Path::new(".")).unwrap();
        assert_eq!(layer.keys["panel"], ["ctrl-t"]);
        assert_eq!(layer.keys["search"], ["/"]);
        assert_eq!(layer.gestures["double-escape-ms"], 300);
        assert_eq!(layer.gestures["hold-escape-ms"], 600);
    }
}
