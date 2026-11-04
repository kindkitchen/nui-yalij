//! Keys: their notation in the state and in plugins, and their bytes for terminals.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Keys with a name in the notation, besides characters and `f1`..`f12`.
const NAMED: [(&str, KeyCode); 16] = [
    ("space", KeyCode::Char(' ')),
    ("enter", KeyCode::Enter),
    ("esc", KeyCode::Esc),
    ("tab", KeyCode::Tab),
    ("backtab", KeyCode::BackTab),
    ("backspace", KeyCode::Backspace),
    ("delete", KeyCode::Delete),
    ("insert", KeyCode::Insert),
    ("home", KeyCode::Home),
    ("end", KeyCode::End),
    ("pageup", KeyCode::PageUp),
    ("pagedown", KeyCode::PageDown),
    ("up", KeyCode::Up),
    ("down", KeyCode::Down),
    ("left", KeyCode::Left),
    ("right", KeyCode::Right),
];

/// The notation of a key: `n`, `N`, `ctrl-g`, `alt-enter`, `shift-up`, `f5`.
pub fn notation(key: &KeyEvent) -> Option<String> {
    format(key.code, key.modifiers)
}

/// A notation in its canonical form, so `Ctrl-G` matches `ctrl-g`.
pub fn canonical(notation: &str) -> Option<String> {
    let (code, modifiers) = parse(notation)?;
    format(code, modifiers)
}

fn parse(notation: &str) -> Option<(KeyCode, KeyModifiers)> {
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = notation;
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some((prefix, modifier)) = [("ctrl-", KeyModifiers::CONTROL), ("alt-", KeyModifiers::ALT), ("shift-", KeyModifiers::SHIFT)]
            .into_iter()
            .find(|(prefix, _)| lower.starts_with(prefix) && rest.len() > prefix.len())
        else {
            break;
        };
        modifiers |= modifier;
        rest = &rest[prefix.len()..];
    }
    let mut chars = rest.chars();
    let code = match (chars.next(), chars.next()) {
        (Some(c), None) => KeyCode::Char(c),
        _ => {
            let name = rest.to_ascii_lowercase();
            match name.strip_prefix('f').and_then(|n| n.parse().ok()) {
                Some(n @ 1..=12) => KeyCode::F(n),
                _ => NAMED.iter().find(|(known, _)| *known == name)?.1,
            }
        }
    };
    Some((code, modifiers))
}

fn format(code: KeyCode, modifiers: KeyModifiers) -> Option<String> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let mut prefix = String::new();
    if ctrl {
        prefix += "ctrl-";
    }
    if modifiers.contains(KeyModifiers::ALT) {
        prefix += "alt-";
    }
    let name = match code {
        KeyCode::Char(' ') => "space".to_string(),
        // terminals send ctrl with a letter the same, shifted or not
        KeyCode::Char(c) if ctrl => c.to_ascii_lowercase().to_string(),
        // the case of a character tells shift already
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("f{n}"),
        other => NAMED.iter().find(|(_, known)| *known == other)?.0.to_string(),
    };
    if modifiers.contains(KeyModifiers::SHIFT) && !matches!(code, KeyCode::Char(_)) {
        prefix += "shift-";
    }
    Some(prefix + &name)
}

/// The bytes a program in a terminal reads for a key, as xterm sends them.
/// `application_cursor` is the cursor key mode the program asked for.
pub fn encode(key: &KeyEvent, application_cursor: bool) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // xterm's modifier parameter: 1 + shift + 2 alt + 4 ctrl
    let modifier = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let escape = if alt { "\x1b" } else { "" };
    let text = match key.code {
        KeyCode::Char(c) => match ctrl.then(|| control(c)).flatten() {
            Some(byte) => format!("{escape}{}", byte as char),
            None => format!("{escape}{c}"),
        },
        KeyCode::Enter => format!("{escape}\r"),
        KeyCode::Tab if shift => "\x1b[Z".into(),
        KeyCode::Tab => format!("{escape}\t"),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Backspace => format!("{escape}{}", if ctrl { '\x08' } else { '\x7f' }),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left | KeyCode::Home | KeyCode::End => {
            let letter = match key.code {
                KeyCode::Up => 'A',
                KeyCode::Down => 'B',
                KeyCode::Right => 'C',
                KeyCode::Left => 'D',
                KeyCode::Home => 'H',
                _ => 'F',
            };
            match (modifier, application_cursor) {
                (1, true) => format!("\x1bO{letter}"),
                (1, false) => format!("\x1b[{letter}"),
                _ => format!("\x1b[1;{modifier}{letter}"),
            }
        }
        KeyCode::Insert | KeyCode::Delete | KeyCode::PageUp | KeyCode::PageDown => {
            let number = match key.code {
                KeyCode::Insert => 2,
                KeyCode::Delete => 3,
                KeyCode::PageUp => 5,
                _ => 6,
            };
            tilde(number, modifier)
        }
        KeyCode::F(n @ 1..=4) => {
            let letter = ['P', 'Q', 'R', 'S'][n as usize - 1];
            if modifier == 1 { format!("\x1bO{letter}") } else { format!("\x1b[1;{modifier}{letter}") }
        }
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5], modifier),
        _ => String::new(),
    };
    text.into_bytes()
}

/// `CSI number ~`, with a modifier parameter when there is one.
fn tilde(number: u8, modifier: u8) -> String {
    if modifier == 1 { format!("\x1b[{number}~") } else { format!("\x1b[{number};{modifier}~") }
}

/// The control character of ctrl with a key.
fn control(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '/' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn notation_round_trips() {
        for text in ["n", "N", "ctrl-g", "alt-enter", "shift-up", "f5", "space", "-", "ctrl--", "right", "backtab"] {
            assert_eq!(canonical(text).as_deref(), Some(text), "{text}");
        }
        assert_eq!(canonical("Ctrl-G").as_deref(), Some("ctrl-g"));
        assert_eq!(canonical("ctrl-nope"), None);
        let shifted = key(KeyCode::Char('N'), KeyModifiers::SHIFT);
        assert_eq!(notation(&shifted).as_deref(), Some("N"));
        let ctrl = key(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(notation(&ctrl), canonical("ctrl-g"));
    }

    #[test]
    fn keys_encode_as_xterm() {
        assert_eq!(encode(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), false), b"\x03");
        assert_eq!(encode(&key(KeyCode::Char('x'), KeyModifiers::ALT), false), b"\x1bx");
        assert_eq!(encode(&key(KeyCode::Up, KeyModifiers::NONE), false), b"\x1b[A");
        assert_eq!(encode(&key(KeyCode::Up, KeyModifiers::NONE), true), b"\x1bOA");
        assert_eq!(encode(&key(KeyCode::Left, KeyModifiers::CONTROL), true), b"\x1b[1;5D");
        assert_eq!(encode(&key(KeyCode::PageDown, KeyModifiers::SHIFT), false), b"\x1b[6;2~");
        assert_eq!(encode(&key(KeyCode::F(5), KeyModifiers::NONE), false), b"\x1b[15~");
        assert_eq!(encode(&key(KeyCode::Char('é'), KeyModifiers::NONE), false), "é".as_bytes());
    }
}
