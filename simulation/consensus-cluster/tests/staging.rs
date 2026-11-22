//! Snapshot staging admission over the `switchy_fs` POSIX simulator: the same
//! symlink, ancestor, privacy, exclusive-ownership and leftover rules the native
//! backend enforces, now replayable under deterministic simulation.

#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::path::Path;
use std::sync::Arc;

use bmux_cluster_plugin::consensus_wire::SnapshotStaging;
use switchy_fs::simulator::{with_filesystem, CrashPolicy, Filesystem};
use switchy_fs::sync::{create_dir_all, set_permissions, symlink, write, Permissions};

const QUOTA: u64 = 1024 * 1024;

fn private_dir(path: &str) {
    create_dir_all(path).unwrap();
    set_permissions(path, Permissions::from_mode(0o700)).unwrap();
}

fn open(path: &str) -> std::io::Result<Arc<SnapshotStaging>> {
    SnapshotStaging::open(Path::new(path), QUOTA, QUOTA)
}

#[test]
fn staging_is_private_exclusive_and_rejects_untrusted_paths() {
    let filesystem = Arc::new(Filesystem::new());
    with_filesystem(&filesystem, || {
        private_dir("/node/staging");

        let owner = open("/node/staging").unwrap();
        // One admission authority per directory while the owner lives.
        assert!(open("/node/staging").is_err());
        drop(owner);
        drop(open("/node/staging").unwrap());

        // Readable by others: not private.
        private_dir("/node/public");
        set_permissions("/node/public", Permissions::from_mode(0o755)).unwrap();
        assert!(open("/node/public").is_err());

        // A world-writable ancestor without the sticky bit is untrusted. The
        // simulator never sets that bit (nor models its semantics), so simulated
        // staging must live under a private directory chain.
        private_dir("/shared/staging");
        set_permissions("/shared", Permissions::from_mode(0o777)).unwrap();
        assert!(open("/shared/staging").is_err());

        // Symbolic links are never followed, at any component.
        symlink("/node", "/alias").unwrap();
        assert!(open("/alias/staging").is_err());
        assert!(open("relative/staging").is_err());

        // Unknown leftovers are preserved and block admission.
        write("/node/staging/unknown", b"preserve").unwrap();
        assert!(open("/node/staging").is_err());
        assert_eq!(
            switchy_fs::sync::read("/node/staging/unknown").unwrap(),
            b"preserve"
        );
    });
}

#[test]
fn staging_owner_survives_power_loss() {
    let filesystem = Arc::new(Filesystem::new());
    let crashed = with_filesystem(&filesystem, || {
        private_dir("/node/staging");
        switchy_fs::sync::sync_directory("/node").unwrap();
        switchy_fs::sync::sync_directory("/").unwrap();
        open("/node/staging").unwrap()
    });
    // A crash releases the lock with the process, and the durable marker lets
    // the restarted owner recover the directory.
    filesystem.crash(&CrashPolicy::drop_all_pending());
    with_filesystem(&filesystem, || {
        let owner = open("/node/staging").unwrap();
        assert!(open("/node/staging").is_err());
        drop(owner);
    });
    drop(crashed);
}
