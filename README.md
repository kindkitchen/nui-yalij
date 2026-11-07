# nui-yalij

The first variant of nui: **ya**zi + ze**lij**. A terminal multiplexer derived
from authoritative KDL state. Processes, layouts and navigation follow state;
runtime observations enter it as facts. Configuration lives separately, as
**decoration**.

Requires Unix (macOS/Linux). Building from source requires Rust 1.95+ and a C
compiler for vendored Lua. Licensed under [MIT](LICENSE).

## Install, update, uninstall

Install from crates.io, or use Binstall for the matching GitHub Release binary:

```nu
cargo install nui-yalij --locked
# Alternative, with cargo-binstall installed:
cargo binstall nui-yalij
nui-yalij --version
```

Both use Cargo's bin directory, normally `~/.cargo/bin`; it must be on `PATH`.
For an unreleased checkout, use `cargo install --path . --locked` instead.

Lifecycle commands **observe by default**; `--apply` performs the operation:

```nu
nui-yalij update                  # Show the plan
nui-yalij update --apply          # Install latest, then stop old servers
nui-yalij update 0.3.0 --apply     # Or install an exact version
nui-yalij uninstall               # List everything that will be wiped
nui-yalij uninstall --apply       # Stop servers, remove the binary and wipe data
```

Run these **outside nui-yalij panes**. Update uses `cargo install --locked --force`;
it verifies the replacement before stopping old servers. Failed installation
leaves the old servers running. Configuration and persisted session structure
stay; running programs stop, and the next attachment starts the new server.
Version 0.3.0 changes the client/server protocol: stop an old coordinator before
attaching with the new binary; `update --apply` already handles this.

**Uninstall is destructive, with no retained settings or backup.** It stops all
servers in the current user's runtime locations, unregisters the managed Cargo
executable, and removes the app's state, sessions, configuration, logs, cache,
legacy files and sockets from the current XDG and default home locations.
Custom state/configuration must be supplied explicitly:

```nu
nui-yalij --state /path/global.kdl --config /path/app-config uninstall
nui-yalij --state /path/global.kdl --config /path/app-config uninstall --apply
```

Only that custom catalog's files/sidecars and the dedicated custom config
directory are wiped, not their parent folders. Other custom locations are not
guessed; external plugins, source checkouts, and shared Rust/Cargo caches remain.
Directory symlinks are removed rather than followed. Protected home/source
folders are refused. `remove` is an alias for `uninstall`.

`install [<version>] --apply` also works from an uninstalled development binary;
it refuses to overwrite an existing managed installation (use `update`). The
managed install root is `CARGO_INSTALL_ROOT`, then `CARGO_HOME`, then `~/.cargo`.
If you installed with a custom `cargo --root`, set `CARGO_INSTALL_ROOT` to that
same directory before lifecycle operations.

## Run

Without installing, run directly from the checkout:

```nu
cargo run --release -- attach main
# In another terminal: the same processes and shared active windows/panes.
./target/release/nui-yalij attach main
# A different session, in the same catalog.
./target/release/nui-yalij attach other
# Same session, another instance's decoration.
./target/release/nui-yalij --profile work attach main
```

The first attachment starts a background coordinator. It hosts **one serialized
session server/PTY owner per session**, reused by every connection to that session.
These are independent owners in **one process**, not one OS process per session.
There is no second local copy of session state in the terminal client.

The coordinator routes navigation operations to the target session's owner.
Previewing/renaming/stopping a pane in another session changes **that session**,
even if you never enter it. Switching attachment changes routing/presence only:
it neither transfers nor restarts processes. Everyone viewing the changed session
reacts to the same update.

Closing an instance disconnects it; its sessions keep running. A terminal shared
by differently sized instances takes the size of the instance that typed last.
The navigation cursor/prompt is per instance, but active workspaces, windows,
panes, layout names and terminal status are shared.

## Commands

