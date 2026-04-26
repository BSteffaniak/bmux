-- Rainbow snake decoration -- a small snake game on the focused pane border.

local INITIAL_SNAKE_SIZE = 8
local VERTICAL_ASPECT = 2.0
local SPEED = 12
local SPACING = 2.0
local EAT_RADIUS = 0.75
local APPLE_PAD = 0.75
local DEATH_HOLD_MS = 3000
local DEATH_SHRINK_MS = 1200
local FLASH_MS = 150

local snake_size = INITIAL_SNAKE_SIZE
local apple_v = nil
local rng_state = nil
local death_started_ms = nil
local death_segments = nil

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

local function seed_rng(ctx)
    if rng_state ~= nil then
        return
    end
    local rect = ctx.rect
    rng_state = (ctx.frame * 97 + ctx.time_ms + rect.w * 131 + rect.h * 197) % 233280
end

local function rand(ctx, max_value)
    seed_rng(ctx)
    rng_state = (rng_state * 9301 + 49297) % 233280
    return math.floor((rng_state / 233280) * max_value)
end

local function unit_rand(ctx)
    return rand(ctx, 1000000) / 1000000.0
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

local function spawn_apple(ctx, head, snake_len, visual_total)
    for _ = 1, 32 do
        local candidate = unit_rand(ctx) * visual_total
        if not apple_on_snake(head, candidate, snake_len, visual_total, APPLE_PAD) then
            apple_v = candidate
            return
        end
    end

    local step = math.max(0.5, SPACING / 2)
    local scan_count = math.floor(visual_total / step)
    local start = unit_rand(ctx) * visual_total
    for i = 0, scan_count do
        local candidate = (start + i * step) % visual_total
        if not apple_on_snake(head, candidate, snake_len, visual_total, APPLE_PAD) then
            apple_v = candidate
            return
        end
    end

    apple_v = nil
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

local function render_apple(cmds, rect, visual_total)
    if apple_v == nil then
        return
    end
    local col, row = perimeter_cell(rect, apple_v, visual_total)
    put(cmds, col, row, 19, "●", 255, 95, 95)
end

local function reset_game(raw_total)
    snake_size = math.min(INITIAL_SNAKE_SIZE, raw_total)
    apple_v = nil
    death_started_ms = nil
    death_segments = nil
end

local function render_death(cmds, ctx, raw_total)
    if death_started_ms == nil or death_segments == nil then
        return false
    end

    local elapsed = ctx.time_ms - death_started_ms
    if elapsed >= DEATH_HOLD_MS + DEATH_SHRINK_MS then
        reset_game(raw_total)
        return false
    end

    if math.floor(elapsed / FLASH_MS) % 2 == 0 then
        local visible_count = #death_segments
        if elapsed > DEATH_HOLD_MS then
            local t = (elapsed - DEATH_HOLD_MS) / DEATH_SHRINK_MS
            local min_count = math.min(INITIAL_SNAKE_SIZE, #death_segments)
            visible_count = math.max(
                min_count,
                math.ceil(#death_segments - (#death_segments - min_count) * t)
            )
        end

        local visible_segments = {}
        for i = 1, visible_count do
            visible_segments[i] = death_segments[i]
        end
        render_snake(cmds, visible_segments, 1, true)
    end
    return true
end

function decorate(ctx)
    if not ctx.focused then
        return {}
    end

    local raw_total, visual_total = border_metrics(ctx.rect)
    if raw_total <= 0 or visual_total <= 0 then
        return {}
    end

    local cmds = {}
    if render_death(cmds, ctx, raw_total) then
        return cmds
    end

    local head = ((ctx.time_ms / 1000.0) * SPEED) % visual_total
    local snake_len = math.min(snake_size, raw_total)
    local segments = snake_segments(ctx.rect, head, snake_len, visual_total)

    if apple_v == nil or apple_on_snake(head, apple_v, snake_len, visual_total, APPLE_PAD) then
        spawn_apple(ctx, head, snake_len, visual_total)
    end

    if apple_v ~= nil and circular_distance(head, apple_v, visual_total) <= EAT_RADIUS then
        snake_size = math.min(snake_size + 1, raw_total)
        snake_len = math.min(snake_size, raw_total)
        segments = snake_segments(ctx.rect, head, snake_len, visual_total)
        spawn_apple(ctx, head, snake_len, visual_total)
    end

    local body_span = math.max(0, snake_len - 1) * SPACING
    local tail_gap = visual_total - body_span
    if tail_gap <= SPACING then
        death_started_ms = ctx.time_ms
        death_segments = segments
        apple_v = nil
        render_death(cmds, ctx, raw_total)
        return cmds
    end

    if apple_on_snake(head, apple_v, snake_len, visual_total, APPLE_PAD) then
        spawn_apple(ctx, head, snake_len, visual_total)
    end

    render_apple(cmds, ctx.rect, visual_total)
    render_snake(cmds, segments, visual_total, false)
    return cmds
end
