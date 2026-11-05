//! Decoration is configuration, not session state. Global/profile layers are
//! snapshots (start/reload); the session layer is read live by its owner.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use kdl::{KdlDocument, KdlNode, KdlValue};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    style::Color,
};

use crate::{
    global::{check, children},
    keys,
};

const ACTIONS: [(&str, &[&str]); 16] = [
    ("panel", &["ctrl-g"]),
    ("left", &["h", "left"]),
    ("right", &["l", "right"]),
    ("next", &["j", "down"]),
    ("previous", &["k", "up"]),
    ("previous-tab", &["["]),
    ("next-tab", &["]"]),
    ("add", &["a"]),
    ("rename", &["r"]),
    ("remove", &["x"]),
    ("stop", &["s"]),
    ("enter", &["enter"]),
    ("back", &["esc"]),
    ("detach", &["q"]),
    ("quit", &["Q"]),
    ("restart", &["enter"]),
];

/// Commands inside the terminals of a session, as their default names:
/// `t` opens and closes navigation, as the panel key does.
const COMMANDS: [(&str, &[&str]); 1] = [("panel", &["t"])];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layer {
    pub keys: BTreeMap<String, Vec<String>>,
    pub commands: BTreeMap<String, Vec<String>>,
    pub theme: BTreeMap<String, String>,
    pub icons: Option<Icons>,
    pub plugins: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub accent: Color,
    pub active: Color,
    pub muted: Color,
    pub notice: Color,
    pub error: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::Blue,
            active: Color::Green,
            muted: Color::DarkGray,
            notice: Color::Yellow,
            error: Color::Red,
        }
    }
}

/// The symbols telling items apart, a folder from a terminal: a Nerd Font's,
/// or plain ones for a terminal whose font has none.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Icons {
    #[default]
    Nerd,
    Plain,
}

impl Icons {
    fn parse(name: &str) -> Option<Icons> {
        match name {
            "nerd" => Some(Icons::Nerd),
            "plain" => Some(Icons::Plain),
            _ => None,
        }
    }

    pub fn folder(self) -> &'static str {
        match self {
            // nf-fa-folder
            Icons::Nerd => "\u{f07b}",
            Icons::Plain => "▸",
        }
    }

    pub fn terminal(self) -> &'static str {
        match self {
            // nf-fa-terminal
            Icons::Nerd => "\u{f120}",
            Icons::Plain => "$",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Decoration {
    keys: BTreeMap<String, Vec<String>>,
    commands: BTreeMap<String, Vec<String>>,
    pub theme: Theme,
    pub icons: Icons,
    pub plugins: Vec<String>,
}

/// The bindings of actions, as named by default.
fn defaults(actions: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
    actions
        .iter()
        .map(|(action, names)| {
            (
                action.to_string(),
                names.iter().map(|name| name.to_string()).collect(),
            )
        })
        .collect()
}

impl Default for Decoration {
    fn default() -> Self {
        Self {
            keys: defaults(&ACTIONS),
            commands: defaults(&COMMANDS),
            theme: Theme::default(),
            icons: Icons::default(),
            plugins: Vec::new(),
        }
    }
}

impl Decoration {
    fn overlay(&mut self, layer: &Layer) {
        self.keys.extend(layer.keys.clone());
        self.commands.extend(layer.commands.clone());
        for (name, value) in &layer.theme {
            let color = Color::from_str(value).expect("validated color");
            match name.as_str() {
                "accent" => self.theme.accent = color,
                "active" => self.theme.active = color,
                "muted" => self.theme.muted = color,
                "notice" => self.theme.notice = color,
                "error" => self.theme.error = color,
                _ => unreachable!(),
            }
        }
        if let Some(icons) = layer.icons {
            self.icons = icons;
        }
        for path in &layer.plugins {
            self.plugins.retain(|known| known != path);
            self.plugins.push(path.clone());
        }
    }

    pub fn binds(&self, action: &str, key: &str) -> bool {
        self.keys
            .get(action)
            .is_some_and(|bound| bound.iter().any(|known| known == key))
    }

    /// Whether `name` is a command for `action`.
    pub fn calls(&self, action: &str, name: &str) -> bool {
        self.commands
            .get(action)
            .is_some_and(|names| names.iter().any(|known| known == name))
    }

    pub fn hint(&self, action: &str) -> &str {
        self.keys
            .get(action)
            .and_then(|keys| keys.first())
            .map(String::as_str)
            .unwrap_or("unbound")
    }

    /// Translate configured actions to the small built-in navigation vocabulary.
    /// Text prompts bypass this function entirely.
    pub fn navigation_key(&self, notation: &str) -> Option<KeyEvent> {
        let action = ACTIONS
            .iter()
            .find(|(action, _)| {
                *action != "panel" && *action != "restart" && self.binds(action, notation)
            })?
            .0;
        let code = match action {
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "next" => KeyCode::Down,
            "previous" => KeyCode::Up,
            "previous-tab" => KeyCode::Char('['),
            "next-tab" => KeyCode::Char(']'),
            "add" => KeyCode::Char('a'),
            "rename" => KeyCode::Char('r'),
            "remove" => KeyCode::Char('x'),
            "stop" => KeyCode::Char('s'),
            "enter" => KeyCode::Enter,
            "back" => KeyCode::Esc,
            "detach" => KeyCode::Char('q'),
            "quit" => KeyCode::Char('Q'),
            _ => return None,
        };
        Some(KeyEvent::new(code, KeyModifiers::NONE))
    }

    pub fn lua_theme(&self) -> BTreeMap<&'static str, String> {
        [
            ("accent", self.theme.accent),
            ("active", self.theme.active),
            ("muted", self.theme.muted),
            ("notice", self.theme.notice),
            ("error", self.theme.error),
        ]
        .into_iter()
        .map(|(key, color)| (key, color.to_string()))
        .collect()
    }
}

