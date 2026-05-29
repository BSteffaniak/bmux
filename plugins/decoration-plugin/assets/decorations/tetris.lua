-- Tetris decoration -- a compact computer-played Tetris board behind pane content.
-- The bot uses a simple placement heuristic plus a little noise so it looks
-- competent without playing perfectly forever.

local BOARD_W = 10
local BOARD_H = 20
local STEP_MS = 115
local GAME_OVER_HOLD_MS = 2200
local MAX_STEPS_PER_RENDER = 18

local pane_states = {}

local PIECES = {
    I = {
        color = { 95, 255, 255 },
        rotations = {
            { {0, 1}, {1, 1}, {2, 1}, {3, 1} },
            { {2, 0}, {2, 1}, {2, 2}, {2, 3} },
        },
    },
    O = {
        color = { 255, 215, 95 },
        rotations = {
            { {1, 0}, {2, 0}, {1, 1}, {2, 1} },
        },
    },
    T = {
        color = { 215, 95, 255 },
        rotations = {
            { {1, 0}, {0, 1}, {1, 1}, {2, 1} },
            { {1, 0}, {1, 1}, {2, 1}, {1, 2} },
            { {0, 1}, {1, 1}, {2, 1}, {1, 2} },
            { {1, 0}, {0, 1}, {1, 1}, {1, 2} },
        },
    },
    S = {
        color = { 95, 255, 135 },
        rotations = {
            { {1, 0}, {2, 0}, {0, 1}, {1, 1} },
            { {1, 0}, {1, 1}, {2, 1}, {2, 2} },
        },
    },
    Z = {
        color = { 255, 95, 95 },
        rotations = {
            { {0, 0}, {1, 0}, {1, 1}, {2, 1} },
            { {2, 0}, {1, 1}, {2, 1}, {1, 2} },
        },
    },
    J = {
        color = { 95, 135, 255 },
        rotations = {
            { {0, 0}, {0, 1}, {1, 1}, {2, 1} },
            { {1, 0}, {2, 0}, {1, 1}, {1, 2} },
            { {0, 1}, {1, 1}, {2, 1}, {2, 2} },
            { {1, 0}, {1, 1}, {0, 2}, {1, 2} },
        },
    },
    L = {
        color = { 255, 175, 95 },
        rotations = {
            { {2, 0}, {0, 1}, {1, 1}, {2, 1} },
            { {1, 0}, {1, 1}, {1, 2}, {2, 2} },
            { {0, 1}, {1, 1}, {2, 1}, {0, 2} },
            { {0, 0}, {1, 0}, {1, 1}, {1, 2} },
        },
    },
}

local PIECE_NAMES = { "I", "O", "T", "S", "Z", "J", "L" }

local function put(cmds, col, row, z, text, color, bold, dim)
    table.insert(cmds, {
        kind = "text",
        col = col,
        row = row,
        z = z,
        text = text,
        style = { fg = bmux.rgb(color[1], color[2], color[3]), bold = bold or false, dim = dim or false },
    })
end

local function component_active(pane)
    if pane.component_active ~= nil then
        return pane.component_active
    end
    return pane.focused
end

local function pane_seed(pane)
    local text = tostring(pane.id or "default")
    local hash = 5381
    for i = 1, #text do
        hash = (hash * 33 + string.byte(text, i)) % 1000003
    end
    return hash
end

local function rand01(state)
    state.rng = (state.rng * 1103515245 + 12345) % 2147483648
    return state.rng / 2147483648
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

local function new_board()
    local board = {}
    for y = 1, BOARD_H do
        board[y] = {}
    end
    return board
end

local function new_state(pane)
    return {
        board = new_board(),
        active = nil,
        next_piece = nil,
        rng = pane_seed(pane),
        last_ms = nil,
        accum_ms = 0,
        game_over_ms = nil,
        lines = 0,
    }
end

local function pane_state(pane)
    local key = pane.id or "default"
    if pane_states[key] == nil then
        pane_states[key] = new_state(pane)
    end
    return pane_states[key]
end

local function reset_state(state)
    state.board = new_board()
    state.active = nil
    state.next_piece = nil
    state.accum_ms = 0
    state.game_over_ms = nil
    state.lines = 0
end

local function board_get(board, x, y)
    if y < 1 or y > BOARD_H or x < 1 or x > BOARD_W then
        return nil
    end
    return board[y][x]
end

