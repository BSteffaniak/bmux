-- Pong decoration -- a tiny deterministic Pong match around each focused pane.

local RALLY_MS = 2600
local WIN_HOLD_MS = 3000
local WIN_SCORE = 10
local PADDLE_SIZE = 5
local SCORERS = { "right", "left", "right", "right", "left", "right", "left", "right", "right", "left", "right", "left", "right", "right", "left", "left", "right" }
local pane_states = {}

local function pane_state(pane)
    local key = pane.id or "default"
    if pane_states[key] == nil then
        pane_states[key] = { last_focus_ms = nil, active_ms = 0 }
    end
    return pane_states[key]
end

local function put(cmds, col, row, z, text, r, g, b, bold, dim)
    table.insert(cmds, {
        kind = "text",
        col = col,
        row = row,
        z = z,
        text = text,
        style = { fg = bmux.rgb(r, g, b), bold = bold or false, dim = dim or false },
    })
end

local function clamp(value, min_value, max_value)
    return math.max(min_value, math.min(max_value, value))
end

local function score_before(rally_index)
    local left = 0
    local right = 0
    for i = 1, rally_index do
        if SCORERS[i] == "left" then
            left = left + 1
        else
            right = right + 1
        end
    end
    return left, right
end

local function game_snapshot(active_ms)
    local match_rallies = #SCORERS
    local match_ms = match_rallies * RALLY_MS + WIN_HOLD_MS
    local t = active_ms % match_ms
    local winner = SCORERS[match_rallies]

    if t >= match_rallies * RALLY_MS then
        local left, right = score_before(match_rallies)
        return { win = true, winner = winner, left_score = left, right_score = right }
    end

    local rally_index = math.floor(t / RALLY_MS) + 1
    local rally_t = (t % RALLY_MS) / RALLY_MS
    local left, right = score_before(rally_index - 1)
    local scorer = SCORERS[rally_index]
    return {
        win = false,
        rally_index = rally_index,
        rally_t = rally_t,
        scorer = scorer,
        left_score = left,
        right_score = right,
    }
end

local function ball_position(content, snapshot)
    local w = math.max(1, content.w)
    local h = math.max(1, content.h)
    local p = snapshot.rally_t
    local left_to_right = snapshot.scorer == "right"
    local x
    if left_to_right then
        x = math.floor(p * (w - 1))
    else
        x = math.floor((1.0 - p) * (w - 1))
    end

    local bounces = 1.5 + (snapshot.rally_index % 3) * 0.5
    local phase = (p * bounces + (snapshot.rally_index % 5) * 0.13) % 1.0
    local tri = 1.0 - math.abs(phase * 2.0 - 1.0)
    local y = math.floor(tri * (h - 1))
    return content.x + x, content.y + y, y
end

local function render_ball(cmds, pane, snapshot)
    if snapshot.win then
        return
    end
    local col, row = ball_position(pane.content_rect, snapshot)
    put(cmds, col, row, 0, "●", 95, 175, 255, false, true)
end

local function render_paddles(cmds, pane, snapshot)
    local rect = pane.rect
    local content = pane.content_rect
    if rect.h < 3 then
        return
    end

    local _, _, ball_y = ball_position(content, snapshot)
    local paddle_len = math.min(PADDLE_SIZE, math.max(1, rect.h - 2))
    local max_top = math.max(0, rect.h - 2 - paddle_len)
    local base_top = clamp(ball_y - math.floor(paddle_len / 2), 0, max_top)
    local left_top = base_top
    local right_top = base_top

    if not snapshot.win and snapshot.rally_t > 0.72 then
        local miss_offset = math.max(2, math.floor(paddle_len / 2) + 1)
        if snapshot.scorer == "right" then
            left_top = clamp(base_top + miss_offset, 0, max_top)
        else
            right_top = clamp(base_top - miss_offset, 0, max_top)
        end
    end

    for i = 0, paddle_len - 1 do
        put(cmds, rect.x, content.y + left_top + i, 20, "▌", 255, 95, 215, true, false)
        put(cmds, rect.x + rect.w - 1, content.y + right_top + i, 20, "▐", 95, 255, 255, true, false)
    end
end

local function render_score(cmds, pane, snapshot)
    local rect = pane.rect
    local score = tostring(snapshot.left_score) .. " : " .. tostring(snapshot.right_score)
    local col = rect.x + math.max(1, math.floor((rect.w - #score) / 2))
    put(cmds, col, rect.y, 30, score, 255, 215, 95, true, false)

    if snapshot.win then
        local label = string.upper(snapshot.winner) .. " WINS"
        local label_col = rect.x + math.max(1, math.floor((rect.w - #label) / 2))
        local label_row = rect.y + math.max(1, math.floor(rect.h / 2))
        put(cmds, label_col, label_row, 31, label, 255, 255, 255, true, false)
    end
end

local function render_pane(pane, message)
    local state = pane_state(pane)
    if not pane.focused then
        state.last_focus_ms = nil
        return {}
    end
    if state.last_focus_ms ~= nil then
        state.active_ms = state.active_ms + math.max(0, message.time_ms - state.last_focus_ms)
    end
    state.last_focus_ms = message.time_ms

    if pane.rect.w < 6 or pane.rect.h < 4 then
        return {}
    end

    local entrypoint = message.component and message.component.entrypoint or "all"
    local snapshot = game_snapshot(state.active_ms)
    local cmds = {}
    if entrypoint == "ball" or entrypoint == "all" then
        render_ball(cmds, pane, snapshot)
    end
    if entrypoint == "paddles" or entrypoint == "all" then
        render_paddles(cmds, pane, snapshot)
    end
    if entrypoint == "score" or entrypoint == "all" then
        render_score(cmds, pane, snapshot)
    end
    return cmds
end

local function render(message)
    local surfaces = {}
    for _, pane in ipairs(message.panes or {}) do
        local commands = render_pane(pane, message)
        if #commands > 0 then
            surfaces[pane.id] = commands
        end
    end
    return { surfaces = surfaces }
end

function decorate(message)
    if message.kind == "render" then
        return render(message)
    end
    return nil
end
