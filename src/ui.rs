//! Drawing: shared session state and client presence, styled by decoration.

use std::{cmp::Ordering, path::Path};

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph},
};

use crate::decoration::{Decoration, Theme};
use crate::folders::{self, Item, Tree, View};
use crate::layout::{self, Container, Draw, Placement, Scene, Shown};
use crate::runtime::{Screens, lock};
use crate::state::{
    Level, Location, Navigation, Nui, Operation, StateRepresentation, Status, Terminal,
};

/// The keys of a representation: the fs one is only moved in.
fn hints(d: &Decoration, representation: StateRepresentation) -> String {
    let operations = match representation {
        StateRepresentation::Native => format!(
            "  {} add  {} rename  {} remove  {} stop",
            d.hint("add"),
            d.hint("rename"),
            d.hint("remove"),
            d.hint("stop")
        ),
        StateRepresentation::Fs => String::new(),
    };
    format!(
        "{}{}{}{} move  {} go  {}{} tabs{operations}  {} back  {} detach  {} shutdown",
        d.hint("left"),
        d.hint("next"),
        d.hint("previous"),
        d.hint("right"),
        d.hint("enter"),
        d.hint("previous-tab"),
        d.hint("next-tab"),
        d.hint("back"),
        d.hint("detach"),
        d.hint("quit")
    )
}

/// The view: the scene of the attached session, with the cursor of the
/// focused pane.
pub fn view(
    frame: &mut Frame,
    nui: &Nui,
    runtime: &impl Screens,
    scene: &Scene,
    decoration: &Decoration,
) {
    let here = nui.location();
    for shown in &scene.panes {
        let focused = Some(shown.at) == here;
        let cursor = pane(frame.buffer_mut(), nui, runtime, shown, focused, decoration);
        if let Some(cursor) = cursor.filter(|_| focused) {
            frame.set_cursor_position(cursor);
        }
    }
    draws(frame.buffer_mut(), &scene.draws);
}

/// The navigation panel: the tabs of the representations of the state, and
/// the sections of the shown one over the preview of what it points to.
pub fn navigation(
    frame: &mut Frame,
    nui: &Nui,
    runtime: &impl Screens,
    decoration: &Decoration,
    place: &mut dyn FnMut(&str, &Container) -> Placement,
) {
    let Some(navigation) = &nui.navigation else {
        return;
    };
    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Fill(1)]).areas(frame.area());
    let [bar, sections, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(top);
    let columns = Layout::horizontal([Constraint::Fill(1); 4])
        .spacing(1)
        .split(sections);
    let buffer = frame.buffer_mut();
    let theme = &decoration.theme;
    tabs(buffer, bar, navigation.representation, theme);
    match navigation.representation {
        StateRepresentation::Native => {
            for (level, column) in Level::ALL.into_iter().zip(columns.iter()) {
                section(buffer, nui, runtime, navigation, level, *column, theme);
            }
            rules(buffer, &columns, theme);
        }
        StateRepresentation::Fs => fs(buffer, nui, runtime, navigation, sections, decoration),
    }
    // the first line of a message: the rest, such as a traceback, stays in the state
    let (text, color) = match nui
        .message
        .as_deref()
        .and_then(|message| message.lines().next())
    {
        Some(message) => (message.to_string(), theme.notice),
        None => (hints(decoration, navigation.representation), theme.muted),
    };
    if !status.is_empty() {
        buffer.set_stringn(status.x + 1, status.y, text, status.width as usize, color);
    }
    preview(buffer, nui, runtime, navigation, bottom, decoration, place);
    let column = columns[navigation.focus as usize];
    match &navigation.operation {
        Some(Operation::Rename { input }) => prompt(
            frame,
            navigation,
            "Rename:",
            input,
            column,
            top,
            &decoration.theme,
        ),
        Some(Operation::Create { input }) => {
            let title = format!("New {}:", navigation.focus.name());
            prompt(
                frame,
                navigation,
                &title,
                input,
                column,
                top,
                &decoration.theme,
            );
        }
        Some(operation) => confirm(frame, nui, navigation, operation, top, &decoration.theme),
        None => {}
    }
}

