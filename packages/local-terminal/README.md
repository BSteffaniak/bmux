# bmux_local_terminal

The server side of bmux's plugin-less local terminal: one shell on a
pseudo-terminal, served to whichever client attaches over a stream. A client
says hello with its terminal size, then sends input and resize frames; the
server sends output frames and, when the shell ends, its exit status.
Detaching leaves the shell and its queued output for the next attachment.

The bmux CLI serves it on a local socket; the simulation in
`simulation/local-terminal` serves it over an in-memory stream on simulated
terminals and processes.
