-- Pulse decoration -- breathes the focused border between two RGB endpoints.

local function lerp(a, b, t)
    return a + (b - a) * t
end

local function render(message)
    local surfaces = {}
    local phase = (message.time_ms % 2000) / 2000.0
    local t = 0.5 + 0.5 * math.sin(phase * 2 * math.pi)
    local r = lerp(57, 0, t)
    local g = lerp(255, 255, t)
    local b = lerp(20, 200, t)

    for _, pane in ipairs(message.panes or {}) do
        if pane.focused then
            surfaces[pane.id] = {
                {
                    kind = "box_border",
                    rect = pane.rect,
                    z = 10,
                    glyphs = "thick",
                    style = {
                        fg = bmux.rgb(math.floor(r), math.floor(g), math.floor(b)),
                        bold = true,
                    },
                },
            }
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