/// The top border: a tab for each representation of the state, the shown
/// one marked.
fn tabs(buffer: &mut Buffer, area: Rect, shown: StateRepresentation, theme: &Theme) {
    if area.is_empty() {
        return;
    }
    buffer.set_string(
        area.x,
        area.y,
        "─".repeat(area.width as usize),
        Style::new().fg(theme.muted),
    );
    let mut x = area.x + 1;
    for representation in StateRepresentation::ALL {
        let style = if representation == shown {
            Style::new().fg(theme.accent).bold().reversed()
        } else {
            Style::new().fg(theme.muted)
        };
        let name = format!(" {} ", representation.name());
        let room = area.right().saturating_sub(x) as usize;
        x = buffer
            .set_stringn(x, area.y, name, room, style)
            .0
            .saturating_add(1);
    }
}

/// The rules between sections.
fn rules(buffer: &mut Buffer, columns: &[Rect], theme: &Theme) {
    for column in columns.iter().skip(1).filter(|column| column.x > 0) {
        for y in column.top()..column.bottom() {
            let position = Position::new(column.x - 1, y);
            if buffer.area.contains(position) {
                buffer[position].set_symbol("│").set_fg(theme.muted);
            }
        }
    }
}

/// A section: the title of its level and its items, the hovered one marked
/// as yazi does: reversed in the focused section and the ones left of it,
/// underlined in the ones right of it.
fn section(
    buffer: &mut Buffer,
    nui: &Nui,
    runtime: &impl Screens,
    navigation: &Navigation,
    level: Level,
    area: Rect,
    theme: &Theme,
) {
    let title = match level {
        Level::Session => " sessions",
        Level::Workspace => " workspaces",
        Level::Window => " windows",
        Level::Pane => " panes",
    };
    let title = if level == navigation.focus {
        Span::raw(title).fg(theme.accent).bold()
    } else {
        Span::raw(title).fg(theme.muted)
    };
    let hovered = navigation.cursor.get(level);
    let style = match level.cmp(&navigation.focus) {
        Ordering::Equal => Style::new().reversed().bold(),
        Ordering::Less => Style::new().reversed(),
        Ordering::Greater => Style::new().underlined(),
    };
    let (count, marked) = (nui.count(level, navigation.cursor), Some((hovered, style)));
    listing(buffer, area, title, count, marked, |index| {
        let at = if index == hovered {
            navigation.cursor
        } else {
            nui.descend(level, navigation.cursor.with(level, index))
        };
        item(nui, runtime, level, at, index, theme)
    });
}

/// The sections of the fs representation: the list the person came from,
/// the folder they are in, and where its hovered item leads; the items on
/// the way to the terminal pointed at marked as in the native one.
fn fs(
    buffer: &mut Buffer,
    nui: &Nui,
    runtime: &impl Screens,
    navigation: &Navigation,
    area: Rect,
    decoration: &Decoration,
) {
    let theme = &decoration.theme;
    let columns = Layout::horizontal([Constraint::Fill(1); 3])
        .spacing(1)
        .split(area);
    rules(buffer, &columns, theme);
    let Some(view) = View::new(nui, navigation) else {
        return;
    };
    let (tree, home) = (&view.tree, folders::home());
    let home = home.as_deref();
    // the path of a folder; its end, in a narrow section
    let heading = |node: usize, area: Rect| {
        let path = tree.title(node, home);
        Span::raw(format!(" {}", tail(&path, area.width.saturating_sub(1))))
    };
    let line = |node: usize| fs_item(nui, runtime, tree, node, home, decoration);
    let marked = |list: &[usize], node: Option<usize>, style: Style| {
        let index = list.iter().position(|child| Some(*child) == node);
        index.map(|index| (index, style))
    };
    let center = view.center();
    if let Some(parent) = tree.parent(center) {
        let list = tree.children(parent);
        let title = heading(parent, columns[0]).fg(theme.muted);
        let hovered = marked(list, Some(center), Style::new().reversed());
        listing(buffer, columns[0], title, list.len(), hovered, |index| {
            line(list[index])
        });
    }
    let list = tree.children(center);
    let title = heading(center, columns[1]).fg(theme.accent).bold();
    let hovered = marked(list, Some(view.node()), Style::new().reversed().bold());
    listing(buffer, columns[1], title, list.len(), hovered, |index| {
        line(list[index])
    });
    match tree.item(view.node()) {
        Item::Folder(_) => {
            let list = tree.children(view.node());
            let title = heading(view.node(), columns[2]).fg(theme.muted);
            let hovered = marked(list, view.next(), Style::new().underlined());
            listing(buffer, columns[2], title, list.len(), hovered, |index| {
                line(list[index])
            });
        }
        Item::Terminal(at) => {
            let panes = folders::panes(nui, *at);
            let title = Span::raw(" panes").fg(theme.muted);
            let hovered = panes
                .iter()
                .position(|pane| *pane == navigation.cursor)
                .map(|index| (index, Style::new().underlined()));
            listing(buffer, columns[2], title, panes.len(), hovered, |index| {
                levels(nui, panes[index], theme)
            });
        }
    }
}

