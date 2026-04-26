-- Rainbow snake decoration — runs a chasing HSL rainbow around the
-- border of a focused pane.

local INITIAL_SNAKE_SIZE = 8
local snake_size = INITIAL_SNAKE_SIZE
local apple_v = nil
local rng_state = nil
local state_total = nil
local death_started_ms = nil
local death_segments = nil

local function len_border(rect)
    local w = rect.w
    local h = rect.h
    return 2 * (w - 1) + 2 * (h - 1)
end

local function visual_len_border(rect, vertical_aspect)
    local w = rect.w
    local h = rect.h
    return 2 * (w - 1) + 2 * (h - 1) * vertical_aspect
end

-- Map a visual-distance offset along the border perimeter to a
-- (col, row, edge) on the outer rect. Clockwise from top-left. Vertical
-- cells cost more visual distance because terminal cells are taller than
-- they are wide, making side motion otherwise look too fast.
local function perimeter_cell(rect, visual_offset, vertical_aspect)
    local w = rect.w
    local h = rect.h
    local total = visual_len_border(rect, vertical_aspect)
    local p = visual_offset % total
    -- Top edge (left -> right).
    if p < (w - 1) then
        return rect.x + math.floor(p), rect.y, "top"
    end
    p = p - (w - 1)
    -- Right edge (top -> bottom).
    local side_visual = (h - 1) * vertical_aspect
    if p < side_visual then
        return rect.x + w - 1, rect.y + math.floor(p / vertical_aspect), "right"
    end
    p = p - side_visual
    -- Bottom edge (right -> left).
    if p < (w - 1) then
        return rect.x + w - 1 - math.floor(p), rect.y + h - 1, "bottom"
    end
    p = p - (w - 1)
    -- Left edge (bottom -> top).
    return rect.x, rect.y + h - 1 - math.floor(p / vertical_aspect), "left"
end

local function seed_rng(ctx)
    if rng_state ~= nil then
        return
    end
    local rect = ctx.rect
    rng_state = (ctx.frame * 97 + ctx.time_ms + rect.w * 131 + rect.h * 197) % 233280
end

local function next_random(ctx, max_value)
    seed_rng(ctx)
    rng_state = (rng_state * 9301 + 49297) % 233280
    return math.floor((rng_state / 233280) * max_value)
end

local function circular_distance(a, b, total)
    local d = math.abs((a % total) - (b % total))
    return math.min(d, total - d)
end

local function distance_behind_head(head, point, total)
    return (head - point) % total
end

local function apple_on_snake(head, apple, snake_len, segment_visual_spacing, visual_total, pad)
    if apple == nil then
        return false
    end
    local body_span = math.max(0, snake_len - 1) * segment_visual_spacing
    return distance_behind_head(head, apple, visual_total) <= body_span + (pad or 0.5)
end

local function spawn_apple(ctx, head, snake_len, segment_visual_spacing, visual_total)
    for _ = 1, 32 do
        local candidate = (next_random(ctx, 1000000) / 1000000.0) * visual_total
        if not apple_on_snake(head, candidate, snake_len, segment_visual_spacing, visual_total, 0.75) then
            apple_v = candidate
            return
        end
    end

    local step = math.max(0.5, segment_visual_spacing / 2)
    local scan_count = math.floor(visual_total / step)
    local start = (next_random(ctx, 1000000) / 1000000.0) * visual_total
    for i = 0, scan_count do
        local candidate = (start + i * step) % visual_total
        if not apple_on_snake(head, candidate, snake_len, segment_visual_spacing, visual_total, 0.75) then
            apple_v = candidate
            return
        end
    end

    apple_v = nil
end

local function insert_diamond(cmds, col, row, z, r, g, b)
    table.insert(cmds, {
        kind  = "text",
        col   = col,
        row   = row,
        z     = z,
        text  = "◆",
        style = { fg = bmux.rgb(r, g, b) },
    })
end

