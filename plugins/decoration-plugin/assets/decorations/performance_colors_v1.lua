-- Versioned, pure color-provider contract. No painting, animation, or host I/O.
local M = { state_channel = "bmux.performance/metrics-state" }
local function clamp(v, lo, hi) return math.max(lo, math.min(hi, v)) end

function M.metrics(snapshot, pane)
    local panes = snapshot.panes or {}
    local metrics = panes[pane.pane_id or pane.id]
    if metrics and metrics.available then return metrics end
    metrics = panes[pane.id]
    if metrics and metrics.available then return metrics end
    return snapshot.system or {}
end

function M.heat(snapshot, pane, settings)
    local metrics = M.metrics(snapshot, pane)
    local cpu = clamp(metrics.cpu_normalized_percent or metrics.cpu_percent or 0, 0, 100)
    local mode = settings["heat-mode"] or "cpu"
    assert(mode == "cpu" or mode == "cpu-memory", "unsupported heat-mode")
    if mode == "cpu" then return cpu end
    local total = (snapshot.system or {}).memory_total_bytes or 0
    if total <= 0 then return cpu end
    local green = tonumber(settings["memory-green-percent"]) or 5
    local yellow = tonumber(settings["memory-yellow-percent"]) or 15
    local orange = tonumber(settings["memory-orange-percent"]) or 30
    local red = tonumber(settings["memory-red-percent"]) or 50
    assert(0 <= green and green < yellow and yellow < orange and orange < red and red <= 100,
        "memory thresholds must increase within 0..100")
    local percent = (metrics.memory_bytes or metrics.memory_used_bytes or 0) / total * 100
    local heat = 0
    if percent > orange then
        heat = 75 + clamp((percent - orange) / (red - orange), 0, 1) * 25
    elseif percent > yellow then
        heat = 50 + (percent - yellow) / (orange - yellow) * 25
    elseif percent > green then
        heat = (percent - green) / (yellow - green) * 50
    end
    return math.max(cpu, heat)
end

function M.color(heat)
    local t = clamp(heat / 100, 0, 1)
    if t < 0.5 then
        local k = t / 0.5
        return 60 + 195 * k, 220 - 50 * k, 90
    end
    local k = (t - 0.5) / 0.5
    return 255, 170 - 95 * k, 90 - 55 * k
end

return M
