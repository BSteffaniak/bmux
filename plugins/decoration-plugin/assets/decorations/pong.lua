-- Pong decoration -- a tiny fixed-step Pong simulation around each focused pane.
-- Paddles are imperfect controllers: they react late, predict the intercept,
-- move with bounded speed, and miss naturally as the ball speeds up.

local RALLY_MS = 2600
local WIN_HOLD_MS = 3000
local WIN_SCORE = 10
local PADDLE_SIZE = 5
local PADDLE_HIT_PADDING = 1.0
-- Options: "alternate", "scorer", "scored_on", "random".
local SERVE_DIRECTION_MODE = "alternate"
local STEP_MS = 40
local SPEEDUP_PER_HIT = 1.09
local MAX_SPEED_MULT = 3.05
local pane_states = {}

local function pane_state(pane)
    local key = pane.id or "default"
    if pane_states[key] == nil then
        pane_states[key] = { last_focus_ms = nil, active_ms = 0 }
    end
    return pane_states[key]
end

local function put(cmds, col, row, z, text, r, g, b, bold, dim)
    table.insert(cmds, {
        kind = "text",
        col = col,
        row = row,
        z = z,
        text = text,
        style = { fg = bmux.rgb(r, g, b), bold = bold or false, dim = dim or false },
    })
end

local function clamp(value, min_value, max_value)
    return math.max(min_value, math.min(max_value, value))
end

local function component_setting_number(message, key, default_value)
    local component = message.component or {}
    local settings = component.settings or {}
    local value = tonumber(settings[key])
    if value == nil or value <= 0 then
        return default_value
    end
    return value
end

local function pane_seed(pane)
    local text = tostring(pane.id or "default")
    local hash = 5381
    for i = 1, #text do
        hash = (hash * 33 + string.byte(text, i)) % 1000003
    end
    return hash
end

local function rand01(seed, n, salt)
    local x = math.sin(seed * 12.9898 + n * 78.233 + salt * 37.719) * 43758.5453
    return x - math.floor(x)
end

local function rand_range(seed, n, salt, min_value, max_value)
    return min_value + rand01(seed, n, salt) * (max_value - min_value)
end

local function move_toward(current, target, max_delta)
    if current < target then
        return math.min(current + max_delta, target)
    end
    return math.max(current - max_delta, target)
end

local function reflected_y(y, vy, travel_ms, h)
    if h <= 1 then
        return 0
    end
    local period = (h - 1) * 2
    local projected = (y + vy * travel_ms) % period
    if projected > h - 1 then
        return period - projected
    end
    return projected
end

local function new_player(seed, side)
    local salt = side == "left" and 100 or 200
    return {
        center = 0,
        prev_center = 0,
        target = 0,
        swing = 0,
        speed = rand_range(seed, salt, 1, 0.0115, 0.0155),
        recovery_speed = rand_range(seed, salt, 2, 0.0065, 0.0095),
        reaction_ms = rand_range(seed, salt, 3, 45, 95),
        accuracy = rand_range(seed, salt, 4, 0.86, 0.97),
        base_lookahead_ms = rand_range(seed, salt, 5, 320, 520),
        max_lookahead_ms = rand_range(seed, salt, 6, 780, 1150),
        confidence = rand_range(seed, salt, 7, 0.58, 0.82),
        manual = false,
        last_vx = 0,
        direction_changed_at = 0,
    }
end

local function new_game(seed, w, h, rally_ms)
    local center_y = (h - 1) / 2
    local base_vx = math.max(0.004, (w - 1) / math.max(900, rally_ms * 0.82))
    local game = {
        seed = seed,
        w = w,
        h = h,
        paddle_len = math.min(PADDLE_SIZE, math.max(1, h)),
        x = (w - 1) / 2,
        y = center_y,
        vx = base_vx,
        vy = 0,
        base_vx = base_vx,
        speed_mult = 1.0,
        left_score = 0,
        right_score = 0,
        rally = 0,
        hits = 0,
        now_ms = 0,
        win = false,
        winner = nil,
        win_started_ms = nil,
        left = new_player(seed, "left"),
        right = new_player(seed, "right"),
    }
    game.left.center = center_y
    game.left.prev_center = center_y
    game.left.target = center_y
    game.right.center = center_y
    game.right.prev_center = center_y
    game.right.target = center_y
    return game
end

