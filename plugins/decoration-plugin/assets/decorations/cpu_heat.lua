-- CPU heat decoration -- colors pane borders from performance metrics.

local latest = { panes = {}, system = { cpu_percent = 0 } }

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

local function pane_cpu(pane)
    local pane_metrics = latest.panes and latest.panes[pane.id]
    if pane_metrics ~= nil and pane_metrics.available then
        return pane_metrics.cpu_normalized_percent or pane_metrics.cpu_percent or 0
    end
    if latest.system ~= nil then
        return latest.system.cpu_normalized_percent or latest.system.cpu_percent or 0
    end
    return 0
end

local function remember_metrics(event)
    local payload = event.payload or event.snapshot
    if payload == nil then
        return
    end
    latest = payload
    latest.panes = latest.panes or {}
    latest.system = latest.system or { cpu_percent = 0 }
end

local function render(message)
    local surfaces = {}
    for _, pane in ipairs(message.panes or {}) do
        local cpu = clamp(pane_cpu(pane), 0, 100)
        local r, g, b = heat_color(cpu)
        local glyphs = "single-line"
        local z = 11
        if cpu >= 80 then
            glyphs = "thick"
            z = 14
        elseif cpu >= 50 then
            glyphs = "rounded"
            z = 12
        end

        local cmds = {
            {
                kind = "box_border",
                rect = pane.rect,
                z = z,
                glyphs = glyphs,
                style = {
                    fg = bmux.rgb(r, g, b),
                    bold = cpu >= 50,
                },
            },
        }

        if pane.focused or cpu >= 70 then
            local label = string.format(" CPU %d%% ", math.floor(cpu + 0.5))
            table.insert(cmds, {
                kind = "text",
                col = pane.rect.x + 2,
                row = pane.rect.y,
                z = z + 1,
                text = label,
                style = {
                    fg = bmux.rgb(r, g, b),
                    bold = true,
                },
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
