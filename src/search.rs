//! Navigation's live query and captured, fuzzy-ranked pane list. Matching is
//! in-process, with fzf-style atoms (AND, OR, exact, inverse, prefix, suffix).

use std::cmp::Reverse;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::{
    Matcher, Utf32Str,
    pattern::{Atom, AtomKind, CaseMatching, Normalization, Pattern},
};

use crate::{
    nav::Outcome,
    state::{Level, Location, Nui, Search, SearchHit},
};

impl SearchHit {
    pub fn resolve(&self, nui: &Nui) -> Option<Location> {
        let session = nui
            .sessions
            .iter()
            .position(|session| session.id == self.session)?;
        let at = Location {
            session,
            workspace: self.workspace,
            window: self.window,
            pane: self.pane,
        };
        if nui
            .pane(at)
            .is_some_and(|pane| pane.terminal == self.terminal)
        {
            return Some(at);
        }
        for (workspace, group) in nui.sessions[session].workspaces.iter().enumerate() {
            for (window, group) in group.windows.iter().enumerate() {
                if let Some(pane) = group
                    .panes
                    .iter()
                    .position(|pane| pane.terminal == self.terminal)
                {
                    return Some(Location {
                        session,
                        workspace,
                        window,
                        pane,
                    });
                }
            }
        }
        None
    }
}

pub fn start(nui: &mut Nui) {
    if let Some(nav) = &mut nui.navigation {
        nav.search = Some(Search {
            editing: true,
            ..Search::default()
        });
    }
    refresh(nui);
}

/// Each term may match any field; inverse terms must match none of them.
/// Adjacent alternatives separated by `|` form an OR group, groups are ANDed.
struct Query(Vec<Vec<Atom>>);

impl Query {
    fn new(text: &str) -> Self {
        let mut groups: Vec<Vec<Atom>> = Vec::new();
        let mut alternative = false;
        for mut atom in Pattern::parse(text, CaseMatching::Smart, Normalization::Smart).atoms {
            let needle = atom.needle_text().to_string();
            if atom.kind == AtomKind::Fuzzy && !atom.negative && needle == "|" {
                alternative = true;
                continue;
            }
            if needle == "\\|" {
                let negative = atom.negative;
                atom = Atom::new(
                    "|",
                    CaseMatching::Smart,
                    Normalization::Smart,
                    AtomKind::Substring,
                    false,
                );
                atom.negative = negative;
            }
            if alternative && !groups.is_empty() {
                groups.last_mut().unwrap().push(atom);
            } else {
                groups.push(vec![atom]);
            }
            alternative = false;
        }
        Self(groups)
    }

    fn score(
        &self,
        fields: &[String],
        matcher: &mut Matcher,
        buffer: &mut Vec<char>,
    ) -> Option<u32> {
        let mut total = 0u32;
        for alternatives in &self.0 {
            let best = alternatives
                .iter()
                .filter_map(|atom| {
                    if atom.negative {
                        fields
                            .iter()
                            .all(|field| {
                                atom.score(Utf32Str::new(field, buffer), matcher).is_some()
                            })
                            .then_some(0)
                    } else {
                        fields
                            .iter()
                            .filter_map(|field| atom.score(Utf32Str::new(field, buffer), matcher))
                            .max()
                            .map(u32::from)
                    }
                })
                .max()?;
            total = total.saturating_add(best);
        }
        Some(total)
    }
}

fn hits(nui: &Nui, query: &str) -> Vec<SearchHit> {
    let query = Query::new(query);
    let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
    let mut buffer = Vec::new();
    let mut matches = Vec::new();
    for (session, owner) in nui.sessions.iter().enumerate() {
        for (workspace, group) in owner.workspaces.iter().enumerate() {
            for (window, group) in group.windows.iter().enumerate() {
                for (pane, view) in group.panes.iter().enumerate() {
                    let at = Location {
                        session,
                        workspace,
                        window,
                        pane,
                    };
                    let Some(terminal) = owner.terminal(view.terminal) else {
                        continue;
                    };
                    let mut fields: Vec<String> = Level::ALL
                        .into_iter()
                        .map(|level| nui.label(level, at))
                        .collect();
                    let mut label = fields.join(" › ");
                    for field in [&terminal.cwd, &terminal.program, &terminal.title]
                        .into_iter()
                        .flatten()
                    {
                        if !field.is_empty() && !fields.contains(field) {
                            label.push_str(" · ");
                            label.push_str(field);
                        }
                        fields.push(field.clone());
                    }
                    fields.push(terminal.command.clone());
                    fields.extend(terminal.args.clone());
                    let Some(score) = query.score(&fields, &mut matcher, &mut buffer) else {
                        continue;
                    };
                    matches.push((
                        score,
                        SearchHit {
                            session: owner.id,
                            terminal: view.terminal,
                            workspace,
                            window,
                            pane,
                            label,
                        },
                    ));
                }
            }
        }
    }
    // Stable sorting keeps native pane order for equal scores and empty queries.
    matches.sort_by_key(|(score, _)| Reverse(*score));
    matches.into_iter().map(|(_, hit)| hit).collect()
}

