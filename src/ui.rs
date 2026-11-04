//! Drawing: the view and the navigation panel, both derived from the state.

use std::cmp::Ordering;

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph},
};

use crate::layout::{self, Container, Draw, Placement, Scene, Shown};
use crate::runtime::{Runtime, lock};
use crate::state::{Level, Location, Navigation, Nui, Operation, Status, Terminal};

const HINTS: &str = "hjkl move  enter go  r rename  x remove  s stop  esc back  q quit";

/// The view: the scene of the attached session, with the cursor of the
/// focused pane.
pub fn view(frame: &mut Frame, nui: &Nui, runtime: &Runtime, scene: &Scene) {
    let here = nui.location();
    for shown in &scene.panes {
        let focused = Some(shown.at) == here;
        let cursor = pane(frame.buffer_mut(), nui, runtime, shown, focused);
        if let Some(cursor) = cursor.filter(|_| focused) {
            frame.set_cursor_position(cursor);
        }
    }
    draws(frame.buffer_mut(), &scene.draws);
}

/// The navigation panel: the sections over the preview of the hovered item.
pub fn navigation(
    frame: &mut Frame,
    nui: &Nui,
    runtime: &Runtime,
    place: &mut dyn FnMut(&str, &Container) -> Placement,
) {
    let Some(navigation) = &nui.navigation else { return };
    let [top, bottom] = Layout::vertical([Constraint::Percentage(50), Constraint::Fill(1)]).areas(frame.area());
    let [sections, status] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(top);
    let columns = Layout::horizontal([Constraint::Fill(1); 4]).spacing(1).split(sections);
    let buffer = frame.buffer_mut();
    for (level, column) in Level::ALL.into_iter().zip(columns.iter()) {
        section(buffer, nui, navigation, level, *column);
    }
    for column in columns.iter().skip(1) {
        for y in column.top()..column.bottom() {
            buffer[(column.x - 1, y)].set_symbol("│").set_fg(Color::DarkGray);
        }
    }
    // the first line of a message: the rest, such as a traceback, stays in the state
    match nui.message.as_deref().and_then(|message| message.lines().next()) {
        Some(message) => buffer.set_stringn(status.x + 1, status.y, message, status.width as usize, Color::Yellow),
        None => buffer.set_stringn(status.x + 1, status.y, HINTS, status.width as usize, Color::DarkGray),
    };
    preview(buffer, nui, runtime, navigation, bottom, place);
    match &navigation.operation {
        Some(Operation::Rename { input }) => rename(frame, navigation, input, columns[navigation.focus as usize], top),
        Some(operation) => confirm(frame, nui, navigation, operation, top),
        None => {}
    }
}

/// A section: the title of its level and its items, the hovered one marked
/// as yazi does: reversed in the focused section and the ones left of it,
/// underlined in the ones right of it.
fn section(buffer: &mut Buffer, nui: &Nui, navigation: &Navigation, level: Level, area: Rect) {
    let title = match level {
        Level::Session => " sessions",
        Level::Window => " windows",
        Level::Tab => " tabs",
        Level::Pane => " panes",
    };
    let title_style = if level == navigation.focus { Style::new().blue().bold() } else { Style::new().dark_gray() };
    buffer.set_stringn(area.x, area.y, title, area.width as usize, title_style);
    let list = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
    let hovered = navigation.cursor.get(level);
    let first = scroll(hovered, list.height);
    for (row, index) in (first..nui.count(level, navigation.cursor)).enumerate().take(list.height as usize) {
        let at = if index == hovered { navigation.cursor } else { nui.descend(level, navigation.cursor.with(level, index)) };
        let line = Rect { y: list.y + row as u16, height: 1, ..list };
        if index == hovered {
            buffer.set_style(line, match level.cmp(&navigation.focus) {
                Ordering::Equal => Style::new().reversed().bold(),
                Ordering::Less => Style::new().reversed(),
                Ordering::Greater => Style::new().underlined(),
            });
        }
        buffer.set_line(line.x, line.y, &item(nui, level, at, index), line.width);
    }
}

/// The first item shown, so the hovered one stays in a list of `height`.
fn scroll(hovered: usize, height: u16) -> usize {
    (hovered + 1).saturating_sub(height.max(1) as usize)
}

