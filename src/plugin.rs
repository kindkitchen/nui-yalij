//! Plugins: Lua scripts extending and managing the Nui state.
//!
//! A plugin is a file returning a table with any of:
//!
//! - `layouts`: layout strategies by name, `function(container) -> placement`;
//! - `keys`: handlers by mode (`view`, `navigate`) and key, `function(state) -> state?`;
//! - `update`: a hook after every change of the state, `function(state) -> state?`.
//!
//! Keys receive the plain Nui projection and viewer context, indices from 1.
//! Automatic updates receive only their owner session and never a profile.
//! Layouts receive the effective theme. A handler returns the next state, or
//! nothing to keep its in-place edits. A later plugin overrides an earlier one; plugin layouts and keys
//! override the built-in ones. `print` reaches the person as the message; the
//! global `nui` holds helpers, see `globals`.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    path::{Path, PathBuf},
    rc::Rc,
    str::FromStr,
};

use mlua::{
    Function, Lua, LuaSerdeExt, Table, Value, Variadic, serde::ser::Options as SerializeOptions,
};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
};

use crate::layout::{Container, Draw, Placement};
use crate::state::{DEFAULT_LAYOUT, Level, Location, Nui};

#[derive(serde::Serialize)]
pub struct ClientContext {
    pub id: u64,
    pub session: u64,
    pub profile: Option<String>,
}

pub struct Plugins {
    lua: Lua,
    /// The loaded plugins with their paths, in the order of the state.
    loaded: Vec<(String, Table)>,
    /// The paths of the last load, to load again when the state lists others.
    paths: Vec<String>,
    /// For the person: what plugins printed, and their errors.
    notices: Rc<RefCell<Vec<String>>>,
    shell: String,
    pub theme: BTreeMap<&'static str, String>,
    pub client: Option<ClientContext>,
}

impl Plugins {
    /// No plugins yet; `shell` is the command of new terminals.
    pub fn new(shell: String) -> Plugins {
        let notices = Rc::default();
        Plugins {
            lua: vm(&notices, &shell),
            loaded: Vec::new(),
            paths: Vec::new(),
            notices,
            shell,
            theme: BTreeMap::new(),
            client: None,
        }
    }

    /// Load the plugins the state lists, when the list changed: in a new VM,
    /// as unloading is starting over. Relative paths start from `base`.
    pub fn sync(&mut self, paths: &[String], base: &Path) {
        if self.paths == paths {
            return;
        }
        self.loaded.clear();
        self.lua = vm(&self.notices, &self.shell);
        self.paths = paths.to_vec();
        for path in paths {
            match self.load(&resolve(path, base)) {
                Ok(plugin) => self.loaded.push((path.clone(), plugin)),
                Err(error) => self.notice(format!("plugin {path}: {error}")),
            }
        }
    }

    fn load(&self, file: &Path) -> Result<Table, String> {
        let code = std::fs::read_to_string(file)
            .map_err(|error| format!("{}: {error}", file.display()))?;
        let chunk = self.lua.load(code).set_name(format!("@{}", file.display()));
        chunk.eval::<Table>().map_err(|error| error.to_string())
    }

    /// Place a container with the plugin layout `name`; none when no plugin
    /// has it, or it failed.
    pub fn layout(&self, name: &str, container: &Container) -> Option<Placement> {
        let found = self.loaded.iter().rev().find_map(|(plugin, table)| {
            let layouts = table.get::<Option<Table>>("layouts").ok()??;
            Some((plugin, layouts.get::<Option<Function>>(name).ok()??))
        });
        let Some((plugin, function)) = found else {
            if name != DEFAULT_LAYOUT {
                self.notice(format!("unknown layout {name}, placed as {DEFAULT_LAYOUT}"));
            }
            return None;
        };
        self.place(&function, container)
            .map_err(|error| self.notice(format!("plugin {plugin}, layout {name}: {error}")))
            .ok()
    }

