//! The fs representation of the state: the terminals panes show, by the
//! folders they run in. A folder is an item where their paths part or where
//! some of them run; the folders between merge into it, so the depth is what
//! the paths make it. Navigation shows three sections of it, as yazi does:
//! the list the person came from, the folder they are in, and where its
//! hovered item leads.
//!
//! It is only moved in: moving points the cursor of navigation to a pane of
//! the hovered terminal, the one every representation points to.

use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};

use crate::state::{Location, Navigation, Nui};

/// The terminals of all sessions by folders: the root is the folder their
/// paths share, and holds the terminals of an unknown folder too.
pub struct Tree {
    nodes: Vec<Node>,
}

struct Node {
    parent: Option<usize>,
    children: Vec<usize>,
    item: Item,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Folder(PathBuf),
    /// A terminal, as the pane at the location shows it.
    Terminal(Location),
}

/// A terminal on its way into the tree: the pane showing it, and the
/// components of its path.
type Leaf<'a> = (Location, Vec<Component<'a>>);

impl Tree {
    /// The tree of the terminals panes show, each at one of its panes: the
    /// one at `cursor`, or else the active one of its session, or else the
    /// first.
    pub fn new(nui: &Nui, cursor: Location) -> Tree {
        let leaves: Vec<Leaf> = shown(nui, cursor)
            .into_iter()
            .map(|at| {
                let cwd = nui.shown(at).and_then(|terminal| terminal.cwd.as_deref());
                let path = cwd.map_or_else(Vec::new, |cwd| Path::new(cwd).components().collect());
                (at, path)
            })
            .collect();
        let known: Vec<&[Component]> = leaves
            .iter()
            .map(|(_, path)| path.as_slice())
            .filter(|path| !path.is_empty())
            .collect();
        let shared = known
            .iter()
            .map(|path| common(known[0], path))
            .min()
            .unwrap_or(0);
        let root = known
            .first()
            .map_or_else(PathBuf::new, |path| path[..shared].iter().collect());
        let mut tree = Tree {
            nodes: vec![Node {
                parent: None,
                children: Vec::new(),
                item: Item::Folder(root),
            }],
        };
        tree.grow(0, shared, leaves);
        tree
    }

    /// Grow the leaves sharing the path of `parent` to `depth` under it:
    /// the folders where they part, by name, then the terminals in it.
    fn grow(&mut self, parent: usize, depth: usize, leaves: Vec<Leaf>) {
        let (here, deeper): (Vec<Leaf>, Vec<Leaf>) = leaves
            .into_iter()
            .partition(|(_, path)| path.len() <= depth);
        let mut groups: Vec<Vec<Leaf>> = Vec::new();
        for leaf in deeper {
            match groups
                .iter_mut()
                .find(|group| group[0].1[depth] == leaf.1[depth])
            {
                Some(group) => group.push(leaf),
                None => groups.push(vec![leaf]),
            }
        }
        groups.sort_by_cached_key(|group| {
            let name = group[0].1[depth].as_os_str().to_string_lossy().into_owned();
            (name.to_lowercase(), name)
        });
        for group in groups {
            let shared = group
                .iter()
                .map(|(_, path)| common(&group[0].1, path))
                .min()
                .unwrap_or(depth + 1);
            let path = group[0].1[..shared].iter().collect();
            let node = self.push(parent, Item::Folder(path));
            self.grow(node, shared, group);
        }
        for (at, _) in here {
            self.push(parent, Item::Terminal(at));
        }
    }

    fn push(&mut self, parent: usize, item: Item) -> usize {
        self.nodes.push(Node {
            parent: Some(parent),
            children: Vec::new(),
            item,
        });
        let node = self.nodes.len() - 1;
        self.nodes[parent].children.push(node);
        node
    }

    pub fn item(&self, node: usize) -> &Item {
        &self.nodes[node].item
    }

    pub fn parent(&self, node: usize) -> Option<usize> {
        self.nodes[node].parent
    }

    pub fn children(&self, node: usize) -> &[usize] {
        &self.nodes[node].children
    }

    /// The path of a folder, as navigation remembers it.
    fn folder(&self, node: usize) -> Option<String> {
        match self.item(node) {
            Item::Folder(path) => Some(path.to_string_lossy().into_owned()),
            Item::Terminal(_) => None,
        }
    }

    /// The node of the terminal shown at `at`.
    fn leaf(&self, at: Location) -> Option<usize> {
        self.nodes
            .iter()
            .position(|node| node.item == Item::Terminal(at))
    }