impl Layer {
    pub fn decode(text: &str, base: &Path) -> Result<Layer, String> {
        let doc: KdlDocument = text
            .parse()
            .map_err(|error| format!("decoration: {error}"))?;
        let mut layer = Layer::default();
        let mut blocks = std::collections::HashSet::new();
        for node in doc.nodes() {
            let name = node.name().value();
            if ["keys", "commands", "theme", "icons"].contains(&name) && !blocks.insert(name) {
                return Err(format!("duplicate {name} block"));
            }
            match name {
                "keys" => {
                    layer.keys = actions(node, &ACTIONS, |key| {
                        keys::canonical(key).ok_or_else(|| format!("{key} is not a key"))
                    })?;
                }
                "commands" => {
                    layer.commands = actions(node, &COMMANDS, |name| {
                        valid_command(name)
                            .then(|| name.to_string())
                            .ok_or_else(|| format!("{name} is not a command name"))
                    })?;
                }
                "theme" => {
                    let names = ["accent", "active", "muted", "notice", "error"];
                    check(node, "theme", &names, &[], 0)?;
                    for entry in node.entries() {
                        let name = entry.name().unwrap().value();
                        let color = entry
                            .value()
                            .as_string()
                            .ok_or("a theme color must be a string")?;
                        Color::from_str(color).map_err(|_| format!("{color} is not a color"))?;
                        layer.theme.insert(name.into(), color.into());
                    }
                }
                "icons" => {
                    check(node, "icons", &[], &[], 1)?;
                    let name = node
                        .get(0)
                        .and_then(KdlValue::as_string)
                        .ok_or("icons take a name: nerd or plain")?;
                    let icons = Icons::parse(name)
                        .ok_or_else(|| format!("{name} is no icons: nerd or plain"))?;
                    layer.icons = Some(icons);
                }
                "plugin" => {
                    check(node, "plugin", &[], &[], 1)?;
                    let path = node
                        .get(0)
                        .and_then(KdlValue::as_string)
                        .ok_or("plugin needs a path")?;
                    let path = match path.strip_prefix("~/") {
                        Some(rest) => {
                            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest)
                        }
                        None => base.join(path),
                    };
                    layer.plugins.push(path.to_string_lossy().into_owned());
                }
                other => return Err(format!("unknown decoration node {other}")),
            }
        }
        Ok(layer)
    }

    fn read(path: &Path) -> Result<Layer, String> {
        match fs::read_to_string(path) {
            Ok(text) => Layer::decode(&text, path.parent().unwrap_or(Path::new(".")))
                .map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Layer::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }
}

