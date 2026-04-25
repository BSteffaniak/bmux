-- Rainbow snake decoration — runs a chasing HSL rainbow around the
-- border of a focused pane.

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
    local snake_len = math.min(8, total)
    local segment_visual_spacing = 2.0
    local seen = {}
    local emitted = 0
    local attempts = 0
    while emitted < snake_len and attempts < snake_len * 4 do
        local offset = head - attempts * segment_visual_spacing
        local col, row = perimeter_cell(ctx.rect, offset, vertical_aspect)
        local key = col .. ":" .. row
        if not seen[key] then
            seen[key] = true
            emitted = emitted + 1
            local hue = ((offset % visual_total) / visual_total * 360) % 360
            local lightness = 0.45 - ((emitted - 1) / snake_len) * 0.20
            local r, g, b = bmux.hsl_to_rgb(hue, 0.95, lightness)
            insert_diamond(cmds, col, row, 20, r, g, b)
        end
        attempts = attempts + 1
    end
    return cmds
end
