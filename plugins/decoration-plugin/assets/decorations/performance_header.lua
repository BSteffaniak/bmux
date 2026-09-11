-- Performance owns resource-to-color policy; rendering stays nonanimated.
local colors = bmux.module("performance-colors-v1")
local latest = { panes = {}, system = {} }

local function render(message)
    local surfaces = {}
    local component = message.component or {}
    local settings = component.settings or {}
    local entrypoint = component.entrypoint or "all"
    for _, pane in ipairs(message.panes or {}) do
        local metrics = colors.metrics(latest, pane)
        local cpu = math.max(0, math.min(100, metrics.cpu_normalized_percent or metrics.cpu_percent or 0))
        local heat = colors.heat(latest, pane, settings)
        local r, g, b = colors.color(heat)
        local fg = bmux.rgb(math.floor(r), math.floor(g), math.floor(b))
        local parts = { string.format("CPU %d%%", math.floor(cpu + 0.5)) }
        local bytes = metrics.memory_bytes or metrics.memory_used_bytes
        if bytes ~= nil then
            table.insert(parts, string.format("MEM %dM", math.floor(bytes / 1048576 + 0.5)))
        end
        if metrics.process_count ~= nil then
            table.insert(parts, string.format("P %d", metrics.process_count))
        end
        local glyphs, z = "single-line", 11
        if pane.focused or heat >= 80 then
            glyphs, z = "thick", 14
        elseif heat >= 50 then
            glyphs, z = "rounded", 12
        end
        local cmds = {}
        if entrypoint == "all" or entrypoint == "border" then
            table.insert(cmds, {
                kind = "semantic_border", rect = pane.rect, z = z,
                fallback_glyphs = glyphs,
                thickness_px = pane.focused and 3 or 1,
                radius_px = pane.focused and 2 or 0,
                style = { fg = fg, bold = pane.focused or heat >= 50 },
            })
        end
        if entrypoint == "all" or entrypoint == "header" then
            table.insert(cmds, {
                kind = "text", col = pane.rect.x + 2, row = pane.rect.y, z = z + 1,
                text = " " .. table.concat(parts, " ") .. " ",
                style = { fg = fg, bold = true },
            })
        end
        surfaces[pane.id] = cmds
    end
    return { surfaces = surfaces }
end

function decorate(message)
    if message.kind == "event" and message.event ~= nil then
        if message.event.source == "bmux.performance/metrics-state" then
            latest = message.event.payload or message.event.snapshot or latest
        end
    elseif message.kind == "render" then
        return render(message)
    end
    return nil
end
