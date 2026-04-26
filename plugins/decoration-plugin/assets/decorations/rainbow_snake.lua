-- Rainbow snake decoration -- a small snake game on the focused pane border.

local INITIAL_SNAKE_SIZE = 8
local VERTICAL_ASPECT = 2.0
local SPEED = 12
local SPACING = 2.0
local EAT_RADIUS = 0.75
local APPLE_PAD = 0.75
local DEATH_HOLD_MS = 3000
local SHRINK_BLOCKS_PER_SECOND = 24
local FLASH_MS = 150

local pane_states = {}

local function new_pane_state()
    return {
        snake_size = INITIAL_SNAKE_SIZE,
        apple_v = nil,
        rng_state = nil,
        head_offset_v = 0,
        death_started_ms = nil,
        death_segments = nil,
        active_ms = 0,
        last_focus_ms = nil,
    }
end

local function pane_state(pane)
    local key = pane.id or "default"
    if pane_states[key] == nil then
        pane_states[key] = new_pane_state()
    end
    return pane_states[key]
end

local function border_metrics(rect)
    local w = rect.w
    local h = rect.h
    local raw_total = 2 * (w - 1) + 2 * (h - 1)
    local visual_total = 2 * (w - 1) + 2 * (h - 1) * VERTICAL_ASPECT
    return raw_total, visual_total
end

-- Map a visual-distance offset around the border to a terminal cell.
-- Vertical cells cost more distance because terminal cells are taller than wide.
local function perimeter_cell(rect, visual_offset, visual_total)
    local w = rect.w
    local h = rect.h
    local p = visual_offset % visual_total

    if p < (w - 1) then
        return rect.x + math.floor(p), rect.y
    end
    p = p - (w - 1)

    local side_visual = (h - 1) * VERTICAL_ASPECT
    if p < side_visual then
        return rect.x + w - 1, rect.y + math.floor(p / VERTICAL_ASPECT)
    end
    p = p - side_visual

    if p < (w - 1) then
        return rect.x + w - 1 - math.floor(p), rect.y + h - 1
    end
    p = p - (w - 1)

    return rect.x, rect.y + h - 1 - math.floor(p / VERTICAL_ASPECT)
end

local function seed_rng(state, pane, message)
    if state.rng_state ~= nil then
        return
    end
    local rect = pane.rect
    state.rng_state = (message.frame * 97 + message.time_ms + rect.w * 131 + rect.h * 197) % 233280
end

local function rand(state, pane, message, max_value)
    seed_rng(state, pane, message)
    state.rng_state = (state.rng_state * 9301 + 49297) % 233280
    return math.floor((state.rng_state / 233280) * max_value)
end

local function unit_rand(state, pane, message)
    return rand(state, pane, message, 1000000) / 1000000.0
end

local function circular_distance(a, b, total)
    local d = math.abs((a % total) - (b % total))
    return math.min(d, total - d)
end

local function apple_on_snake(head, apple, snake_len, visual_total, pad)
    if apple == nil then
        return false
    end
    local body_span = math.max(0, snake_len - 1) * SPACING
    local behind_head = (head - apple) % visual_total
    return behind_head <= body_span + (pad or APPLE_PAD)
end

local function spawn_apple(state, pane, message, head, snake_len, visual_total)
    for _ = 1, 32 do
        local candidate = unit_rand(state, pane, message) * visual_total
        if not apple_on_snake(head, candidate, snake_len, visual_total, APPLE_PAD) then
            state.apple_v = candidate
            return
        end
    end

    local step = math.max(0.5, SPACING / 2)
    local scan_count = math.floor(visual_total / step)
    local start = unit_rand(state, pane, message) * visual_total
    for i = 0, scan_count do
        local candidate = (start + i * step) % visual_total
        if not apple_on_snake(head, candidate, snake_len, visual_total, APPLE_PAD) then
            state.apple_v = candidate
            return
        end
    end

    state.apple_v = nil
end

local function put(cmds, col, row, z, text, r, g, b)
    table.insert(cmds, {
        kind = "text",
        col = col,
        row = row,
        z = z,
        text = text,
        style = { fg = bmux.rgb(r, g, b) },
    })
end

local function snake_segments(rect, head, snake_len, visual_total)
    local segments = {}
    for i = 0, snake_len - 1 do
        local offset = head - i * SPACING
        local col, row = perimeter_cell(rect, offset, visual_total)
        table.insert(segments, { col = col, row = row, offset = offset })
    end
    return segments
