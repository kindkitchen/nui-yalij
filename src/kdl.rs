//! Session state in KDL. The decoder also reads the legacy aggregate shape
//! below for migration; new persistence uses encode_session, without presence.
//!
//! ```kdl
//! attached 1
//! session "main" active=1 {
//!     terminal 1 command="/bin/zsh" cwd="/home/me" pid=4242 program="vim"
//!     workspace {
//!         window layout="columns" {
//!             pane terminal=1
//!             pane "logs" terminal=1
//!         }
//!     }
//! }
//! navigation focus="pane" session=1 workspace=1 window=1 pane=2 {
//!     rename "draft"
//! }
//! ```
//!
//! Indices count from 1. Defaults are left out when writing: `layout="tabs"`,
//! `active=1`, `status="running"`, `representation="native"`. The older
//! shape, sessions of `window`s of `tab`s, still reads: the same levels under
//! their former names.

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};

use crate::state::{
    DEFAULT_LAYOUT, Level, Location, Navigation, Nui, Operation, Pane, Session,
    StateRepresentation, Status, Terminal, Window, Workspace,
};

const HEADER: &str = "// nui-yalij state: the single source of truth, applied live when changed.\n";

/// Aggregate projection codec, used to verify legacy/Lua-compatible shapes.
#[cfg(test)]
pub fn encode(nui: &Nui) -> String {
    let mut document = KdlDocument::new();
    let nodes = document.nodes_mut();
    nodes.push(node("attached").arg(index(nui.attached)).0);
    nodes.extend(nui.sessions.iter().map(session));
    nodes.extend(nui.navigation.as_ref().map(navigation));
    nodes.extend(
        nui.message
            .as_deref()
            .map(|message| node("message").arg(message).0),
    );
    document.autoformat();
    format!("{HEADER}{document}")
}

pub fn encode_session(state: &Session) -> String {
    let mut document = KdlDocument::new();
    document.nodes_mut().push(session(state));
    document.autoformat();
    format!("{HEADER}{document}")
}

fn session(session: &Session) -> KdlNode {
    let mut node = container(
        node("session")
            .arg(session.name.as_str())
            .prop("id", session.id as i128),
        &session.layout,
        session.active,
    );
    for terminal in &session.terminals {
        node = node.child(self::terminal(terminal));
    }
    for workspace in &session.workspaces {
        let mut child = container(
            named("workspace", &workspace.name),
            &workspace.layout,
            workspace.active,
        );
        for window in &workspace.windows {
            let mut grandchild =
                container(named("window", &window.name), &window.layout, window.active);
            for pane in &window.panes {
                grandchild = grandchild.child(
                    named("pane", &pane.name)
                        .prop("terminal", pane.terminal as i128)
                        .0,
                );
            }
            child = child.child(grandchild.0);
        }
        node = node.child(child.0);
    }
    node.0
}

fn terminal(terminal: &Terminal) -> KdlNode {
    let mut node = node("terminal")
        .arg(terminal.id as i128)
        .prop("command", terminal.command.as_str())
        .option("cwd", terminal.cwd.as_deref());
    if terminal.status != Status::Running {
        node = node.prop("status", status_name(terminal.status));
    }
    node = node
        .option("exit", terminal.exit.map(i128::from))
        .option("pid", terminal.pid.map(i128::from))
        .option("program", terminal.program.as_deref())
        .option("title", terminal.title.as_deref());
    if !terminal.args.is_empty() {
        let args = terminal
            .args
            .iter()
            .fold(self::node("args"), |args, arg| args.arg(arg.as_str()));
        node = node.child(args.0);
    }
    node.0
}

pub(crate) fn navigation(navigation: &Navigation) -> KdlNode {
    let cursor = navigation.cursor;
    let mut node = node("navigation");
    if navigation.representation != StateRepresentation::Native {
        node = node.prop("representation", navigation.representation.name());
    }
    node = node.prop("focus", navigation.focus.name());
    for level in Level::ALL {
        node = node.prop(level.name(), index(cursor.get(level)));
    }
    node = node.option("folder", navigation.folder.as_deref());
    match &navigation.operation {
        Some(Operation::Rename { input }) => {
            node = node.child(self::node("rename").arg(input.as_str()).0)
        }
        Some(Operation::Create { input }) => {
            node = node.child(self::node("create").arg(input.as_str()).0)
        }
        Some(Operation::Remove) => node = node.child(self::node("remove").0),
        Some(Operation::Stop) => node = node.child(self::node("stop").0),
        None => {}
    }
    node.0
}