    fn place(&self, function: &Function, container: &Container) -> mlua::Result<Placement> {
        let lua = &self.lua;
        let children = lua.create_table()?;
        for (index, label) in container.children.iter().enumerate() {
            children.set(
                index + 1,
                lua.create_table_from([("name", label.as_str())])?,
            )?;
        }
        let argument = lua.create_table()?;
        argument.set("kind", container.kind.name())?;
        argument.set("theme", lua.to_value(&self.theme)?)?;
        argument.set("area", area_table(lua, container.area)?)?;
        argument.set("active", container.active + 1)?;
        argument.set("children", children)?;
        let result: Table = function.call(argument)?;
        let mut placement = Placement {
            areas: vec![None; container.children.len()],
            draws: Vec::new(),
        };
        if let Some(areas) = result.get::<Option<Table>>("areas")? {
            for (index, area) in placement.areas.iter_mut().enumerate() {
                if let Some(table) = areas.get::<Option<Table>>(index + 1)? {
                    *area = Some(Rect::new(
                        number(&table, "x")?,
                        number(&table, "y")?,
                        number(&table, "w")?,
                        number(&table, "h")?,
                    ));
                }
            }
        }
        if let Some(draws) = result.get::<Option<Table>>("draw")? {
            for draw in draws.sequence_values::<Table>() {
                let draw = draw?;
                placement.draws.push(Draw {
                    x: number(&draw, "x")?,
                    y: number(&draw, "y")?,
                    text: draw.get("text")?,
                    style: style(draw.get("style")?)?,
                    width: u16::MAX,
                });
            }
        }
        Ok(placement)
    }

    /// Run the plugin handler of `key` in `mode` over the state: the next
    /// state, or an error; none when no plugin binds the key.
    pub fn key(&self, mode: &str, key: &str, nui: &Nui) -> Option<Result<Nui, String>> {
        let (plugin, function) = self.loaded.iter().rev().find_map(|(plugin, table)| {
            let keys = table.get::<Option<Table>>("keys").ok()??;
            let bound = keys.get::<Option<Table>>(mode).ok()??;
            Some((plugin, bound.get::<Option<Function>>(key).ok()??))
        })?;
        Some(
            self.lua
                .to_value(&self.client)
                .and_then(|context| self.transform(&function, nui, context))
                .map_err(|error| format!("plugin {plugin}, key {key}: {error}")),
        )
    }

    /// Run the `update` hooks over a new state, in the order of the plugins.
    pub fn update(&self, mut nui: Nui) -> Result<Nui, String> {
        for (plugin, table) in &self.loaded {
            if let Ok(Some(function)) = table.get::<Option<Function>>("update") {
                nui = self
                    .transform(
                        &function,
                        &nui,
                        Value::Integer(nui.sessions.first().map_or(0, |session| session.id) as i64),
                    )
                    .map_err(|error| format!("plugin {plugin}, update: {error}"))?;
            }
        }
        Ok(nui)
    }

    fn transform(&self, function: &Function, nui: &Nui, context: Value) -> mlua::Result<Nui> {
        let state = to_lua(&self.lua, nui)?;
        let next: Value = function.call((state.clone(), context))?;
        self.lua
            .from_value(if next.is_nil() { state } else { next })
    }

    /// Re-read scripts even when their paths stayed the same (explicit reload).
    pub fn reload(&mut self) {
        self.paths.clear();
        self.loaded.clear();
        self.lua = vm(&self.notices, &self.shell);
    }

    /// Take the notices for the person.
    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut self.notices.borrow_mut())
    }

    fn notice(&self, text: String) {
        self.notices.borrow_mut().push(text);
    }
}

/// A Lua VM with the `nui` helpers, and `print` going to the person.
fn vm(notices: &Rc<RefCell<Vec<String>>>, shell: &str) -> Lua {
    let lua = Lua::new();
    globals(&lua, notices.clone(), shell).expect("the nui globals are valid Lua");
    lua
}