local function serve_direction(game, scorer)
    if SERVE_DIRECTION_MODE == "alternate" then
        return game.rally % 2 == 0 and -1 or 1
    elseif SERVE_DIRECTION_MODE == "scorer" then
        return scorer == "left" and 1 or -1
    elseif SERVE_DIRECTION_MODE == "scored_on" then
        return scorer == "left" and -1 or 1
    end

    return rand01(game.seed, game.rally, 11) < 0.5 and -1 or 1
end

local function serve(game, scorer)
    game.rally = game.rally + 1
    game.hits = 0
    game.speed_mult = 1.0
    game.x = (game.w - 1) / 2
    game.y = rand_range(game.seed, game.rally, 10, 0, math.max(0, game.h - 1))
    local dir = serve_direction(game, scorer)
    game.vx = game.base_vx * dir
    game.vy = rand_range(game.seed, game.rally, 12, -0.0062, 0.0062)
    if math.abs(game.vy) < 0.0026 then
        game.vy = game.vy < 0 and -0.0026 or 0.0026
    end
    game.left.last_vx = game.vx
    game.right.last_vx = game.vx
    game.left.direction_changed_at = game.now_ms
    game.right.direction_changed_at = game.now_ms

    -- On a serve, a real player reads the direction early, but not perfectly.
    -- Seed a rough receiving zone and let normal dynamic lookahead refine it.
    local receiver = dir < 0 and game.left or game.right
    local side_x = dir < 0 and 0 or (game.w - 1)
    local travel_ms = math.abs((side_x - game.x) / game.vx)
    local predicted = reflected_y(game.y, game.vy, travel_ms, game.h)
    if not receiver.manual then
        local center_y = (game.h - 1) / 2
        local uncertainty = rand_range(game.seed, game.rally, 91, -1.2, 1.2)
        receiver.target = clamp(predicted * 0.45 + game.y * 0.25 + center_y * 0.30 + uncertainty, 0, game.h - 1)
    end
end

local function dynamic_prediction(game, player, side, travel_ms)
    local pressure = clamp(game.speed_mult / MAX_SPEED_MULT, 0.0, 1.0)
    local closeness = 1.0 - clamp(travel_ms / 1500.0, 0.0, 1.0)
    local lookahead = player.base_lookahead_ms
        + (player.max_lookahead_ms - player.base_lookahead_ms) * (0.25 * pressure + 0.75 * closeness)
    local horizon_ms = math.min(travel_ms, lookahead)
    local horizon_y = reflected_y(game.y, game.vy, horizon_ms, game.h)
    local intercept_y = reflected_y(game.y, game.vy, travel_ms, game.h)
    local confidence = clamp(player.confidence * (0.25 + closeness * 0.75), 0.0, 0.92)
    local center_y = (game.h - 1) / 2
    local rough = horizon_y * (1.0 - confidence) + intercept_y * confidence
    local center_bias = (1.0 - confidence) * 0.25
    local error_mag = (1.0 - player.accuracy) * (0.9 + pressure * 2.6 + (1.0 - closeness) * 2.2)
    local error = rand_range(game.seed, game.rally + game.hits, side == "left" and 301 or 401, -error_mag, error_mag)
    return clamp(rough * (1.0 - center_bias) + center_y * center_bias + error, 0, game.h - 1), closeness, pressure
end

local function update_player_target(game, player, side)
    local center_y = (game.h - 1) / 2
    local incoming = (side == "left" and game.vx < 0) or (side == "right" and game.vx > 0)
    if player.last_vx * game.vx <= 0 then
        player.direction_changed_at = game.now_ms
    end
    player.last_vx = game.vx

    if not incoming then
        -- No exact future knowledge while the ball is moving away. Shade toward
        -- a broad likely return lane using short lookahead and a strong center
        -- bias, then refine only when the ball actually comes back.
        local horizon_ms = math.min(player.base_lookahead_ms * 0.85, math.abs((game.w - 1) / game.vx))
        local lane_y = reflected_y(game.y, game.vy, horizon_ms, game.h)
        local noise = rand_range(game.seed, game.rally + game.hits, side == "left" and 331 or 431, -0.8, 0.8)
        player.target = clamp(lane_y * 0.38 + center_y * 0.62 + noise, 0, game.h - 1)
        return player.recovery_speed * 0.95
    end
    local reaction_ms = game.hits == 0 and player.reaction_ms * 0.20 or player.reaction_ms
    if game.now_ms - player.direction_changed_at < reaction_ms then
        return player.speed * 0.85
    end

    local side_x = side == "left" and 0 or (game.w - 1)
    local travel_ms = math.abs((side_x - game.x) / game.vx)
    local predicted, closeness, pressure = dynamic_prediction(game, player, side, travel_ms)
    local swing_window = travel_ms < 360
    local swing_mag = (0.16 + pressure * 0.48) * (game.hits < 2 and 0.25 or 1.0) * (0.45 + closeness * 0.55)
    if swing_window then
        player.swing = rand_range(game.seed, game.rally + game.hits, side == "left" and 801 or 901, -swing_mag, swing_mag)
    else
        player.swing = player.swing * 0.78
    end
    player.target = clamp(predicted + player.swing, 0, game.h - 1)
    return player.speed * (game.hits < 2 and 1.25 or 1.0) * (0.85 + closeness * 0.35)
