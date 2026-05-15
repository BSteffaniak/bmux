-- Performance header decoration. Kept generic: render whatever supported
-- metrics are published by bmux.performance.

local latest = { panes = {}, system = { cpu_percent = 0, cpu_normalized_percent = 0 } }

local function clamp(value, min_value, max_value)
    if value < min_value then
        return min_value
    end
    if value > max_value then
        return max_value
    end
    return value
end

local function heat_color(cpu)
    local t = clamp(cpu / 100.0, 0.0, 1.0)
    if t < 0.5 then
        local k = t / 0.5
        return math.floor(60 + 195 * k), math.floor(220 - 50 * k), 90
    end
    local k = (t - 0.5) / 0.5
    return 255, math.floor(170 - 95 * k), math.floor(90 - 55 * k)
end

local function pane_metrics(pane)
    local pane_id = pane.pane_id or pane.id
    local metrics = latest.panes and latest.panes[pane_id]
    if metrics ~= nil and metrics.available then
        return metrics
    end
    metrics = latest.panes and latest.panes[pane.id]
    if metrics ~= nil and metrics.available then
        return metrics
    end
    return latest.system or { cpu_percent = 0, cpu_normalized_percent = 0 }
end

local function cpu_for(metrics)
    return clamp(metrics.cpu_normalized_percent or metrics.cpu_percent or 0, 0, 100)
end

local function memory_label(metrics)
    local bytes = metrics.memory_bytes or metrics.memory_used_bytes
    if bytes == nil then
        return nil
    end
    local mib = math.floor((bytes / 1048576) + 0.5)
    return string.format("MEM %dM", mib)
end

local function process_count_label(metrics)
    if metrics.process_count == nil then
        return nil
    end
    return string.format("P %d", metrics.process_count)
end

local function label_for(metrics)
    local cpu = cpu_for(metrics)
    local parts = { string.format("CPU %d%%", math.floor(cpu + 0.5)) }
    local memory = memory_label(metrics)
    if memory ~= nil then
        table.insert(parts, memory)
    end
    local processes = process_count_label(metrics)
    if processes ~= nil then
        table.insert(parts, processes)
    end
    return " " .. table.concat(parts, " ") .. " ", cpu
end

local function remember_metrics(event)
    local payload = event.payload or event.snapshot
    if payload == nil then
        return
    end
    latest = payload
    latest.panes = latest.panes or {}
    latest.system = latest.system or { cpu_percent = 0, cpu_normalized_percent = 0 }
end

local function component_entrypoint(message)
    if message.component ~= nil and message.component.entrypoint ~= nil then
        return message.component.entrypoint
    end
    return "all"
end

local function render(message)
    local surfaces = {}
    local entrypoint = component_entrypoint(message)
    for _, pane in ipairs(message.panes or {}) do
        local metrics = pane_metrics(pane)
        local label, cpu = label_for(metrics)
        local r, g, b = heat_color(cpu)
        local glyphs = "single-line"
        local z = 11
        if pane.focused then
            glyphs = "thick"
            z = 14
        elseif cpu >= 80 then
            glyphs = "thick"
            z = 14
        elseif cpu >= 50 then
            glyphs = "rounded"
            z = 12
        end
        local cmds = {}
        if entrypoint == "all" or entrypoint == "border" then
            table.insert(cmds, {
                kind = "semantic_border",
                rect = pane.rect,
                z = z,
                fallback_glyphs = glyphs,
                thickness_px = pane.focused and 3 or 1,
                radius_px = pane.focused and 2 or 0,
                style = { fg = bmux.rgb(r, g, b), bold = pane.focused or cpu >= 50 },
            })
        end
        if entrypoint == "all" or entrypoint == "header" then
            table.insert(cmds, {
                kind = "text",
                col = pane.rect.x + 2,
                row = pane.rect.y,
                z = z + 1,
                text = label,
                style = { fg = bmux.rgb(r, g, b), bold = true },
            })
        end
        surfaces[pane.id] = cmds
    end
    return { surfaces = surfaces }
end

function decorate(message)
    if message.kind == "event" and message.event ~= nil then
        if message.event.source == "bmux.performance/metrics-state" then
            remember_metrics(message.event)
        end
        return nil
    end
    if message.kind == "render" then
        return render(message)
    end
    return nil
end
