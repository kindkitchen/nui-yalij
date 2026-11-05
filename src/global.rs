//! The thin global state: session identities and connected-client presence.
//! Session contents live in independent files, each held by its session server.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use ::kdl::{KdlDocument, KdlNode, KdlValue};

use crate::{
    kdl::{self, node},
    state::{Navigation, Nui, StateRepresentation},
    store::suffixed,
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    pub sessions: Vec<u64>,
    pub clients: Vec<Client>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Client {
    pub id: u64,
    pub session: u64,
    pub profile: Option<String>,
    pub navigation: Option<Navigation>,
    /// The representation navigation shows, kept while it is closed.
    pub representation: StateRepresentation,
    pub message: Option<String>,
}

pub fn session_dir(global: &Path, id: u64) -> PathBuf {
    suffixed(global, ".sessions").join(id.to_string())
}

pub fn session_path(global: &Path, id: u64) -> PathBuf {
    session_dir(global, id).join("state.kdl")
}

impl Catalog {
    pub fn client(&self, id: u64) -> Option<&Client> {
        self.clients.iter().find(|client| client.id == id)
    }
    pub fn client_mut(&mut self, id: u64) -> Option<&mut Client> {
        self.clients.iter_mut().find(|client| client.id == id)
    }

    pub fn encode(&self) -> String {
        let mut root = node("catalog").prop("version", 1);
        for id in &self.sessions {
            root = root.child(node("session").arg(*id as i128).0);
        }
        for client in &self.clients {
            let mut child = node("client")
                .arg(client.id as i128)
                .prop("session", client.session as i128);
            if let Some(profile) = &client.profile {
                child = child.prop("profile", profile.as_str());
            }
            if client.representation != StateRepresentation::Native {
                child = child.prop("representation", client.representation.name());
            }
            if let Some(nav) = &client.navigation {
                let mut nav = nav.clone();
                // A navigation location uses indices in memory, stable ids on disk.
                nav.cursor.session = self
                    .sessions
                    .get(nav.cursor.session)
                    .copied()
                    .unwrap_or(client.session) as usize
                    - 1;
                child = child.child(kdl::navigation(&nav));
            }
            if let Some(message) = &client.message {
                child = child.child(node("message").arg(message.as_str()).0);
            }
            root = root.child(child.0);
        }
        let mut doc = KdlDocument::new();
        doc.nodes_mut().push(root.0);
        doc.autoformat();
        format!("// nui-yalij global state: session catalog and live client presence.\n{doc}")
    }

    pub fn decode(text: &str) -> Result<Self, String> {
        let doc: KdlDocument = text
            .parse()
            .map_err(|error| format!("global state: {error}"))?;
        let [root] = doc.nodes() else {
            return Err("expected one catalog node".into());
        };
        check(root, "catalog", &["version"], &["session", "client"], 0)?;
        if root.get("version").and_then(KdlValue::as_integer) != Some(1) {
            return Err("unsupported catalog version (expected 1)".into());
        }
        let mut catalog = Catalog::default();
        let mut ids = HashSet::new();
        let mut clients = HashSet::new();
        for child in children(root) {
            match child.name().value() {
                "session" => {
                    check(child, "session", &[], &[], 1)?;
                    let id = positive(child.get(0), "session id")?;
                    if !ids.insert(id) {
                        return Err(format!("session {id} is listed twice"));
                    }
                    catalog.sessions.push(id);
                }
                "client" => {
                    check(
                        child,
                        "client",
                        &["session", "profile", "representation"],
                        &["navigation", "message"],
                        1,
                    )?;
                    let id = positive(child.get(0), "client id")?;
                    if !clients.insert(id) {
                        return Err(format!("client {id} is listed twice"));
                    }
                    let session = positive(child.get("session"), "client session")?;
                    let profile = optional_string(child.get("profile"), "profile")?;
                    let representation =
                        match optional_string(child.get("representation"), "representation")? {
                            None => StateRepresentation::Native,
                            Some(name) => StateRepresentation::parse(&name).ok_or_else(|| {
                                format!("client {id}: unknown representation {name}")
                            })?,
                        };
                    let mut projection = Nui::default();
                    if !children(child).is_empty() {
                        let inner = children(child)
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("\n");
                        projection = kdl::decode(&inner)?;
                    }
                    catalog.clients.push(Client {
                        id,
                        session,
                        profile,
                        navigation: projection.navigation,
                        representation,
                        message: projection.message,
                    });
                }
                _ => unreachable!(),
            }
        }
        for client in &mut catalog.clients {
            if !ids.contains(&client.session) {
                return Err(format!(
                    "client {}: unknown session {}",
                    client.id, client.session
                ));
            }
            if let Some(nav) = &mut client.navigation {
                let id = nav.cursor.session as u64 + 1;
                nav.cursor.session = catalog
                    .sessions
                    .iter()
                    .position(|known| *known == id)
                    .ok_or_else(|| {
                        format!(
                            "client {}: navigation names unknown session {id}",
                            client.id
                        )
                    })?;
            }
        }
        Ok(catalog)
    }
}

pub(crate) fn children(node: &KdlNode) -> &[KdlNode] {
    node.children().map(|doc| doc.nodes()).unwrap_or_default()
}

pub(crate) fn optional_string(
    value: Option<&KdlValue>,
    what: &str,
) -> Result<Option<String>, String> {
    value
        .map(|value| {
            value
                .as_string()
                .map(str::to_owned)
                .ok_or_else(|| format!("{what} must be a string"))
        })
        .transpose()
}

pub(crate) fn positive(value: Option<&KdlValue>, what: &str) -> Result<u64, String> {
    value
        .and_then(KdlValue::as_integer)
        .and_then(|n| u64::try_from(n).ok())
        .filter(|n| *n > 0 && *n < usize::MAX as u64)
        .ok_or_else(|| format!("{what} must be a positive integer"))
}

pub(crate) fn check(
    node: &KdlNode,
    name: &str,
    props: &[&str],
    allowed: &[&str],
    args: usize,
) -> Result<(), String> {
    let mut seen = HashSet::new();
    if node.name().value() != name {
        return Err(format!("expected {name}"));
    }
    let mut count = 0;
    for entry in node.entries() {
        match entry.name() {
            None => count += 1,
            Some(key) if props.contains(&key.value()) && seen.insert(key.value()) => {}
            Some(key) => {
                return Err(format!(
                    "{name}: unknown or duplicate property {}",
                    key.value()
                ));
            }
        }
    }
    if count != args {
        return Err(format!("{name}: expected {args} argument(s)"));
    }
    if let Some(child) = children(node)
        .iter()
        .find(|child| !allowed.contains(&child.name().value()))
    {
        return Err(format!("{name}: unknown child {}", child.name().value()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Level, Location, StateRepresentation};

    #[test]
    fn catalog_is_thin_and_navigation_uses_stable_ids() {
        let catalog = Catalog {
            sessions: vec![8, 3],
            clients: vec![Client {
                id: 5,
                session: 8,
                profile: Some("work".into()),
                message: Some("hello".into()),
                navigation: Some(Navigation {
                    focus: Level::Pane,
                    cursor: Location {
                        session: 1,
                        ..Location::default()
                    },
                    folder: Some("/home/me".into()),
                    operation: None,
                }),
                representation: StateRepresentation::Fs,
            }],
        };
        let text = catalog.encode();
        assert!(
            text.contains("session=3") && text.contains("representation=fs"),
            "{text}"
        );
        assert!(!text.contains("terminal"));
        assert_eq!(Catalog::decode(&text).unwrap(), catalog);
    }

    #[test]
    fn invalid_catalogs_are_rejected() {
        for text in [
            "catalog version=2",
            "catalog version=1 { session 0 }",
            "catalog version=1 { session 1; session 1 }",
            "catalog version=1 { client 1 session=2 }",
            "catalog version=1 { session 1; client 1 session=1 representation=tree }",
            "catalog version=1 oops=1",
        ] {
            assert!(Catalog::decode(text).is_err(), "{text}");
        }
    }
}
