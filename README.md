# v0.1.0-nui-yalij

The first variant of nui: **ya**zi + ze**lij**. A terminal multiplexer derived
from authoritative KDL state. Processes, layouts and navigation follow state;
runtime observations enter it as facts. Configuration lives separately, as
**decoration**.

Requires Unix (macOS/Linux) and Rust 1.89+.

## Run

No installation or PATH registration needed:

```nu
cargo run --release -- attach main
# In another terminal: the same processes and shared active tabs/panes.
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
The navigation cursor/prompt is per instance, but active windows, tabs, panes,
layout names and terminal status are shared.

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
```

`--state <global.kdl>` selects a catalog; different catalogs are independent.
`--config <dir>` selects its global/profile decoration directory. Every client of
a catalog must use the same config directory; `--profile <name>` is per instance.

Inside a pane, `nui-yalij attach other` switches the connected instance that last
typed into that terminal, not every viewer. The spawned shell inherits
`NUI_YALIJ_STATE`, `NUI_YALIJ_SESSION` and `NUI_YALIJ_TERMINAL` for this routing.

**REPL:** complete a command with j/k or arrows, enter to choose, esc to go back
(or cancel at the root). Choose an existing session or name a new one. The
completed command is shown before `Apply? (N/y)`. Only lowercase `y` executes.
Even a supplied `--apply` does not bypass this confirmation. Cancellation never
starts a daemon or writes state.

**Installation is deferred:** `install`, `update` and `remove` only explain that
they will be implemented when the application reaches a more stable release.
They make no changes, including with `--apply`. No installer/download script or
partial uninstall is provided yet.

## Primitives and keys

`Session -> Window -> Tab -> Pane`, each container holding at least one child.

- A **session** owns terminals, independently persisted.
- **Windows** and **tabs** are groupings, each a marker of one visualization.
- A **pane** shows a terminal by id; many panes may show the same terminal.
- A container's layout strategy places its children. Built-in `tabs` shows the
  active child; plugins provide other strategies.

Default keys (overridable by decoration):

| Where      | Key         | Action |
|------------|-------------|--------|
| view       | ctrl-g      | Open/close navigation |
| view       | enter       | Restart the focused stopped/exited terminal |
| navigation | h/l, ←/→    | Focus the section left/right |
| navigation | j/k, ↓/↑    | Hover next/previous; sections to the right follow |
| navigation | a           | Add after hovered item, naming each level |
| navigation | r/x/s       | Rename/remove/stop hovered item |
| navigation | enter       | Approve operation; enter pane or next section |
| navigation | esc         | Cancel operation or return to original attachment |
| navigation | q           | Disconnect **this instance only** |
| navigation | Q           | Stop **all session servers and clients in this catalog** |

Adding creates one of every child level down to a new shell, in the hovered
terminal's directory. Enter takes a typed name and continues naming the next
level; esc keeps the remaining automatic names (`window-2`, `tab-1`, etc.).

Removing cascades through empty containers and drops terminals no pane shows
anymore. Stopping keeps structure; enter restarts the terminal.

## State: global wrapper and independent session files

Default global file:

`$XDG_STATE_HOME/nui-yalij/global-state.kdl` (fallback `~/.local/state/...`).

It is only a catalog of stable session identities and live client presence:

```kdl
catalog version=1 {
    session 1
    session 4
    client 7 session=1 profile=work {
        navigation focus=pane session=4 window=1 tab=1 pane=2
        message "the last notice"
    }
    client 8 session=4
}
```

A client node exists only while connected. Edit its `session` to switch that
instance, or delete its node to disconnect it. Navigation's session is also a
**stable id**, not the session's position in the catalog. The other locations
count from 1. Disconnected clients are discarded on server restart.

For a global file `/path/global-state.kdl`, session 4 lives in:

`/path/global-state.kdl.sessions/4/state.kdl`

```kdl
session main id=4 active=1 {
    terminal 1 command="/bin/zsh" cwd="/home/me" pid=4242 program=vim title="~"
    terminal 2 command=htop status=stopped {
        args "-d" "10"
    }
    window dev {
        tab layout=columns {
            pane terminal=1
            pane monitor terminal=2
        }
    }
}
```

The session id stays fixed across renames and catalog reordering. Terminal ids
are local to a session. Defaults are `layout=tabs`, `active=1`, `status=running`;
other statuses are `stopped` and `exited`. Unnamed windows/tabs/panes derive their
labels from what they show.

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
    panel ctrl-g
    left h left
    right l right
    next j down
    previous k up
    add a
    rename r
    remove x
    stop s
    enter enter
    back esc
    detach q
    quit Q
    restart enter
}
theme accent=blue active=green muted=darkgray notice=yellow error=red
plugin "~/path/to/plugins/example.lua"
```

Later layers replace individual key actions and theme fields; a key action with
no arguments disables it. Plugin lists accumulate, later entries take precedence,
and duplicate paths load once. Paths are relative to **their decoration file**,
or start with `~/`. Unknown nodes, actions, properties, keys and colors are errors.

A profile decorates its instance; it cannot impose active tabs, layout names or
other session state. Attaching/reloading a profile never runs its automatic
`update` hooks. Its layouts and explicitly invoked key handlers can differ, but
a user's explicit state-changing action is still shared. A session override
wins over every viewer's profile, including while previewing that session.

A broken static reload retains the previous snapshot. A broken live session
decoration retains the previous valid layer until fixed. `reload` also re-reads
plugin source files even if their paths have not changed; editing a Lua source
alone does not hot-reload it.

## Plugins

Lua 5.4 scripts return a table. Scripts are trusted code, not sandboxed.

```lua
return {
  layouts = {
    columns = function(container)
      -- kind, area={x,y,w,h}, active (1-based), children={{name}}, theme
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
one-argument handlers still work. Session tables now have a stable `id`.
Layouts/keys from later plugins override earlier plugins and built-ins; automatic
hooks run in global/session order. `print` is reported as a notice.

Helpers on global `nui`: `shell`, `next_id(state)`, `remove(state, level, location)`,
`stop(…)`, `rename(…, name)`, `enter(state, location)`. Locations count from 1.

[`plugins/example.lua`](plugins/example.lua) supplies `columns`, `rows`, `bar`,
`n` (new item), `c` (next layout), and closes panes whose shell exited successfully.
Put it in global/session decoration to enable that automatic cleanup, not just
a profile.

## Checks and limits

```nu
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Tests include real multi-client PTYs, same/different sessions, foreign-session
navigation edits, concurrent startup, live files, reconnect/restart, per-instance
profiles, decoration precedence/reload, inside attach, resize ownership, REPL
confirmation, and forced shutdown.

PTY content stays in memory: disconnect/reconnect preserves it, server restart
does not. State persists and starts the terminals that should run again. No
network transport, scrollback or mouse input yet. For background startup errors,
see `<global.kdl>.server.log`.
