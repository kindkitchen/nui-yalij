//! The navigation panel, yazi style: a section per level, sessions to panes.
//!
//! Like everything else it only changes the state: the panel is drawn from
//! `Nui::navigation`, and its operations are the ones of the state.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::{Level, Location, Navigation, Nui, Operation};

/// What a key in the panel asks of the application, beyond the state.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Stay,
    Quit,
    Shutdown,
}

/// Open the panel at the location of the person.
pub fn open(nui: &mut Nui) {
    if let Some(cursor) = nui.location() {
        nui.navigation = Some(Navigation {
            focus: Level::Pane,
            cursor,
            operation: None,
        });
    }
}

/// Close the panel: back to where the person was.
pub fn close(nui: &mut Nui) {
    nui.navigation = None;
}

/// Handle a key of the panel; new terminals run `shell`.
pub fn key(nui: &mut Nui, key: &KeyEvent, shell: &str) -> Outcome {
    let Some(Navigation {
        focus,
        cursor,
        operation,
    }) = nui.navigation.clone()
    else {
        return Outcome::Stay;
    };
    nui.message = None;
    match operation {
        Some(mut operation @ (Operation::Rename { .. } | Operation::Create { .. })) => {
            match key.code {
                KeyCode::Esc => set_operation(nui, None),
                KeyCode::Enter => {
                    if let Err(error) = approve(nui, focus, cursor, operation) {
                        nui.message = Some(error);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(input) = operation.input_mut() {
                        input.pop();
                    }
                    set_operation(nui, Some(operation));
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if let Some(input) = operation.input_mut() {
                        input.push(c);
                    }
                    set_operation(nui, Some(operation));
                }
                _ => {}
            }
        }
        Some(operation) => match key.code {
            KeyCode::Esc => set_operation(nui, None),
            KeyCode::Enter => {
                set_operation(nui, None);
                if operation == Operation::Remove {
                    nui.remove(focus, cursor);
                    let cursor = nui.descend(focus, cursor);
                    if let Some(navigation) = nui.navigation.as_mut() {
                        navigation.cursor = cursor;
                    }
                } else {
                    nui.stop(focus, cursor);
                }
            }
            _ => {}
        },
        None => match key.code {
            KeyCode::Char('h') | KeyCode::Left => move_focus(nui, -1),
            KeyCode::Char('l') | KeyCode::Right => move_focus(nui, 1),
            KeyCode::Char('j') | KeyCode::Down => move_cursor(nui, 1),
            KeyCode::Char('k') | KeyCode::Up => move_cursor(nui, -1),
            KeyCode::Char('a') => create(nui, focus, cursor, shell),
            KeyCode::Char('r') => {
                let input = name(nui, focus, cursor);
                set_operation(nui, Some(Operation::Rename { input }));
            }
            KeyCode::Char('x') => set_operation(nui, Some(Operation::Remove)),
            KeyCode::Char('s') => set_operation(nui, Some(Operation::Stop)),
            KeyCode::Enter if focus == Level::Pane => nui.enter(cursor),
            KeyCode::Enter => move_focus(nui, 1),
            KeyCode::Esc => close(nui),
            KeyCode::Char('q') => {
                close(nui);
                return Outcome::Quit;
            }
            KeyCode::Char('Q') => return Outcome::Shutdown,
            _ => {}
        },
    }
    Outcome::Stay
}

/// Pasted text goes into a name being typed, as typed.
pub fn paste(nui: &mut Nui, text: &str) {
    let operation = nui
        .navigation
        .as_mut()
        .and_then(|navigation| navigation.operation.as_mut());
    if let Some(input) = operation.and_then(Operation::input_mut) {
        input.extend(text.chars().filter(|c| !c.is_control()));
    }
}

/// Create an item after the hovered one, its terminal where the hovered one
/// is, and name it.
fn create(nui: &mut Nui, focus: Level, cursor: Location, shell: &str) {
    let cwd = nui.shown(cursor).and_then(|terminal| terminal.cwd.clone());
    let Some(at) = nui.create(focus, cursor, shell, cwd) else {
        return;
    };
    if let Some(navigation) = nui.navigation.as_mut() {
        navigation.cursor = at;
        navigation.operation = Some(Operation::Create {
            input: String::new(),
        });
    }
}

/// Give the hovered item the typed name. A new one keeps its automatic name
/// when none is typed, and the first item inside it is named next.
fn approve(
    nui: &mut Nui,
    focus: Level,
    cursor: Location,
    operation: Operation,
) -> Result<(), String> {
    let next = match operation {
        Operation::Create { input } => {
            if !input.trim().is_empty() {
                nui.rename(focus, cursor, &input)?;
            }
            (focus < Level::Pane).then(|| Operation::Create {
                input: String::new(),
            })
        }
        Operation::Rename { input } => {
            nui.rename(focus, cursor, &input)?;
            None
        }
        Operation::Remove | Operation::Stop => None,
    };
    if let Some(navigation) = nui.navigation.as_mut() {
        if next.is_some() {
            navigation.focus = focus.step(1);
        }
        navigation.operation = next;
    }
    Ok(())
}

fn set_operation(nui: &mut Nui, operation: Option<Operation>) {
    if let Some(navigation) = nui.navigation.as_mut() {
        navigation.operation = operation;
    }
}

/// Focus the section `delta` steps to the right.
fn move_focus(nui: &mut Nui, delta: isize) {
    if let Some(navigation) = nui.navigation.as_mut() {
        navigation.focus = navigation.focus.step(delta);
    }
}

/// Hover the item `delta` steps down in the focused section; the sections to
/// the right follow it, to its active children.
fn move_cursor(nui: &mut Nui, delta: isize) {
    let Some(Navigation { focus, cursor, .. }) = nui.navigation.clone() else {
        return;
    };
    let last = nui.count(focus, cursor).saturating_sub(1) as isize;
    let index = (cursor.get(focus) as isize + delta).clamp(0, last) as usize;
    let cursor = nui.descend(focus, cursor.with(focus, index));
    if let Some(navigation) = nui.navigation.as_mut() {
        navigation.cursor = cursor;
    }
}

/// The name a rename starts from: the node's own, so an unnamed one stays
/// unnamed unless typed.
fn name(nui: &Nui, level: Level, at: Location) -> String {
    let name = match level {
        Level::Session => nui
            .sessions
            .get(at.session)
            .map(|session| session.name.clone()),
        Level::Window => nui.window(at).and_then(|window| window.name.clone()),
        Level::Tab => nui.tab(at).and_then(|tab| tab.name.clone()),
        Level::Pane => nui.pane(at).and_then(|pane| pane.name.clone()),
    };
    name.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Status, tests::sample};

    fn press(nui: &mut Nui, keys: &str) -> Outcome {
        let mut outcome = Outcome::Stay;
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\x1b' => KeyCode::Esc,
                '\x7f' => KeyCode::Backspace,
                c => KeyCode::Char(c),
            };
            outcome = key(nui, &KeyEvent::new(code, KeyModifiers::NONE), "sh");
        }
        outcome
    }

    fn cursor(nui: &Nui) -> (Level, Location) {
        let navigation = nui.navigation.as_ref().unwrap();
        (navigation.focus, navigation.cursor)
    }

    #[test]
    fn hjkl_move_and_sections_follow() {
        let mut nui = sample();
        open(&mut nui);
        assert_eq!(cursor(&nui), (Level::Pane, Location::default()));
        press(&mut nui, "hhj");
        // the second window, and its active tab and pane
        assert_eq!(
            cursor(&nui),
            (
                Level::Window,
                Location {
                    window: 1,
                    ..Location::default()
                }
            )
        );
        press(&mut nui, "jjjk");
        assert_eq!(cursor(&nui).1.window, 0);
        // enter on a window moves to its tabs; on a pane, into the pane
        press(&mut nui, "\nj\n\n");
        assert!(nui.navigation.is_none());
        assert_eq!(
            nui.location(),
            Some(Location {
                tab: 1,
                ..Location::default()
            })
        );
    }

    #[test]
    fn esc_returns_unchanged() {
        let mut nui = sample();
        let before = nui.clone();
        open(&mut nui);
        press(&mut nui, "hhjr");
        press(&mut nui, "\x1b\x1b");
        assert_eq!(nui, before);
    }

    #[test]
    fn add_names_the_new_item_then_the_ones_inside() {
        let mut nui = sample();
        open(&mut nui);
        let names = |nui: &Nui, at| Level::ALL.map(|level| nui.label(level, at));
        // a session after the hovered one, then its window, named; esc keeps
        // the automatic names of the rest
        press(&mut nui, "hhhawork\ndev\n\x1b");
        assert_eq!(
            cursor(&nui),
            (
                Level::Tab,
                Location {
                    session: 1,
                    ..Location::default()
                }
            )
        );
        assert_eq!(
            names(&nui, cursor(&nui).1),
            ["work", "dev", "tab-1", "pane-1"]
        );
        // no name keeps the automatic one, and goes on
        press(&mut nui, "a\n\n");
        assert_eq!(
            cursor(&nui),
            (
                Level::Pane,
                Location {
                    session: 1,
                    tab: 1,
                    ..Location::default()
                }
            )
        );
        assert_eq!(
            names(&nui, cursor(&nui).1),
            ["work", "dev", "tab-2", "pane-1"]
        );
        assert!(nui.navigation.as_ref().unwrap().operation.is_none());
        // a session needs a name of its own
        press(&mut nui, "hhhamain\n");
        assert!(nui.message.is_some());
        press(&mut nui, "\x1b");
        assert_eq!(nui.sessions[2].name, "session-3");
        nui.normalize().unwrap();
    }

    #[test]
    fn rename_remove_and_stop_need_approval() {
        let mut nui = sample();
        open(&mut nui);
        press(&mut nui, "rlogs\x7fz\n");
        assert_eq!(nui.label(Level::Pane, Location::default()), "logz");
        press(&mut nui, "s");
        assert_eq!(nui.sessions[0].terminals[0].status, Status::Running);
        press(&mut nui, "\n");
        assert_eq!(nui.sessions[0].terminals[0].status, Status::Stopped);
        press(&mut nui, "x\x1b");
        assert_eq!(nui.sessions[0].windows[0].tabs[0].panes.len(), 2);
        press(&mut nui, "x\n");
        assert_eq!(nui.sessions[0].windows[0].tabs[0].panes.len(), 1);
        // a session can't lose its name
        press(&mut nui, "hhhr\x7f\x7f\x7f\x7f\n");
        assert!(nui.message.is_some());
        assert_eq!(press(&mut nui, "\x1bq"), Outcome::Quit);
    }
}