/// A list under its title, the hovered item marked and kept in sight.
fn listing(
    buffer: &mut Buffer,
    area: Rect,
    title: Span,
    count: usize,
    hovered: Option<(usize, Style)>,
    item: impl Fn(usize) -> Line<'static>,
) {
    if area.is_empty() {
        return;
    }
    buffer.set_span(area.x, area.y, &title, area.width);
    let list = Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    };
    let first = hovered.map_or(0, |(index, _)| scroll(index, list.height));
    for (row, index) in (first..count).enumerate().take(list.height as usize) {
        let line = Rect {
            y: list.y + row as u16,
            height: 1,
            ..list
        };
        if let Some((_, style)) = hovered.filter(|(hovered, _)| *hovered == index) {
            buffer.set_style(line, style);
        }
        buffer.set_line(line.x, line.y, &item(index), line.width);
    }
}

/// The end of `text` in `width` cells, its start given to an ellipsis: the
/// end tells paths apart.
fn tail(text: &str, width: u16) -> String {
    let width = width as usize;
    if Line::raw(text).width() <= width {
        return text.into();
    }
    let mut start = text.len();
    for (index, _) in text.char_indices().rev() {
        if Line::raw(&text[index..]).width() >= width {
            break;
        }
        start = index;
    }
    format!("…{}", &text[start..])
}

/// The first item shown, so the hovered one stays in a list of `height`.
fn scroll(hovered: usize, height: u16) -> usize {
    (hovered + 1).saturating_sub(height.max(1) as usize)
}

/// An item: a dot when its parent shows it, its label, whether its terminals
/// run, and what their programs call attention to.
fn item(
    nui: &Nui,
    runtime: &impl Screens,
    level: Level,
    at: Location,
    index: usize,
    theme: &Theme,
) -> Line<'static> {
    let dot = if nui.is_active(level, at) {
        Span::raw("●").fg(theme.active)
    } else {
        Span::raw(" ")
    };
    let label = match level {
        Level::Session => nui.label(level, at),
        _ => format!("{} {}", index + 1, nui.label(level, at)),
    };
    let mut spans = vec![" ".into(), dot, " ".into(), label.into()];
    let inside: Vec<(usize, u64)> = nui
        .terminals_in(level, at)
        .into_iter()
        .map(|id| (at.session, id))
        .collect();
    spans.extend(marks(nui, runtime, &inside, level == Level::Pane, theme));
    Line::from(spans)
}