pub struct Library {
    pub root: PathBuf,
    global: Layer,
    profiles: HashMap<String, Layer>,
}

/// A block of actions, each bound to the names its arguments give, in the
/// canonical form of `value`; one without arguments is unbound.
fn actions(
    node: &KdlNode,
    known: &[(&str, &[&str])],
    value: impl Fn(&str) -> Result<String, String>,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let block = node.name().value();
    let names: Vec<&str> = known.iter().map(|(name, _)| *name).collect();
    check(node, block, &[], &names, 0)?;
    let mut actions = BTreeMap::new();
    for binding in children(node) {
        let action = binding.name().value();
        let args = binding
            .entries()
            .iter()
            .filter(|entry| entry.name().is_none())
            .count();
        check(binding, action, &[], &[], args)?;
        let bound: Vec<String> = binding
            .entries()
            .iter()
            .map(|entry| {
                let name = entry
                    .value()
                    .as_string()
                    .ok_or_else(|| format!("{block}: {action} takes strings"))?;
                value(name)
            })
            .collect::<Result<_, String>>()?;
        if actions.insert(action.to_string(), bound).is_some() {
            return Err(format!("{block}: duplicate action {action}"));
        }
    }
    Ok(actions)
}

/// A command name: a file name a shell runs as it is.
pub fn valid_command(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.+".contains(c))
}