fn globals(lua: &Lua, notices: Rc<RefCell<Vec<String>>>, shell: &str) -> mlua::Result<()> {
    let nui = lua.create_table()?;
    // the command of new terminals
    nui.set("shell", shell)?;
    // an id no terminal of a state has: nui.next_id(state)
    nui.set(
        "next_id",
        lua.create_function(|lua, state: Value| {
            Ok(lua.from_value::<Nui>(state)?.next_terminal_id())
        })?,
    )?;
    // the operations of the navigation panel, as functions of a state:
    // nui.remove(state, "window", location), with a location like
    // { session = 1, workspace = 1, window = 2, pane = 1 }
    nui.set(
        "remove",
        operation(lua, |nui, level, at| nui.remove(level, at))?,
    )?;
    nui.set(
        "stop",
        operation(lua, |nui, level, at| nui.stop(level, at))?,
    )?;
    nui.set(
        "enter",
        lua.create_function(|lua, (state, at): (Value, Value)| {
            let mut nui: Nui = lua.from_value(state)?;
            nui.enter(lua.from_value(at)?);
            to_lua(lua, &nui)
        })?,
    )?;
    nui.set(
        "rename",
        lua.create_function(
            |lua, (state, level, at, name): (Value, String, Value, String)| {
                let mut nui: Nui = lua.from_value(state)?;
                nui.rename(level_of(&level)?, lua.from_value(at)?, &name)
                    .map_err(mlua::Error::runtime)?;
                to_lua(lua, &nui)
            },
        )?,
    )?;
    lua.globals().set("nui", nui)?;
    let print = lua.create_function(move |_, values: Variadic<Value>| {
        let texts: Vec<String> = values
            .iter()
            .map(|value| value.to_string().unwrap_or_else(|_| format!("{value:?}")))
            .collect();
        notices.borrow_mut().push(texts.join("\t"));
        Ok(())
    })?;
    lua.globals().set("print", print)
}

fn operation(lua: &Lua, apply: fn(&mut Nui, Level, Location)) -> mlua::Result<Function> {
    lua.create_function(move |lua, (state, level, at): (Value, String, Value)| {
        let mut nui: Nui = lua.from_value(state)?;
        apply(&mut nui, level_of(&level)?, lua.from_value(at)?);
        to_lua(lua, &nui)
    })
}

/// The state as Lua sees it: absent values are nil.
fn to_lua(lua: &Lua, nui: &Nui) -> mlua::Result<Value> {
    lua.to_value_with(
        nui,
        SerializeOptions::new()
            .serialize_none_to_null(false)
            .serialize_unit_to_null(false),
    )
}

fn level_of(name: &str) -> mlua::Result<Level> {
    Level::parse(name).ok_or_else(|| {
        mlua::Error::runtime(format!(
            "{name} is no level: session, workspace, window or pane"
        ))
    })
}

fn area_table(lua: &Lua, area: Rect) -> mlua::Result<Table> {
    lua.create_table_from([
        ("x", area.x),
        ("y", area.y),
        ("w", area.width),
        ("h", area.height),
    ])
}

/// A cell coordinate or size: Lua numbers may be floats.
fn number(table: &Table, key: &str) -> mlua::Result<u16> {
    let value: f64 = table.get(key)?;
    Ok(value.clamp(0.0, u16::MAX as f64) as u16)
}

/// A style like `{ fg = "blue", bg = "#202020", bold = true, reversed = true }`.
fn style(table: Option<Table>) -> mlua::Result<Style> {
    let Some(table) = table else {
        return Ok(Style::new());
    };
    let color = |key: &str| -> mlua::Result<Option<Color>> {
        let Some(name) = table.get::<Option<String>>(key)? else {
            return Ok(None);
        };
        Color::from_str(&name)
            .map(Some)
            .map_err(|_| mlua::Error::runtime(format!("{name} is no color")))
    };
    let mut style = Style::new();
    if let Some(fg) = color("fg")? {
        style = style.fg(fg);
    }
    if let Some(bg) = color("bg")? {
        style = style.bg(bg);
    }
    let modifiers = [
        ("bold", Modifier::BOLD),
        ("dim", Modifier::DIM),
        ("italic", Modifier::ITALIC),
        ("underline", Modifier::UNDERLINED),
        ("reversed", Modifier::REVERSED),
    ];
    for (key, modifier) in modifiers {
        if table.get::<Option<bool>>(key)?.unwrap_or(false) {
            style = style.add_modifier(modifier);
        }
    }
    Ok(style)
}