/// An item of the fs representation, by an icon of its kind: a folder, with
/// what its terminals tell; or a terminal by its pane, with a dot when its
/// session shows it, and the session when there are more.
fn fs_item(
    nui: &Nui,
    runtime: &impl Screens,
    tree: &Tree,
    node: usize,
    home: Option<&Path>,
    decoration: &Decoration,
) -> Line<'static> {
    let (theme, icons) = (&decoration.theme, decoration.icons);
    let Item::Terminal(at) = tree.item(node) else {
        let icon = Span::raw(icons.folder()).fg(theme.accent);
        let name = format!("{}/", tree.name(node, home).trim_end_matches('/'));
        let inside = tree.terminals(nui, node);
        let mut spans = vec!["   ".into(), icon, " ".into(), name.into()];
        spans.extend(marks(nui, runtime, &inside, false, theme));
        return Line::from(spans);
    };
    let session = &nui.sessions[at.session];
    let id = nui.pane(*at).map_or(0, |pane| pane.terminal);
    let dot = if session.active_terminal() == Some(id) {
        Span::raw("●").fg(theme.active)
    } else {
        Span::raw(" ")
    };
    let label = format!(" {} {}", icons.terminal(), nui.label(Level::Pane, *at));
    let mut spans = vec![" ".into(), dot, label.into()];
    if nui.sessions.len() > 1 {
        spans.push(Span::styled(format!(" · {}", session.name), theme.muted));
    }
    spans.extend(marks(nui, runtime, &[(at.session, id)], true, theme));
    Line::from(spans)
}

/// Where a pane is: the names of its levels, with a dot when its session
/// shows it.
fn levels(nui: &Nui, at: Location, theme: &Theme) -> Line<'static> {
    let dot = if nui.active_location(at.session) == Some(at) {
        Span::raw("●").fg(theme.active)
    } else {
        Span::raw(" ")
    };
    let names = Level::ALL.map(|level| nui.label(level, at)).join(" › ");
    Line::from(vec![" ".into(), dot, " ".into(), names.into()])
}

/// What the terminals inside an item tell, by session and id: how the one of
/// a pane ended, when it doesn't run; that all are stopped, when none runs;
/// and what the programs running call attention to.
fn marks(
    nui: &Nui,
    runtime: &impl Screens,
    inside: &[(usize, u64)],
    pane: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let terminal = |(session, id): (usize, u64)| nui.sessions.get(session)?.terminal(id);
    if pane
        && let [one] = inside
        && let Some(terminal) = terminal(*one).filter(|terminal| terminal.status != Status::Running)
    {
        let (text, color) = status(terminal, theme);
        return vec![Span::styled(format!(" {text}"), color)];
    }
    let running: Vec<(usize, u64)> = inside
        .iter()
        .copied()
        .filter(|inside| {
            terminal(*inside).is_some_and(|terminal| terminal.status == Status::Running)
        })
        .collect();
    let mut spans = Vec::new();
    if !inside.is_empty() && running.is_empty() {
        spans.push(Span::styled(" stopped", theme.notice));
    }
    let (progress, bell) = attention(runtime, &running);
    if let Some(progress) = progress {
        spans.push(Span::styled(format!(" {progress}"), theme.accent));
    }
    if bell {
        spans.push(Span::styled(" bell", theme.notice));
    }
    spans
}

/// What the programs of terminals, by session and id, call attention to:
/// their progress in a word, and whether a bell rang that nobody saw.
fn attention(runtime: &impl Screens, terminals: &[(usize, u64)]) -> (Option<String>, bool) {
    let mut progress: Option<String> = None;
    let mut bell = false;
    for (session, id) in terminals {
        let Some(screen) = runtime.screen_at(*session, *id) else {
            continue;
        };
        let parser = lock(screen);
        let replies = parser.callbacks();
        bell |= replies.bell;
        let word = match replies.progress() {
            None => continue,
            Some((1, percent)) if terminals.len() == 1 => format!("{percent}%"),
            Some((2, _)) => "error".into(),
            Some((4, _)) if terminals.len() == 1 => "paused".into(),
            Some(_) => "working".into(),
        };
        // among many, an error tells the most
        if progress.as_deref() != Some("error") {
            progress = Some(word);
        }
    }
    (progress, bell)
}

