//! Keys: their notation in decoration and in plugins, and their bytes for terminals.

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

/// The kitty keyboard flag of escape codes telling keys apart: the only one
/// emulated. A program pushing more gets this one, and can ask which it got.
pub const DISAMBIGUATE: u8 = 1;

/// The notation of a key: `n`, `N`, `ctrl-g`, `alt-enter`, `shift-up`, `super-k`, `f5`.
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
        let Some((prefix, modifier)) = [
            ("ctrl-", KeyModifiers::CONTROL),
            ("alt-", KeyModifiers::ALT),
            ("super-", KeyModifiers::SUPER),
            ("shift-", KeyModifiers::SHIFT),
        ]
        .into_iter()
        .find(|(prefix, _)| lower.starts_with(prefix) && rest.len() > prefix.len()) else {
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
    // no notation, so no binding takes them for the key without them
    if modifiers.intersects(KeyModifiers::HYPER | KeyModifiers::META) {
        return None;
    }
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let mut prefix = String::new();
    if ctrl {
        prefix += "ctrl-";
    }
    if modifiers.contains(KeyModifiers::ALT) {
        prefix += "alt-";
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        prefix += "super-";
    }
    let name = match code {
        KeyCode::Char(' ') => "space".to_string(),
        // terminals send ctrl with a letter the same, shifted or not
        KeyCode::Char(c) if ctrl => c.to_ascii_lowercase().to_string(),
        // the case of a character tells shift already
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("f{n}"),
        other => NAMED
            .iter()
            .find(|(_, known)| *known == other)?
            .0
            .to_string(),
    };
    // so does backtab, which is shift with tab
    if modifiers.contains(KeyModifiers::SHIFT)
        && !matches!(code, KeyCode::Char(_) | KeyCode::BackTab)
    {
        prefix += "shift-";
    }
    Some(prefix + &name)
}

/// How a program wants its keys: what it asked of its terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Mode {
    /// DECCKM: unmodified cursor keys as SS3.
    pub application_cursor: bool,
    /// The kitty keyboard flags in effect.
    pub kitty: u8,
    /// The level of xterm's modifyOtherKeys.
    pub modify_other_keys: u8,
}

/// The modifiers of a key, as terminals count them.
#[derive(Clone, Copy)]
struct Held {
    shift: bool,
    alt: bool,
    ctrl: bool,
    sup: bool,
}

impl Held {
    fn of(key: &KeyEvent) -> Held {
        let modifiers = key.modifiers;
        Held {
            // an upper case character and backtab tell shift on their own
            shift: modifiers.contains(KeyModifiers::SHIFT)
                || key.code == KeyCode::BackTab
                || matches!(key.code, KeyCode::Char(c) if c.is_uppercase()),
            alt: modifiers.contains(KeyModifiers::ALT),
            ctrl: modifiers.contains(KeyModifiers::CONTROL),
            sup: modifiers.contains(KeyModifiers::SUPER),
        }
    }

    /// The modifier parameter: 1 + shift + 2 alt + 4 ctrl + 8 super.
    fn parameter(self) -> u8 {
        1 + self.shift as u8 + 2 * self.alt as u8 + 4 * self.ctrl as u8 + 8 * self.sup as u8
    }

    fn only_alt(self) -> bool {
        self.alt && !self.shift && !self.ctrl && !self.sup
    }
}

/// The bytes a program in a terminal reads for a key, as it asked for them:
/// kitty's escape codes, xterm's modifyOtherKeys, or as xterm sends them by
/// default.
pub fn encode(key: &KeyEvent, mode: Mode) -> Vec<u8> {
    let held = Held::of(key);
    let modifier = held.parameter();
    let text = match key.code {
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Right
        | KeyCode::Left
        | KeyCode::Home
        | KeyCode::End => {
            let letter = match key.code {
                KeyCode::Up => 'A',
                KeyCode::Down => 'B',
                KeyCode::Right => 'C',
                KeyCode::Left => 'D',
                KeyCode::Home => 'H',
                _ => 'F',
            };
            match (modifier, mode.application_cursor) {
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
        // modified F3 as a number, not to read as a report of the cursor position
        KeyCode::F(3) if modifier > 1 => tilde(13, modifier),
        KeyCode::F(n @ 1..=4) => {
            let letter = ['P', 'Q', 'R', 'S'][n as usize - 1];
            if modifier == 1 {
                format!("\x1bO{letter}")
            } else {
                format!("\x1b[1;{modifier}{letter}")
            }
        }
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5], modifier),
        KeyCode::F(n @ 13..=35) if mode.kitty & DISAMBIGUATE != 0 => {
            csi_u(57363 + n as u32, modifier)
        }
        _ if mode.kitty & DISAMBIGUATE != 0 => kitty(key.code, held),
        _ => match modify_other_keys(key.code, held, mode.modify_other_keys) {
            Some(text) => text,
            None => legacy(key.code, held),
        },
    };
    text.into_bytes()
}