    /// The nodes from the top down to `node`, the root left out.
    fn chain(&self, mut node: usize) -> Vec<usize> {
        let mut chain = Vec::new();
        while let Some(parent) = self.parent(node) {
            chain.push(node);
            node = parent;
        }
        chain.reverse();
        chain
    }

    /// The pane of a terminal, or of the first terminal in a folder as listed.
    fn pane(&self, mut node: usize) -> Option<Location> {
        while let Some(child) = self.children(node).first() {
            node = *child;
        }
        match self.item(node) {
            Item::Terminal(at) => Some(*at),
            Item::Folder(_) => None,
        }
    }

    /// The terminals in `node`: their sessions and ids.
    pub fn terminals(&self, nui: &Nui, node: usize) -> Vec<(usize, u64)> {
        let mut terminals = Vec::new();
        let mut nodes = vec![node];
        while let Some(node) = nodes.pop() {
            match self.item(node) {
                Item::Terminal(at) => {
                    terminals.extend(nui.pane(*at).map(|pane| (at.session, pane.terminal)));
                }
                Item::Folder(_) => nodes.extend(self.children(node)),
            }
        }
        terminals
    }

    /// The name of a folder in the list of its parent: its path from the
    /// parent's, or from `~` when that leaves the home.
    pub fn name(&self, node: usize, home: Option<&Path>) -> String {
        let parent = self.parent(node).map(|parent| self.item(parent));
        let (Item::Folder(path), Some(Item::Folder(parent))) = (self.item(node), parent) else {
            return String::new();
        };
        match home.filter(|home| path.starts_with(home) && !parent.starts_with(home)) {
            Some(home) => tilde(path, Some(home)),
            None => path
                .strip_prefix(parent)
                .unwrap_or(path)
                .display()
                .to_string(),
        }
    }

    /// The whole path of a folder, as people read it.
    pub fn title(&self, node: usize, home: Option<&Path>) -> String {
        match self.item(node) {
            Item::Folder(path) => tilde(path, home),
            Item::Terminal(_) => String::new(),
        }
    }
}

/// The tree as navigation shows it: the nodes from its top down to the
/// terminal pointed at, one of them hovered.
pub struct View {
    pub tree: Tree,
    pub chain: Vec<usize>,
    /// Where the hovered node is in the chain.
    pub hovered: usize,
}

impl View {
    /// What navigation shows; none when its cursor shows no terminal.
    pub fn new(nui: &Nui, navigation: &Navigation) -> Option<View> {
        let tree = Tree::new(nui, navigation.cursor);
        let chain = tree.chain(tree.leaf(navigation.cursor)?);
        let last = chain.len() - 1;
        // the folder hovered, or what took its place as the paths changed:
        // the first node of the chain not above it
        let hovered = navigation.folder.as_deref().map_or(last, |folder| {
            let folder = Path::new(folder);
            chain
                .iter()
                .position(|node| match tree.item(*node) {
                    Item::Folder(path) => !folder.starts_with(path) || folder == path,
                    Item::Terminal(_) => true,
                })
                .unwrap_or(last)
        });
        Some(View {
            tree,
            chain,
            hovered,
        })
    }

    /// The hovered node.
    pub fn node(&self) -> usize {
        self.chain[self.hovered]
    }

    /// The folder the middle section lists: where the person is.
    pub fn center(&self) -> usize {
        self.tree.parent(self.node()).unwrap_or(0)
    }

    /// The node after the hovered one, on the way to the terminal.
    pub fn next(&self) -> Option<usize> {
        self.chain.get(self.hovered + 1).copied()
    }
}

/// Hover the item `delta` steps down in the middle section: a terminal, or
/// a folder, pointing to its first terminal.
pub fn step(nui: &mut Nui, delta: isize) {
    let Some(view) = view(nui) else {
        return;
    };
    let list = view.tree.children(view.center());
    let index = list
        .iter()
        .position(|node| *node == view.node())
        .unwrap_or(0);
    let node = list[(index as isize + delta).clamp(0, list.len() as isize - 1) as usize];
    if node == view.node() {
        return;
    }
    if let (Some(cursor), Some(navigation)) = (view.tree.pane(node), nui.navigation.as_mut()) {
        navigation.cursor = cursor;
        navigation.folder = view.tree.folder(node);
    }
}