```nu
./target/release/nui-yalij list
./target/release/nui-yalij reload
./target/release/nui-yalij kill          # observe only
./target/release/nui-yalij kill --apply # stop all owners/clients in this catalog
./target/release/nui-yalij server       # foreground coordinator, for debugging
./target/release/nui-yalij --repl
./target/release/nui-yalij attach --repl
./target/release/nui-yalij kill --repl
# Inside a pane only: what a command of decoration runs (t, by default).
nui-yalij call t
```

`--state <global.kdl>` selects a catalog; different catalogs are independent.
`--config <dir>` selects its global/profile decoration directory. Every client of
a catalog must use the same config directory; `--profile <name>` is per instance.
Shells inside panes inherit `NUI_YALIJ_CONFIG`, so config commands use the right
root there too. A shared pane has no single profile; supply `--profile` explicitly.

Configuration and agent help, without starting a server:

```nu
nui-yalij config path             # Selected file (also the default config action)
nui-yalij config show             # File contents, or a default template if missing
nui-yalij config init             # Preview creating a starter
nui-yalij config init --apply     # Create it; never overwrite an existing file
nui-yalij config check            # Validate global and all profile files
nui-yalij --profile work config path  # Select profiles/work.kdl instead
nui-yalij skill                   # Print the bundled agent skill as Markdown
```

Edit the file from `config path`, validate it, then run `nui-yalij reload` to
apply global/profile changes to a running server. `config check` checks KDL and
the selected profile, not plugin execution or session overrides (checked live).

[`skills/nui-yalij/SKILL.md`](skills/nui-yalij/SKILL.md) explains the application,
config schema, layers, navigation and safe agent workflows. `nui-yalij skill`
prints that exact file, so it can be saved into your agent's skill directory.
It never installs a skill or modifies agent settings automatically.

Inside a pane, `nui-yalij attach other` switches the connected instance that last
typed into that terminal, not every viewer. The spawned shell inherits
`NUI_YALIJ_STATE`, `NUI_YALIJ_SESSION` and `NUI_YALIJ_TERMINAL` for this routing.

Inside a pane, `t` opens/closes navigation for the instance that last typed
there, as `ctrl-t` does. It is a command of decoration, not an installed
program: each session writes its commands as scripts into
`<global.kdl>.sessions/<id>/bin`, first on the `PATH` of its terminals, so they
exist nowhere else. A shell alias or function of the same name shadows them.

**REPL:** complete a command with j/k or arrows, enter to choose, esc to go back
(or cancel at the root). Choose an existing session or name a new one. The
completed command is shown before `Apply? (N/y)`. Only lowercase `y` executes.
Even a supplied `--apply` does not bypass this confirmation. Cancellation never
starts a daemon or writes state.

## Primitives and keys

`Session -> Workspace -> Window -> Pane`, each container holding at least one child.

- A **session** owns terminals, independently persisted.
- **Workspaces** and **windows** are groupings, each a marker of one visualization.
- A **pane** shows a terminal by id; many panes may show the same terminal.
- A container's layout strategy places its children. Built-in `tabs` shows the
  active child; plugins provide other strategies.

Default keys (overridable by decoration):

| Where      | Key         | Action |
|------------|-------------|--------|
| view       | ctrl-t      | Open/close navigation (`t` in a pane does too) |
| view       | esc twice / held | Open navigation (300 ms double press / 600 ms hold) |
| view       | enter       | Restart the focused stopped/exited terminal |
| navigation | [ / ]       | Show the previous/next representation |
| navigation | h/l, ←/→    | Focus the section left/right |
| navigation | j/k, ↓/↑    | Hover next/previous; sections to the right follow |
| navigation | /           | Search all panes; type to filter, enter to browse results |
| navigation | a           | Add after hovered item, naming each level |
| navigation | r/x/s       | Rename/remove/stop hovered item |
| navigation | enter       | Approve operation; enter pane or next section |
| navigation | esc         | Cancel operation or return to original attachment |
| navigation | q           | Disconnect **this instance only** |
| navigation | Q           | Stop **all session servers and clients in this catalog** |