/// A container node: its layout and active child, unless the defaults.
fn container(mut node: Node, layout: &str, active: usize) -> Node {
    if layout != DEFAULT_LAYOUT {
        node = node.prop("layout", layout);
    }
    if active != 0 {
        node = node.prop("active", index(active));
    }
    node
}

fn named(name: &str, label: &Option<String>) -> Node {
    match label {
        Some(label) => node(name).arg(label.as_str()),
        None => node(name),
    }
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Running => "running",
        Status::Stopped => "stopped",
        Status::Exited => "exited",
    }
}

fn index(index: usize) -> i128 {
    index as i128 + 1
}

/// A node under construction.
pub(crate) struct Node(pub KdlNode);

pub(crate) fn node(name: &str) -> Node {
    Node(KdlNode::new(name))
}

impl Node {
    pub(crate) fn arg(mut self, value: impl Into<KdlValue>) -> Node {
        self.0.push(KdlEntry::new(value));
        self
    }

    pub(crate) fn prop(mut self, key: &str, value: impl Into<KdlValue>) -> Node {
        self.0.push(KdlEntry::new_prop(key, value));
        self
    }

    fn option(self, key: &str, value: Option<impl Into<KdlValue>>) -> Node {
        match value {
            Some(value) => self.prop(key, value),
            None => self,
        }
    }

    pub(crate) fn child(mut self, child: KdlNode) -> Node {
        self.0.ensure_children().nodes_mut().push(child);
        self
    }
}

/// The state from a KDL document.
pub fn decode(text: &str) -> Result<Nui, String> {
    let document: KdlDocument = text.parse().map_err(|error: kdl::KdlError| {
        let diagnostics = error.diagnostics.iter().map(|diagnostic| {
            let line = text
                .chars()
                .take(diagnostic.span.offset())
                .filter(|&c| c == '\n')
                .count()
                + 1;
            let what = diagnostic
                .message
                .as_deref()
                .or(diagnostic.label.as_deref())
                .unwrap_or("invalid KDL");
            format!("line {line}: {what}")
        });
        diagnostics.collect::<Vec<_>>().join("; ")
    })?;
    let decoder = Decoder { text };
    let mut nui = Nui::default();
    for node in document.nodes() {
        let simple = || {
            decoder.children(node, &[])?;
            decoder.read(node, &[])
        };
        match node.name().value() {
            // Legacy configuration is deliberately not part of state anymore.
            "toggle" | "plugin" => {
                simple()?.string_arg()?;
            }
            "attached" => nui.attached = simple()?.index_arg()?,
            "message" => nui.message = Some(simple()?.string_arg()?),
            "session" => {
                let mut session = decoder.session(node)?;
                if session.id == 0 {
                    session.id = nui.next_session_id();
                }
                nui.sessions.push(session);
            }
            "navigation" => nui.navigation = Some(decoder.navigation(node)?),
            other => return Err(decoder.error(node, &format!("unknown node {other}"))),
        }
    }
    Ok(nui)
}

struct Decoder<'a> {
    text: &'a str,
}

/// The entries of a node: its arguments, checked properties and children.
struct Entries<'a> {
    decoder: &'a Decoder<'a>,
    node: &'a KdlNode,
    args: Vec<&'a KdlValue>,
}

impl<'a> Decoder<'a> {
    fn error(&self, node: &KdlNode, message: &str) -> String {
        let offset = node.span().offset().min(self.text.len());
        let line = self.text.as_bytes()[..offset]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1;
        format!("line {line}, {}: {message}", node.name().value())
    }

