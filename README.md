# v0.1.0-nui-yalij

The first variant of nui: **ya**zi + ze**lij**. A terminal multiplexer whose whole
application is one serializable state, `Nui`, kept as KDL. Everything derives from it,
never the other way:

- the **view**: the terminals of the attached session, reachable by the person;
- the **navigation panel**: yazi-style sections of the state, over a live preview;
- the **processes**: the runtime starts and stops them to match the state.

An event (a key, a process exiting, an edit of the state file, a plugin) makes a new
state, and the rest follows it. What the runtime learns (pid, exit code, foreground
program, cwd, title) reaches the state as a fact, through the same flow.

```sh
cargo run --release -- [--state <file>] [attach [<session>] | list | help]
```

## Primitives

`Session -> Window -> Tab -> Pane`, each container holding at least one child.

- **Session**: the entry point, persisted between runs. It owns its terminals.
- **Window**, **Tab**: groupings, each a marker of one visualization.
- **Pane**: shows a terminal by id; many panes may show the same one, or none (detached).

The path from a session to a pane is a *location* of a terminal. A container places its
children with its *layout strategy*: `tabs` by default and built in, one child at a time.
Any other strategy comes from a plugin.

## Keys

| where      | key            | does                                                              |
|------------|----------------|-------------------------------------------------------------------|
| view       | `ctrl-g`       | toggle the panel (the state's `toggle`)                           |
| view       | `enter`        | start the focused terminal, when stopped or exited                |
| navigation | `h` `l`        | focus the section left, right                                     |
| navigation | `j` `k`        | hover the next, previous item; sections to the right follow       |
| navigation | `a`            | add an item after the hovered one, then name it and those inside  |
| navigation | `r` `x` `s`    | rename, remove, stop the hovered item                             |
| navigation | `enter`        | approve the operation; enter the pane, or go to the next section  |
| navigation | `esc`          | cancel the operation, or close the panel: back where you were     |
| navigation | `q`            | quit: processes stop, sessions start again with the next run      |

Adding an item adds one of every level inside it, down to a pane with a new shell that
starts where the hovered one is. Each is named automatically, as `window-2`: enter takes
the typed name instead and goes on to the next level, esc keeps the automatic names.

Removing an item removes all it holds and the containers it leaves empty, and the
terminals no pane shows anymore. Stopping keeps the structure: entering a stopped pane
starts it again.

## State

`$XDG_STATE_HOME/nui-yalij/state.kdl` (`~/.local/state/...`), or `--state <file>`. It's
written after every change, and read again when someone else changes it: editing it while
nui-yalij runs changes the TUI. A broken edit is reported and kept until fixed.
`nui-yalij attach <session>` from inside a pane makes the running instance follow.

```kdl
toggle ctrl-g
attached 1
plugin "~/.config/nui-yalij/example.lua"
session main active=1 {
    terminal 1 command="/bin/zsh" cwd="/home/me" pid=4242 program=vim title="~"
    terminal 2 command=htop status=stopped {
        args "-d" "10"
    }
    window "dev" {
        tab layout=columns {
            pane terminal=1
            pane "monitor" terminal=2
        }
    }
}
navigation focus=pane session=1 window=1 tab=1 pane=2 {
    rename "draft"
}
message "the last notice"
```

Indices count from 1. Omitted values take defaults: `layout=tabs`, `active=1`,
`status=running` (the others are `stopped`, by the person, and `exited`, on its own).
Unnamed windows, tabs and panes show the name of what they show.

## Plugins

Lua 5.4 files listed by `plugin` (paths relative to the state file, or `~/`), loaded
again when the list changes. A plugin returns a table with any of:

```lua
return {
  -- layout strategies by name: the areas of the children, and what to draw
  layouts = {
    columns = function(container) -- { kind, area = { x, y, w, h }, active, children = { { name } } }
      return { areas = { [1] = { x = 0, y = 0, w = 40, h = 24 } }, draw = { { x = 0, y = 0, text = "…", style = { fg = "blue" } } } }
    end,
  },
  -- keys by mode, view or navigate: a new state, or nothing to keep the table changed in place
  keys = { navigate = { n = function(state) end } },
  -- after every change of the state
  update = function(state) return state end,
}
```

The state is the plain `Nui` table, as in KDL. Helpers in the global `nui`: `shell`, `next_id(state)`,
and the operations of the panel `remove(state, level, location)`, `stop(…)`, `rename(…, name)`,
`enter(state, location)`. `print` shows in the panel. Later plugins override earlier ones,
plugin layouts and keys override the built-in ones.

[`plugins/example.lua`](plugins/example.lua) adds the `columns`, `rows` and `bar` (tabs with a bar)
layouts, `n` (new item) and `c` (next layout) in the panel, and closes panes whose shell
exited well.

## Not here (yet)

- A server: processes live while nui-yalij runs; on the next run, sessions start again
  from the state, in their last directories.
- Scrollback and the mouse in panes.
