# macOS named-pasteboard interop

Run `python3 scripts/clipboard/macos/interop.py` in the repository's macOS Nix
shell with a reachable AppKit pasteboard server. It builds only the clipboard
crate's unit-test binary, an independent Objective-C AppKit peer, and temporary
reference probes. Each writer exits normally before a fresh reader starts.
Assertions compare Unicode text or dimensions and all four opaque RGBA pixels,
not encoded container bytes.

The native test injects only the pasteboard lookup. Production still calls
`generalPasteboard` with no runtime override. The probe creates a UUID-named
board, releases it between cases, and never requests a system board. The peer
publishes one image type at a time; in the reverse direction it requests each
advertised format explicitly.

The reference constructors only support the general board. For a safe baseline
the script needs the cached crates.io archives and offline build dependencies
for Arboard 3.7.0 (workspace lock at implementation) and 3.6.1 (historical
format reference). It verifies their SHA-256 pins and substitutes only the
constructor selector/argument in temporary copies. These are **named-board
adaptations**, not runs of unmodified Arboard on the general board. The
reference image lane uses TIFF, the format Arboard reads.

Passing here proves named-board interop and retention after normal writer exit
in this session. It does **not** prove general-board privacy/prompt behavior,
GUI copy/paste gestures, or operation without a GUI pasteboard service. A true
general-board/GUI acceptance test still requires a disposable graphical login or
VM; never run one against an active user's board.
