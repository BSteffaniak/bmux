-- Pulse decoration — breathes the focused border between two RGB
-- endpoints over a 2-second period.
--
-- Drop into `~/.config/bmux/decorations/pulse.lua` and reference from
-- your theme file:
--
--   [plugins."bmux.decoration".animation]
--   kind = "pulse"
--   hz   = 30
--
--   [plugins."bmux.decoration"]
--   script = "decorations/pulse.lua"

local function lerp(a, b, t)
    return a + (b - a) * t
end

-- Inputs:
--   ctx.rect = { x, y, w, h }          outer bounds of the pane
--   ctx.content_rect = { x, y, w, h }  interior (PTY) bounds
--   ctx.focused, ctx.zoomed, ctx.bell  bools
--   ctx.time_ms                        monotonic ms since attach start
--   ctx.frame                          monotonic frame counter
--
-- Returns:
--   array of paint-command tables — each has a `kind` field plus
--   the fields specific to that variant.
function decorate(ctx)
    -- Unfocused panes get no pulse; the built-in fallback paints
    -- them.
    if not ctx.focused then
        return {}
    end

    -- Breath period = 2 s; `t` sweeps 0 → 1 → 0 with sin().
    local phase = (ctx.time_ms % 2000) / 2000.0
    local t = 0.5 + 0.5 * math.sin(phase * 2 * math.pi)

    -- Interpolate from lime-green to cyan.
    local r = lerp(57, 0, t)
    local g = lerp(255, 255, t)
    local b = lerp(20, 200, t)

    return {
        {
            kind  = "box_border",
            rect  = ctx.rect,
            z     = 10,
            glyphs = "thick",
            style = {
                fg = bmux.rgb(math.floor(r), math.floor(g), math.floor(b)),
                bold = true,
            },
        },
    }
end
