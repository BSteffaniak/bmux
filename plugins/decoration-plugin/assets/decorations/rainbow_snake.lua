-- Rainbow snake decoration — runs a chasing HSL rainbow around the
-- border of a focused pane.

local function len_border(rect)
    local w = rect.w
    local h = rect.h
    return 2 * (w - 1) + 2 * (h - 1)
end

-- Map a 0..1 position along the border perimeter to a (col, row) on
-- the outer rect. Clockwise from top-left.
local function perimeter_cell(rect, u)
    local w = rect.w
    local h = rect.h
    local total = len_border(rect)
    local p = (u % 1.0) * total
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
    local head = (ctx.time_ms / 50) % 1.0
    local snake_len = 10
    for i = 0, snake_len - 1 do
        local u = (head - i / snake_len) % 1.0
        local col, row = perimeter_cell(ctx.rect, u)
        local hue = (u * 360) % 360
        local r, g, b = bmux.hsl_to_rgb(hue, 1.0, 0.5)
        table.insert(cmds, {
            kind  = "text",
            col   = col,
            row   = row,
            z     = 20,
            text  = "\u{2588}",  -- full block
            style = { fg = bmux.rgb(r, g, b) },
        })
    end
    return cmds
end
