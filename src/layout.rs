//! Layout strategies: how a container places its children in its area.
//!
//! `tabs` is built in: the active child takes the whole area, the others
//! wait their turn. Plugins add strategies, or replace this one, by name.

use ratatui::{
    layout::{Position, Rect},
    style::Style,
};

use crate::state::{Level, Location, Nui};

/// A container about to place its children.
pub struct Container {
    /// The level of the container: a session places workspaces, and so on.
    pub kind: Level,
    pub area: Rect,
    pub active: usize,
    /// The labels of the children.
    pub children: Vec<String>,
}

/// Where the children of a container go, and what it draws around them.
#[derive(Default)]
pub struct Placement {
    /// An area for each shown child; hidden ones have none.
    pub areas: Vec<Option<Rect>>,
    pub draws: Vec<Draw>,
}

/// Text a layout draws, e.g. a bar naming the children.
pub struct Draw {
    pub x: u16,
    pub y: u16,
    pub text: String,
    pub style: Style,
    /// The width it may take: up to the edge of its container.
    pub width: u16,
}

/// The built-in strategy: only the active child, over the whole area.
pub fn tabs(container: &Container) -> Placement {
    let mut areas = vec![None; container.children.len()];
    if let Some(area) = areas.get_mut(container.active) {
        *area = Some(container.area);
    }
    Placement {
        areas,
        draws: Vec::new(),
    }
}

/// What a node shows in an area: its visible panes and what layouts drew.
#[derive(Default)]
pub struct Scene {
    pub panes: Vec<Shown>,
    pub draws: Vec<Draw>,
}

/// A visible pane.
pub struct Shown {
    pub at: Location,
    pub terminal: u64,
    pub area: Rect,
}

/// The scene of the node at `level` of `at` in `area`; `place` runs the layout
/// strategy of each container by name.
pub fn scene(
    nui: &Nui,
    level: Level,
    at: Location,
    area: Rect,
    place: &mut dyn FnMut(&str, &Container) -> Placement,
) -> Scene {
    let mut scene = Scene::default();
    walk(nui, level, at, area, place, &mut scene);
    scene
}

fn walk(
    nui: &Nui,
    level: Level,
    at: Location,
    area: Rect,
    place: &mut dyn FnMut(&str, &Container) -> Placement,
    scene: &mut Scene,
) {
    if area.is_empty() {
        return;
    }
    let (layout, active, count) = match level {
        Level::Session => match nui.sessions.get(at.session) {
            Some(session) => (&session.layout, session.active, session.workspaces.len()),
            None => return,
        },
        Level::Workspace => match nui.workspace(at) {
            Some(workspace) => (&workspace.layout, workspace.active, workspace.windows.len()),
            None => return,
        },
        Level::Window => match nui.window(at) {
            Some(window) => (&window.layout, window.active, window.panes.len()),
            None => return,
        },
        Level::Pane => {
            if let Some(pane) = nui.pane(at) {
                scene.panes.push(Shown {
                    at,
                    terminal: pane.terminal,
                    area,
                });
            }
            return;
        }
    };
    let child = level.step(1);
    let children = (0..count)
        .map(|index| nui.label(child, at.with(child, index)))
        .collect();
    let placement = place(
        layout,
        &Container {
            kind: level,
            area,
            active,
            children,
        },
    );
    // a layout draws inside its own area only
    scene
        .draws
        .extend(placement.draws.into_iter().filter_map(|mut draw| {
            draw.width = draw.width.min(area.right().saturating_sub(draw.x));
            area.contains(Position::new(draw.x, draw.y)).then_some(draw)
        }));
    for (index, child_area) in placement.areas.into_iter().enumerate().take(count) {
        if let Some(child_area) = child_area {
            walk(
                nui,
                child,
                at.with(child, index),
                child_area.intersection(area),
                place,
                scene,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::sample;

    #[test]
    fn tabs_show_the_active_path() {
        let nui = sample();
        let area = Rect::new(0, 0, 80, 24);
        let scene = scene(
            &nui,
            Level::Session,
            Location::default(),
            area,
            &mut |_, container| tabs(container),
        );
        assert_eq!(scene.panes.len(), 1);
        assert_eq!(
            (scene.panes[0].at, scene.panes[0].area),
            (Location::default(), area)
        );
        // a workspace shows its own active window, whatever the session shows
        let at = Location {
            workspace: 1,
            ..Location::default()
        };
        let scene = self::scene(&nui, Level::Workspace, at, area, &mut |_, container| {
            tabs(container)
        });
        assert_eq!(scene.panes[0].terminal, 3);
    }

    #[test]
    fn layouts_place_children_side_by_side() {
        let nui = sample();
        let columns = |container: &Container| {
            let width = container.area.width / container.children.len() as u16;
            let areas = (0..container.children.len() as u16)
                .map(|i| {
                    Some(Rect {
                        x: container.area.x + i * width,
                        width,
                        ..container.area
                    })
                })
                .collect();
            Placement {
                areas,
                draws: Vec::new(),
            }
        };
        let area = Rect::new(0, 0, 80, 24);
        let scene = scene(
            &nui,
            Level::Window,
            Location::default(),
            area,
            &mut |_, container| columns(container),
        );
        let xs: Vec<u16> = scene.panes.iter().map(|shown| shown.area.x).collect();
        assert_eq!(xs, [0, 40]);
    }
}