/// Kitty's escape codes telling keys apart: text stays text; escape, and
/// modified characters, enter, tab and backspace become `CSI code;mods u`.
fn kitty(code: KeyCode, held: Held) -> String {
    let modifier = held.parameter();
    let (code, plain) = match code {
        KeyCode::Char(c) if !held.alt && !held.ctrl && !held.sup => return c.to_string(),
        KeyCode::Char(c) => (unshifted(c) as u32, ""),
        KeyCode::Enter => (13, "\r"),
        KeyCode::Tab | KeyCode::BackTab => (9, "\t"),
        KeyCode::Backspace => (127, "\x7f"),
        KeyCode::Esc => (27, ""),
        _ => return String::new(),
    };
    if modifier == 1 && !plain.is_empty() {
        return plain.into();
    }
    csi_u(code, modifier)
}

/// xterm's modifyOtherKeys: modified keys that read the same as others by
/// default become `CSI 27;mods;code~`. Level 1 keeps the ones having a
/// distinct default, such as ctrl with a letter; level 2 takes those too.
fn modify_other_keys(code: KeyCode, held: Held, level: u8) -> Option<String> {
    let modifier = held.parameter();
    if level == 0 || modifier == 1 {
        return None;
    }
    let all = level >= 2;
    let code = match code {
        KeyCode::Enter if all || !held.only_alt() => 13,
        KeyCode::Esc if all || !held.only_alt() => 27,
        KeyCode::Tab | KeyCode::BackTab
            if all || !(held.only_alt() || (held.shift && modifier == 2)) =>
        {
            9
        }
        // ctrl with backspace has its own byte, at every level
        KeyCode::Backspace if all && !(held.ctrl && modifier == 5) => 127,
        // shift alone changes the text of a character, but space
        KeyCode::Char(' ') if held.shift && modifier == 2 && all => 32,
        KeyCode::Char(c) if held.alt || held.ctrl || held.sup => {
            let distinct = (held.ctrl && modifier == 5 && control(c).is_some()) || held.only_alt();
            if !all && distinct {
                return None;
            }
            c as u32
        }
        _ => return None,
    };
    Some(format!("\x1b[27;{modifier};{code}~"))
}

/// The bytes xterm sends by default: ctrl as control characters and alt as
/// an escape before the key.
fn legacy(code: KeyCode, held: Held) -> String {
    let escape = if held.alt { "\x1b" } else { "" };
    match code {
        KeyCode::Char(c) => match held.ctrl.then(|| control(c)).flatten() {
            Some(byte) => format!("{escape}{}", byte as char),
            None => format!("{escape}{c}"),
        },
        KeyCode::Enter => format!("{escape}\r"),
        KeyCode::Tab if held.shift => "\x1b[Z".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Tab => format!("{escape}\t"),
        KeyCode::Backspace => format!("{escape}{}", if held.ctrl { '\x08' } else { '\x7f' }),
        KeyCode::Esc => format!("{escape}\x1b"),
        _ => String::new(),
    }
}

/// `CSI number ~`, with a modifier parameter when there is one.
fn tilde(number: u8, modifier: u8) -> String {
    if modifier == 1 {
        format!("\x1b[{number}~")
    } else {
        format!("\x1b[{number};{modifier}~")
    }
}

/// `CSI code u`, with a modifier parameter when there is one.
fn csi_u(code: u32, modifier: u8) -> String {
    if modifier == 1 {
        format!("\x1b[{code}u")
    } else {
        format!("\x1b[{code};{modifier}u")
    }
}