/// The bottom half: what the representation points to as the view would
/// show it, under a rule naming it: the hovered item of a native section, or
/// the terminal of fs.
fn preview(
    buffer: &mut Buffer,
    nui: &Nui,
    runtime: &impl Screens,
    navigation: &Navigation,
    area: Rect,
    decoration: &Decoration,
    place: &mut dyn FnMut(&str, &Container) -> Placement,
) {
    if area.height < 2 {
        return;
    }
    buffer.set_string(
        area.x,
        area.y,
        "─".repeat(area.width as usize),
        Style::new().fg(decoration.theme.muted),
    );
    let (level, name) = match navigation.representation {
        StateRepresentation::Native => (
            navigation.focus,
            format!(
                " {} {} ",
                navigation.focus.name(),
                nui.label(navigation.focus, navigation.cursor)
            ),
        ),
        StateRepresentation::Fs => (Level::Pane, pointed(nui, navigation.cursor)),
    };
    buffer.set_stringn(
        area.x + 1,
        area.y,
        name,
        area.width.saturating_sub(2) as usize,
        Style::new().fg(decoration.theme.accent),
    );
    let inner = Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    };
    let scene = layout::scene(nui, level, navigation.cursor, inner, place);
    for shown in &scene.panes {
        pane(buffer, nui, runtime, shown, false, decoration);
    }
    draws(buffer, &scene.draws);
}

/// What the fs representation points to: a terminal by its pane, and where
/// it runs.
fn pointed(nui: &Nui, at: Location) -> String {
    let label = nui.label(Level::Pane, at);
    match nui.shown(at).and_then(|terminal| terminal.cwd.as_deref()) {
        Some(cwd) => {
            let home = folders::home();
            let cwd = folders::tilde(Path::new(cwd), home.as_deref());
            format!(" terminal {label} in {cwd} ")
        }
        None => format!(" terminal {label} "),
    }
}

/// The input of a name, under the hovered item as in yazi.
fn prompt(
    frame: &mut Frame,
    navigation: &Navigation,
    title: &str,
    input: &str,
    column: Rect,
    top: Rect,
    theme: &Theme,
) {
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
    let shown: String = input
        .chars()
        .skip(input.chars().count().saturating_sub(room))
        .collect();
    let cursor = Position::new(
        popup.x + 1 + Line::raw(shown.as_str()).width() as u16,
        popup.y + 1,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(shown).block(popup_block(title, theme)),
        popup,
    );
    frame.set_cursor_position(cursor);
}

/// The question of a removal or a stop, centered.
fn confirm(
    frame: &mut Frame,
    nui: &Nui,
    navigation: &Navigation,
    operation: &Operation,
    top: Rect,
    theme: &Theme,
) {
    let (focus, cursor) = (navigation.focus, navigation.cursor);
    let what = format!("{} {}", focus.name(), nui.label(focus, cursor));
    let (title, question) = match operation {
        Operation::Remove if focus == Level::Pane => ("Remove?", format!("Remove {what}?")),
        Operation::Remove => ("Remove?", format!("Remove {what} and all inside it?")),
        _ => {
            let session = &nui.sessions[cursor.session];
            let inside = nui.terminals_in(focus, cursor);
            let running = inside
                .iter()
                .filter_map(|id| session.terminal(*id))
                .filter(|t| t.status == Status::Running);
            (
                "Stop?",
                format!("Stop {what}: {} running terminal(s)?", running.count()),
            )
        }
    };
    let width = (Line::raw(question.as_str()).width() as u16 + 4)
        .max(34)
        .min(top.width);
    let height = 5.min(top.height);
    let popup = Rect::new(
        top.x + (top.width - width) / 2,
        top.y + (top.height - height) / 2,
        width,
        height,
    );
    if popup.height < 3 {
        return;
    }
    let lines = vec![
        Line::raw(question),
        Line::raw(""),
        Line::raw("enter: confirm   esc: cancel").fg(theme.muted),
    ];
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .centered()
            .block(popup_block(title, theme)),
        popup,
    );
}

fn popup_block<'a>(title: &'a str, theme: &Theme) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.accent))
        .title(title)
}

