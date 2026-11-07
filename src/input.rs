//! Input from the terminal around a client: keys in every encoding terminals
//! use (by default, with xterm's modifyOtherKeys, with kitty's escape codes),
//! pastes and focus, and the answers to the client's own questions.
//!
//! A key pressed with ctrl, alt or super on a layout other than Latin counts
//! as the key of the base layout, when the terminal tells it: ctrl with the
//! key of `g` is `ctrl-g`, whatever that key types.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// What the terminal around a client tells it.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// For the server: a key, a paste, focus.
    Event(Event),
    /// The kitty keyboard flags in effect: the terminal speaks the protocol.
    Keyboard(u16),
    /// The device attributes: the terminal answered all asked before them.
    Attributes,
    /// The color scheme: 1 dark, 2 light.
    Scheme(u8),
    /// A color: `10` foreground, `11` background or `4;n` of the palette, and its value.
    Color(String, String),
}

const PASTE_END: &[u8] = b"\x1b[201~";
/// What waits for its end longer is garbage, not to block input.
const MAX_SEQUENCE: usize = 4096;
const MAX_PASTE: usize = 16 << 20;

/// Bytes in, inputs out; a sequence split between reads waits for its rest.
#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    /// The text of a paste in progress.
    paste: Option<Vec<u8>>,
}