/// An item: a dot when its parent shows it, its label, and whether its
/// terminals run.
fn item(nui: &Nui, level: Level, at: Location, index: usize) -> Line<'static> {
    let dot = if nui.is_active(level, at) { Span::raw("●").green() } else { Span::raw(" ") };
    let label = match level {
        Level::Session => nui.label(level, at),
        _ => format!("{} {}", index + 1, nui.label(level, at)),
    };
    let mut spans = vec![" ".into(), dot, " ".into(), label.into()];
    match nui.shown(at).filter(|_| level == Level::Pane) {
        Some(terminal) if terminal.status != Status::Running => {
            let (text, color) = status(terminal);
            spans.push(Span::styled(format!(" {text}"), color));
        }
        Some(_) => {}
        None => {
            let session = &nui.sessions[at.session];
            let inside = nui.terminals_in(level, at);
            let running = |id: &u64| session.terminal(*id).is_some_and(|terminal| terminal.status == Status::Running);
            if !inside.is_empty() && !inside.iter().any(running) {
                spans.push(" stopped".yellow());
            }
        }
    }
    Line::from(spans)
}

/// The bottom half: the hovered item as the view would show it, under a rule
/// naming it.
fn preview(
    buffer: &mut Buffer,
    nui: &Nui,
    runtime: &Runtime,
    navigation: &Navigation,
    area: Rect,
    place: &mut dyn FnMut(&str, &Container) -> Placement,
) {
    if area.height < 2 {
        return;
    }
    buffer.set_string(area.x, area.y, "─".repeat(area.width as usize), Style::new().dark_gray());
    let name = format!(" {} {} ", navigation.focus.name(), nui.label(navigation.focus, navigation.cursor));
    buffer.set_stringn(area.x + 1, area.y, name, area.width.saturating_sub(2) as usize, Style::new().blue());
    let inner = Rect { y: area.y + 1, height: area.height - 1, ..area };
    let scene = layout::scene(nui, navigation.focus, navigation.cursor, inner, place);
    for shown in &scene.panes {
        pane(buffer, nui, runtime, shown, false);
    }
    draws(buffer, &scene.draws);
}

/// The input of a rename, under the hovered item as in yazi.
fn rename(frame: &mut Frame, navigation: &Navigation, input: &str, column: Rect, top: Rect) {
    let screen = frame.area();
    let hovered = navigation.cursor.get(navigation.focus);
    let row = column.y + 1 + (hovered - scroll(hovered, column.height.saturating_sub(1))) as u16;
    let width = column.width.max(32).min(screen.width);
    let y = (row + 1).min(top.bottom().saturating_sub(3));
    let popup = Rect::new(column.x.min(screen.width - width), y, width, 3).intersection(screen);
    if popup.height < 3 {
        return;
    }
    // the end of a long input stays in sight
    let room = width.saturating_sub(3) as usize;
    let shown: String = input.chars().skip(input.chars().count().saturating_sub(room)).collect();
    let cursor = Position::new(popup.x + 1 + Line::raw(shown.as_str()).width() as u16, popup.y + 1);
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(shown).block(popup_block("Rename:")), popup);
    frame.set_cursor_position(cursor);
}

/// The question of a removal or a stop, centered.
fn confirm(frame: &mut Frame, nui: &Nui, navigation: &Navigation, operation: &Operation, top: Rect) {
    let (focus, cursor) = (navigation.focus, navigation.cursor);
    let what = format!("{} {}", focus.name(), nui.label(focus, cursor));
    let (title, question) = match operation {
        Operation::Remove if focus == Level::Pane => ("Remove?", format!("Remove {what}?")),
        Operation::Remove => ("Remove?", format!("Remove {what} and all inside it?")),
        _ => {
            let session = &nui.sessions[cursor.session];
            let inside = nui.terminals_in(focus, cursor);
            let running = inside.iter().filter_map(|id| session.terminal(*id)).filter(|t| t.status == Status::Running);
            ("Stop?", format!("Stop {what}: {} running terminal(s)?", running.count()))
        }
    };
    let width = (Line::raw(question.as_str()).width() as u16 + 4).max(34).min(top.width);
    let height = 5.min(top.height);
    let popup = Rect::new(top.x + (top.width - width) / 2, top.y + (top.height - height) / 2, width, height);
    if popup.height < 3 {
        return;
    }
    let lines = vec![Line::raw(question), Line::raw(""), Line::raw("enter: confirm   esc: cancel").dark_gray()];
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).centered().block(popup_block(title)), popup);
}

fn popup_block(title: &str) -> Block<'_> {
    Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().blue()).title(title)
}