Adding creates one of every child level down to a new shell, in the hovered
terminal's directory. Enter takes a typed name and continues naming the next
level; esc keeps the remaining automatic names (`workspace-2`, `window-1`, etc.).

Removing cascades through empty containers and drops terminals no pane shows
anymore. Stopping keeps structure; enter restarts the terminal.

When the focused terminal's process exits, navigation opens for its viewers:
`exit`, EOF (`ctrl-d`), nonzero exits and signals all work the same way. Exiting
a nested shell or finishing a command inside a live shell does not switch views.
Existing navigation and viewers of other terminals are left alone. The terminal's
screen stays unobstructed; stopped/exited status is shown in navigation, where
enter restarts the hovered pane's terminal.

## Navigation: representations of the state

Navigation shows a **representation** of the state: a mapping of it into
sections, as in yazi. The tabs on its top border name them; `[` and `]`
switch, around, and the panel opens again on the one it showed last, at the
focused pane. Every representation points to the same terminal, the one its
last section previews: what one hovers or enters, the next one shown points
to as well. The bottom half stays the terminal you opened the panel from.

- **native**: the state as it is, a section per level as above, then the
  screen of the hovered pane's terminal. Sessions stay folded to the hovered
  one but while focused; the preview folds then, so neither widens the rest.
- **fs**: the terminals panes show, by the folders they run in, only to move
  in: no add, rename, remove or stop. A folder is an item where their paths
  part or where some of them run; the folders between merge into it
  (`web/src/`), and the root is the folder all share. Folders come first, by
  name, then terminals in the order of their panes; one of an unknown folder
  is at the top. Three sections, as in yazi: the list you came from, the
  folder you are in, and where its hovered item leads, the items of a folder
  or the screen of a terminal. h/l go out/in, j/k hover (a folder points
  to its first terminal), enter goes into a folder or the pane of a terminal:
  the active pane of its session when that shows it, else the first. The
  folders follow shells as they `cd`; the place stays by path, not position.
  An icon tells a folder from a terminal: see `icons` in decoration.

### Pane search

`/` in either representation opens a search prompt with a pane list below and
a preview alongside. Every key updates the fuzzy-ranked matches across **all
sessions**, using session/workspace/window/pane names, cwd paths, terminal
programs/titles, commands and arguments. Rename/create prompts keep `/` as text.
No external `fzf` executable is needed.

Matching is Unicode-aware and fzf-style: fuzzy subsequences, smart case
(uppercase makes a term case-sensitive), space-separated AND terms (which may
match different fields), `|` alternatives, `'text` exact substring, `^text`
prefix, `text$` suffix and `!text` exclusion. For example, `work src$ !logs`
finds panes under a matching workspace/session and path, excluding log panes.

- **Typing:** Backspace deletes, Ctrl-U clears, paste updates the query;
  Escape cancels back to normal navigation.
- **Enter freezes the list** and focuses its results, without entering a pane.
  j/k or arrows browse; another Enter opens the selected pane.
- `/` re-edits the query; Escape leaves the results for normal navigation.

The original terminal remains below the panel. Search is per instance; merely
hovering does not switch anybody's active pane. New matches do not join a frozen
list. Removed targets disappear safely; reordered sessions/placements resolve
by session/terminal identity, never to unrelated terminals at old indices.

## State: global wrapper and independent session files

Default global file:

`$XDG_STATE_HOME/nui-yalij/global-state.kdl` (fallback `~/.local/state/...`).

It is only a catalog of stable session identities and live client presence:

```kdl
catalog version=1 {
    session 1
    session 4
    client 7 session=1 profile=work {
        navigation focus=pane session=4 workspace=1 window=1 pane=2
        message "the last notice"
    }
    client 8 session=4
}
```