/// The key of a character typed with shift: the lower case of a letter.
fn unshifted(c: char) -> char {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(lower), None) => lower,
        _ => c,
    }
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

    fn bytes(code: KeyCode, modifiers: KeyModifiers, mode: Mode) -> String {
        String::from_utf8(encode(&key(code, modifiers), mode)).unwrap()
    }

    const LEGACY: Mode = Mode {
        application_cursor: false,
        kitty: 0,
        modify_other_keys: 0,
    };
    const KITTY: Mode = Mode {
        kitty: DISAMBIGUATE,
        ..LEGACY
    };

    #[test]
    fn notation_round_trips() {
        for text in [
            "n",
            "N",
            "ctrl-g",
            "alt-enter",
            "shift-up",
            "super-k",
            "f5",
            "space",
            "-",
            "ctrl--",
            "right",
            "backtab",
        ] {
            assert_eq!(canonical(text).as_deref(), Some(text), "{text}");
        }
        assert_eq!(canonical("Ctrl-G").as_deref(), Some("ctrl-g"));
        assert_eq!(canonical("ctrl-nope"), None);
        let shifted = key(KeyCode::Char('N'), KeyModifiers::SHIFT);
        assert_eq!(notation(&shifted).as_deref(), Some("N"));
        let ctrl = key(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(notation(&ctrl), canonical("ctrl-g"));
        // terminals send backtab with shift
        let backtab = key(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(notation(&backtab), canonical("backtab"));
        assert_eq!(canonical("shift-backtab").as_deref(), Some("backtab"));
        // super is its own key, not the one without it
        let quit = key(KeyCode::Char('q'), KeyModifiers::SUPER);
        assert_eq!(notation(&quit).as_deref(), Some("super-q"));
        assert_eq!(
            notation(&key(KeyCode::Char('q'), KeyModifiers::HYPER)),
            None
        );
    }

    #[test]
    fn keys_encode_as_xterm() {
        let none = KeyModifiers::NONE;
        let application = Mode {
            application_cursor: true,
            ..LEGACY
        };
        assert_eq!(
            bytes(KeyCode::Char('c'), KeyModifiers::CONTROL, LEGACY),
            "\x03"
        );
        assert_eq!(
            bytes(KeyCode::Char('x'), KeyModifiers::ALT, LEGACY),
            "\x1bx"
        );
        assert_eq!(bytes(KeyCode::Up, none, LEGACY), "\x1b[A");
        assert_eq!(bytes(KeyCode::Up, none, application), "\x1bOA");
        assert_eq!(
            bytes(KeyCode::Left, KeyModifiers::CONTROL, application),
            "\x1b[1;5D"
        );
        assert_eq!(
            bytes(KeyCode::PageDown, KeyModifiers::SHIFT, LEGACY),
            "\x1b[6;2~"
        );
        assert_eq!(bytes(KeyCode::F(5), none, LEGACY), "\x1b[15~");
        assert_eq!(
            bytes(KeyCode::F(3), KeyModifiers::SHIFT, LEGACY),
            "\x1b[13;2~"
        );
        assert_eq!(bytes(KeyCode::Char('é'), none, LEGACY), "é");
        // the default can't tell shift with enter
        assert_eq!(bytes(KeyCode::Enter, KeyModifiers::SHIFT, LEGACY), "\r");
        assert_eq!(
            bytes(KeyCode::BackTab, KeyModifiers::SHIFT, LEGACY),
            "\x1b[Z"
        );
    }

    #[test]
    fn kitty_tells_keys_apart() {
        let none = KeyModifiers::NONE;
        assert_eq!(
            bytes(KeyCode::Enter, KeyModifiers::SHIFT, KITTY),
            "\x1b[13;2u"
        );
        assert_eq!(bytes(KeyCode::Enter, none, KITTY), "\r");
        assert_eq!(bytes(KeyCode::Esc, none, KITTY), "\x1b[27u");
        assert_eq!(
            bytes(KeyCode::Char('c'), KeyModifiers::CONTROL, KITTY),
            "\x1b[99;5u"
        );
        assert_eq!(
            bytes(KeyCode::Char('i'), KeyModifiers::CONTROL, KITTY),
            "\x1b[105;5u"
        );
        assert_eq!(bytes(KeyCode::Tab, none, KITTY), "\t");
        assert_eq!(
            bytes(KeyCode::BackTab, KeyModifiers::SHIFT, KITTY),
            "\x1b[9;2u"
        );
        // shift is in the text; with alt, in the modifiers
        assert_eq!(bytes(KeyCode::Char('A'), KeyModifiers::SHIFT, KITTY), "A");
        assert_eq!(
            bytes(KeyCode::Char('A'), KeyModifiers::ALT, KITTY),
            "\x1b[97;4u"
        );
        assert_eq!(bytes(KeyCode::Char('п'), none, KITTY), "п");
        assert_eq!(
            bytes(KeyCode::Char('k'), KeyModifiers::SUPER, KITTY),
            "\x1b[107;9u"
        );
        assert_eq!(bytes(KeyCode::Up, none, KITTY), "\x1b[A");
    }

    #[test]
    fn modify_other_keys_by_level() {
        let one = Mode {
            modify_other_keys: 1,
            ..LEGACY
        };
        let two = Mode {
            modify_other_keys: 2,
            ..LEGACY
        };
        for mode in [one, two] {
            assert_eq!(
                bytes(KeyCode::Enter, KeyModifiers::SHIFT, mode),
                "\x1b[27;2;13~"
            );
            assert_eq!(
                bytes(KeyCode::Char('1'), KeyModifiers::CONTROL, mode),
                "\x1b[27;5;49~"
            );
            assert_eq!(bytes(KeyCode::Char('a'), KeyModifiers::NONE, mode), "a");
            assert_eq!(bytes(KeyCode::Char('A'), KeyModifiers::SHIFT, mode), "A");
            assert_eq!(
                bytes(KeyCode::Backspace, KeyModifiers::CONTROL, mode),
                "\x08"
            );
        }
        assert_eq!(
            bytes(KeyCode::Char('c'), KeyModifiers::CONTROL, one),
            "\x03"
        );
        assert_eq!(
            bytes(KeyCode::Char('c'), KeyModifiers::CONTROL, two),
            "\x1b[27;5;99~"
        );
        assert_eq!(bytes(KeyCode::Enter, KeyModifiers::ALT, one), "\x1b\r");
        assert_eq!(
            bytes(KeyCode::Enter, KeyModifiers::ALT, two),
            "\x1b[27;3;13~"
        );
        assert_eq!(bytes(KeyCode::BackTab, KeyModifiers::SHIFT, one), "\x1b[Z");
        assert_eq!(
            bytes(KeyCode::BackTab, KeyModifiers::SHIFT, two),
            "\x1b[27;2;9~"
        );
    }
}