end

local function render_snake(cmds, segments, visual_total, red)
    local snake_len = math.max(#segments, 1)
    for i, segment in ipairs(segments) do
        local r, g, b
        if red then
            r, g, b = 255, 95, 95
        else
            local hue = ((segment.offset % visual_total) / visual_total * 360) % 360
            local lightness = 0.45 - ((i - 1) / snake_len) * 0.20
            r, g, b = bmux.hsl_to_rgb(hue, 0.95, lightness)
        end
        put(cmds, segment.col, segment.row, 20, "◆", r, g, b)
    end
end

local function render_apple(cmds, state, rect, visual_total)
    if state.apple_v == nil then
        return
    end
    local col, row = perimeter_cell(rect, state.apple_v, visual_total)
    put(cmds, col, row, 19, "●", 255, 95, 95)
end

local function reset_game(state, raw_total)
    state.snake_size = math.min(INITIAL_SNAKE_SIZE, raw_total)
    state.apple_v = nil
    state.death_started_ms = nil
    state.death_segments = nil
end

local function render_death(cmds, state, raw_total, visual_total)
    if state.death_started_ms == nil or state.death_segments == nil then
        return false
    end

    local elapsed = state.active_ms - state.death_started_ms
    local min_count = math.min(INITIAL_SNAKE_SIZE, #state.death_segments)
    if elapsed > DEATH_HOLD_MS then
        local shrink_elapsed = (elapsed - DEATH_HOLD_MS) / 1000.0
        local removed = math.floor(shrink_elapsed * SHRINK_BLOCKS_PER_SECOND)
        if #state.death_segments - removed <= min_count then
            local resume_index = math.max(1, #state.death_segments - min_count + 1)
            local resume_head_v = state.death_segments[resume_index].offset
            state.head_offset_v = (resume_head_v - (state.active_ms / 1000.0) * SPEED) % visual_total
            reset_game(state, raw_total)
            return false
        end
    end

    if math.floor(elapsed / FLASH_MS) % 2 == 0 then
        local visible_count = #state.death_segments
        if elapsed > DEATH_HOLD_MS then
            local shrink_elapsed = (elapsed - DEATH_HOLD_MS) / 1000.0
            local removed = math.floor(shrink_elapsed * SHRINK_BLOCKS_PER_SECOND)
            visible_count = math.max(min_count, #state.death_segments - removed)
        end

        local visible_segments = {}
        local start_index = 1
        if elapsed > DEATH_HOLD_MS then
            start_index = #state.death_segments - visible_count + 1
        end
        for i = start_index, #state.death_segments do
            table.insert(visible_segments, state.death_segments[i])
        end
        render_snake(cmds, visible_segments, 1, true)
    end
    return true
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

    local raw_total, visual_total = border_metrics(pane.rect)
    if raw_total <= 0 or visual_total <= 0 then
        return {}
    end

    local cmds = {}
    if render_death(cmds, state, raw_total, visual_total) then
        return cmds
    end

    local head = (state.head_offset_v + (state.active_ms / 1000.0) * SPEED) % visual_total
    local snake_len = math.min(state.snake_size, raw_total)
    local segments = snake_segments(pane.rect, head, snake_len, visual_total)

    if state.apple_v == nil or apple_on_snake(head, state.apple_v, snake_len, visual_total, APPLE_PAD) then
        spawn_apple(state, pane, message, head, snake_len, visual_total)
    end

    if state.apple_v ~= nil and circular_distance(head, state.apple_v, visual_total) <= EAT_RADIUS then
        state.snake_size = math.min(state.snake_size + 1, raw_total)
        snake_len = math.min(state.snake_size, raw_total)
        segments = snake_segments(pane.rect, head, snake_len, visual_total)
        spawn_apple(state, pane, message, head, snake_len, visual_total)
    end

    local body_span = math.max(0, snake_len - 1) * SPACING
    local tail_gap = visual_total - body_span
    if tail_gap <= SPACING then
        state.death_started_ms = state.active_ms
        state.death_segments = segments
        state.apple_v = nil
        render_death(cmds, state, raw_total, visual_total)
        return cmds
    end

    if apple_on_snake(head, state.apple_v, snake_len, visual_total, APPLE_PAD) then
        spawn_apple(state, pane, message, head, snake_len, visual_total)
    end

    render_apple(cmds, state, pane.rect, visual_total)
    render_snake(cmds, segments, visual_total, false)
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
