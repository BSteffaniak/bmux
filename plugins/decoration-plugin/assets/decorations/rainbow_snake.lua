-- Rainbow snake decoration — runs a chasing HSL rainbow around the
-- border of a focused pane.

local function len_border(rect)
    local w = rect.w
    local h = rect.h
    return 2 * (w - 1) + 2 * (h - 1)
end

-- Map a cell offset along the border perimeter to a (col, row) on the
-- outer rect. Clockwise from top-left.
local function perimeter_cell(rect, offset)
    local w = rect.w
    local h = rect.h
    local total = len_border(rect)
    local p = offset % total
    -- Top edge (left -> right).
    if p < (w - 1) then
        return rect.x + math.floor(p), rect.y
    end
    p = p - (w - 1)
    -- Right edge (top -> bottom).
    if p < (h - 1) then
        return rect.x + w - 1, rect.y + math.floor(p)
    end
    p = p - (h - 1)
    -- Bottom edge (right -> left).
    if p < (w - 1) then
        return rect.x + w - 1 - math.floor(p), rect.y + h - 1
    end
    p = p - (w - 1)
    -- Left edge (bottom -> top).
    return rect.x, rect.y + h - 1 - math.floor(p)
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

    local cells_per_second = 12
    local head = math.floor((ctx.time_ms / 1000.0) * cells_per_second) % total
    local snake_len = math.min(8, total)
    for i = 0, snake_len - 1 do
        local offset = head - i
        local col, row = perimeter_cell(ctx.rect, offset)
        local hue = ((offset % total) / total * 360) % 360
        local lightness = 0.45 - (i / snake_len) * 0.20
        local r, g, b = bmux.hsl_to_rgb(hue, 0.95, lightness)
        table.insert(cmds, {
            kind  = "text",
            col   = col,
            row   = row,
            z     = 20,
            text  = "◆",
            style = { fg = bmux.rgb(r, g, b) },
        })
    end
    return cmds
end