A client node exists only while connected. Edit its `session` to switch that
instance, or delete its node to disconnect it. Navigation's session is also a
**stable id**, not the session's position in the catalog. The other locations
count from 1. An instance whose panel shows fs, open or not, has
`representation=fs`; its navigation has `folder="/path"` while a folder is
hovered rather than the terminal. Disconnected clients are discarded on
server restart.

For a global file `/path/global-state.kdl`, session 4 lives in:

`/path/global-state.kdl.sessions/4/state.kdl`

```kdl
session main id=4 active=1 {
    terminal 1 command="/bin/zsh" cwd="/home/me" pid=4242 program=vim title="~"
    terminal 2 command=htop status=stopped {
        args "-d" "10"
    }
    workspace dev {
        window layout=columns {
            pane terminal=1
            pane monitor terminal=2
        }
    }
}
```

The session id stays fixed across renames and catalog reordering. Terminal ids
are local to a session. Defaults are `layout=tabs`, `active=1`, `status=running`;
other statuses are `stopped` and `exited`. Unnamed workspaces/windows/panes derive
their labels from what they show. The earlier shape, sessions of `window`s of
`tab`s, still reads as the same levels and is written in the current one.

Each file has one locked writer. Client input, runtime facts, plugins and file
edits are serialized; clients send **events, never stale replacement states**.
Files are atomically replaced, and external edits are consumed before the next
interactive mutation. Invalid edits are reported and left untouched until fixed.
Valid edits affect every viewer of that session. The global file is live too.

Removing a session from the catalog stops its owner but keeps its files for
recovery. A matching session entry can reference those files again. A navigation
removal likewise forgets the session; its old files are not silently purged.
Stable ids are not automatically reused.

### Old state files

On first start, an existing default `state.kdl` is imported into the new catalog
and session files, leaving the old file untouched. If `--state` points directly
at an old monolithic file, it is backed up as `<file>.legacy.kdl` before migration.
Stop the old application first: its lock is respected.

Sessions, layout/active selection and terminal definitions survive. Legacy
top-level `toggle`/`plugin` settings are no longer state: move them into decoration.
Old singleton `attached`/`navigation`/`message` presence is not restored.

## Decoration: global → profile → session

All three levels have the same KDL schema:

| Layer | File | When read |
|-------|------|-----------|
| global | `$XDG_CONFIG_HOME/nui-yalij/decoration.kdl` (`~/.config/...`) | Server start / explicit `reload` |
| profile | `<config>/profiles/<name>.kdl` | Server start / explicit `reload` |
| session | `<global.kdl>.sessions/<id>/decoration.kdl` | Live, including deletion |

```kdl
keys {
    panel ctrl-t
    left h left
    right l right
    next j down
    previous k up
    previous-tab "["
    next-tab "]"
    add a
    rename r
    remove x
    stop s
    search "/"
    enter enter
    back esc
    detach q
    quit Q
    restart enter
}
commands {
    panel t
}
gestures double-escape-ms=300 hold-escape-ms=600
theme accent=blue active=green muted=darkgray notice=yellow error=red
icons nerd
plugin "~/path/to/plugins/example.lua"
```

`icons` tell a folder from a terminal: `nerd`, the symbols of a Nerd Font, by
default, or `plain` (`▸`, `$`) where the font has none. No terminal tells
which, so a profile, decorating the instance in that terminal, can say it.

Later layers replace individual key and command actions, gesture properties,
theme fields and icons; an action with no arguments disables it. Keys may hold `ctrl-`,
`alt-`, `super-` and `shift-`. Command names are plain file names. Every
profile's commands are written into a session, but a call only works for the
calling instance's own; `reload` and live session edits rewrite them. Plugin
lists accumulate, later entries take precedence, and duplicate paths load
once. Paths are relative to **their decoration file**, or start with `~/`.
Unknown nodes, actions, properties, keys and colors are errors.