end

local function move_player(game, player, side)
    if player.manual then
        player.prev_center = player.center
        player.center = clamp(player.target, 0, game.h - 1)
        return
    end
    local speed = update_player_target(game, player, side)
    local distance = math.abs(player.target - player.center)
    local boost = 0.85
    if distance > game.paddle_len then
        boost = 1.35
    elseif distance > 1.0 then
        boost = 1.12
    end
    player.prev_center = player.center
    player.center = move_toward(player.center, player.target, speed * boost * STEP_MS)
    player.center = clamp(player.center, 0, game.h - 1)
end

local function paddle_contains(game, player, y)
    local max_top = math.max(0, game.h - game.paddle_len)
    local top = clamp(math.floor(player.center - game.paddle_len / 2 + 0.5), 0, max_top)
    return y >= top - PADDLE_HIT_PADDING and y <= top + game.paddle_len - 1 + PADDLE_HIT_PADDING
end

local function bounce(game, player, side)
    local max_top = math.max(0, game.h - game.paddle_len)
    local top = clamp(math.floor(player.center - game.paddle_len / 2 + 0.5), 0, max_top)
    local rendered_center = top + (game.paddle_len - 1) / 2
    local half = math.max(0.5, (game.paddle_len - 1) / 2)
    local rel = clamp((game.y - rendered_center) / half, -1.0, 1.0)
    local incoming_vy = game.vy
    local paddle_vy = (player.center - (player.prev_center or player.center)) / STEP_MS
    local aim_jitter = rand_range(game.seed, game.rally + game.hits, side == "left" and 501 or 601, -0.0014, 0.0014)
    game.x = side == "left" and 0 or (game.w - 1)
    game.vx = math.abs(game.vx) * (side == "left" and 1 or -1)
    game.hits = game.hits + 1
    game.speed_mult = math.min(MAX_SPEED_MULT, game.speed_mult * SPEEDUP_PER_HIT)
    game.vx = game.base_vx * game.speed_mult * (game.vx < 0 and -1 or 1)

    -- A moving paddle imparts "spin". This keeps well-centered hits from
    -- degenerating into flat horizontal rallies and makes aggressive last-
    -- moment corrections both powerful and risky.
    local spin_vy = paddle_vy * (0.55 + game.speed_mult * 0.28)
    local next_vy = incoming_vy * 0.32 + rel * (0.0045 + game.speed_mult * 0.0033) + spin_vy + aim_jitter
    local min_angle_vy = math.abs(game.vx) * 0.26
    local max_angle_vy = math.abs(game.vx) * 0.95
    if math.abs(next_vy) < min_angle_vy then
        local fallback_sign = next_vy < 0 and -1 or 1
        if next_vy == 0 then
            fallback_sign = rand01(game.seed, game.rally + game.hits, 701) < 0.5 and -1 or 1
        end
        next_vy = fallback_sign * rand_range(game.seed, game.rally + game.hits, 702, min_angle_vy, min_angle_vy * 1.45)
    end
    game.vy = clamp(next_vy, -max_angle_vy, max_angle_vy)
end

local function score(game, scorer)
    if scorer == "left" then
        game.left_score = game.left_score + 1
    else
        game.right_score = game.right_score + 1
    end
    if game.left_score >= WIN_SCORE or game.right_score >= WIN_SCORE then
        game.win = true
        game.winner = game.left_score >= WIN_SCORE and "left" or "right"
        game.win_started_ms = game.now_ms
        return
    end
    serve(game, scorer)
end