local function collides(board, piece_name, rotation_idx, px, py)
    local cells = PIECES[piece_name].rotations[rotation_idx]
    for _, cell in ipairs(cells) do
        local x = px + cell[1] + 1
        local y = py + cell[2] + 1
        if x < 1 or x > BOARD_W or y > BOARD_H then
            return true
        end
        if y >= 1 and board_get(board, x, y) ~= nil then
            return true
        end
    end
    return false
end

local function copy_board(board)
    local copy = new_board()
    for y = 1, BOARD_H do
        for x = 1, BOARD_W do
            copy[y][x] = board[y][x]
        end
    end
    return copy
end

local function lock_piece(board, piece_name, rotation_idx, px, py)
    local piece = PIECES[piece_name]
    for _, cell in ipairs(piece.rotations[rotation_idx]) do
        local x = px + cell[1] + 1
        local y = py + cell[2] + 1
        if y < 1 then
            return false
        end
        if x >= 1 and x <= BOARD_W and y <= BOARD_H then
            board[y][x] = piece.color
        end
    end
    return true
end

local function clear_lines(board)
    local cleared = 0
    local y = BOARD_H
    while y >= 1 do
        local full = true
        for x = 1, BOARD_W do
            if board[y][x] == nil then
                full = false
                break
            end
        end
        if full then
            cleared = cleared + 1
            for yy = y, 2, -1 do
                board[yy] = board[yy - 1]
            end
            board[1] = {}
        else
            y = y - 1
        end
    end
    return cleared
end

local function column_height(board, x)
    for y = 1, BOARD_H do
        if board[y][x] ~= nil then
            return BOARD_H - y + 1
        end
    end
    return 0
end

local function board_stats(board)
    local aggregate_height = 0
    local holes = 0
    local bumpiness = 0
    local prev_height = nil
    for x = 1, BOARD_W do
        local height = column_height(board, x)
        aggregate_height = aggregate_height + height
        if prev_height ~= nil then
            bumpiness = bumpiness + math.abs(prev_height - height)
        end
        prev_height = height

        local seen_block = false
        for y = 1, BOARD_H do
            if board[y][x] ~= nil then
                seen_block = true
            elseif seen_block then
                holes = holes + 1
            end
        end
    end
    return aggregate_height, holes, bumpiness
end