/// A plugin path: `~/` is the home; a relative one starts from `base`.
fn resolve(path: &str, base: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest),
        None => base.join(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Status, tests::sample};

    fn plugins(code: &str) -> Plugins {
        static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let count = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("nui-yalij-plugin-{}-{count}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.lua"), code).unwrap();
        let mut plugins = Plugins::new("sh".into());
        plugins.sync(&["plugin.lua".into()], &dir);
        assert!(plugins.take_notices().is_empty());
        plugins
    }

    #[test]
    fn layouts_place_and_draw() {
        let plugins = plugins(
            r#"return { layouts = { bar = function(c)
                local a = c.area
                return {
                    areas = { [c.active] = { x = a.x, y = a.y + 1, w = a.w, h = a.h - 1 } },
                    draw = { { x = a.x, y = a.y, text = c.children[c.active].name, style = { fg = "blue", bold = true } } },
                }
            end } }"#,
        );
        let container = Container {
            kind: Level::Window,
            area: Rect::new(0, 0, 10, 5),
            active: 1,
            children: vec!["a".into(), "b".into()],
        };
        let placement = plugins.layout("bar", &container).unwrap();
        assert_eq!(placement.areas, [None, Some(Rect::new(0, 1, 10, 4))]);
        assert_eq!(placement.draws[0].text, "b");
        assert!(plugins.layout("tabs", &container).is_none());
        assert!(plugins.take_notices().is_empty());
        plugins.layout("nope", &container);
        assert_eq!(plugins.take_notices().len(), 1);
    }

    #[test]
    fn keys_and_hooks_manage_the_state() {
        let plugins = plugins(
            r#"return {
                keys = { navigate = { n = function(state)
                    local id = nui.next_id(state)
                    local session = state.sessions[1]
                    table.insert(session.terminals, { id = id, command = nui.shell })
                    table.insert(session.workspaces[1].windows[1].panes, { terminal = id })
                end } },
                update = function(state)
                    local location = { session = 1, workspace = 1, window = 1, pane = 1 }
                    print("panes", #state.sessions[1].workspaces[1].windows[1].panes)
                    return nui.stop(state, "pane", location)
                end,
            }"#,
        );
        let next = plugins.key("navigate", "n", &sample()).unwrap().unwrap();
        let window = &next.sessions[0].workspaces[0].windows[0];
        assert_eq!(window.panes.len(), 3);
        assert_eq!(window.panes[2].terminal, 4);
        assert!(plugins.key("view", "n", &next).is_none());
        let hooked = plugins.update(next).unwrap();
        assert_eq!(hooked.sessions[0].terminals[0].status, Status::Stopped);
        assert_eq!(plugins.take_notices(), ["panes\t3"]);
    }

    #[test]
    fn keys_get_viewer_context_and_hooks_get_session_identity() {
        let mut plugins = plugins(
            r#"return {
            keys = { view = { x = function(s, c)
                assert(c.id == 7 and c.session == 1 and c.profile == "work")
                s.message = "viewer"
            end } },
            update = function(s, id) assert(id == 1); s.message = "owner" end,
        }"#,
        );
        plugins.client = Some(ClientContext {
            id: 7,
            session: 1,
            profile: Some("work".into()),
        });
        assert_eq!(
            plugins
                .key("view", "x", &sample())
                .unwrap()
                .unwrap()
                .message
                .as_deref(),
            Some("viewer")
        );
        assert_eq!(
            plugins.update(sample()).unwrap().message.as_deref(),
            Some("owner")
        );
    }

    #[test]
    fn errors_are_reported() {
        let plugins =
            plugins("return { keys = { view = { x = function(state) state.sessions = 7 end } } }");
        let error = plugins.key("view", "x", &sample()).unwrap().unwrap_err();
        assert!(error.starts_with("plugin plugin.lua, key x"), "{error}");
    }
}