/// Go into the hovered folder, on the way to the terminal pointed at.
pub fn inward(nui: &mut Nui) {
    if let Some(view) = view(nui)
        && let Some(next) = view.next()
    {
        hover(nui, view.tree.folder(next));
    }
}

/// Go back out to the folder the middle section lists, unless at the top.
pub fn outward(nui: &mut Nui) {
    if let Some(view) = view(nui)
        && view.hovered > 0
    {
        hover(nui, view.tree.folder(view.center()));
    }
}

/// Go into the hovered folder, or into the pane of the hovered terminal.
pub fn enter(nui: &mut Nui) {
    let Some(view) = view(nui) else {
        return;
    };
    match view.next() {
        Some(next) => hover(nui, view.tree.folder(next)),
        None => {
            if let Some(cursor) = nui.navigation.as_ref().map(|navigation| navigation.cursor) {
                nui.enter(cursor);
            }
        }
    }
}

/// A path as people read it: in the home, from `~`.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The home of the person, to read paths from.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
}

fn view(nui: &Nui) -> Option<View> {
    View::new(nui, nui.navigation.as_ref()?)
}

fn hover(nui: &mut Nui, folder: Option<String>) {
    if let Some(navigation) = nui.navigation.as_mut() {
        navigation.folder = folder;
    }
}

/// Every terminal a pane shows, in the order of their first panes, each at
/// the pane the cursor or else its session points to when that shows it.
fn shown(nui: &Nui, cursor: Location) -> Vec<Location> {
    let terminal = |at: Location| nui.pane(at).map(|pane| (at.session, pane.terminal));
    let mut seen = HashSet::new();
    let mut panes: Vec<Location> = nui
        .locations()
        .into_iter()
        .filter(|at| seen.insert(terminal(*at)))
        .collect();
    let pointed = (0..nui.sessions.len())
        .filter_map(|session| nui.active_location(session))
        .chain([cursor]);
    for at in pointed {
        if let Some(shown) = terminal(at)
            && let Some(pane) = panes
                .iter_mut()
                .find(|pane| terminal(**pane) == Some(shown))
        {
            *pane = at;
        }
    }
    panes
}

