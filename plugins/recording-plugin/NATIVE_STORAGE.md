# Recording native storage

`platform.rs` selects complete Unix and Windows storage implementations. Shared
recording code must not open directories as ordinary files to flush them.

Unix commits flushed file data, renames, and synchronizes the containing
directory. Windows commits flushed file data, closes the staging handle, and
publishes with `MoveFileExW(MOVEFILE_WRITE_THROUGH)`. Exclusive publication
omits
`MOVEFILE_REPLACE_EXISTING`; destination collisions never replace existing data.
Windows has no supported POSIX directory-fsync equivalent: `sync_directory`
validates the directory, while namespace durability is established at creation
and publication by write-through moves. It is not a substitute for flushing
file contents. Errors from file flushes and publication propagate before an
acknowledgement. Neither backend promises recovery from hardware that lies about
flush completion.

Windows files and directories are created with a protected current-user ACL;
directory ACEs inherit only to that user's children. Existing private objects
are checked for owner, ACL, object type, and reparse-point substitution. Parent
paths may be readable, but not writable by unprivileged other users. Validation
never silently repairs public Windows state. Filesystem/ACL errors remain
blocking and carry operation/path context at durable metadata boundaries.

Focused cross-platform tests exercise replacement, destination collision,
private recursive directory creation, and failure diagnostics. Native Windows
recorded-process acceptance is additionally required; Linux tests alone cannot
establish Windows filesystem or process behavior.