pub fn valid_profile(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

impl Library {
    pub fn load(root: PathBuf) -> Result<Library, String> {
        let global = Layer::read(&root.join("decoration.kdl"))?;
        let mut profiles = HashMap::new();
        match fs::read_dir(root.join("profiles")) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry.map_err(|error| error.to_string())?.path();
                    if path.extension().is_some_and(|ext| ext == "kdl") {
                        let name = path
                            .file_stem()
                            .and_then(|stem| stem.to_str())
                            .ok_or("non-UTF8 profile name")?;
                        if !valid_profile(name) {
                            return Err(format!("invalid profile name {name}"));
                        }
                        profiles.insert(name.to_owned(), Layer::read(&path)?);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        Ok(Library {
            root,
            global,
            profiles,
        })
    }

    pub fn effective(&self, profile: Option<&str>, session: &Layer) -> Result<Decoration, String> {
        let mut decoration = Decoration::default();
        decoration.overlay(&self.global);
        if let Some(name) = profile {
            decoration.overlay(
                self.profiles.get(name).ok_or_else(|| {
                    format!("unknown profile {name}; profiles load at start/reload")
                })?,
            );
        }
        decoration.overlay(session);
        Ok(decoration)
    }

    /// The names of the commands any viewer of a session may call, of every
    /// profile: each one tells whether a name is its own.
    pub fn commands(&self, session: &Layer) -> BTreeSet<String> {
        std::iter::once(None)
            .chain(self.profiles.keys().map(|name| Some(name.as_str())))
            .filter_map(|profile| self.effective(profile, session).ok())
            .flat_map(|decoration| decoration.commands.into_values().flatten())
            .collect()
    }

    /// Automatic hooks never depend on the profile of an attached viewer.
    pub fn hooks(&self, session: &Layer) -> Vec<String> {
        let mut decoration = Decoration::default();
        decoration.overlay(&self.global);
        decoration.overlay(session);
        decoration.plugins
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_override_actions_and_theme_without_owning_state() {
        let layer = |text| Layer::decode(text, Path::new("/config")).unwrap();
        let library = Library {
            root: PathBuf::new(),
            global: layer("keys { panel ctrl-a }\ntheme accent=red\nplugin global.lua"),
            profiles: HashMap::from([(
                "work".into(),
                layer("keys { panel ctrl-b }\ntheme muted=white\nplugin work.lua"),
            )]),
        };
        let session = layer("keys { panel ctrl-c }\ntheme accent=green\nplugin session.lua");
        let effective = library.effective(Some("work"), &session).unwrap();
        assert!(effective.binds("panel", "ctrl-c"));
        assert_eq!(effective.theme.accent, Color::Green);
        assert_eq!(effective.theme.muted, Color::White);
        assert_eq!(
            effective.plugins,
            [
                "/config/global.lua",
                "/config/work.lua",
                "/config/session.lua"
            ]
        );
        assert_eq!(
            library.hooks(&session),
            ["/config/global.lua", "/config/session.lua"]
        );
        assert!(library.effective(Some("missing"), &session).is_err());
    }

    #[test]
    fn icons_are_nerd_unless_plain() {
        let layer = |text| Layer::decode(text, Path::new(".")).unwrap();
        assert_eq!(Decoration::default().icons, Icons::Nerd);
        // plain for every instance, but the one whose terminal has the font
        let library = Library {
            root: PathBuf::new(),
            global: layer("icons plain"),
            profiles: HashMap::from([("nerd".into(), layer("icons nerd"))]),
        };
        let icons = |profile| library.effective(profile, &Layer::default()).unwrap().icons;
        assert_eq!(
            (icons(None), icons(Some("nerd"))),
            (Icons::Plain, Icons::Nerd)
        );
        assert_eq!(Icons::Plain.folder(), "▸");
    }

    #[test]
    fn brackets_switch_tabs_unless_bound_otherwise() {
        let key = |decoration: &Decoration, notation| {
            decoration.navigation_key(notation).map(|key| key.code)
        };
        let plain = Decoration::default();
        assert_eq!(key(&plain, "["), Some(KeyCode::Char('[')));
        assert_eq!(key(&plain, "]"), Some(KeyCode::Char(']')));
        let library = Library {
            root: PathBuf::new(),
            global: Layer::decode(
                "keys { previous-tab \"<\"; next-tab \">\" }",
                Path::new("."),
            )
            .unwrap(),
            profiles: HashMap::new(),
        };
        let bound = library.effective(None, &Layer::default()).unwrap();
        assert_eq!(key(&bound, ">"), Some(KeyCode::Char(']')));
        assert_eq!(key(&bound, "]"), None);
        assert_eq!(bound.hint("previous-tab"), "<");
    }

    #[test]
    fn commands_are_named_per_layer() {
        let layer = |text| Layer::decode(text, Path::new("/config")).unwrap();
        let library = Library {
            root: PathBuf::new(),
            global: Layer::default(),
            profiles: HashMap::from([
                ("work".into(), layer("commands { panel w nav }")),
                ("quiet".into(), layer("commands { panel }")),
            ]),
        };
        let plain = library.effective(None, &Layer::default()).unwrap();
        assert!(plain.calls("panel", "t") && !plain.calls("panel", "w"));
        let work = library.effective(Some("work"), &Layer::default()).unwrap();
        assert!(work.calls("panel", "nav") && !work.calls("panel", "t"));
        assert!(
            !library
                .effective(Some("quiet"), &Layer::default())
                .unwrap()
                .calls("panel", "t")
        );
        assert_eq!(
            library.commands(&Layer::default()),
            BTreeSet::from(["nav".into(), "t".into(), "w".into()])
        );
        // a session's own wins over every profile
        assert_eq!(
            library.commands(&layer("commands { panel s }")),
            BTreeSet::from(["s".into()])
        );
    }

    #[test]
    fn invalid_configuration_is_not_silently_accepted() {
        for text in [
            "attached 2",
            "keys { panel ctrl-nope }",
            "theme accent=notacolor",
            "plugin foo bad=1",
            "keys { nope x }",
            "commands { panel ../t }",
            "commands { panel -t }",
            "commands { nope x }",
            "commands { panel t }\ncommands { panel t }",
            "icons",
            "icons nope",
            "icons plain x=1",
            "icons plain\nicons nerd",
        ] {
            assert!(Layer::decode(text, Path::new(".")).is_err(), "{text}");
        }
    }
}
