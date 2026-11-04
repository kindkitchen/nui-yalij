//! The Nui state in KDL: the format of the state file.
//!
//! ```kdl
//! toggle "ctrl-g"
//! attached 1
//! plugin "plugins/example.lua"
//! session "main" active=1 {
//!     terminal 1 command="/bin/zsh" cwd="/home/me" pid=4242 program="vim"
//!     window {
//!         tab layout="columns" {
//!             pane terminal=1
//!             pane "logs" terminal=1
//!         }
//!     }
//! }
//! navigation focus="pane" session=1 window=1 tab=1 pane=2 {
//!     rename "draft"
//! }
//! ```
//!
//! Indices count from 1. Defaults are left out when writing: `layout="tabs"`,
//! `active=1`, `status="running"`.

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};

use crate::state::{
    DEFAULT_LAYOUT, Level, Location, Navigation, Nui, Operation, Pane, Session, Status, Tab, Terminal, Window,
};

const HEADER: &str = "// nui-yalij state: the single source of truth, applied live when changed.\n";

/// The state as a KDL document.
pub fn encode(nui: &Nui) -> String {
    let mut document = KdlDocument::new();
    let nodes = document.nodes_mut();
    nodes.push(node("toggle").arg(nui.toggle.as_str()).0);
    nodes.push(node("attached").arg(index(nui.attached)).0);
    nodes.extend(nui.plugins.iter().map(|plugin| node("plugin").arg(plugin.as_str()).0));
    nodes.extend(nui.sessions.iter().map(session));
    nodes.extend(nui.navigation.as_ref().map(navigation));
    nodes.extend(nui.message.as_deref().map(|message| node("message").arg(message).0));
    document.autoformat();
    format!("{HEADER}{document}")
}

