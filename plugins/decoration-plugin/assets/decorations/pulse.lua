-- Pulse owns animation. Optional versioned providers own color policy.
local latest = {}
local history = {}
local providers = {}

local function number(settings, key, default, minimum, maximum)
    local value = tonumber(settings[key]) or default
    assert(value >= minimum and value <= maximum, "invalid pulse setting: " .. key)
    return value
end

local function render(message)
    local settings = (message.component or {}).settings or {}
    local source = settings["color-source"] or "fixed"
    local provider = nil
    if source ~= "fixed" then
        if not providers[source] then providers[source] = bmux.module(source) end
        provider = providers[source]
    end
    local period = number(settings, "period-ms", 2000, 1, 60000)
    local smoothing = number(settings, "smoothing-ms", 0, 0, 60000)
    local dim = number(settings, "brightness-min", 0.6, 0, 1)
    local bright = number(settings, "brightness-max", 1, dim, 1)
    local now = message.time_ms or 0
    local t = 0.5 + 0.5 * math.sin((now % period) / period * 2 * math.pi)
    local surfaces, next_history = {}, {}
    for _, pane in ipairs(message.panes or {}) do
        if pane.focused or settings["all-panes"] == "true" then
            local r, g, b
            local snapshot = provider and latest[provider.state_channel]
            if provider and snapshot ~= nil then
                local heat = provider.heat(snapshot, pane, settings)
                local id = pane.pane_id or pane.id
                local previous = history[id]
                if previous and smoothing > 0 then
                    local alpha = 1 - math.exp(-math.max(0, now - previous.time_ms) / smoothing)
                    heat = previous.heat + (heat - previous.heat) * alpha
                end
                next_history[id] = { heat = heat, time_ms = now }
                r, g, b = provider.color(heat)
                local brightness = dim + (bright - dim) * t
                r, g, b = r * brightness, g * brightness, b * brightness
            else
                -- No provider snapshot: preserve the standalone demo colors.
                r, g, b = 57 * (1 - t), 255, 20 + 180 * t
            end
            surfaces[pane.id] = {{
                kind = "box_border", rect = pane.rect, z = 10, glyphs = "thick",
                style = { fg = bmux.rgb(math.floor(r), math.floor(g), math.floor(b)), bold = true },
            }}
        end
    end
    history = next_history
    return { surfaces = surfaces }
end

function decorate(message)
    if message.kind == "event" and message.event ~= nil then
        -- Channel access is explicitly granted by the composing theme.
        if message.event.payload ~= nil then
            latest[message.event.source] = message.event.payload
        end
    elseif message.kind == "render" then
        return render(message)
    end
    return nil
end
