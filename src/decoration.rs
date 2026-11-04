//! Decoration is configuration, not session state. Global/profile layers are
//! snapshots (start/reload); the session layer is read live by its owner.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use kdl::{KdlDocument, KdlValue};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    style::Color,
};

use crate::{
    global::{check, children},
    keys,
};

const ACTIONS: [(&str, &[&str]); 14] = [
    ("panel", &["ctrl-g"]),
    ("left", &["h", "left"]),
    ("right", &["l", "right"]),
    ("next", &["j", "down"]),
    ("previous", &["k", "up"]),
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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layer {
    pub keys: BTreeMap<String, Vec<String>>,
    pub theme: BTreeMap<String, String>,
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

#[derive(Clone, Debug)]
pub struct Decoration {
    keys: BTreeMap<String, Vec<String>>,
    pub theme: Theme,
    pub plugins: Vec<String>,
}

impl Default for Decoration {
    fn default() -> Self {
        Self {
            keys: ACTIONS
                .iter()
                .map(|(action, keys)| {
                    (
                        action.to_string(),
                        keys.iter().map(|key| key.to_string()).collect(),
                    )
                })
                .collect(),
            theme: Theme::default(),
            plugins: Vec::new(),
        }
    }
}

impl Decoration {
    fn overlay(&mut self, layer: &Layer) {
        self.keys.extend(layer.keys.clone());
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
            match node.name().value() {
                "keys" => {
                    check(node, "keys", &[], &ACTIONS.map(|(name, _)| name), 0)?;
                    if !blocks.insert("keys") {
                        return Err("duplicate keys block".into());
                    }
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
                                let key =
                                    entry.value().as_string().ok_or("a key must be a string")?;
                                keys::canonical(key).ok_or_else(|| format!("{key} is not a key"))
                            })
                            .collect::<Result<_, String>>()?;
                        if layer.keys.insert(action.into(), bound).is_some() {
                            return Err(format!("duplicate key action {action}"));
                        }
                    }
                }
                "theme" => {
                    let names = ["accent", "active", "muted", "notice", "error"];
                    check(node, "theme", &names, &[], 0)?;
                    if !blocks.insert("theme") {
                        return Err("duplicate theme block".into());
                    }
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
    fn invalid_configuration_is_not_silently_accepted() {
        for text in [
            "attached 2",
            "keys { panel ctrl-nope }",
            "theme accent=notacolor",
            "plugin foo bad=1",
            "keys { nope x }",
        ] {
            assert!(Layer::decode(text, Path::new(".")).is_err(), "{text}");
        }
    }
}