fn refresh(nui: &mut Nui) {
    let Some(search) = nui.navigation.as_ref().and_then(|nav| nav.search.as_ref()) else {
        return;
    };
    let results = hits(nui, &search.query);
    if let Some(search) = nui.navigation.as_mut().and_then(|nav| nav.search.as_mut()) {
        search.results = results;
        search.selected = 0;
    }
}

/// Removing a target can remove a captured result, but cannot substitute an
/// unrelated terminal or add new matches to a frozen list.
pub fn settle(nui: &mut Nui) {
    let Some(mut search) = nui.navigation.as_mut().and_then(|nav| nav.search.take()) else {
        return;
    };
    let selected = search.results.get(search.selected).cloned();
    search.results.retain(|hit| hit.resolve(nui).is_some());
    search.selected = selected
        .and_then(|selected| search.results.iter().position(|hit| *hit == selected))
        .unwrap_or(search.selected)
        .min(search.results.len().saturating_sub(1));
    nui.navigation.as_mut().unwrap().search = Some(search);
}

pub fn key(nui: &mut Nui, key: &KeyEvent) -> Outcome {
    let Some(search) = nui.navigation.as_mut().and_then(|nav| nav.search.as_mut()) else {
        return Outcome::Stay;
    };
    if key.code == KeyCode::Esc {
        nui.navigation.as_mut().unwrap().search = None;
        return Outcome::Stay;
    }
    if search.editing {
        match key.code {
            KeyCode::Enter => {
                search.editing = false;
                return Outcome::Stay;
            }
            KeyCode::Backspace => {
                search.query.pop();
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => search.query.clear(),
            KeyCode::Char(c)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                search.query.push(c)
            }
            _ => return Outcome::Stay,
        }
        refresh(nui);
    } else {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                search.selected = search
                    .selected
                    .saturating_add(1)
                    .min(search.results.len().saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => search.selected = search.selected.saturating_sub(1),
            KeyCode::Enter => {
                let hit = search.results.get(search.selected).cloned();
                if let Some(at) = hit.and_then(|hit| hit.resolve(nui)) {
                    nui.enter(at);
                }
            }
            KeyCode::Char('/') => {
                search.editing = true;
                refresh(nui);
            }
            KeyCode::Char('q') => return Outcome::Quit,
            KeyCode::Char('Q') => return Outcome::Shutdown,
            _ => {}
        }
    }
    Outcome::Stay
}

