# bmux_terminal

Terminal control for bmux: raw mode, size queries, ANSI output commands, and
decoded keyboard, mouse, focus, paste, and resize input on Unix and Windows.

## Output

Commands such as `cursor::MoveTo`, `style::SetForegroundColor`, and
`terminal::EnterAlternateScreen` implement `Command`. Write them with
`queue!` (buffer only) or `execute!` (buffer and flush).

## Input

`event::poll`, `event::read`, and `event::EventStream` share one process-wide
reader. On Unix it reads stdin when stdin is a terminal, otherwise
`/dev/tty`, and reports `SIGWINCH` as `Event::Resize`. On Windows it reads
console input records.