/// A pane: the screen of its terminal, with a badge when it doesn't run.
/// Returns the cursor of the terminal, when in sight.
fn pane(buffer: &mut Buffer, nui: &Nui, runtime: &Runtime, shown: &Shown, focused: bool) -> Option<Position> {
    let terminal = nui.shown(shown.at)?;
    let running = terminal.status == Status::Running;
    let cursor = runtime.screen(terminal.id).and_then(|screen| {
        let parser = lock(screen);
        let cursor = draw_screen(buffer, shown.area, parser.screen(), !running);
        cursor.filter(|_| running && !parser.screen().hide_cursor())
    });
    if !running {
        let (text, color) = status(terminal);
        let hint = if focused { " · enter: start" } else { "" };
        let badge = format!(" {text}{hint} ");
        let width = Line::raw(badge.as_str()).width() as u16;
        let x = shown.area.right().saturating_sub(width).max(shown.area.x);
        buffer.set_stringn(x, shown.area.y, badge, shown.area.width as usize, Style::new().black().bg(color));
    }
    cursor
}

/// How a terminal is, in words and color.
fn status(terminal: &Terminal) -> (String, Color) {
    match (terminal.status, terminal.exit) {
        (Status::Running, _) => ("running".into(), Color::Green),
        (Status::Stopped, _) => ("stopped".into(), Color::Yellow),
        (Status::Exited, Some(code)) => (format!("exited {code}"), Color::Red),
        (Status::Exited, None) => ("failed".into(), Color::Red),
    }
}

/// Draw a terminal screen in an area. A taller screen shows its rows down to
/// the cursor; returns where the cursor is, when inside the area.
fn draw_screen(buffer: &mut Buffer, area: Rect, screen: &vt100::Screen, dim: bool) -> Option<Position> {
    let (rows, cols) = (screen.size().0.get(), screen.size().1.get());
    let (cursor_row, cursor_col) = screen.cursor_position();
    let top = if rows > area.height { (cursor_row + 1).saturating_sub(area.height).min(rows - area.height) } else { 0 };
    for y in 0..area.height.min(rows - top) {
        let mut x = 0;
        while x < area.width.min(cols) {
            let Some(cell) = screen.cell(top + y, x) else { break };
            let position = (area.x + x, area.y + y);
            if cell.is_wide() && x + 1 >= area.width {
                // no room for the second half of a wide character
                buffer[position].reset();
                break;
            }
            let symbol = if cell.has_contents() { cell.contents() } else { " " };
            buffer[position].set_symbol(symbol).set_style(cell_style(cell, dim));
            if cell.is_wide() {
                buffer[(position.0 + 1, position.1)].reset();
                x += 2;
            } else {
                x += 1;
            }
        }
    }
    let inside = cursor_row >= top && cursor_row - top < area.height && cursor_col < area.width;
    inside.then(|| Position::new(area.x + cursor_col, area.y + cursor_row - top))
}

fn cell_style(cell: &vt100::Cell, dim: bool) -> Style {
    let mut modifier = Modifier::empty();
    for (on, flag) in [
        (cell.bold(), Modifier::BOLD),
        (cell.dim() || dim, Modifier::DIM),
        (cell.italic(), Modifier::ITALIC),
        (cell.underline(), Modifier::UNDERLINED),
        (cell.inverse(), Modifier::REVERSED),
    ] {
        if on {
            modifier |= flag;
        }
    }
    Style::new().fg(color(cell.fgcolor())).bg(color(cell.bgcolor())).add_modifier(modifier)
}

fn color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// What layouts drew, over the panes.
fn draws(buffer: &mut Buffer, draws: &[Draw]) {
    for draw in draws {
        if buffer.area.contains(Position::new(draw.x, draw.y)) {
            buffer.set_stringn(draw.x, draw.y, &draw.text, draw.width as usize, draw.style);
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::state::tests::sample;

    #[test]
    fn draws_at_any_size() {
        let (events, _) = std::sync::mpsc::channel();
        let runtime = Runtime::new(events, (24, 80));
        let operations = [None, Some(Operation::Rename { input: "x".repeat(50) }), Some(Operation::Remove), Some(Operation::Stop)];
        for (width, height) in [(1, 1), (10, 3), (40, 2), (30, 8), (200, 60)] {
            for operation in operations.clone() {
                for focus in Level::ALL {
                    let mut nui = sample();
                    nui.navigation = Some(Navigation { focus, cursor: Location::default(), operation: operation.clone() });
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|frame| navigation(frame, &nui, &runtime, &mut |_, c| layout::tabs(c))).unwrap();
                }
            }
        }
    }
}