pub fn paste(nui: &mut Nui, text: &str) {
    if let Some(search) = nui
        .navigation
        .as_mut()
        .and_then(|nav| nav.search.as_mut())
        .filter(|search| search.editing)
    {
        search
            .query
            .extend(text.chars().filter(|c| !c.is_control()));
        refresh(nui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        nav,
        state::{Pane, tests::sample},
    };

    fn press(nui: &mut Nui, code: KeyCode) {
        key(nui, &KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn search(nui: &Nui) -> &Search {
        nui.navigation.as_ref().unwrap().search.as_ref().unwrap()
    }

    #[test]
    fn fuzzy_terms_find_every_field_across_sessions_and_rank_matches() {
        let mut nui = sample();
        nui.sessions[0].name = "development".into();
        let terminal = &mut nui.sessions[0].terminals[0];
        terminal.cwd = Some("/projects/yalij/src".into());
        terminal.program = Some("nvim".into());
        terminal.title = Some("résumé".into());
        terminal.args = vec!["special-argument".into()];
        nui.sessions[0].workspaces[0].name = Some("backend".into());
        nui.sessions[0].workspaces[0].windows[0].name = Some("editor".into());
        nui.sessions[0].workspaces[0].windows[0].panes[0].name = Some("coding".into());
        for query in [
            "ylj",
            "src$",
            "^back",
            "'editor",
            "cdg",
            "resume",
            "nvm",
            "special-argument",
            "dev src$ nvm",
            "editor !missing",
            "missing | coding",
            "^backend coding | absent",
        ] {
            assert!(
                hits(&nui, query)
                    .iter()
                    .any(|hit| hit.terminal == 1 && hit.session == nui.sessions[0].id),
                "{query}"
            );
        }
        for query in ["!editor", "coding missing", "NVIM"] {
            assert!(
                !hits(&nui, query)
                    .iter()
                    .any(|hit| hit.terminal == 1 && hit.session == nui.sessions[0].id),
                "{query}"
            );
        }
        let other = nui.add_session("elsewhere", "sh", Some("/unique/directory".into()));
        assert_eq!(hits(&nui, "unq dir")[0].session, nui.sessions[other].id);
        nui.sessions[0].terminals[1].title = Some("cg".into());
        assert_eq!(
            hits(&nui, "cg")[0].terminal,
            nui.sessions[0].terminals[1].id
        );
    }

    #[test]
    fn query_is_live_then_enter_freezes_and_browses_results() {
        let mut nui = sample();
        nav::open(&mut nui);
        start(&mut nui);
        let all = search(&nui).results.len();
        paste(&mut nui, "impossible\nquery");
        assert!(search(&nui).results.is_empty());
        assert_eq!(search(&nui).query, "impossiblequery");
        key(
            &mut nui,
            &KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert_eq!(search(&nui).results.len(), all);
        press(&mut nui, KeyCode::Char('s'));
        let captured = search(&nui).results.clone();
        press(&mut nui, KeyCode::Enter);
        assert!(!search(&nui).editing);
        nui.add_session("should-not-appear", "sh", None);
        nui.normalize().unwrap();
        assert_eq!(search(&nui).results, captured);
        press(&mut nui, KeyCode::Down);
        let target = search(&nui).results[search(&nui).selected]
            .resolve(&nui)
            .unwrap();
        press(&mut nui, KeyCode::Enter);
        assert!(nui.navigation.is_none());
        assert_eq!(nui.location(), Some(target));
    }

    #[test]
    fn escape_restores_navigation_and_slash_in_prompts_is_literal() {
        let mut nui = sample();
        nav::open(&mut nui);
        let before = nui.navigation.clone();
        nav::key(
            &mut nui,
            &KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            "sh",
        );
        assert!(search(&nui).editing);
        press(&mut nui, KeyCode::Esc);
        assert_eq!(nui.navigation, before);
        nav::key(
            &mut nui,
            &KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            "sh",
        );
        nav::key(
            &mut nui,
            &KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            "sh",
        );
        assert!(nui.navigation.as_ref().unwrap().search.is_none());
        assert_eq!(
            nui.navigation
                .as_mut()
                .unwrap()
                .operation
                .as_mut()
                .unwrap()
                .input_mut()
                .unwrap(),
            "/"
        );
    }

    #[test]
    fn snapshots_follow_identity_not_reordered_catalog_or_pane_indices() {
        let mut nui = sample();
        nui.add_session("other", "sh", None);
        nav::open(&mut nui);
        start(&mut nui);
        press(&mut nui, KeyCode::Enter);
        let hit = search(&nui).results[0].clone();
        nui.sessions.reverse();
        let session = nui
            .sessions
            .iter()
            .position(|session| session.id == hit.session)
            .unwrap();
        nui.sessions[session].workspaces[0].windows[0].panes.insert(
            0,
            Pane {
                terminal: 2,
                name: Some("interloper".into()),
            },
        );
        let at = hit.resolve(&nui).unwrap();
        assert_eq!(nui.pane(at).unwrap().terminal, hit.terminal);
        assert_eq!(at.pane, 1);
        nui.sessions.retain(|session| session.id != hit.session);
        nui.normalize().unwrap();
        assert!(hit.resolve(&nui).is_none());
        assert!(
            search(&nui)
                .results
                .iter()
                .all(|found| found.session != hit.session)
        );
    }
}