Escape gestures apply only in the terminal view and **open**, never toggle,
navigation. `double-escape-ms` and `hold-escape-ms` take integers from 0 to
60000 milliseconds; **0 disables that gesture independently**. For example:

```kdl
// Double Escape off; hold for 800 ms instead. ctrl-t still works.
gestures double-escape-ms=0 hold-escape-ms=800
```

The first Escape is sent to the program immediately, keeping ordinary Escape
responsive. Precise holds require the outer terminal's kitty keyboard repeat/
release reports, which the client now requests. Other terminals use a recent
auto-repeat burst: no repeat means no detectable hold, and rapid presses cannot
always be distinguished from repeats. `ctrl-t` remains the portable fallback.
Gestures never run in navigation/search or rename/create prompts.

A profile decorates its instance; it cannot impose active windows, layout names or
other session state. Attaching/reloading a profile never runs its automatic
`update` hooks. Its layouts and explicitly invoked key handlers can differ, but
a user's explicit state-changing action is still shared. A session override
wins over every viewer's profile, including while previewing that session.

A broken static reload retains the previous snapshot. A broken live session
decoration retains the previous valid layer until fixed. `reload` also re-reads
plugin source files even if their paths have not changed; editing a Lua source
alone does not hot-reload it.

## Terminals inside

The terminal of a pane emulates what programs ask of a modern one, and the
client asks the terminal around it on attach, giving everything back on exit:

- **Keys told apart:** kitty's keyboard protocol (its disambiguate flag; a
  program pushing more flags gets that one and can ask which), or xterm's
  `modifyOtherKeys`, per program and per screen, so shift+enter reaches Pi as
  shift+enter. The client reads keys in any encoding of the terminal around; a
  program asking for neither gets enter.
- **Layouts:** ctrl, alt or super with a key counts as the key of the base
  layout, when the terminal around tells it: `ctrl-t` opens navigation on a
  Ukrainian layout too.
- **Focus** (DEC 1004), **colors** (OSC 10/11/4) and the **color scheme**
  (`CSI ? 996 n`, DEC 2031): from the terminal around the instance that typed
  there last; a change of scheme reaches the programs asking.
- **Synchronized output** (DEC 2026): an update shows whole, or once it took
  over 200 ms.
- **Passed on** to the terminal around: copies to the clipboard (OSC 52, to the
  instance that typed there last), notifications (OSC 9, 777), progress
  (OSC 9;4), bells, and the title of the focused pane. Reading the clipboard is
  not passed on.
- **In navigation:** progress shows by its pane (and the containers above it)
  as a percentage or `working`, a bell nobody saw as `bell` until the pane is
  focused.

A full reset (`reset`, RIS) forgets what a program asked, as after a crash.

## Plugins

Lua 5.4 scripts return a table. Scripts are trusted code, not sandboxed.

```lua
return {
  layouts = {
    columns = function(container)
      -- kind (session, workspace, window), area={x,y,w,h},
      -- active (1-based), children={{name}}, theme
      return {
        areas = { [1] = { x=0, y=0, w=40, h=24 } },
        draw = { { x=0, y=0, text="…", style={fg=container.theme.accent} } },
      }
    end,
  },
  keys = {
    navigate = {
      n = function(state, client)
        -- Explicit action: state is a projection of all sessions plus this
        -- client's attached (1-based index), navigation and message.
        -- client = { id, session (stable id), profile? }
      end,
    },
  },
  update = function(state, session_id)
    -- Automatic hooks from global/session decoration only.
    -- Scoped to exactly this session at state.sessions[1]; no viewer profile.
    -- May remove that session, but cannot create/change other sessions.
    return state
  end,
}
```

Handlers return a new state or nothing to keep their in-place edits. Existing
one-argument handlers still work. `navigate` keys run while the native
representation shows; `representation` tells which (`native`, `fs`), and
`navigation.folder` the folder fs hovers. Session tables have a
stable `id`, and `workspaces` of `windows` of `panes`; levels and locations
name `session`, `workspace`, `window` and `pane`. Plugins of the earlier
shape, with `windows` of `tabs`, need the new names.
Layouts/keys from later plugins override earlier plugins and built-ins; automatic
hooks run in global/session order. `print` is reported as a notice.

