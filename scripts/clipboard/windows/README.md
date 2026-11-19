# Windows clipboard lane (native x64 MSVC)

Copy this directory **and** `packages/clipboard/examples/interop_peer.rs` from
the candidate. Use two disposable, trusted E: checkouts: the pinned shipping
source with its shipping lockfile, and the actual candidate with its candidate
lockfile. The script copies the shared scenario into a fresh example in each
tree, adds only a fail-closed station guard and a `verify-only` operation, and
removes the temporary source. It does not patch either clipboard implementation.
Do not point both inputs at the same source. Record both revisions, candidate
diff, and lockfile hashes.

## Operator preflight

1. From the locked flake, obtain the expected release **before** selecting a compiler: `nix eval --raw .#rustVersion.x86_64-linux` (the same selector CI uses). Use the already provisioned native `x86_64-pc-windows-msvc` toolchain's real `bin` directory on E:, not a rustup proxy or floating channel. No tool, target, SDK, or package installation is part of this lane.
2. In an **unelevated** existing x64 MSVC tools environment, inspect the following read-only output. Match the physical device (model/serial/bus), volume ID and disk ID with the intended external disk. A label alone does not establish identity. Stop if ambiguous, system/boot, same physical disk as Windows, or if the volume has insufficient space.

   ```powershell
   Get-Volume -DriveLetter E | Format-List DriveLetter,FileSystemLabel,UniqueId,SizeRemaining
   Get-Partition -DriveLetter E | Get-Disk | Format-List Number,FriendlyName,SerialNumber,UniqueId,BusType,IsSystem,IsBoot
   Get-Partition -DriveLetter ($env:SystemDrive.Substring(0,1)) | Get-Disk | Format-List Number,UniqueId
   ```

3. Start in the verified volume, set **process-local** `CARGO_HOME`, `RUSTUP_HOME`, `TEMP`, and `TMP` to existing E: directories (`CARGO_HOME` must already contain all frozen dependencies). Use vetted checkout/cache trees: no nested junctions, redirected config, compiler wrappers, linked worktrees with metadata on C:, environment/config target overrides, redirected linker/PDB caches, or build scripts writing elsewhere. The path guard checks the specified paths and ancestors; it is not a filesystem sandbox. Use an existing E: log directory if capturing stdout/stderr. No personal clipboard reads, administrator access, settings changes, or UI forwarding are necessary.

## Build and run

Example invocation from PowerShell; substitute the verified paths and
**recorded** IDs and release. `Output` must not exist yet. Use a fresh process
so the build environment changes expire on exit.

```powershell
Set-Location E:\bmux
$env:CARGO_HOME = 'E:\bmux\cargo-home'
$env:RUSTUP_HOME = 'E:\bmux\rustup-home'
$env:TEMP = 'E:\bmux\tmp'; $env:TMP = $env:TEMP
powershell.exe -NoProfile -NonInteractive -File E:\bmux\candidate\scripts\clipboard\windows\run.ps1 `
  -Shipping E:\bmux\shipping -Candidate E:\bmux\candidate -Output E:\bmux\run-001 `
  -VolumeId '<recorded volume UniqueId>' -DiskId '<recorded disk UniqueId>' `
  -RustVersion '<locked release>' -ToolchainBin 'E:\bmux\rustup-home\toolchains\<locked-toolchain>\bin'
if ($LASTEXITCODE -ne 0) { throw "Windows lane failed: $LASTEXITCODE" }
```

Add `-BuildOnly` to build both peers and the launcher and run just the
candidate's clipboard-free Windows codec unit tests. This is also the fallback
when station creation is unavailable. In the same verified environment, rerun
only the GUI matrix using:

```powershell
Set-Location E:\bmux\run-001
.\lane.exe run E:\bmux\run-001\shipping.exe E:\bmux\run-001\candidate.exe
if ($LASTEXITCODE -ne 0) { throw 'Windows clipboard matrix failed' }
```

Do **not** run an unguarded peer or `tests/interop.py` on the ordinary station.
The launcher creates a random create-only window station plus desktop, restores
the launcher's station, and specifies `station\desktop` explicitly for **each**
child. Only stdin/stdout pipes are inherited. Each child checks the actual
process station and calling-thread desktop and rejects visible stations before
invoking any clipboard API. The launcher never opens a clipboard, never switches
the visible desktop, and holds the station and desktop until all readers and
writers have finished. PATH is removed from the peers so a failed native write
cannot silently hand off to `clip.exe`. This measures the library backend, not
command fallback.

## Interpret the output

The shipping↔independent legs run first, then candidate↔independent and
shipping↔candidate. Each direction covers empty text, repeated Unicode text
(including a supplementary character, combining mark and LF), and a non-square
37×29 pixel fixture. The independent peer uses CF_UNICODETEXT and CF_DIBV5;
pixel comparison is directional and includes every alpha byte (opaque fixture).
The reader checks while the writer is alive, then checks again in a fresh
process after normal writer exit. Individual transfer failures are reported, not
baseline-adjusted. The final held-open test requires a bounded candidate write
failure, then successful write and independent readback after release. The
independent peer caps accepted buffers at 1 MiB; waits are bounded to 15 s per
phase. Nonzero exit, missing matrix lines, or no private station means no
acceptance claim. This is not a 256 MiB stress test, a leak-rate measurement, an
exact retry timing measurement, or a test of desktop gestures/clipboard history.
