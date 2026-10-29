# Windows installed recording acceptance

Run by the coordinator on Windows, against disposable copies of the release
installation (including its artifact inventory), never a live user runtime.
Do not substitute a PATH helper or a Cargo-built test helper. Capture
executable,
helper, and inventory hashes plus effective slot and runtime paths for every
case.

For each case allocate a fresh owner-private root with independent config,
runtime,
data, state, logs, and TEMP/TMP. Invoke the absolute installed `bmux-<slot>.exe`
with a matching explicit slots manifest. Include a dotted slot, e.g.
`release.1`,
whose TOML table is `[slots."release.1"]`. Use native `cmd.exe /D /C` as target.

1. **Baseline:** Run a target printing a unique marker and exiting 0, then another
   exiting 7. Require exact output, actual exit codes, drained capture, terminal
   session end, finalized recording, and acknowledged cleanup. Re-read status
   offline after explicit server cleanup. Do not equate service transport success
   with successful target launch.
2. **Missing helper:** In a separate installation copy remove only its declared
   helper before starting any server. Require explicit helper-unavailable failure
   with retained runtime identity. A target that would create a sentinel must not
   run. No direct-shell fallback, relaunch, or implicit cleanup of uncertain work.
3. **Tampered helper:** Change one helper byte without changing its inventory hash.
   Require hash-validation failure before helper/target execution, no sentinel,
   and no finalized/successful recording claim.
4. **Wrong slot:** Supply slot A's CLI with slot B's helper/inventory identity in
   a disposable installation. Require slot/artifact validation failure, not
   borrowing B's helper or connecting to B's runtime. Keep B's owned server
   running as a control; confirm its PID, runtime identity and state are unchanged.
   A separately valid helper at an undeclared location must not rescue the case.
5. **Persistence:** Repeat short successful runs while bounded status/output
   readers are active. Require capture metadata replacement through EOF under
   the existing private DACL; include an open metadata reader permitting delete
   sharing, and a negative reader denying it that must produce an honest retained
   error. Never solve replacement errors by broadening ACLs or claiming EOF from
   a discarded temporary file. Replacing a destination must retain a valid
   complete prior record or publish the complete new record, never truncate it.

Retain stdout/stderr, launch/reconciliation receipts, capture boundaries, ACLs,
artifact hashes and exit status for each case. Failed cases must identify the
original error, not just a later missing pane. Stop on unknown ownership or
identity mismatch. Cleanup requires explicit coordinator authorization and real
acknowledgment; do not use PID guesses or delete uncertain runtime roots.

These are acceptance requirements, not a claim that the Windows matrix passed.
The recording lane has read-only SSH authority; the coordinator owns
installation
mutations, Windows builds, execution and replay.