/// How many components two paths share from their start.
fn common(a: &[Component], b: &[Component]) -> usize {
    a.iter().zip(b).take_while(|(a, b)| a == b).count()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::state::{Level, StateRepresentation, tests::sample};

    /// The sample with its terminals in folders: sh in `nui/yalij`, top in
    /// `nui/web/src` and vim in `dotfiles`, all in `/home/me`; and a session
    /// `other` with a shell in `nui/yalij` too.
    pub(crate) fn foldered() -> Nui {
        let mut nui = sample();
        let cwds = [
            "/home/me/nui/yalij",
            "/home/me/nui/web/src",
            "/home/me/dotfiles",
        ];
        for (terminal, cwd) in nui.sessions[0].terminals.iter_mut().zip(cwds) {
            terminal.cwd = Some(cwd.into());
        }
        nui.add_session("other", "sh", Some(cwds[0].into()));
        nui
    }

    /// Navigation in the fs representation, the folder `folder` hovered.
    fn navigate(nui: &mut Nui, cursor: Location, folder: Option<&str>) {
        nui.representation = StateRepresentation::Fs;
        nui.navigation = Some(Navigation {
            focus: Level::Pane,
            cursor,
            folder: folder.map(Into::into),
            operation: None,
            search: None,
        });
    }

    fn at(session: usize, workspace: usize, window: usize, pane: usize) -> Location {
        Location {
            session,
            workspace,
            window,
            pane,
        }
    }

    /// The names a node lists: folders, and terminals as `session:id`.
    fn names(nui: &Nui, tree: &Tree, node: usize, home: Option<&Path>) -> Vec<String> {
        let name = |child: &usize| match tree.item(*child) {
            Item::Folder(_) => tree.name(*child, home),
            Item::Terminal(at) => format!("{}:{}", at.session, nui.pane(*at).unwrap().terminal),
        };
        tree.children(node).iter().map(name).collect()
    }

    /// The hovered node, and the folder listing it.
    fn hovered(nui: &Nui) -> (Item, Item) {
        let view = view(nui).unwrap();
        (
            view.tree.item(view.node()).clone(),
            view.tree.item(view.center()).clone(),
        )
    }

    fn folder(path: &str) -> Item {
        Item::Folder(path.into())
    }

    #[test]
    fn folders_are_where_paths_part_or_terminals_run() {
        let mut nui = foldered();
        let tree = Tree::new(&nui, Location::default());
        assert_eq!(tree.title(0, None), "/home/me");
        assert_eq!(names(&nui, &tree, 0, None), ["dotfiles", "nui"]);
        let shared = tree.children(0)[1];
        // nothing parts at `web`: one item to `src`
        assert_eq!(names(&nui, &tree, shared, None), ["web/src", "yalij"]);
        let yalij = tree.children(shared)[1];
        assert_eq!(names(&nui, &tree, yalij, None), ["0:1", "1:4"]);
        let home = Some(Path::new("/home/me"));
        assert_eq!(tree.title(yalij, home), "~/nui/yalij");
        // a terminal elsewhere: the root is what all share, the home from ~
        nui.sessions[1].terminals[0].cwd = Some("/tmp".into());
        let tree = Tree::new(&nui, Location::default());
        assert_eq!(tree.title(0, home), "/");
        assert_eq!(names(&nui, &tree, 0, home), ["~", "tmp"]);
        // folders come first; a terminal of an unknown folder is at the top
        nui.sessions[1].terminals[0].cwd = None;
        let tree = Tree::new(&nui, Location::default());
        assert_eq!(names(&nui, &tree, 0, None), ["dotfiles", "nui", "1:4"]);
    }

    #[test]
    fn a_terminal_shown_again_is_at_the_pane_pointed_to() {
        let nui = foldered();
        // where the first terminal is
        let pane = |cursor| {
            let tree = Tree::new(&nui, cursor);
            tree.nodes.iter().find_map(|node| match node.item {
                Item::Terminal(at) if at.session == 0 && nui.pane(at)?.terminal == 1 => Some(at),
                _ => None,
            })
        };
        // the second pane of the first window shows the first terminal again
        assert_eq!(pane(at(0, 0, 0, 1)), Some(at(0, 0, 0, 1)));
        // pointed elsewhere, it is at the active pane of its session
        assert_eq!(pane(at(0, 1, 0, 0)), Some(at(0, 0, 0, 0)));
    }

    #[test]
    fn moving_points_to_terminals() {
        let mut nui = foldered();
        navigate(&mut nui, at(0, 0, 0, 0), None);
        assert_eq!(hovered(&nui).1, folder("/home/me/nui/yalij"));
        outward(&mut nui);
        assert_eq!(
            hovered(&nui),
            (folder("/home/me/nui/yalij"), folder("/home/me/nui"))
        );
        // the last item stays; the one before is a folder, its terminal pointed at
        step(&mut nui, 1);
        assert_eq!(hovered(&nui).0, folder("/home/me/nui/yalij"));
        step(&mut nui, -1);
        let navigation = nui.navigation.clone().unwrap();
        assert_eq!(navigation.cursor, at(0, 0, 1, 0));
        assert_eq!(hovered(&nui).0, folder("/home/me/nui/web/src"));
        inward(&mut nui);
        assert_eq!(hovered(&nui).0, Item::Terminal(at(0, 0, 1, 0)));
        // up to the top, and no further
        outward(&mut nui);
        outward(&mut nui);
        outward(&mut nui);
        assert_eq!(hovered(&nui), (folder("/home/me/nui"), folder("/home/me")));
        step(&mut nui, -1);
        assert_eq!(nui.navigation.as_ref().unwrap().cursor, at(0, 1, 0, 0));
        // enter goes into the folder, then into the pane
        enter(&mut nui);
        assert!(nui.navigation.is_some());
        enter(&mut nui);
        assert!(nui.navigation.is_none());
        assert_eq!(nui.location(), Some(at(0, 1, 0, 0)));
    }

    #[test]
    fn the_place_stays_as_paths_change() {
        let mut nui = foldered();
        navigate(&mut nui, at(0, 0, 0, 0), Some("/home/me/nui"));
        // top joins the shells: `nui` parts no more, and merges into `nui/yalij`
        nui.sessions[0].terminals[1].cwd = Some("/home/me/nui/yalij".into());
        assert_eq!(
            hovered(&nui),
            (folder("/home/me/nui/yalij"), folder("/home/me"))
        );
        // the shell goes elsewhere: its way, at the same depth
        nui.sessions[0].terminals[0].cwd = Some("/home/me/dotfiles".into());
        assert_eq!(
            hovered(&nui),
            (folder("/home/me/dotfiles"), folder("/home/me"))
        );
    }
}