local function choose_piece(state)
    local idx = math.floor(rand01(state) * #PIECE_NAMES) + 1
    return PIECE_NAMES[idx]
end

local function drop_y_for(board, piece_name, rotation_idx, px)
    local y = -3
    while not collides(board, piece_name, rotation_idx, px, y + 1) do
        y = y + 1
    end
    if collides(board, piece_name, rotation_idx, px, y) then
        return nil
    end
    return y
end

local function choose_placement(state, message, piece_name)
    local skill = component_setting_number(message, "skill", 78) / 100.0
    local noise = (1.0 - math.max(0.0, math.min(1.0, skill))) * 26.0 + 1.75
    local choices = {}
    for rotation_idx, _ in ipairs(PIECES[piece_name].rotations) do
        for px = -3, BOARD_W - 1 do
            local py = drop_y_for(state.board, piece_name, rotation_idx, px)
            if py ~= nil then
                local trial = copy_board(state.board)
                if lock_piece(trial, piece_name, rotation_idx, px, py) then
                    local cleared = clear_lines(trial)
                    local aggregate_height, holes, bumpiness = board_stats(trial)
                    local score = cleared * 9.0 - aggregate_height * 0.52 - holes * 3.1 - bumpiness * 0.38
                    score = score + (rand01(state) * 2.0 - 1.0) * noise
                    table.insert(choices, { score = score, x = px, y = py, rotation = rotation_idx })
                end
            end
        end
    end
    table.sort(choices, function(a, b) return a.score > b.score end)
    if #choices == 0 then
        return nil
    end
    -- Occasionally take a near-best move to make the computer fallible.
    local pick_span = math.min(#choices, 1 + math.floor((1.0 - skill) * 5.0))
    local idx = math.floor(rand01(state) * pick_span) + 1
    return choices[idx]
end

local function spawn_piece(state, message)
    local piece_name = state.next_piece or choose_piece(state)
    state.next_piece = choose_piece(state)
    local placement = choose_placement(state, message, piece_name)
    if placement == nil or collides(state.board, piece_name, placement.rotation, placement.x, -3) then
        state.game_over_ms = 0
        state.active = nil
        return
    end
    state.active = {
        name = piece_name,
        rotation = placement.rotation,
        x = placement.x,
        y = -3,
        target_y = placement.y,
    }
end

local function step_game(state, message)
    if state.game_over_ms ~= nil then
        state.game_over_ms = state.game_over_ms + STEP_MS
        if state.game_over_ms >= component_setting_number(message, "game_over_ms", GAME_OVER_HOLD_MS) then
            reset_state(state)
        end
        return
    end

    if state.active == nil then
        spawn_piece(state, message)
        return
    end

    if state.active.y < state.active.target_y then
        state.active.y = state.active.y + 1
        return
    end

    if not lock_piece(state.board, state.active.name, state.active.rotation, state.active.x, state.active.y) then
        state.game_over_ms = 0
        state.active = nil
        return
    end
    state.lines = state.lines + clear_lines(state.board)
    state.active = nil

    for x = 1, BOARD_W do
        if state.board[1][x] ~= nil then
            state.game_over_ms = 0
            return
        end
    end
end

local function simulate(state, message)
    if state.last_ms == nil then
        state.last_ms = message.time_ms
        if state.active == nil then
            spawn_piece(state, message)
        end
        return
    end

    local step_ms = component_setting_number(message, "step_ms", STEP_MS)
    state.accum_ms = state.accum_ms + math.max(0, message.time_ms - state.last_ms)
    state.last_ms = message.time_ms

    local steps = 0
    while state.accum_ms >= step_ms and steps < MAX_STEPS_PER_RENDER do
        state.accum_ms = state.accum_ms - step_ms
        step_game(state, message)
        steps = steps + 1
    end
    if steps >= MAX_STEPS_PER_RENDER then
        state.accum_ms = 0
    end
end

local function board_origin(pane)
    local rect = pane.content_rect or pane.rect
    local visible_h = math.min(BOARD_H, math.max(0, rect.h))
    local origin_x = rect.x + math.max(0, math.floor((rect.w - BOARD_W) / 2))
    local origin_y = rect.y + math.max(0, rect.h - visible_h)
    return rect, origin_x, origin_y, visible_h
end

local function render_cell(cmds, pane, bx, by, color, glyph, bold, dim)
    local rect, origin_x, origin_y, visible_h = board_origin(pane)
    if rect.w < BOARD_W or visible_h <= 0 then
        return
    end
    local top_board_y = BOARD_H - visible_h + 1
    if bx < 1 or bx > BOARD_W or by < top_board_y or by > BOARD_H then
        return
    end
    put(cmds, origin_x + bx - 1, origin_y + by - top_board_y, 2, glyph or "█", color, bold, dim)
end

local function render_board(cmds, pane, state, message)
    local rect, origin_x, origin_y, visible_h = board_origin(pane)
    if rect.w < BOARD_W or visible_h <= 0 then
        return
    end

    local top_board_y = BOARD_H - visible_h + 1
    local flash = state.game_over_ms ~= nil and math.floor((state.game_over_ms or 0) / 130) % 2 == 0
    if flash then
        for by = top_board_y, BOARD_H do
            for bx = 1, BOARD_W do
                put(cmds, origin_x + bx - 1, origin_y + by - top_board_y, 3, "▓", { 255, 255, 255 }, true, false)
            end
        end
        return
    end

    for by = top_board_y, BOARD_H do
        for bx = 1, BOARD_W do
            local color = state.board[by][bx]
            if color ~= nil then
                render_cell(cmds, pane, bx, by, color, "█", true, false)
            elseif by == top_board_y or bx == 1 or bx == BOARD_W then
                render_cell(cmds, pane, bx, by, { 45, 52, 75 }, "·", false, true)
            end
        end
    end

    if state.active ~= nil then
        local piece = PIECES[state.active.name]
        for _, cell in ipairs(piece.rotations[state.active.rotation]) do
            local bx = state.active.x + cell[1] + 1
            local by = state.active.y + cell[2] + 1
            render_cell(cmds, pane, bx, by, piece.color, "█", true, false)
        end
    end

    local label = tostring(state.lines)
    if rect.w >= BOARD_W + #label + 2 then
        put(cmds, origin_x + BOARD_W + 1, origin_y, 2, label, { 135, 255, 215 }, false, true)
    end
end

local function render_pane(pane, message)
    local state = pane_state(pane)
    if not component_active(pane) then
        state.last_ms = nil
        return {}
    end
    if pane.rect.w < 12 or pane.rect.h < 5 then
        return {}
    end

    simulate(state, message)
    local cmds = {}
    render_board(cmds, pane, state, message)
    return cmds
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
    end
    return nil
end