local function step_game(game)
    if game.win then
        return
    end

    move_player(game, game.left, "left")
    move_player(game, game.right, "right")

    game.x = game.x + game.vx * STEP_MS
    game.y = game.y + game.vy * STEP_MS

    if game.y < 0 then
        game.y = -game.y
        game.vy = -game.vy
    elseif game.y > game.h - 1 then
        game.y = (game.h - 1) - (game.y - (game.h - 1))
        game.vy = -game.vy
    end

    if game.x <= 0 then
        if game.vx < 0 and paddle_contains(game, game.left, game.y) then
            bounce(game, game.left, "left")
        else
            score(game, "right")
        end
    elseif game.x >= game.w - 1 then
        if game.vx > 0 and paddle_contains(game, game.right, game.y) then
            bounce(game, game.right, "right")
        else
            score(game, "left")
        end
    end
end

local function start_game(seed, w, h, rally_ms, now_ms)
    local game = new_game(seed, w, h, rally_ms)
    game.now_ms = now_ms
    serve(game, nil)
    return game
end

local function simulation_cache_key(pane, w, h, rally_ms)
    return tostring(pane.id or "default") .. ":" .. tostring(w) .. "x" .. tostring(h) .. ":" .. tostring(rally_ms)
end

local function simulate(pane, active_ms, rally_ms, win_hold_ms)
    local content = pane.content_rect
    local w = math.max(2, content.w)
    local h = math.max(1, content.h)
    local seed = pane_seed(pane)
    local state = pane_state(pane)
    local cache_key = simulation_cache_key(pane, w, h, rally_ms)

    if state.game == nil or state.game_key ~= cache_key or active_ms < (state.game_active_ms or 0) then
        state.game = start_game(seed, w, h, rally_ms, 0)
        state.game_key = cache_key
        state.game_active_ms = 0
    end

    local game = state.game
    local simulated_ms = state.game_active_ms or 0
    while simulated_ms + STEP_MS <= active_ms do
        simulated_ms = simulated_ms + STEP_MS
        game.now_ms = simulated_ms
        if game.win then
            if game.now_ms - game.win_started_ms >= win_hold_ms then
                game = start_game(seed, w, h, rally_ms, simulated_ms)
                state.game = game
            end
        else
            step_game(game)
        end
    end
    state.game_active_ms = simulated_ms
    return game
end

local function paddle_top(game, player)
    local max_top = math.max(0, game.h - game.paddle_len)
    return clamp(math.floor(player.center - game.paddle_len / 2 + 0.5), 0, max_top)
end

local function render_ball(cmds, pane, game)
    if game.win then
        return
    end
    put(cmds, pane.content_rect.x + clamp(math.floor(game.x + 0.5), 0, game.w - 1), pane.content_rect.y + clamp(math.floor(game.y + 0.5), 0, game.h - 1), 0, "●", 95, 175, 255, false, true)
end

local function render_paddles(cmds, pane, game)
    local rect = pane.rect
    local content = pane.content_rect
    if rect.h < 3 then
        return
    end
    local left_top = paddle_top(game, game.left)
    local right_top = paddle_top(game, game.right)
    for i = 0, game.paddle_len - 1 do
        put(cmds, rect.x, content.y + left_top + i, 20, "▌", 255, 95, 215, true, false)
        put(cmds, rect.x + rect.w - 1, content.y + right_top + i, 20, "▐", 95, 255, 255, true, false)
    end
end

