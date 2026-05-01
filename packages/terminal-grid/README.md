# bmux_terminal_grid

Neutral structured terminal grid primitives for bmux. The crate stores parsed terminal rows, style runs, cursor state, alternate-screen state, and soft-wrap metadata so retained scrollback can be reflowed on resize without replaying raw PTY bytes.