enum Step {
    /// More bytes may finish it.
    Wait,
    /// So many bytes were this, maybe nothing worth telling.
    Took(usize, Option<Input>),
    /// So many bytes started a paste.
    Paste(usize),
}

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Input> {
        self.buffer.extend_from_slice(bytes);
        self.decode(false)
    }

    /// Nothing more came in time: what waits is whole as it is, such as a
    /// lone escape.
    pub fn flush(&mut self) -> Vec<Input> {
        self.decode(true)
    }

    /// Whether bytes wait for more.
    pub fn pending(&self) -> bool {
        !self.buffer.is_empty()
    }

    fn decode(&mut self, flush: bool) -> Vec<Input> {
        let mut inputs = Vec::new();
        let mut at = 0;
        while at < self.buffer.len() {
            let rest = &self.buffer[at..];
            if let Some(paste) = &mut self.paste {
                match find(rest, PASTE_END) {
                    Some(end) => {
                        paste.extend_from_slice(&rest[..end]);
                        at += end + PASTE_END.len();
                    }
                    None if paste.len() + rest.len() <= MAX_PASTE => {
                        // the end may be coming in parts
                        let taken = rest.len().saturating_sub(PASTE_END.len() - 1);
                        paste.extend_from_slice(&rest[..taken]);
                        at += taken;
                        break;
                    }
                    None => {
                        paste.extend_from_slice(rest);
                        at = self.buffer.len();
                    }
                }
                let text = String::from_utf8_lossy(&self.paste.take().unwrap()).into_owned();
                inputs.push(Input::Event(Event::Paste(text)));
                continue;
            }
            match step(rest, flush) {
                Step::Wait => break,
                Step::Took(length, input) => {
                    at += length;
                    inputs.extend(input);
                }
                Step::Paste(length) => {
                    at += length;
                    self.paste = Some(Vec::new());
                }
            }
        }
        self.buffer.drain(..at);
        inputs
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Option<Input> {
    key_kind(code, modifiers, 1)
}

fn key_kind(code: KeyCode, modifiers: KeyModifiers, kind: u32) -> Option<Input> {
    let kind = match kind {
        2 => KeyEventKind::Repeat,
        3 => KeyEventKind::Release,
        _ => KeyEventKind::Press,
    };
    Some(Input::Event(Event::Key(KeyEvent::new_with_kind(
        code, modifiers, kind,
    ))))
}

fn step(bytes: &[u8], flush: bool) -> Step {
    let control = |code, modifiers| Step::Took(1, key(code, modifiers));
    match bytes[0] {
        0x1b => escape(bytes, flush),
        b'\r' => control(KeyCode::Enter, KeyModifiers::NONE),
        b'\t' => control(KeyCode::Tab, KeyModifiers::NONE),
        0x7f => control(KeyCode::Backspace, KeyModifiers::NONE),
        0 => control(KeyCode::Char(' '), KeyModifiers::CONTROL),
        c @ 1..=0x1a => control(KeyCode::Char((c - 1 + b'a') as char), KeyModifiers::CONTROL),
        c @ 0x1c..=0x1f => control(
            KeyCode::Char((c - 0x1c + b'4') as char),
            KeyModifiers::CONTROL,
        ),
        _ => character(bytes, flush),
    }
}

fn character(bytes: &[u8], flush: bool) -> Step {
    let length = match bytes[0] {
        0..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Step::Took(1, None),
    };
    if bytes.len() < length {
        return if flush {
            Step::Took(bytes.len(), None)
        } else {
            Step::Wait
        };
    }
    match std::str::from_utf8(&bytes[..length]).map(|text| text.chars().next()) {
        Ok(Some(c)) => {
            let shift = if c.is_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            Step::Took(length, key(KeyCode::Char(c), shift))
        }
        _ => Step::Took(1, None),
    }
}

fn escape(bytes: &[u8], flush: bool) -> Step {
    let Some(&next) = bytes.get(1) else {
        return if flush {
            Step::Took(1, key(KeyCode::Esc, KeyModifiers::NONE))
        } else {
            Step::Wait
        };
    };
    match next {
        b'[' => csi(bytes),
        b'O' => match bytes.get(2) {
            Some(&c) => Step::Took(3, ss3(c)),
            None if flush => Step::Took(
                2,
                key(KeyCode::Char('O'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            ),
            None => Step::Wait,
        },
        b']' | b'P' | b'_' | b'^' | b'X' => string(bytes),
        0x1b => Step::Took(1, key(KeyCode::Esc, KeyModifiers::NONE)),
        // alt as an escape before the key
        _ => match step(&bytes[1..], flush) {
            Step::Took(length, Some(Input::Event(Event::Key(mut key)))) => {
                key.modifiers |= KeyModifiers::ALT;
                Step::Took(length + 1, Some(Input::Event(Event::Key(key))))
            }
            Step::Wait => Step::Wait,
            _ => Step::Took(1, key(KeyCode::Esc, KeyModifiers::NONE)),
        },
    }
}

fn ss3(c: u8) -> Option<Input> {
    let code = match c {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P'..=b'S' => KeyCode::F(c - b'P' + 1),
        b'M' => KeyCode::Enter,
        _ => return None,
    };
    key(code, KeyModifiers::NONE)
}

/// An OSC, DCS, APC, PM or SOS string, up to its end: a bell (OSC only) or ST.
fn string(bytes: &[u8]) -> Step {
    let osc = bytes[1] == b']';
    let end = |length, body: &[u8]| Step::Took(length, osc.then(|| color(body)).flatten());
    for (i, &byte) in bytes.iter().enumerate().skip(2) {
        match byte {
            0x07 if osc => return end(i + 1, &bytes[2..i]),
            0x1b => {
                return match bytes.get(i + 1) {
                    Some(b'\\') => end(i + 2, &bytes[2..i]),
                    // an escape cuts the string: what follows is another sequence
                    Some(_) => Step::Took(i, None),
                    None => Step::Wait,
                };
            }
            _ => {}
        }
    }
    if bytes.len() > MAX_SEQUENCE {
        Step::Took(bytes.len(), None)
    } else {
        Step::Wait
    }
}

/// An answer about a color: `10;value`, `11;value` or `4;n;value`.
fn color(body: &[u8]) -> Option<Input> {
    let text = std::str::from_utf8(body).ok()?;
    let (number, rest) = text.split_once(';')?;
    let (name, value) = match number {
        "10" | "11" => (number.to_string(), rest),
        "4" => {
            let (index, value) = rest.split_once(';')?;
            index.parse::<u8>().ok()?;
            (format!("4;{index}"), value)
        }
        _ => return None,
    };
    // it is said back to programs as it is: nothing but a value
    let plain = !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic());
    plain.then(|| Input::Color(name, value.into()))
}

fn csi(bytes: &[u8]) -> Step {
    let Some(end) = bytes
        .iter()
        .skip(2)
        .position(|b| !(0x20..=0x3f).contains(b))
        .map(|i| i + 2)
    else {
        return if bytes.len() > MAX_SEQUENCE {
            Step::Took(bytes.len(), None)
        } else {
            Step::Wait
        };
    };
    let last = bytes[end];
    if !(0x40..=0x7e).contains(&last) {
        // not a sequence: what follows is read on its own
        return Step::Took(end, None);
    }
    let body = &bytes[2..end];
    if body == b"200" && last == b'~' {
        return Step::Paste(end + 1);
    }
    Step::Took(end + 1, sequence(body, last))
}

fn sequence(body: &[u8], last: u8) -> Option<Input> {
    let (marker, body) = match body.first() {
        Some(&marker @ (b'?' | b'>' | b'<' | b'=')) => (Some(marker), &body[1..]),
        _ => (None, body),
    };
    let params: Vec<Vec<Option<u32>>> = std::str::from_utf8(body)
        .ok()?
        .split(';')
        .map(|param| param.split(':').map(|n| n.parse().ok()).collect())
        .collect();
    let number = |i: usize, j: usize| {
        params
            .get(i)
            .and_then(|param| param.get(j))
            .copied()
            .flatten()
    };
    let modifiers = number(1, 0).unwrap_or(1);
    match (marker, last) {
        (Some(b'?'), b'u') => Some(Input::Keyboard(number(0, 0).unwrap_or(0) as u16)),
        (Some(b'?'), b'c') => Some(Input::Attributes),
        (Some(b'?'), b'n') if number(0, 0) == Some(997) => Some(Input::Scheme(number(1, 0)? as u8)),
        (None, b'u') => keyed(
            number(0, 0)?,
            modifiers,
            number(0, 1),
            number(0, 2),
            number(1, 1).unwrap_or(1),
        ),
        // modifyOtherKeys: CSI 27;mods;code~
        (None, b'~') if number(0, 0) == Some(27) => keyed(number(2, 0)?, modifiers, None, None, 1),
        (None, b'~') => {
            let code = match number(0, 0)? {
                1 | 7 => KeyCode::Home,
                2 => KeyCode::Insert,
                3 => KeyCode::Delete,
                4 | 8 => KeyCode::End,
                5 => KeyCode::PageUp,
                6 => KeyCode::PageDown,
                n @ 11..=15 => KeyCode::F(n as u8 - 10),
                n @ 17..=21 => KeyCode::F(n as u8 - 11),
                n @ 23..=26 => KeyCode::F(n as u8 - 12),
                n @ 28..=29 => KeyCode::F(n as u8 - 15),
                n @ 31..=34 => KeyCode::F(n as u8 - 17),
                _ => return None,
            };
            key_kind(code, held(modifiers), number(1, 1).unwrap_or(1))
        }
        (None, b'I') => Some(Input::Event(Event::FocusGained)),
        (None, b'O') => Some(Input::Event(Event::FocusLost)),
        (None, b'Z') => key_kind(
            KeyCode::BackTab,
            held(modifiers) | KeyModifiers::SHIFT,
            number(1, 1).unwrap_or(1),
        ),
        (None, b'A'..=b'D' | b'H' | b'F' | b'P'..=b'S') => {
            let code = match last {
                b'A' => KeyCode::Up,
                b'B' => KeyCode::Down,
                b'C' => KeyCode::Right,
                b'D' => KeyCode::Left,
                b'H' => KeyCode::Home,
                b'F' => KeyCode::End,
                // CSI row;col R reports the cursor; the client never asks
                b'R' if number(0, 0).is_some_and(|n| n != 1) => return None,
                _ => KeyCode::F(last - b'P' + 1),
            };
            key_kind(code, held(modifiers), number(1, 1).unwrap_or(1))
        }
        _ => None,
    }
}

/// The modifiers in a parameter: 1 + shift + 2 alt + 4 ctrl + 8 super + 16
/// hyper + 32 meta; the locks are no modifiers.
fn held(parameter: u32) -> KeyModifiers {
    let bits = parameter.saturating_sub(1);
    [
        (1, KeyModifiers::SHIFT),
        (2, KeyModifiers::ALT),
        (4, KeyModifiers::CONTROL),
        (8, KeyModifiers::SUPER),
        (16, KeyModifiers::HYPER),
        (32, KeyModifiers::META),
    ]
    .into_iter()
    .filter(|(bit, _)| bits & bit != 0)
    .fold(KeyModifiers::NONE, |all, (_, modifier)| all | modifier)
}

/// A key by its code, as kitty's escape codes and modifyOtherKeys tell it,
/// with the shifted key and the key of the base layout when told.
fn keyed(
    code: u32,
    modifiers: u32,
    shifted: Option<u32>,
    base: Option<u32>,
    kind: u32,
) -> Option<Input> {
    let mut modifiers = held(modifiers);
    let code = match code {
        27 => KeyCode::Esc,
        13 | 57414 => KeyCode::Enter,
        9 if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        9 => KeyCode::Tab,
        8 | 127 => KeyCode::Backspace,
        // the keypad
        57399..=57408 => KeyCode::Char(char::from_digit(code - 57399, 10)?),
        57409..=57413 | 57415 | 57416 => KeyCode::Char(match code {
            57409 => '.',
            57410 => '/',
            57411 => '*',
            57412 => '-',
            57413 => '+',
            57415 => '=',
            _ => ',',
        }),
        57417 => KeyCode::Left,
        57418 => KeyCode::Right,
        57419 => KeyCode::Up,
        57420 => KeyCode::Down,
        57421 => KeyCode::PageUp,
        57422 => KeyCode::PageDown,
        57423 => KeyCode::Home,
        57424 => KeyCode::End,
        57425 => KeyCode::Insert,
        57426 => KeyCode::Delete,
        57376..=57398 => KeyCode::F((code - 57363) as u8),
        // the rest of the private use area: locks, media and modifiers alone
        57344..=63743 => return None,
        _ => {
            let mut c = char::from_u32(code)?;
            let shortcut = modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
            if let Some(base) = base
                .and_then(char::from_u32)
                .filter(|_| shortcut && !c.is_ascii())
            {
                c = if modifiers.contains(KeyModifiers::SHIFT) {
                    base.to_ascii_uppercase()
                } else {
                    base
                };
            } else if let Some(upper) = shifted
                .and_then(char::from_u32)
                .filter(|_| modifiers.contains(KeyModifiers::SHIFT))
            {
                // the character tells shift itself
                c = upper;
                modifiers.remove(KeyModifiers::SHIFT);
            }
            KeyCode::Char(c)
        }
    };
    key_kind(code, modifiers, kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::notation;

    fn keys(decoder: &mut Decoder, bytes: &[u8]) -> Vec<String> {
        decoder
            .feed(bytes)
            .into_iter()
            .map(|input| match input {
                Input::Event(Event::Key(key)) => {
                    notation(&key).unwrap_or_else(|| format!("{key:?}"))
                }
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn every_encoding_reads_the_same() {
        let mut decoder = Decoder::default();
        // by default
        assert_eq!(
            keys(
                &mut decoder,
                b"a\x07\r\x1b[A\x1bOP\x1b[1;5C\x1b[3~\x1b[Z\x7f"
            ),
            [
                "a",
                "ctrl-g",
                "enter",
                "up",
                "f1",
                "ctrl-right",
                "delete",
                "backtab",
                "backspace"
            ]
        );
        assert_eq!(
            keys(&mut decoder, "Жж\x1bx\x1b\r".as_bytes()),
            ["Ж", "ж", "alt-x", "alt-enter"]
        );
        // modifyOtherKeys, as Ghostty sends shift with enter by default
        assert_eq!(
            keys(&mut decoder, b"\x1b[27;2;13~\x1b[27;5;105~"),
            ["shift-enter", "ctrl-i"]
        );
        // kitty's escape codes
        assert_eq!(
            keys(
                &mut decoder,
                b"\x1b[13;2u\x1b[27u\x1b[99;5u\x1b[9;2u\x1b[97;9u"
            ),
            ["shift-enter", "esc", "ctrl-c", "backtab", "super-a"]
        );
        // the shifted key, and the key of the base layout on a Ukrainian one
        assert_eq!(
            keys(
                &mut decoder,
                "\x1b[97:65;6u\x1b[1087::103;5u\x1b[1087:1055:103;4u".as_bytes()
            ),
            ["ctrl-a", "ctrl-g", "alt-G"]
        );
        // the keypad's enter, and a release
        let inputs = decoder.feed(b"\x1b[57414u\x1b[97;1:3u");
        let Input::Event(Event::Key(release)) = &inputs[1] else {
            panic!()
        };
        assert_eq!(release.kind, KeyEventKind::Release);
        assert_eq!(keys(&mut decoder, b"\x1b[57414u"), ["enter"]);
    }

    #[test]
    fn named_keys_keep_repeat_and_release_reports() {
        let mut decoder = Decoder::default();
        let inputs = decoder.feed(b"\x1b[1;1:3A\x1b[3;1:3~\x1b[1;2:3Z\x1b[1;1:2P\x1b[27;1:2u");
        for (input, (code, kind)) in inputs.iter().zip([
            (KeyCode::Up, KeyEventKind::Release),
            (KeyCode::Delete, KeyEventKind::Release),
            (KeyCode::BackTab, KeyEventKind::Release),
            (KeyCode::F(1), KeyEventKind::Repeat),
            (KeyCode::Esc, KeyEventKind::Repeat),
        ]) {
            let Input::Event(Event::Key(key)) = input else {
                panic!()
            };
            assert_eq!((key.code, key.kind), (code, kind));
        }
        assert_eq!(inputs.len(), 5);
    }

    #[test]
    fn sequences_wait_for_their_rest() {
        let mut decoder = Decoder::default();
        assert!(keys(&mut decoder, b"\x1b[13;").is_empty());
        assert_eq!(keys(&mut decoder, b"2u"), ["shift-enter"]);
        assert!(keys(&mut decoder, &"ж".as_bytes()[..1]).is_empty());
        assert_eq!(keys(&mut decoder, &"ж".as_bytes()[1..]), ["ж"]);
        // a lone escape is one once nothing follows in time
        assert!(keys(&mut decoder, b"\x1b").is_empty());
        assert!(decoder.pending());
        assert!(
            matches!(&decoder.flush()[..], [Input::Event(Event::Key(key))] if key.code == KeyCode::Esc)
        );
        assert!(!decoder.pending());
        assert_eq!(keys(&mut decoder, b"\x1b\x1b"), ["esc"]);
        assert_eq!(decoder.flush().len(), 1);
    }

    #[test]
    fn pastes_focus_and_answers() {
        let mut decoder = Decoder::default();
        let inputs = decoder.feed(b"\x1b[200~one\x1b[A\x1b[20");
        assert!(inputs.is_empty());
        assert_eq!(
            decoder.feed(b"1~x\x1b[I\x1b[O"),
            [
                Input::Event(Event::Paste("one\x1b[A".into())),
                Input::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('x'),
                    KeyModifiers::NONE
                ))),
                Input::Event(Event::FocusGained),
                Input::Event(Event::FocusLost),
            ]
        );
        assert_eq!(
            decoder.feed(b"\x1b[?5u\x1b[?997;2n\x1b]11;rgb:1d1d/1f1f/2121\x1b\\\x1b]4;1;rgb:cc/00/00\x07\x1b]7501;?\x1b\\\x1b[?62;22c"),
            [
                Input::Keyboard(5),
                Input::Scheme(2),
                Input::Color("11".into(), "rgb:1d1d/1f1f/2121".into()),
                Input::Color("4;1".into(), "rgb:cc/00/00".into()),
                Input::Attributes,
            ]
        );
        // what is said back to programs must be a plain value
        assert!(decoder.feed(b"\x1b]10;bad value\x07").is_empty());
    }
}
