# local-terminal: the local terminal simulation

Deterministic simulation testing for bmux's plugin-less local terminal (the
`bmux_local_terminal` crate, which the CLI serves on a local socket). Run it
with `cargo sim local-terminal`; see [the simulation README](../README.md).

Each run serves one simulated shell with `serve_attachment` over in-memory
streams, through several attachments: a client says hello with a size, types
lines, sometimes resizes, and detaches at a random point; the last attachment
ends the shell with an exit code. The run's seed decides every interleaving of
the server, the client, the shell and the terminal.

Invariants checked:

1. Every attachment's handshake succeeds.
2. The shell answers every line typed, in order, across detaching and
   reattaching; and what clients receive is, line by line, an in-order part of
   what it wrote (output already sent to a client that detaches without reading
   it is that client's to lose).
3. A resize reaches the shell's terminal.
4. Ending the shell delivers its exit code, empties the server's slot, and
   leaves nothing running.
5. Every simulated thread ends with the run.

A mutation that ignores resize frames is caught at once (invariant 3).
