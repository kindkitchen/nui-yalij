---
name: nui-yalij
description: Explain and configure nui-yalij, a Unix terminal multiplexer with shared sessions, layered KDL configuration, and fuzzy pane navigation. Use when editing its config, key bindings, Escape gestures, themes, profiles, or plugins.
---

# nui-yalij

nui-yalij (yazi + zellij) is a Unix terminal multiplexer, not a library.
One background coordinator owns independent session servers and their PTYs.
Clients can share a session; disconnecting a client leaves programs running.
The hierarchy is **Session -> Workspace -> Window -> Pane**. A pane shows a
terminal; several panes may show the same terminal. Active selections and
structure are shared, but navigation/search and the profile are per instance.

## Inspect first

Use the installed executable, or `cargo run --` from its source checkout:

```nu
nui-yalij --version
nui-yalij help
nui-yalij config path
nui-yalij config show
nui-yalij config check
nui-yalij list
```

These commands do not start a server. `config show` prints the selected file
verbatim, or an editable default template if it is missing. `config check`
validates global/profile KDL, not plugin execution or session overrides.
`nui-yalij skill` prints this complete skill without installing or writing it.

Ask which catalog/config directory and profile the user uses. Do not assume
that a default path is their actual one. `--config DIR` chooses the config
root, `--state FILE` chooses a session catalog, and `--profile NAME` chooses
this client's profile. Shells inside panes inherit `NUI_YALIJ_CONFIG` and
`NUI_YALIJ_STATE`, so these commands default to that pane's configuration/catalog.
There is no inherited profile: a shared terminal can have viewers with different
profiles. Supply `--profile NAME` explicitly when configuring one.

## Configuration, not session state

Configuration is called **decoration**, with the same KDL schema at each layer:

1. Global: `$XDG_CONFIG_HOME/nui-yalij/decoration.kdl`, falling back to
   `~/.config/nui-yalij/decoration.kdl` (or `DIR/decoration.kdl` with `--config`).
2. Profile: `DIR/profiles/NAME.kdl`, selected with `--profile NAME`.
3. Session: `<global-state-file>.sessions/<stable-session-id>/decoration.kdl`.

Later layers override individual key/command actions, gesture settings, theme
fields, and icons. Plugin lists accumulate; later plugins take precedence.
Missing files/settings use defaults. A session override wins over profiles.

Global/profile files load at server start and with `nui-yalij reload`.
Session decoration is read live, including deletion. Invalid reloads retain
the last valid configuration. Editing Lua source also needs `reload`.

Before any edit, inspect the existing file and preserve unrelated settings.
Get approval before creating/changing files, attaching, reloading, or executing
lifecycle commands. To create a starter without overwriting an existing file:

```nu
nui-yalij config init                         # Preview only
nui-yalij config init --apply                 # After approval
nui-yalij --profile work config path
nui-yalij --profile work config init --apply  # Only when requested
```

Prefer small override files rather than copying every default. Example:

```kdl
keys {
    panel ctrl-t
    search "/"
    next j down
    previous k up
    back esc
}
gestures double-escape-ms=300 hold-escape-ms=600
commands {
    panel t
}
theme accent=blue active=green muted=darkgray notice=yellow error=red
icons plain
// plugin "~/path/to/plugin.lua"
```

KDL nodes need newlines or semicolons between them. Quote punctuation such as
`"/"`, `"["`, `"]"`, and paths. Unknown nodes, actions, properties, keys, and
colors are errors; do not invent settings.

### Keys and gestures

`keys` actions and defaults:

```kdl
keys {
    panel ctrl-t
    search "/"
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
    enter enter
    back esc
    detach q
    quit Q
    restart enter
}
```

Each action replaces its inherited list. An action with no arguments disables
it, e.g. `keys { search; }`. Supported modifiers: `ctrl-`, `alt-`, `super-`,
`shift-`; named keys include `esc`, `enter`, arrows, and `f1` through `f12`.
Text prompts use literal input rather than navigation remappings.

`gestures` has exactly two integer properties, each **0..60000 milliseconds**:

- `double-escape-ms=300`: two fast unmodified Escape presses open navigation.
- `hold-escape-ms=600`: holding unmodified Escape opens navigation.
- **0 disables that gesture independently.** For example,
  `gestures double-escape-ms=0 hold-escape-ms=800` keeps only held Escape.

Gestures apply in the terminal view, open rather than toggle, and never delay
the first Escape sent to the program. Kitty keyboard event reports allow
precise holds. Other terminals need auto-repeat; holds and rapid repeat presses
cannot be distinguished perfectly there. A lone Escape must never be treated
as a hold without release reports. `ctrl-t` remains the portable fallback.

### Other settings

- `commands { panel t; }`: shell command names, available only inside panes.
  A command with no names disables it. Names are plain file names, not paths.
- `theme`: only `accent`, `active`, `muted`, `notice`, `error`, with valid
  ratatui colors such as `blue`, `darkgray`, or `"#88aaff"`.
- `icons nerd` (default) or `icons plain` for fonts without Nerd Font glyphs.
- `plugin PATH`: trusted Lua 5.4 code, not sandboxed. Paths resolve relative to
  the decoration file; `~/` expands to HOME. Never add an untrusted plugin.

Validate with the same `--config`/`--profile` arguments, then reload only after
approval. If a setting appears ignored, inspect later profile/session overrides.
Do not change `global-state.kdl` or session `state.kdl` to configure appearance,
keys, or gestures: those files are authoritative shared session state.

## Navigation and search

`ctrl-t`, double Escape, held Escape, or `t` inside a pane opens navigation.
`[`/`]` switch native hierarchy and filesystem representations. `h/j/k/l` or
arrows move; Enter enters; Escape returns to the original attachment.
`r`, `a`, `x`, `s` rename/add/remove/stop in native navigation.
`q` disconnects only this instance; **`Q` stops the entire catalog**.

`/` in either representation starts a live pane search across every session.
It is literal text during rename/create prompts. Search matches names at every
level, cwd paths, terminal programs/titles, commands, and arguments. No external
`fzf` executable is required. Matching is Unicode-aware, fuzzy-ranked, and smart
case (uppercase makes a term case-sensitive). Space-separated terms are ANDed
and may match different fields; `|` joins alternative terms. `'text` is exact
substring, `^text` prefix, `text$` suffix, and `!text` excludes matching fields.

While typing: Backspace deletes, Ctrl-U clears, pasted text updates the query,
Escape cancels, and **Enter freezes the result list**, not opens a pane.
Then j/k or arrows browse and another Enter opens the selected pane. `/`
re-edits the query; Escape leaves results for normal navigation. The original
terminal stays visible below the search panel. New panes do not join a frozen
list; deleted targets are removed rather than replaced by unrelated panes.

## Safety

Do not install/update/uninstall, publish, stop servers, or rewrite session state
unless explicitly requested. Lifecycle commands observe without `--apply`, but
`uninstall --apply` is destructive. `reload` changes every connected viewer's
static config snapshot; confirm that effect before running it.