local function render_score(cmds, pane, game)
    local rect = pane.rect
    local score_text = tostring(game.left_score) .. " : " .. tostring(game.right_score)
    local col = rect.x + math.max(1, math.floor((rect.w - #score_text) / 2))
    put(cmds, col, rect.y, 30, score_text, 255, 215, 95, true, false)
    if game.win then
        local label = string.upper(game.winner) .. " WINS"
        local label_col = rect.x + math.max(1, math.floor((rect.w - #label) / 2))
        local label_row = rect.y + math.max(1, math.floor(rect.h / 2))
        put(cmds, label_col, label_row, 31, label, 255, 255, 255, true, false)
    end
end

local function render_pane(pane, message)
    local state = pane_state(pane)
    if not pane.focused then
        state.last_focus_ms = nil
        return {}
    end
    if state.last_focus_ms ~= nil then
        state.active_ms = state.active_ms + math.max(0, message.time_ms - state.last_focus_ms)
    end
    state.last_focus_ms = message.time_ms

    if pane.rect.w < 6 or pane.rect.h < 4 then
        return {}
    end

    local entrypoint = message.component and message.component.entrypoint or "all"
    local rally_ms = component_setting_number(message, "rally_ms", RALLY_MS)
    local win_hold_ms = component_setting_number(message, "win_hold_ms", WIN_HOLD_MS)
    local game = simulate(pane, state.active_ms, rally_ms, win_hold_ms)
    local cmds = {}
    if entrypoint == "ball" or entrypoint == "all" then render_ball(cmds, pane, game) end
    if entrypoint == "paddles" or entrypoint == "all" then render_paddles(cmds, pane, game) end
    if entrypoint == "score" or entrypoint == "all" then render_score(cmds, pane, game) end
    return cmds
end

local function pane_from_input(input)
    return input.hovered_pane or input.focused_pane
end

local function manual_player_for_side(game, side)
    if side == "left" then return game.left end
    if side == "right" then return game.right end
    return nil
end

local function input_content_row(input, pane, clamp_outside)
    if input.row == nil or pane == nil or pane.content_rect == nil then
        return nil
    end
    local max_row = math.max(0, pane.content_rect.h - 1)
    local row = input.row - pane.content_rect.y
    if clamp_outside then
        return clamp(row, 0, max_row)
    end
    if row < 0 or row > max_row then
        return nil
    end
    return row
end

local function hit_paddle(game, pane, input)
    if input.col == nil or input.row == nil or pane == nil then
        return nil
    end
    local row = input_content_row(input, pane, false)
    if row == nil then return nil end
    local left_top = paddle_top(game, game.left)
    local right_top = paddle_top(game, game.right)
    if input.col == pane.rect.x and row >= left_top and row <= left_top + game.paddle_len - 1 then
        return "left"
    end
    if input.col == pane.rect.x + pane.rect.w - 1 and row >= right_top and row <= right_top + game.paddle_len - 1 then
        return "right"
    end
    return nil
end

local function set_manual_target(game, side, row)
    local player = manual_player_for_side(game, side)
    if player == nil then return end
    player.manual = true
    player.target = clamp(row, 0, game.h - 1)
    player.center = player.target
end

local function handle_input(message)
    local input = message.input or {}
    local pane = pane_from_input(input)
    if pane == nil or pane.pane_id == nil then
        return { consumed = false }
    end
    local state = pane_states[tostring(pane.pane_id)]
    if state == nil or state.game == nil then
        return { consumed = false }
    end
    local game = state.game

    if input.event_kind == "key" then
        local side = state.captured_side
        local player = manual_player_for_side(game, side)
        if player == nil then
            return { consumed = false }
        end
        if input.key == "esc" then
            player.manual = false
            state.captured_side = nil
            return { consumed = true, release_capture = true, dirty = true }
        end
        local delta = input.key == "up" and -1 or (input.key == "down" and 1 or 0)
        if delta == 0 then
            return { consumed = false }
        end
        set_manual_target(game, side, player.target + delta)
        return { consumed = true, capture_keyboard = { "up", "down", "esc" }, dirty = true }
    end

    if input.event_kind ~= "mouse" or input.button ~= "left" then
        return { consumed = false }
    end

    local row = input_content_row(input, pane, input.phase == "drag" or input.phase == "up")
    if row == nil then
        return { consumed = false }
    end

    if input.phase == "down" then
        local side = hit_paddle(game, pane, input)
        if side == nil then
            if state.captured_side ~= nil then
                state.captured_side = nil
                game.left.manual = false
                game.right.manual = false
                return { consumed = false, release_capture = true, dirty = true }
            end
            return { consumed = false }
        end
        state.captured_side = side
        set_manual_target(game, side, row)
        return {
            consumed = true,
            capture_pointer = true,
            capture_keyboard = { "up", "down", "esc" },
            dirty = true,
        }
    end

    if input.phase == "drag" and state.captured_side ~= nil then
        set_manual_target(game, state.captured_side, row)
        return {
            consumed = true,
            capture_pointer = true,
            capture_keyboard = { "up", "down", "esc" },
            dirty = true,
        }
    end

    if input.phase == "up" and state.captured_side ~= nil then
        set_manual_target(game, state.captured_side, row)
        return { consumed = true, capture_keyboard = { "up", "down", "esc" }, dirty = true }
    end

    return { consumed = false }
end

local function render(message)
    local surfaces = {}
    for _, pane in ipairs(message.panes or {}) do
        local commands = render_pane(pane, message)
        if #commands > 0 then
            surfaces[pane.id] = commands
        end
    end
    return { surfaces = surfaces }
end

function decorate(message)
    if message.kind == "render" then
        return render(message)
    elseif message.kind == "input" then
        return handle_input(message)
    end
    return nil
end