local function insert_apple(cmds, rect, vertical_aspect)
    if apple_v == nil then
        return
    end
    local col, row = perimeter_cell(rect, apple_v, vertical_aspect)
    table.insert(cmds, {
        kind  = "text",
        col   = col,
        row   = row,
        z     = 19,
        text  = "●",
        style = { fg = bmux.rgb(255, 95, 95) },
    })
end

local function reset_game(total)
    snake_size = math.min(INITIAL_SNAKE_SIZE, total)
    apple_v = nil
    death_started_ms = nil
    death_segments = nil
end

local function render_death_flash(cmds, ctx, total)
    if death_started_ms == nil or death_segments == nil then
        return false
    end

    local elapsed = ctx.time_ms - death_started_ms
    if elapsed >= 900 then
        reset_game(total)
        return false
    end

    local flash_on = math.floor(elapsed / 150) % 2 == 0
    if flash_on then
        for _, segment in ipairs(death_segments) do
            insert_diamond(cmds, segment.col, segment.row, 20, 255, 95, 95)
        end
    end
    return true
end

local function collect_snake_cells(rect, head, visual_total, vertical_aspect, segment_visual_spacing, total)
    local segments = {}
    local max_size = math.min(snake_size, total)
    for i = 0, max_size - 1 do
        local offset = head - i * segment_visual_spacing
        local col, row = perimeter_cell(rect, offset, vertical_aspect)
        table.insert(segments, {
            col = col,
            row = row,
            offset = offset,
        })
    end
    return segments
end

function decorate(ctx)
    if not ctx.focused then
        return {}
    end
    local cmds = {}
    local total = len_border(ctx.rect)
    if total <= 0 then
        return {}
    end

    local vertical_aspect = 2.0
    local visual_total = visual_len_border(ctx.rect, vertical_aspect)
    local visual_cells_per_second = 12
    local head = ((ctx.time_ms / 1000.0) * visual_cells_per_second) % visual_total
    local segment_visual_spacing = 2.0

    if state_total ~= total then
        state_total = total
    end

    if render_death_flash(cmds, ctx, total) then
        return cmds
    end

    local segments = collect_snake_cells(
        ctx.rect,
        head,
        visual_total,
        vertical_aspect,
        segment_visual_spacing,
        total
    )

    local effective_snake_size = math.min(snake_size, total)
    if apple_v == nil or apple_on_snake(head, apple_v, effective_snake_size, segment_visual_spacing, visual_total) then
        spawn_apple(ctx, head, effective_snake_size, segment_visual_spacing, visual_total)
    end

    if apple_v ~= nil and circular_distance(head, apple_v, visual_total) <= 0.75 then
        snake_size = math.min(snake_size + 1, total)
        effective_snake_size = math.min(snake_size, total)
        segments = collect_snake_cells(
            ctx.rect,
            head,
            visual_total,
            vertical_aspect,
            segment_visual_spacing,
            total
        )
        spawn_apple(ctx, head, effective_snake_size, segment_visual_spacing, visual_total)
    end

    if snake_size * segment_visual_spacing >= visual_total - segment_visual_spacing then
        death_started_ms = ctx.time_ms
        death_segments = segments
        apple_v = nil
        render_death_flash(cmds, ctx, total)
        return cmds
    end

    if apple_on_snake(head, apple_v, effective_snake_size, segment_visual_spacing, visual_total) then
        spawn_apple(ctx, head, effective_snake_size, segment_visual_spacing, visual_total)
    end
    insert_apple(cmds, ctx.rect, vertical_aspect)

    local snake_len = math.max(#segments, 1)
    for i, segment in ipairs(segments) do
        local hue = ((segment.offset % visual_total) / visual_total * 360) % 360
        local lightness = 0.45 - ((i - 1) / snake_len) * 0.20
        local r, g, b = bmux.hsl_to_rgb(hue, 0.95, lightness)
        insert_diamond(cmds, segment.col, segment.row, 20, r, g, b)
    end

    return cmds
end
