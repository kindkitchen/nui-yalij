-- An example plugin: what the core leaves out, built on the state alone.
--
-- Load it from global or session decoration (not a profile, for update hooks):
-- paths are relative to that decoration file, or from ~/:
--   plugin "~/path/to/plugins/example.lua"
--
-- layouts: columns, rows, and bar: a bar naming the children over the active one
-- keys in the panel: n  a new item after the hovered one, with a new terminal
--                    c  the next layout of the section's container
-- update: panes whose terminal ended well close, as in most multiplexers

local M = {}

-- Children side by side along `axis` ("w" or "h"), a line apart.
local function split(container, axis, line)
  local a, n = container.area, #container.children
  local start = axis == "w" and "x" or "y"
  local areas, draw = {}, {}
  for i = 1, n do
    local from = a[start] + (i - 1) * a[axis] // n
    local to = a[start] + i * a[axis] // n
    if i < n then
      to = to - 1
      if axis == "w" then
        for y = a.y, a.y + a.h - 1 do
          draw[#draw + 1] = { x = to, y = y, text = line, style = { fg = "darkgray" } }
        end
      else
        draw[#draw + 1] = { x = a.x, y = to, text = line:rep(a.w), style = { fg = "darkgray" } }
      end
    end
    local area = { x = a.x, y = a.y, w = a.w, h = a.h }
    area[start], area[axis] = from, to - from
    areas[i] = area
  end
  return { areas = areas, draw = draw }
end

M.layouts = {
  columns = function(container) return split(container, "w", "│") end,
  rows = function(container) return split(container, "h", "─") end,
  bar = function(container)
    local a, x, draw = container.area, container.area.x, {}
    for i, child in ipairs(container.children) do
      local text = " " .. i .. " " .. child.name .. " "
      draw[#draw + 1] = { x = x, y = a.y, text = text, style = { fg = "blue", reversed = i == container.active } }
      x = x + utf8.len(text)
    end
    local area = { x = a.x, y = a.y + 1, w = a.w, h = a.h - 1 }
    return { areas = { [container.active] = area }, draw = draw }
  end,
}

local function terminal(state, session)
  local id = nui.next_id(state)
  table.insert(state.sessions[session].terminals, { id = id, command = nui.shell })
  return id
end

local function pane(state, session) return { terminal = terminal(state, session) } end
local function window(state, session) return { panes = { pane(state, session) } } end
local function workspace(state, session) return { windows = { window(state, session) } } end

-- Insert `item` at `index`, keeping `owner[key]` on the item it was on.
local function insert(list, index, item, owner, key)
  table.insert(list, index, item)
  if owner[key] >= index then owner[key] = owner[key] + 1 end
end

local function unique(state)
  local taken = {}
  for _, session in ipairs(state.sessions) do taken[session.name] = true end
  local n = #state.sessions + 1
  while taken["s" .. n] do n = n + 1 end
  return "s" .. n
end

local layouts = { tabs = "columns", columns = "rows", rows = "bar", bar = "tabs" }

M.keys = {
  navigate = {
    n = function(state)
      local focus, at = state.navigation.focus, state.navigation.cursor
      local session = state.sessions[at.session]
      if focus == "session" then
        local index = at.session + 1
        insert(state.sessions, index, { name = unique(state), terminals = {}, workspaces = {} }, state, "attached")
        state.sessions[index].workspaces = { workspace(state, index) }
        at.session, at.workspace, at.window, at.pane = index, 1, 1, 1
      elseif focus == "workspace" then
        insert(session.workspaces, at.workspace + 1, workspace(state, at.session), session, "active")
        at.workspace, at.window, at.pane = at.workspace + 1, 1, 1
      elseif focus == "window" then
        local parent = session.workspaces[at.workspace]
        insert(parent.windows, at.window + 1, window(state, at.session), parent, "active")
        at.window, at.pane = at.window + 1, 1
      else
        local parent = session.workspaces[at.workspace].windows[at.window]
        insert(parent.panes, at.pane + 1, pane(state, at.session), parent, "active")
        at.pane = at.pane + 1
      end
    end,
    c = function(state)
      local focus, at = state.navigation.focus, state.navigation.cursor
      local session = state.sessions[at.session]
      local workspace = session.workspaces[at.workspace]
      local container = ({ workspace = session, window = workspace, pane = workspace.windows[at.window] })[focus]
      if not container then
        return print("sessions have no layout: one is attached at a time")
      end
      container.layout = layouts[container.layout] or "tabs"
      print(focus .. "s placed as " .. container.layout)
    end,
  },
}

-- The first pane showing a terminal that ended well.
local function ended(state)
  for s, session in ipairs(state.sessions) do
    local done = {}
    for _, terminal in ipairs(session.terminals) do
      done[terminal.id] = terminal.status == "exited" and terminal.exit == 0
    end
    for w, workspace in ipairs(session.workspaces) do
      for v, window in ipairs(workspace.windows) do
        for p, pane in ipairs(window.panes) do
          if done[pane.terminal] then return { session = s, workspace = w, window = v, pane = p } end
        end
      end
    end
  end
end

function M.update(state)
  local at = ended(state)
  while at do
    state = nui.remove(state, "pane", at)
    at = ended(state)
  end
  return state
end

return M
