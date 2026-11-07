//! Temporal Escape gestures belong to an instance, never to its shared PTY.
//! Single presses pass through immediately. Kitty's event reports make holds
//! precise; without releases, only a recent auto-repeat burst is evidence.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gestures {
    pub double_escape_ms: u64,
    pub hold_escape_ms: u64,
}

impl Default for Gestures {
    fn default() -> Self {
        Self {
            double_escape_ms: 300,
            hold_escape_ms: 600,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Pass,
    Consume,
    Open,
}

const REPEAT_GAP: Duration = Duration::from_millis(150);

#[derive(Default)]
pub struct Escapes {
    /// The terminal reports repeats and releases (kitty flag 2).
    pub events: bool,
    last_press: Option<Instant>,
    down: Option<Down>,
    fired: bool,
}

struct Down {
    since: Instant,
    latest: Instant,
    repeats: usize,
}

impl Escapes {
    pub fn reset(&mut self) {
        *self = Self {
            events: self.events,
            ..Self::default()
        };
    }

    pub fn key(
        &mut self,
        key: &KeyEvent,
        in_view: bool,
        settings: Gestures,
        now: Instant,
    ) -> Action {
        if key.code != KeyCode::Esc || !key.modifiers.is_empty() {
            if key.kind != KeyEventKind::Release && !self.fired {
                self.reset();
            }
            return Action::Pass;
        }
        if key.kind == KeyEventKind::Release {
            // A release can arrive between timer polls, after the threshold.
            let open = in_view && self.poll(settings, now);
            if self.fired {
                self.last_press = None;
            }
            self.down = None;
            self.fired = false;
            return if open { Action::Open } else { Action::Pass };
        }
        if self.fired {
            if self.events
                || self
                    .down
                    .as_ref()
                    .is_some_and(|down| now.saturating_duration_since(down.latest) <= REPEAT_GAP)
            {
                if let Some(down) = &mut self.down {
                    down.latest = now;
                }
                return Action::Consume;
            }
            self.reset();
        }
        if !in_view || (settings.double_escape_ms == 0 && settings.hold_escape_ms == 0) {
            self.reset();
            return Action::Pass;
        }
        let repeating = self.down.as_ref().is_some_and(|down| {
            self.events
                || (settings.hold_escape_ms > 0
                    && down.repeats > 0
                    && now.saturating_duration_since(down.latest) <= REPEAT_GAP)
        });
        if !repeating
            && settings.double_escape_ms > 0
            && self.last_press.is_some_and(|last| {
                now.saturating_duration_since(last)
                    <= Duration::from_millis(settings.double_escape_ms)
            })
        {
            self.fire(now);
            return Action::Open;
        }
        if let Some(down) = &mut self.down
            && (repeating
                || (!self.events
                    && settings.hold_escape_ms > 0
                    && down.repeats == 0
                    && now.saturating_duration_since(down.latest)
                        <= Duration::from_millis(settings.hold_escape_ms.max(1000))))
        {
            down.latest = now;
            down.repeats = down.repeats.saturating_add(1);
        } else {
            self.down = Some(Down {
                since: now,
                latest: now,
                repeats: 0,
            });
        }
        self.last_press = Some(now);
        if self.poll(settings, now) {
            Action::Open
        } else {
            Action::Pass
        }
    }

    /// Whether a timer could recognize a hold, without processing another key.
    pub fn waiting(&self) -> bool {
        !self.fired
            && self
                .down
                .as_ref()
                .is_some_and(|down| self.events || down.repeats >= 2)
    }

    pub fn poll(&mut self, settings: Gestures, now: Instant) -> bool {
        if !self.events
            && !self.fired
            && self.down.as_ref().is_some_and(|down| {
                down.repeats >= 2 && now.saturating_duration_since(down.latest) > REPEAT_GAP
            })
        {
            self.down = None;
        }
        if settings.hold_escape_ms > 0
            && self.waiting()
            && self.down.as_ref().is_some_and(|down| {
                now.saturating_duration_since(down.since)
                    >= Duration::from_millis(settings.hold_escape_ms)
                    && (self.events || now.saturating_duration_since(down.latest) <= REPEAT_GAP)
            })
        {
            self.fire(now);
            true
        } else {
            false
        }
    }

    fn fire(&mut self, now: Instant) {
        self.fired = true;
        self.last_press = None;
        self.down.get_or_insert(Down {
            since: now,
            latest: now,
            repeats: 0,
        });
        if let Some(down) = &mut self.down {
            down.latest = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn esc(kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Esc, KeyModifiers::NONE, kind)
    }

    #[test]
    fn double_press_is_per_view_and_does_not_delay_single_escape() {
        let now = Instant::now();
        let settings = Gestures::default();
        for events in [false, true] {
            let mut keys = Escapes {
                events,
                ..Escapes::default()
            };
            assert_eq!(
                keys.key(&esc(KeyEventKind::Press), true, settings, now),
                Action::Pass
            );
            if events {
                keys.key(
                    &esc(KeyEventKind::Release),
                    true,
                    settings,
                    now + Duration::from_millis(40),
                );
            }
            assert_eq!(
                keys.key(
                    &esc(KeyEventKind::Press),
                    true,
                    settings,
                    now + Duration::from_millis(100)
                ),
                Action::Open
            );
            assert_eq!(
                keys.key(
                    &esc(KeyEventKind::Repeat),
                    false,
                    settings,
                    now + Duration::from_millis(130)
                ),
                Action::Consume
            );
            keys.reset();
            assert_eq!(
                keys.key(&esc(KeyEventKind::Press), false, settings, now),
                Action::Pass
            );
            assert_eq!(
                keys.key(
                    &esc(KeyEventKind::Press),
                    true,
                    settings,
                    now + Duration::from_millis(100)
                ),
                Action::Pass
            );
        }
    }

    #[test]
    fn hold_uses_release_reports_not_a_guess_from_a_single_press() {
        let now = Instant::now();
        let settings = Gestures::default();
        let mut keys = Escapes {
            events: true,
            ..Escapes::default()
        };
        keys.key(&esc(KeyEventKind::Press), true, settings, now);
        assert!(!keys.poll(settings, now + Duration::from_millis(599)));
        assert_eq!(
            keys.key(
                &esc(KeyEventKind::Repeat),
                true,
                settings,
                now + Duration::from_millis(200)
            ),
            Action::Pass
        );
        assert!(keys.poll(settings, now + Duration::from_millis(600)));
        assert!(!keys.poll(settings, now + Duration::from_millis(700)));
        keys.key(
            &esc(KeyEventKind::Release),
            false,
            settings,
            now + Duration::from_millis(800),
        );
        assert_eq!(
            keys.key(
                &esc(KeyEventKind::Press),
                false,
                settings,
                now + Duration::from_millis(900)
            ),
            Action::Pass
        );
        keys.key(
            &esc(KeyEventKind::Press),
            true,
            settings,
            now + Duration::from_secs(2),
        );
        keys.key(
            &esc(KeyEventKind::Release),
            true,
            settings,
            now + Duration::from_millis(2100),
        );
        assert!(!keys.poll(settings, now + Duration::from_secs(3)));
        keys.events = false;
        keys.key(
            &esc(KeyEventKind::Press),
            true,
            settings,
            now + Duration::from_secs(4),
        );
        assert!(!keys.poll(settings, now + Duration::from_secs(5)));
    }

    #[test]
    fn legacy_hold_requires_a_recent_repeat_burst() {
        let now = Instant::now();
        let settings = Gestures::default();
        let mut keys = Escapes::default();
        for ms in [0, 500] {
            assert_eq!(
                keys.key(
                    &esc(KeyEventKind::Press),
                    true,
                    settings,
                    now + Duration::from_millis(ms)
                ),
                Action::Pass
            );
        }
        assert!(!keys.poll(settings, now + Duration::from_millis(600)));
        for ms in [530, 560, 590] {
            assert_eq!(
                keys.key(
                    &esc(KeyEventKind::Press),
                    true,
                    settings,
                    now + Duration::from_millis(ms)
                ),
                Action::Pass
            );
        }
        assert!(keys.poll(settings, now + Duration::from_millis(600)));
        assert_eq!(
            keys.key(
                &esc(KeyEventKind::Press),
                false,
                settings,
                now + Duration::from_millis(620)
            ),
            Action::Consume
        );
        assert_eq!(
            keys.key(
                &esc(KeyEventKind::Press),
                false,
                settings,
                now + Duration::from_millis(900)
            ),
            Action::Pass
        );
    }

    #[test]
    fn disabling_holds_does_not_turn_taps_into_repeats_and_idle_bursts_expire() {
        let now = Instant::now();
        let mut keys = Escapes::default();
        let double_only = Gestures {
            hold_escape_ms: 0,
            ..Gestures::default()
        };
        for ms in [0, 500] {
            assert_eq!(
                keys.key(
                    &esc(KeyEventKind::Press),
                    true,
                    double_only,
                    now + Duration::from_millis(ms)
                ),
                Action::Pass
            );
        }
        assert_eq!(
            keys.key(
                &esc(KeyEventKind::Press),
                true,
                double_only,
                now + Duration::from_millis(600)
            ),
            Action::Open
        );
        keys.reset();
        let hold_only = Gestures {
            double_escape_ms: 0,
            hold_escape_ms: 1000,
        };
        for ms in [0, 500, 530] {
            keys.key(
                &esc(KeyEventKind::Press),
                true,
                hold_only,
                now + Duration::from_millis(ms),
            );
        }
        assert!(keys.waiting());
        assert!(!keys.poll(hold_only, now + Duration::from_millis(800)));
        assert!(!keys.waiting());
    }

    #[test]
    fn settings_and_intervening_keys_cancel_gestures() {
        let now = Instant::now();
        let mut keys = Escapes {
            events: true,
            ..Escapes::default()
        };
        let settings = Gestures {
            double_escape_ms: 0,
            hold_escape_ms: 0,
        };
        for ms in [0, 100, 200] {
            assert_eq!(
                keys.key(
                    &esc(KeyEventKind::Press),
                    true,
                    settings,
                    now + Duration::from_millis(ms)
                ),
                Action::Pass
            );
        }
        assert!(!keys.poll(settings, now + Duration::from_secs(1)));
        keys.key(&esc(KeyEventKind::Press), true, Gestures::default(), now);
        let other = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        keys.key(
            &other,
            true,
            Gestures::default(),
            now + Duration::from_millis(50),
        );
        assert_eq!(
            keys.key(
                &esc(KeyEventKind::Press),
                true,
                Gestures::default(),
                now + Duration::from_millis(100)
            ),
            Action::Pass
        );
        keys.reset();
        let modified = KeyEvent::new(KeyCode::Esc, KeyModifiers::ALT);
        assert_eq!(
            keys.key(&modified, true, Gestures::default(), now),
            Action::Pass
        );
        assert!(!keys.poll(Gestures::default(), now + Duration::from_secs(1)));
    }
}