fn session(session: &Session) -> KdlNode {
    let mut node = container(node("session").arg(session.name.as_str()), &session.layout, session.active);
    for terminal in &session.terminals {
        node = node.child(self::terminal(terminal));
    }
    for window in &session.windows {
        let mut child = container(named("window", &window.name), &window.layout, window.active);
        for tab in &window.tabs {
            let mut grandchild = container(named("tab", &tab.name), &tab.layout, tab.active);
            for pane in &tab.panes {
                grandchild = grandchild.child(named("pane", &pane.name).prop("terminal", pane.terminal as i128).0);
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
        let args = terminal.args.iter().fold(self::node("args"), |args, arg| args.arg(arg.as_str()));
        node = node.child(args.0);
    }
    node.0
}

fn navigation(navigation: &Navigation) -> KdlNode {
    let cursor = navigation.cursor;
    let mut node = node("navigation").prop("focus", navigation.focus.name());
    for level in Level::ALL {
        node = node.prop(level.name(), index(cursor.get(level)));
    }
    match &navigation.operation {
        Some(Operation::Rename { input }) => node = node.child(self::node("rename").arg(input.as_str()).0),
        Some(Operation::Create { input }) => node = node.child(self::node("create").arg(input.as_str()).0),
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
struct Node(KdlNode);

fn node(name: &str) -> Node {
    Node(KdlNode::new(name))
}

impl Node {
    fn arg(mut self, value: impl Into<KdlValue>) -> Node {
        self.0.push(KdlEntry::new(value));
        self
    }

    fn prop(mut self, key: &str, value: impl Into<KdlValue>) -> Node {
        self.0.push(KdlEntry::new_prop(key, value));
        self
    }

    fn option(self, key: &str, value: Option<impl Into<KdlValue>>) -> Node {
        match value {
            Some(value) => self.prop(key, value),
            None => self,
        }
    }

    fn child(mut self, child: KdlNode) -> Node {
        self.0.ensure_children().nodes_mut().push(child);
        self
    }
}

/// The state from a KDL document.
pub fn decode(text: &str) -> Result<Nui, String> {
    let document: KdlDocument = text.parse().map_err(|error: kdl::KdlError| {
        let diagnostics = error.diagnostics.iter().map(|diagnostic| {
            let line = text.chars().take(diagnostic.span.offset()).filter(|&c| c == '\n').count() + 1;
            let what = diagnostic.message.as_deref().or(diagnostic.label.as_deref()).unwrap_or("invalid KDL");
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
            "toggle" => nui.toggle = simple()?.string_arg()?,
            "attached" => nui.attached = simple()?.index_arg()?,
            "plugin" => nui.plugins.push(simple()?.string_arg()?),
            "message" => nui.message = Some(simple()?.string_arg()?),
            "session" => nui.sessions.push(decoder.session(node)?),
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
        let line = self.text.as_bytes()[..offset].iter().filter(|&&b| b == b'\n').count() + 1;
        format!("line {line}, {}: {message}", node.name().value())
    }

    /// The entries of `node`, failing on a property not in `props`.
    fn read(&'a self, node: &'a KdlNode, props: &[&str]) -> Result<Entries<'a>, String> {
        let mut args = Vec::new();
        for entry in node.entries() {
            match entry.name() {
                None => args.push(entry.value()),
                Some(key) if props.contains(&key.value()) => {}
                Some(key) => return Err(self.error(node, &format!("unknown property {}", key.value()))),
            }
        }
        Ok(Entries { decoder: self, node, args })
    }

    fn children(&self, node: &'a KdlNode, allowed: &[&str]) -> Result<Vec<&'a KdlNode>, String> {
        let children: Vec<&KdlNode> = node.children().map(|document| document.nodes().iter().collect()).unwrap_or_default();
        match children.iter().find(|child| !allowed.contains(&child.name().value())) {
            Some(child) => Err(self.error(child, "unknown node here")),
            None => Ok(children),
        }
    }

    fn session(&self, node: &KdlNode) -> Result<Session, String> {
        let entries = self.read(node, &["layout", "active"])?;
        let mut session = Session {
            name: entries.string_arg()?,
            layout: entries.layout()?,
            active: entries.index("active")?.unwrap_or(0),
            ..Session::default()
        };
        for child in self.children(node, &["terminal", "window"])? {
            match child.name().value() {
                "terminal" => session.terminals.push(self.terminal(child)?),
                _ => session.windows.push(self.window(child)?),
            }
        }
        Ok(session)
    }

    fn window(&self, node: &KdlNode) -> Result<Window, String> {
        let entries = self.read(node, &["layout", "active"])?;
        let tabs = self.children(node, &["tab"])?.into_iter().map(|child| self.tab(child));
        Ok(Window {
            name: entries.optional_string_arg()?,
            layout: entries.layout()?,
            active: entries.index("active")?.unwrap_or(0),
            tabs: tabs.collect::<Result<_, _>>()?,
        })
    }

    fn tab(&self, node: &KdlNode) -> Result<Tab, String> {
        let entries = self.read(node, &["layout", "active"])?;
        let panes = self.children(node, &["pane"])?.into_iter().map(|child| self.pane(child));
        Ok(Tab {
            name: entries.optional_string_arg()?,
            layout: entries.layout()?,
            active: entries.index("active")?.unwrap_or(0),
            panes: panes.collect::<Result<_, _>>()?,
        })
    }

    fn pane(&self, node: &KdlNode) -> Result<Pane, String> {
        let entries = self.read(node, &["terminal"])?;
        self.children(node, &[])?;
        let terminal = entries.number("terminal")?.ok_or_else(|| self.error(node, "terminal=<id> is missing"))?;
        Ok(Pane { name: entries.optional_string_arg()?, terminal })
    }

    fn terminal(&self, node: &KdlNode) -> Result<Terminal, String> {
        let entries = self.read(node, &["command", "cwd", "status", "exit", "pid", "program", "title"])?;
        let id = match entries.args.as_slice() {
            [KdlValue::Integer(id)] => u64::try_from(*id).map_err(|_| self.error(node, "the id should be positive"))?,
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
            args.extend(self.read(child, &[])?.args.iter().map(|arg| string(self, child, arg)).collect::<Result<Vec<_>, _>>()?);
        }
        Ok(Terminal {
            id,
            command: entries.string("command")?.ok_or_else(|| self.error(node, "command=<program> is missing"))?,
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
        let entries = self.read(node, &["focus", "session", "window", "tab", "pane"])?;
        let focus = entries.string("focus")?.unwrap_or_else(|| "pane".into());
        let focus = Level::parse(&focus).ok_or_else(|| self.error(node, &format!("unknown focus {focus}")))?;
        let mut cursor = Location::default();
        for level in Level::ALL {
            cursor.set(level, entries.index(level.name())?.unwrap_or(0));
        }
        let mut operation = None;
        for child in self.children(node, &["rename", "create", "remove", "stop"])? {
            let entries = self.read(child, &[])?;
            operation = Some(match child.name().value() {
                "rename" => Operation::Rename { input: entries.optional_string_arg()?.unwrap_or_default() },
                "create" => Operation::Create { input: entries.optional_string_arg()?.unwrap_or_default() },
                "remove" => Operation::Remove,
                _ => Operation::Stop,
            });
        }
        Ok(Navigation { focus, cursor, operation })
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
        self.optional_string_arg()?.ok_or_else(|| self.error("an argument is missing"))
    }

    fn index_arg(&self) -> Result<usize, String> {
        match self.args.as_slice() {
            [value] => position(self.decoder, self.node, value),
            _ => Err(self.error("one argument expected: an index")),
        }
    }

    fn string(&self, key: &str) -> Result<Option<String>, String> {
        self.node.get(key).map(|value| string(self.decoder, self.node, value)).transpose()
    }

    fn number<T: TryFrom<i128>>(&self, key: &str) -> Result<Option<T>, String> {
        match self.node.get(key) {
            None => Ok(None),
            Some(KdlValue::Integer(n)) => T::try_from(*n).map(Some).map_err(|_| self.error(&format!("{key} is out of range"))),
            Some(_) => Err(self.error(&format!("{key} should be a number"))),
        }
    }

    fn index(&self, key: &str) -> Result<Option<usize>, String> {
        self.node.get(key).map(|value| position(self.decoder, self.node, value)).transpose()
    }

    fn layout(&self) -> Result<String, String> {
        Ok(self.string("layout")?.unwrap_or_else(|| DEFAULT_LAYOUT.into()))
    }
}

fn string(decoder: &Decoder, node: &KdlNode, value: &KdlValue) -> Result<String, String> {
    value.as_string().map(str::to_string).ok_or_else(|| decoder.error(node, &format!("{value} should be a string")))
}

/// An index as written, counting from 1.
fn position(decoder: &Decoder, node: &KdlNode, value: &KdlValue) -> Result<usize, String> {
    match value {
        KdlValue::Integer(n) if *n >= 1 => usize::try_from(*n - 1).map_err(|_| decoder.error(node, "index out of range")),
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
        nui.plugins.push("plugins/example.lua".into());
        nui.sessions[0].layout = "columns".into();
        nui.sessions[0].active = 1;
        let terminal = &mut nui.sessions[0].terminals[1];
        terminal.args = vec!["-d".into(), "1 0".into()];
        terminal.status = Status::Exited;
        terminal.exit = Some(2);
        terminal.title = Some("a \"quoted\" title".into());
        nui.sessions[0].windows[0].tabs[0].panes[0].name = Some("logs".into());
        nui.navigation = Some(Navigation {
            focus: Level::Tab,
            cursor: Location { session: 0, window: 0, tab: 1, pane: 0 },
            operation: Some(Operation::Rename { input: "draft".into() }),
        });
        nui.message = Some("hello".into());
        let text = encode(&nui);
        assert_eq!(decode(&text).unwrap(), nui, "{text}");
        nui.navigation.as_mut().unwrap().operation = Some(Operation::Create { input: String::new() });
        assert_eq!(decode(&encode(&nui)).unwrap(), nui);
    }

    #[test]
    fn reads_what_people_write() {
        let text = r#"
            session main {
                terminal 1 command=nu
                window "dev" { tab { pane terminal=1; pane "again" terminal=1 } }
            }
        "#;
        let nui = decode(text).unwrap();
        assert_eq!(nui.toggle, "ctrl-g");
        let tab = &nui.sessions[0].windows[0].tabs[0];
        assert_eq!((tab.layout.as_str(), tab.panes.len()), ("tabs", 2));
        assert_eq!(nui.sessions[0].windows[0].name.as_deref(), Some("dev"));
    }

    #[test]
    fn errors_say_where() {
        let error = decode("session main {\n  window {\n    tab layuot=columns\n  }\n}").unwrap_err();
        assert!(error.starts_with("line 3, tab: unknown property layuot"), "{error}");
        assert!(decode("session main {").unwrap_err().starts_with("line 1"));
        assert!(decode("attached 0").is_err());
    }
}