/// A pane: the screen of its terminal, with a badge when it doesn't run,
/// telling what to do in the focused one. Returns the cursor of the terminal,
/// when in sight.
fn pane(
    buffer: &mut Buffer,
    nui: &Nui,
    runtime: &impl Screens,
    shown: &Shown,
    focused: bool,
    decoration: &Decoration,
) -> Option<Position> {
    let terminal = nui.shown(shown.at)?;
    let running = terminal.status == Status::Running;
    let cursor = runtime
        .screen_at(shown.at.session, terminal.id)
        .and_then(|screen| {
            let parser = lock(screen);
            let cursor = draw_screen(buffer, shown.area, parser.screen(), !running);
            cursor.filter(|_| running && !parser.screen().hide_cursor())
        });
    if !running {
        let (text, color) = status(terminal, &decoration.theme);
        let hint = if focused {
            format!(
                " · {}: restart · {}: panel",
                decoration.hint("restart"),
                decoration.hint("panel")
            )
        } else {
            String::new()
        };
        let badge = format!(" {} {text}{hint} ", terminal.program_name());
        let width = Line::raw(badge.as_str()).width() as u16;
        let x = shown.area.right().saturating_sub(width).max(shown.area.x);
        buffer.set_stringn(
            x,
            shown.area.y,
            badge,
            shown.area.width as usize,
            Style::new().black().bg(color),
        );
    }
    cursor
}

/// How a terminal is, in words and color: an exit is an error only with a
/// code telling so.
fn status(terminal: &Terminal, theme: &Theme) -> (String, Color) {
    match (terminal.status, terminal.exit) {
        (Status::Running, _) => ("running".into(), theme.active),
        (Status::Stopped, _) => ("stopped".into(), theme.notice),
        (Status::Exited, Some(0)) => ("exited".into(), Color::Gray),
        (Status::Exited, Some(code)) => (format!("exited with {code}"), theme.error),
        (Status::Exited, None) => ("failed to start".into(), theme.error),
    }
}