Helpers on global `nui`: `shell`, `next_id(state)`, `remove(state, level, location)`,
`stop(…)`, `rename(…, name)`, `enter(state, location)`. Locations count from 1.

[`plugins/example.lua`](plugins/example.lua) supplies `columns`, `rows`, `bar`,
`n` (new item), `c` (next layout), and closes panes whose shell exited successfully.
Put it in global/session decoration to enable that automatic cleanup, not just
a profile.

## Versioning and releases

The project-local `justfile` requires `just` and Nushell. Version commands require
a clean `nui-yalij/main` whose HEAD matches `origin`:

```nu
just check                            # Tests, lint, helper tests, package dry-run
just version patch                    # Preview only; no Git or file writes
just version minor --apply            # Bump, validate, commit, annotate a local tag
just version major --apply --release  # Also push and publish the GitHub Release
```

Patch increments the last component; minor resets patch; major resets both.
Only this package's versions in `Cargo.toml` and `Cargo.lock` change. A failed
validation restores those files. Dependency versions stay locked. The resulting
tag is `nui-yalij-<version>`. `--apply` alone never pushes or publishes.
`--release` requires this project's GitHub origin and uses `gh`'s active account
for the push and release, without changing global Git credentials.

To publish an already prepared local version:

```nu
let tag = $"nui-yalij-((open Cargo.toml).package.version)"
git -c credential.helper= -c 'credential.helper=!gh auth git-credential' push --atomic origin HEAD:refs/heads/nui-yalij/main $"refs/tags/($tag)"
gh release create $tag --repo kindkitchen/nui-yalij --verify-tag --title $tag --generate-notes --latest
```

**Publishing a GitHub Release is the only release trigger**, not pushing a tag.
The pipeline checks tests/lint/package contents and the Rust 1.95 minimum, then
builds native Apple Silicon macOS, Intel macOS, and x86-64 Linux archives. It
smoke-tests their version/help, attaches all archives plus `SHA256SUMS`, and only
then publishes the matching source crate to crates.io. Version, tag, archive
names and Binstall metadata must agree. Linux builds use Ubuntu 22.04/glibc 2.35;
macOS binaries target macOS 13+. Windows and Linux ARM binaries are not provided.

Set the repository's `CARGO_REGISTRY_TOKEN` Actions secret to a crates.io token
with publish rights for this crate; never commit credentials or `.env`. The
workflow has no automatic crates.io fallback outside the release job. After the
first publish, crates.io Trusted Publishing is an alternative to a stored token.
Published crate versions cannot be overwritten. If a job fails, rerun **failed
jobs**, not a successful release's entire workflow; existing assets are not
silently replaced. Do not update your installation until the release completes.

To check that Binstall actually uses your binaries rather than a third-party
host or compilation fallback:

```nu
cargo binstall nui-yalij --strategies crate-meta-data
```

## Checks and limits

```nu
just check
# Application checks alone:
cargo test --locked --all-targets -- --test-threads=1
cargo clippy --locked --all-targets -- -D warnings
```

Tests include real multi-client PTYs, same/different sessions, foreign-session
navigation edits, Escape gestures, live/frozen fuzzy search, config/skill CLI
commands, concurrent startup, live files, reconnect/restart, per-instance
profiles, decoration precedence/reload, inside attach and commands, resize
ownership, REPL confirmation, forced shutdown, the fs representation following
shells into folders, and the conversations of programs with the terminal
around (keys, focus, colors, clipboard, titles, progress).

PTY content stays in memory: disconnect/reconnect preserves it, server restart
does not. State persists and starts the terminals that should run again. No
network transport, scrollback or mouse input yet. For background startup errors,
see `<global.kdl>.server.log`.