    /// The entries of `node`, failing on a property not in `props`.
    fn read(&'a self, node: &'a KdlNode, props: &[&str]) -> Result<Entries<'a>, String> {
        let mut args = Vec::new();
        for entry in node.entries() {
            match entry.name() {
                None => args.push(entry.value()),
                Some(key) if props.contains(&key.value()) => {}
                Some(key) => {
                    return Err(self.error(node, &format!("unknown property {}", key.value())));
                }
            }
        }
        Ok(Entries {
            decoder: self,
            node,
            args,
        })
    }

    fn children(&self, node: &'a KdlNode, allowed: &[&str]) -> Result<Vec<&'a KdlNode>, String> {
        let children: Vec<&KdlNode> = node
            .children()
            .map(|document| document.nodes().iter().collect())
            .unwrap_or_default();
        match children
            .iter()
            .find(|child| !allowed.contains(&child.name().value()))
        {
            Some(child) => Err(self.error(child, "unknown node here")),
            None => Ok(children),
        }
    }

    fn session(&self, node: &KdlNode) -> Result<Session, String> {
        let entries = self.read(node, &["id", "layout", "active"])?;
        let mut session = Session {
            id: entries.number("id")?.unwrap_or(0),
            name: entries.string_arg()?,
            layout: entries.layout()?,
            active: entries.index("active")?.unwrap_or(0),
            ..Session::default()
        };
        for child in self.children(node, &["terminal", "workspace", "window"])? {
            match child.name().value() {
                "terminal" => session.terminals.push(self.terminal(child)?),
                // the older shape: a window of tabs is a workspace of windows
                "window" => session.workspaces.push(self.workspace(child, "tab")?),
                _ => session.workspaces.push(self.workspace(child, "window")?),
            }
        }
        Ok(session)
    }

    /// A workspace, its windows in children named `window`.
    fn workspace(&self, node: &KdlNode, window: &str) -> Result<Workspace, String> {
        let entries = self.read(node, &["layout", "active"])?;
        let windows = self
            .children(node, &[window])?
            .into_iter()
            .map(|child| self.window(child));
        Ok(Workspace {
            name: entries.optional_string_arg()?,
            layout: entries.layout()?,
            active: entries.index("active")?.unwrap_or(0),
            windows: windows.collect::<Result<_, _>>()?,
        })
    }

    fn window(&self, node: &KdlNode) -> Result<Window, String> {
        let entries = self.read(node, &["layout", "active"])?;
        let panes = self
            .children(node, &["pane"])?
            .into_iter()
            .map(|child| self.pane(child));
        Ok(Window {
            name: entries.optional_string_arg()?,
            layout: entries.layout()?,
            active: entries.index("active")?.unwrap_or(0),
            panes: panes.collect::<Result<_, _>>()?,
        })
    }

    fn pane(&self, node: &KdlNode) -> Result<Pane, String> {
        let entries = self.read(node, &["terminal"])?;
        self.children(node, &[])?;
        let terminal = entries
            .number("terminal")?
            .ok_or_else(|| self.error(node, "terminal=<id> is missing"))?;
        Ok(Pane {
            name: entries.optional_string_arg()?,
            terminal,
        })
    }

    fn terminal(&self, node: &KdlNode) -> Result<Terminal, String> {
        let entries = self.read(
            node,
            &[
                "command", "cwd", "status", "exit", "pid", "program", "title",
            ],
        )?;
        let id = match entries.args.as_slice() {
            [KdlValue::Integer(id)] => {
                u64::try_from(*id).map_err(|_| self.error(node, "the id should be positive"))?
            }
            _ => return Err(self.error(node, "one argument expected: the id")),
        };
        let status = match entries.string("status")?.as_deref() {
            None | Some("running") => Status::Running,
            Some("stopped") => Status::Stopped,
            Some("exited") => Status::Exited,
            Some(other) => return Err(self.error(node, &format!("unknown status {other}"))),
        };
        let mut args = Vec::new();
        for child in self.children(node, &["args"])? {
            args.extend(
                self.read(child, &[])?
                    .args
                    .iter()
                    .map(|arg| string(self, child, arg))
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        Ok(Terminal {
            id,
            command: entries
                .string("command")?
                .ok_or_else(|| self.error(node, "command=<program> is missing"))?,
            args,
            cwd: entries.string("cwd")?,
            status,
            exit: entries.number("exit")?,
            pid: entries.number("pid")?,
            program: entries.string("program")?,
            title: entries.string("title")?,
        })
    }

    fn navigation(&self, node: &KdlNode) -> Result<Navigation, String> {
        let entries = self.read(
            node,
            &[
                "representation",
                "focus",
                "session",
                "workspace",
                "window",
                "tab",
                "pane",
                "folder",
            ],
        )?;
        let representation = match entries.string("representation")? {
            None => StateRepresentation::Native,
            Some(name) => StateRepresentation::parse(&name)
                .ok_or_else(|| self.error(node, &format!("unknown representation {name}")))?,
        };
        let focus = entries.string("focus")?.unwrap_or_else(|| "pane".into());
        // the older shape named workspaces `window` and windows `tab`
        let older = node.get("tab").is_some() || focus == "tab";
        let name = |level: Level| match (older, level) {
            (true, Level::Workspace) => "window",
            (true, Level::Window) => "tab",
            _ => level.name(),
        };
        let focus = Level::ALL
            .into_iter()
            .find(|level| name(*level) == focus)
            .ok_or_else(|| self.error(node, &format!("unknown focus {focus}")))?;
        let mut cursor = Location::default();
        for level in Level::ALL {
            cursor.set(level, entries.index(name(level))?.unwrap_or(0));
        }
        let mut operation = None;
        for child in self.children(node, &["rename", "create", "remove", "stop"])? {
            let entries = self.read(child, &[])?;
            operation = Some(match child.name().value() {
                "rename" => Operation::Rename {
                    input: entries.optional_string_arg()?.unwrap_or_default(),
                },
                "create" => Operation::Create {
                    input: entries.optional_string_arg()?.unwrap_or_default(),
                },
                "remove" => Operation::Remove,
                _ => Operation::Stop,
            });
        }
        Ok(Navigation {
            representation,
            focus,
            cursor,
            folder: entries.string("folder")?,
            operation,
        })
    }
}

impl Entries<'_> {
    fn error(&self, message: &str) -> String {
        self.decoder.error(self.node, message)
    }

    fn optional_string_arg(&self) -> Result<Option<String>, String> {
        match self.args.as_slice() {
            [] => Ok(None),
            [value] => string(self.decoder, self.node, value).map(Some),
            _ => Err(self.error("one argument expected")),
        }
    }

    fn string_arg(&self) -> Result<String, String> {
        self.optional_string_arg()?
            .ok_or_else(|| self.error("an argument is missing"))
    }

    fn index_arg(&self) -> Result<usize, String> {
        match self.args.as_slice() {
            [value] => position(self.decoder, self.node, value),
            _ => Err(self.error("one argument expected: an index")),
        }
    }

    fn string(&self, key: &str) -> Result<Option<String>, String> {
        self.node
            .get(key)
            .map(|value| string(self.decoder, self.node, value))
            .transpose()
    }

    fn number<T: TryFrom<i128>>(&self, key: &str) -> Result<Option<T>, String> {
        match self.node.get(key) {
            None => Ok(None),
            Some(KdlValue::Integer(n)) => T::try_from(*n)
                .map(Some)
                .map_err(|_| self.error(&format!("{key} is out of range"))),
            Some(_) => Err(self.error(&format!("{key} should be a number"))),
        }
    }

    fn index(&self, key: &str) -> Result<Option<usize>, String> {
        self.node
            .get(key)
            .map(|value| position(self.decoder, self.node, value))
            .transpose()
    }

    fn layout(&self) -> Result<String, String> {
        Ok(self
            .string("layout")?
            .unwrap_or_else(|| DEFAULT_LAYOUT.into()))
    }
}

fn string(decoder: &Decoder, node: &KdlNode, value: &KdlValue) -> Result<String, String> {
    value
        .as_string()
        .map(str::to_string)
        .ok_or_else(|| decoder.error(node, &format!("{value} should be a string")))
}

/// An index as written, counting from 1.
fn position(decoder: &Decoder, node: &KdlNode, value: &KdlValue) -> Result<usize, String> {
    match value {
        KdlValue::Integer(n) if *n >= 1 => {
            usize::try_from(*n - 1).map_err(|_| decoder.error(node, "index out of range"))
        }
        _ => Err(decoder.error(node, &format!("{value} is no index: they count from 1"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::sample;

    #[test]
    fn round_trip() {
        let mut nui = sample();
        nui.sessions[0].layout = "columns".into();
        nui.sessions[0].active = 1;
        let terminal = &mut nui.sessions[0].terminals[1];
        terminal.args = vec!["-d".into(), "1 0".into()];
        terminal.status = Status::Exited;
        terminal.exit = Some(2);
        terminal.title = Some("a \"quoted\" title".into());
        nui.sessions[0].workspaces[0].windows[0].panes[0].name = Some("logs".into());
        nui.navigation = Some(Navigation {
            representation: StateRepresentation::Native,
            focus: Level::Window,
            cursor: Location {
                session: 0,
                workspace: 0,
                window: 1,
                pane: 0,
            },
            folder: None,
            operation: Some(Operation::Rename {
                input: "draft".into(),
            }),
        });
        nui.message = Some("hello".into());
        let text = encode(&nui);
        assert!(!text.contains("representation"), "{text}");
        assert_eq!(decode(&text).unwrap(), nui, "{text}");
        let navigation = nui.navigation.as_mut().unwrap();
        navigation.operation = Some(Operation::Create {
            input: String::new(),
        });
        assert_eq!(decode(&encode(&nui)).unwrap(), nui);
        let navigation = nui.navigation.as_mut().unwrap();
        navigation.representation = StateRepresentation::Fs;
        navigation.folder = Some("/home/me/a \"b\"".into());
        navigation.operation = None;
        let text = encode(&nui);
        assert!(text.contains("representation=fs"), "{text}");
        assert_eq!(decode(&text).unwrap(), nui, "{text}");
        assert!(decode("navigation representation=tree").is_err());
    }

    #[test]
    fn reads_what_people_write() {
        let text = r#"
            session main {
                terminal 1 command=nu
                workspace "dev" { window { pane terminal=1; pane "again" terminal=1 } }
            }
        "#;
        let nui = decode(text).unwrap();
        let window = &nui.sessions[0].workspaces[0].windows[0];
        assert_eq!((window.layout.as_str(), window.panes.len()), ("tabs", 2));
        assert_eq!(nui.sessions[0].workspaces[0].name.as_deref(), Some("dev"));
    }

    #[test]
    fn reads_the_older_shape() {
        let older = r#"
            session main active=2 {
                terminal 1 command=nu
                window "dev" { tab "logs" { pane terminal=1 } }
                window { tab layout=columns { pane terminal=1 }; tab { pane terminal=1 } }
            }
            navigation focus=tab session=1 window=2 tab=2 pane=1
        "#;
        let nui = decode(older).unwrap();
        let session = &nui.sessions[0];
        assert_eq!(session.active, 1);
        assert_eq!(session.workspaces[0].name.as_deref(), Some("dev"));
        assert_eq!(
            session.workspaces[0].windows[0].name.as_deref(),
            Some("logs")
        );
        assert_eq!(session.workspaces[1].windows[0].layout, "columns");
        let navigation = nui.navigation.as_ref().unwrap();
        assert_eq!(navigation.focus, Level::Window);
        assert_eq!(
            (navigation.cursor.workspace, navigation.cursor.window),
            (1, 1)
        );
        // and is written in the current one
        let text = encode(&nui);
        assert!(
            !text.contains("tab ") && text.contains("workspace=2 window=2"),
            "{text}"
        );
        assert_eq!(decode(&text).unwrap(), nui);
    }

    #[test]
    fn errors_say_where() {
        let error =
            decode("session main {\n  workspace {\n    window layuot=columns\n  }\n}").unwrap_err();
        assert!(
            error.starts_with("line 3, window: unknown property layuot"),
            "{error}"
        );
        assert!(decode("session main {").unwrap_err().starts_with("line 1"));
        assert!(decode("attached 0").is_err());
    }
}