/// Draw a terminal screen in an area. A taller screen shows its rows down to
/// the cursor; returns where the cursor is, when inside the area.
fn draw_screen(
    buffer: &mut Buffer,
    area: Rect,
    screen: &vt100::Screen,
    dim: bool,
) -> Option<Position> {
    let (rows, cols) = (screen.size().0.get(), screen.size().1.get());
    let (cursor_row, cursor_col) = screen.cursor_position();
    let top = if rows > area.height {
        (cursor_row + 1)
            .saturating_sub(area.height)
            .min(rows - area.height)
    } else {
        0
    };
    for y in 0..area.height.min(rows - top) {
        let mut x = 0;
        while x < area.width.min(cols) {
            let Some(cell) = screen.cell(top + y, x) else {
                break;
            };
            let position = (area.x + x, area.y + y);
            if cell.is_wide() && x + 1 >= area.width {
                // no room for the second half of a wide character
                buffer[position].reset();
                break;
            }
            let symbol = if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            buffer[position]
                .set_symbol(symbol)
                .set_style(cell_style(cell, dim));
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
    Style::new()
        .fg(color(cell.fgcolor()))
        .bg(color(cell.bgcolor()))
        .add_modifier(modifier)
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
    use crate::decoration::Icons;
    use crate::folders::tests::foldered;
    use crate::runtime::Runtime;
    use crate::state::tests::sample;

    /// The panel drawn, and its rows as text.
    fn draw(nui: &Nui, decoration: &Decoration, width: u16, height: u16) -> (Buffer, Vec<String>) {
        let (events, _) = std::sync::mpsc::channel();
        let runtime = Runtime::new(events, (24, 80));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                navigation(frame, nui, &runtime, decoration, &mut |_, c| {
                    layout::tabs(c)
                })
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        (buffer, rows)
    }

    #[test]
    fn draws_at_any_size() {
        let (events, _) = std::sync::mpsc::channel();
        let runtime = Runtime::new(events, (24, 80));
        let operations = [
            None,
            Some(Operation::Rename {
                input: "x".repeat(50),
            }),
            Some(Operation::Create {
                input: String::new(),
            }),
            Some(Operation::Remove),
            Some(Operation::Stop),
        ];
        let shown = [
            (StateRepresentation::Native, None),
            (StateRepresentation::Fs, None),
            (StateRepresentation::Fs, Some("/home/me/nui")),
        ];
        for (width, height) in [(1, 1), (20, 1), (10, 3), (40, 2), (30, 8), (200, 60)] {
            for operation in operations.clone() {
                for focus in Level::ALL {
                    for (representation, folder) in shown {
                        let mut nui = foldered();
                        nui.navigation = Some(Navigation {
                            representation,
                            focus,
                            cursor: Location::default(),
                            folder: folder.map(Into::into),
                            operation: operation.clone(),
                        });
                        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                        terminal
                            .draw(|frame| {
                                navigation(
                                    frame,
                                    &nui,
                                    &runtime,
                                    &Decoration::default(),
                                    &mut |_, c| layout::tabs(c),
                                )
                            })
                            .unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn fs_shows_where_the_person_came_from_is_and_goes() {
        let mut nui = foldered();
        nui.navigation = Some(Navigation {
            representation: StateRepresentation::Fs,
            focus: Level::Pane,
            cursor: Location::default(),
            folder: None,
            operation: None,
        });
        let (buffer, rows) = draw(&nui, &Decoration::default(), 120, 20);
        assert!(
            rows[0].contains(" native ") && rows[0].contains(" fs "),
            "{}",
            rows[0]
        );
        // from the folder above, in the folder of the terminal, to its panes
        let titles = &rows[1];
        assert!(
            titles.contains(" /home/me/nui ")
                && titles.contains(" /home/me/nui/yalij ")
                && titles.contains(" panes"),
            "{titles}"
        );
        // folders and terminals by their icons, of a Nerd Font or plain
        for icons in [Icons::Nerd, Icons::Plain] {
            let mut decoration = Decoration::default();
            decoration.icons = icons;
            let (_, rows) = draw(&nui, &decoration, 120, 20);
            let (folder, terminal) = (icons.folder(), icons.terminal());
            let expected = [
                (2, format!("{folder} web/src/")),
                (2, format!("● {terminal} sh · main")),
                (3, format!("{folder} yalij/")),
                (3, format!("● {terminal} sh · other")),
            ];
            for (row, text) in expected {
                assert!(rows[row].contains(&text), "{text}: {}", rows[row]);
            }
        }
        for row in [2, 3] {
            assert!(rows[row].contains("main › sh › sh › sh"), "{}", rows[row]);
        }
        // only to move in: no operations among the keys
        assert!(
            rows[9].contains("[] tabs") && !rows[9].contains("rename"),
            "{}",
            rows[9]
        );
        assert!(
            rows[10].contains(" terminal sh in /home/me/nui/yalij "),
            "{}",
            rows[10]
        );
        // marked: the terminal hovered, the folder it is in, the pane it goes to
        let style = |y: usize, text: &str| {
            let x = rows[y].chars().count() - rows[y].split_once(text).unwrap().1.chars().count();
            buffer[((x - text.chars().count()) as u16, y as u16)].modifier
        };
        assert!(style(2, "sh · main").contains(Modifier::REVERSED | Modifier::BOLD));
        assert!(style(3, "yalij/").contains(Modifier::REVERSED));
        assert!(!style(2, "web/src/").contains(Modifier::REVERSED));
        assert!(style(2, "main ›").contains(Modifier::UNDERLINED));
        assert!(!style(3, "main ›").contains(Modifier::UNDERLINED));
        // a long path keeps its end
        assert_eq!(tail("/home/me/nui/yalij", 8), "…i/yalij");
        assert_eq!(tail("/home", 8), "/home");
    }

    #[test]
    fn a_badge_tells_what_happened_and_what_to_do() {
        let (events, _) = std::sync::mpsc::channel();
        let runtime = Runtime::new(events, (24, 80));
        let mut nui = sample();
        let area = Rect::new(0, 0, 80, 3);
        for (exit, badge) in [
            (0, " sh exited · enter: restart · ctrl-g: panel "),
            (2, " sh exited with 2 · "),
        ] {
            let shown = &mut nui.sessions[0].terminals[0];
            (shown.status, shown.exit) = (Status::Exited, Some(exit));
            let scene = layout::scene(
                &nui,
                Level::Session,
                Location::default(),
                area,
                &mut |_, c| layout::tabs(c),
            );
            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            terminal
                .draw(|frame| view(frame, &nui, &runtime, &scene, &Decoration::default()))
                .unwrap();
            let top: String = (0..area.width)
                .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
                .collect();
            assert!(top.contains(badge), "{top}");
        }
    }
}
