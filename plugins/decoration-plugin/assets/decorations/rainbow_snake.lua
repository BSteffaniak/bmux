-- Rainbow snake decoration — runs a chasing HSL rainbow around the
-- border of a focused pane.

local INITIAL_SNAKE_SIZE = 8
local snake_size = INITIAL_SNAKE_SIZE
local apple_index = nil
local apple_key = nil
local rng_state = nil
local state_total = nil

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

local function perimeter_cell_raw(rect, cell_offset)
    local w = rect.w
    local h = rect.h
    local total = len_border(rect)
    local p = cell_offset % total
    if p < (w - 1) then
        return rect.x + math.floor(p), rect.y
    end
    p = p - (w - 1)
    if p < (h - 1) then
        return rect.x + w - 1, rect.y + math.floor(p)
    end
    p = p - (h - 1)
    if p < (w - 1) then
        return rect.x + w - 1 - math.floor(p), rect.y + h - 1
    end
    p = p - (w - 1)
    return rect.x, rect.y + h - 1 - math.floor(p)
end

local function cell_key(col, row)
    return col .. ":" .. row
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

local function spawn_apple(ctx, occupied, total)
    for _ = 1, total do
        local candidate = next_random(ctx, total)
        local col, row = perimeter_cell_raw(ctx.rect, candidate)
        local key = cell_key(col, row)
        if not occupied[key] then
            apple_index = candidate
            apple_key = key
            return
        end
    end
    apple_index = nil
    apple_key = nil
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

local function insert_apple(cmds, rect)
    if apple_index == nil then
        return
    end
    local col, row = perimeter_cell_raw(rect, apple_index)
    table.insert(cmds, {
        kind  = "text",
        col   = col,
        row   = row,
        z     = 19,
        text  = "●",
        style = { fg = bmux.rgb(255, 95, 95) },
    })
end

local function collect_snake_cells(rect, head, visual_total, vertical_aspect, segment_visual_spacing, total)
    local occupied = {}
    local segments = {}
    local max_size = math.min(snake_size, total)
    local emitted = 0
    local attempts = 0
    while emitted < max_size and attempts < max_size * 4 do
        local offset = head - attempts * segment_visual_spacing
        local col, row = perimeter_cell(rect, offset, vertical_aspect)
        local key = cell_key(col, row)
        if not occupied[key] then
            occupied[key] = true
            emitted = emitted + 1
            table.insert(segments, {
                col = col,
                row = row,
                offset = offset,
            })
        end
        attempts = attempts + 1
    end
    return occupied, segments
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
        snake_size = math.min(INITIAL_SNAKE_SIZE, total)
        apple_index = nil
        apple_key = nil
        rng_state = nil
        state_total = total
    end

    if snake_size * segment_visual_spacing >= visual_total - segment_visual_spacing then
        snake_size = math.min(INITIAL_SNAKE_SIZE, total)
        apple_index = nil
        apple_key = nil
    end

    local occupied, segments = collect_snake_cells(
        ctx.rect,
        head,
        visual_total,
        vertical_aspect,
        segment_visual_spacing,
        total
    )

    local head_segment = segments[1]
    local head_key = nil
    if head_segment ~= nil then
        head_key = cell_key(head_segment.col, head_segment.row)
    end

    if apple_index == nil or apple_key == nil then
        spawn_apple(ctx, occupied, total)
    elseif head_key == apple_key then
        snake_size = math.min(snake_size + 1, total)
        occupied, segments = collect_snake_cells(
            ctx.rect,
            head,
            visual_total,
            vertical_aspect,
            segment_visual_spacing,
            total
        )
        spawn_apple(ctx, occupied, total)
    elseif occupied[apple_key] then
        spawn_apple(ctx, occupied, total)
    end

    insert_apple(cmds, ctx.rect)

    local snake_len = math.max(#segments, 1)
    for i, segment in ipairs(segments) do
        local hue = ((segment.offset % visual_total) / visual_total * 360) % 360
        local lightness = 0.45 - ((i - 1) / snake_len) * 0.20
        local r, g, b = bmux.hsl_to_rgb(hue, 0.95, lightness)
        insert_diamond(cmds, segment.col, segment.row, 20, r, g, b)
    end

    return cmds
end
