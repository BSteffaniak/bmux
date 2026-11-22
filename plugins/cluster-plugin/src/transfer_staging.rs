//! Disposable, bounded staging I/O. This module grants no peer or consensus authority.
//! Callers serialize authenticated current-term admission before each operation.
//!
//! All I/O goes through `switchy_fs::sync` capability handles and time through
//! `switchy_async::time`, so staging runs unchanged on the native backend and
//! under deterministic simulation.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use switchy_async::time::Instant;
use switchy_fs::sync::{Dir, File, FileKind, Metadata, OpenOptions};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TransferKey {
    pub peer: [u8; 32],
    pub incarnation: Uuid,
    pub sequence: u64,
}

impl TransferKey {
    /// The staged file name for this transfer. It is derived from the key rather
    /// than drawn at random so simulated runs replay exactly; recovery accepts
    /// exactly this `transfer-<uuid>` shape. Names only need to be distinct
    /// among the eight live transfers of one owner, which a domain-separated
    /// SHA-256 prefix provides; a collision fails exclusive creation safely.
    fn file_name(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(b"bmux transfer staging name v1\0");
        hash.update(self.peer);
        hash.update(self.incarnation.as_bytes());
        hash.update(self.sequence.to_be_bytes());
        let digest = hash.finalize();
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        format!("transfer-{}", Uuid::from_bytes(bytes))
    }
}

#[derive(Debug)]
struct Incoming {
    file: File,
    directory: StagingDirectory,
    name: String,
    binding: [u8; 32],
    total: u64,
    digest: [u8; 32],
    offset: u64,
    last: Option<(u64, usize, [u8; 32])>,
    hash: Sha256,
    progress: Instant,
    sealed: bool,
    failed: bool,
    validation: Arc<std::sync::atomic::AtomicBool>,
}

/// Owns validation I/O while staging retains the byte/count reservation.
/// Dropping a ticket leaves the transfer failed, eligible for explicit cleanup.
pub(crate) struct ValidationTicket {
    file: File,
    total: u64,
    identity: Arc<std::sync::atomic::AtomicBool>,
}

/// A ticket whose canonical validator returned success. This does not grant
/// consensus authority; the executor must revalidate authority before sealing.
pub(crate) struct ValidatedTicket(ValidationTicket);

impl ValidationTicket {
    pub(crate) fn validate(
        mut self,
        validate: impl FnOnce(&mut File, u64) -> io::Result<()>,
    ) -> io::Result<ValidatedTicket> {
        validate(&mut self.file, self.total)?;
        // Retry eligibility is not sealing authority. A retry must validate again.
        self.identity
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(ValidatedTicket(self))
    }
}

/// Owns at most eight temporary files and two per peer. Explicit cleanup retains
/// failed deletions and their reservations so capacity is never falsely released.
#[derive(Debug)]
pub struct TransferStaging {
    transfers: BTreeMap<TransferKey, Incoming>,
    per_peer_bytes: u64,
    total_bytes: u64,
}

/// The exclusive owner of one validated staging directory.
///
/// Clones share one directory handle and one owner lock. The lock is released
/// only when the last clone, held by the executor or an admitted transfer, is
/// dropped.
#[derive(Debug, Clone)]
pub struct StagingDirectory(Arc<Owner>);

#[derive(Debug)]
struct Owner {
    directory: Dir,
    // Holds the exclusive advisory lock that makes this process the single
    // admission authority of the directory. `Dir` has no lock of its own, so
    // the lock lives on the owner marker *inside* the directory rather than on
    // a sibling lock-file path: renaming the directory moves the marker inode
    // with it, so opening the directory under any name still meets the held
    // lock, and a rename can never create a second admission authority. The
    // marker sits in an owner-private directory and staging never unlinks it,
    // so only the owning user could detach the lock from the directory.
    _marker: File,
}

impl StagingDirectory {
    fn dir(&self) -> &Dir {
        &self.0.directory
    }
}

/// Open and validate the same directory handle used for all subsequent I/O.
/// Unknown leftovers require explicit recovery; this never deletes them.
#[cfg(test)]
pub(crate) fn open_directory(path: &Path) -> io::Result<StagingDirectory> {
    let (owner, _) = lock_directory(path)?;
    if owner
        .dir()
        .entries()?
        .iter()
        .any(|entry| entry.name() != OWNER_FILE)
    {
        return Err(io::Error::other(
            "staging directory requires explicit leftover recovery",
        ));
    }
    Ok(owner)
}

/// Take exclusive ownership of the private directory at `path`, creating the
/// owner marker in an empty directory. Returns whether the marker was created.
/// A missing marker never authorizes cleanup; partial markers require repair.
fn lock_directory(path: &Path) -> io::Result<(StagingDirectory, bool)> {
    let directory = open_trusted_path(path)?;
    let metadata = directory.metadata()?;
    if metadata.uid() != Some(switchy_fs::sync::current_uid()?) || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "staging directory must be owned and private",
        ));
    }
    let (marker, created) = match directory.metadata_at(OWNER_FILE) {
        Ok(metadata) => {
            // Check the entry type before opening: switchy_fs has no O_NONBLOCK,
            // and opening a FIFO for reading would block. Only the owning user
            // can replace entries of the private directory in between.
            require_regular(&metadata)?;
            let mut marker = directory
                .open_file_at(OWNER_FILE, &OpenOptions::new().read(true).nofollow(true))?;
            marker.try_lock()?;
            validate_owned_file(&marker.metadata()?)?;
            let mut bytes = Vec::new();
            (&mut marker).take(128).read_to_end(&mut bytes)?;
            if bytes != OWNER_BYTES {
                return Err(io::Error::other("unsupported or corrupt staging marker"));
            }
            (marker, false)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            require_empty(&directory)?;
            // Exclusive creation: of two concurrent initializers only one creates
            // the marker; the other fails here or on the lock below.
            let mut marker = directory.open_file_at(
                OWNER_FILE,
                &OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .nofollow(true)
                    .mode(0o600),
            )?;
            marker.try_lock()?;
            marker.write_all(OWNER_BYTES)?;
            marker.sync_all()?;
            directory.sync()?;
            (marker, true)
        }
        Err(error) => return Err(error),
    };
    Ok((
        StagingDirectory(Arc::new(Owner {
            directory,
            _marker: marker,
        })),
        created,
    ))
}

fn require_empty(directory: &Dir) -> io::Result<()> {
    if !directory.entries()?.is_empty() {
        return Err(io::Error::other(
            "staging directory requires explicit leftover recovery",
        ));
    }
    Ok(())
}

const OWNER_FILE: &str = "bmux-staging-v1";
const OWNER_BYTES: &[u8] = b"BMUX disposable transfer staging version 1\n";
/// Live transfers per owner, and so the bound on recoverable leftovers.
const MAX_TRANSFERS: usize = 8;

/// Initialize an empty staging directory or recover its bounded owned files.
/// A missing marker never authorizes cleanup; partial markers require repair.
/// # Errors
/// Rejects untrusted, foreign or partially owned directories.
pub fn recover_directory(path: &Path) -> io::Result<StagingDirectory> {
    let (owner, created) = lock_directory(path)?;
    if created {
        return Ok(owner);
    }
    let directory = owner.dir();
    // Validate the complete bounded inventory before deleting any file. A crash
    // during deletion leaves the marker and remaining entries for the next run.
    let entries = directory.entries()?;
    if entries.len() > MAX_TRANSFERS + 1 {
        return Err(io::Error::other("unknown or excessive staging leftovers"));
    }
    let mut names = Vec::new();
    for entry in entries {
        if entry.name() == OWNER_FILE {
            continue;
        }
        let name = entry.name().to_str().unwrap_or_default();
        let valid = name
            .strip_prefix("transfer-")
            .and_then(|id| Uuid::parse_str(id).ok())
            .is_some_and(|id| name == format!("transfer-{id}"));
        if !valid || names.len() == MAX_TRANSFERS {
            return Err(io::Error::other("unknown or excessive staging leftovers"));
        }
        require_regular(&directory.metadata_at(name)?)?;
        let file = directory.open_file_at(name, &OpenOptions::new().read(true).nofollow(true))?;
        validate_owned_file(&file.metadata()?)?;
        names.push(name.to_owned());
    }
    for name in names {
        directory.remove_file_at(name)?;
    }
    directory.sync()?;
    Ok(owner)
}

fn require_regular(metadata: &Metadata) -> io::Result<()> {
    if metadata.kind() != FileKind::File {
        return Err(io::Error::other("invalid staging file ownership or type"));
    }
    Ok(())
}

fn validate_owned_file(metadata: &Metadata) -> io::Result<()> {
    if !metadata.is_file()
        || metadata.uid() != Some(switchy_fs::sync::current_uid()?)
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != Some(1)
    {
        return Err(io::Error::other("invalid staging file ownership or type"));
    }
    Ok(())
}

/// In a sticky directory only an entry's owner (or root) may remove or rename it.
const STICKY: u32 = 0o1000;

const fn trusted_owner(metadata: &Metadata, uid: u32) -> bool {
    matches!(metadata.uid(), Some(owner) if owner == 0 || owner == uid)
}

// Walk from root with no-follow opens: a final-component check alone permits
// an attacker-controlled ancestor to redirect initial directory selection.
//
// The sticky-bit exception reads real mode bits. The switchy_fs simulator does
// not model sticky semantics and never sets the bit itself: a simulated shared
// directory (a `/tmp` analogue with mode 0o777) is rejected, and one given the
// bit explicitly would be trusted without the protection it implies. Simulations
// must place staging under a chain of directories that are not group- or
// world-writable.
fn open_trusted_path(path: &Path) -> io::Result<Dir> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(io::Error::other("staging path must be absolute"));
    }
    let uid = switchy_fs::sync::current_uid()?;
    let mut directory = Dir::open("/")?;
    for component in path.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => name,
            _ => return Err(io::Error::other("invalid staging path component")),
        };
        let parent = directory.metadata()?;
        // Sticky shared roots (e.g. /tmp) protect entries owned by this user
        // or root. Every opened child is independently ownership-checked below.
        if !trusted_owner(&parent, uid)
            || (parent.mode() & 0o022 != 0 && parent.mode() & STICKY == 0)
        {
            return Err(io::Error::other("untrusted staging ancestor"));
        }
        // `open_dir_at` requires a directory and never follows a symbolic link.
        directory = directory.open_dir_at(name)?;
        if !trusted_owner(&directory.metadata()?, uid) {
            return Err(io::Error::other("untrusted staging directory owner"));
        }
    }
    Ok(directory)
}

impl TransferStaging {
    /// # Errors
    /// Rejects zero or inconsistent disk reservation quotas.
    pub fn new(per_peer_bytes: u64, total_bytes: u64) -> io::Result<Self> {
        if per_peer_bytes == 0 || total_bytes < per_peer_bytes {
            return Err(io::Error::other("invalid staging quotas"));
        }
        Ok(Self {
            transfers: BTreeMap::new(),
            per_peer_bytes,
            total_bytes,
        })
    }

    /// Check the immutable header on every operation, not only begin retries.
    /// # Errors
    /// Rejects unknown identities and attempts to retarget existing bytes.
    pub fn validate_binding(&self, key: TransferKey, binding: [u8; 32]) -> io::Result<()> {
        let transfer = self
            .transfers
            .get(&key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown transfer binding"))?;
        if transfer.binding != binding {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "conflicting transfer binding",
            ));
        }
        Ok(())
    }

    /// Reserve a complete object. `binding` hashes the validated complete transfer
    /// header; `digest` hashes object bytes. Neither value confers authentication.
    /// # Errors
    /// Rejects conflicting identity reuse, capacity exhaustion and file errors.
    pub fn begin(
        &mut self,
        directory: &StagingDirectory,
        key: TransferKey,
        binding: [u8; 32],
        total: u64,
        digest: [u8; 32],
        now: Instant,
    ) -> io::Result<u64> {
        if let Some(existing) = self.transfers.get(&key) {
            if existing.binding != binding
                || existing.total != total
                || existing.digest != digest
                || existing.failed
            {
                return Err(io::Error::other("conflicting or failed transfer"));
            }
            return Ok(existing.offset);
        }
        let mut peer_count = 0;
        let mut peer_bytes = 0u64;
        let mut bytes = 0u64;
        for (id, transfer) in &self.transfers {
            bytes = bytes
                .checked_add(transfer.total)
                .ok_or_else(|| io::Error::other("quota overflow"))?;
            if id.peer == key.peer {
                peer_count += 1;
                peer_bytes += transfer.total;
            }
        }
        if total == 0
            || self.transfers.len() >= 8
            || peer_count >= 2
            || total > self.total_bytes.saturating_sub(bytes)
            || total > self.per_peer_bytes.saturating_sub(peer_bytes)
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "staging capacity unavailable",
            ));
        }
        let name = key.file_name();
        let file = directory.dir().open_file_at(
            name.as_str(),
            &OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .nofollow(true)
                .mode(0o600),
        )?;
        self.transfers.insert(
            key,
            Incoming {
                file,
                directory: directory.clone(),
                name,
                binding,
                total,
                digest,
                offset: 0,
                last: None,
                hash: Sha256::new(),
                progress: now,
                sealed: false,
                failed: false,
                validation: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        Ok(0)
    }

    /// Write a contiguous chunk or acknowledge an exact last-chunk replay.
    /// # Errors
    /// Rejects gaps, conflicting replay, sealed/failed objects and oversized chunks.
    /// Partial write failure poisons the object until explicit cleanup.
    pub fn chunk(
        &mut self,
        key: TransferKey,
        offset: u64,
        data: &[u8],
        now: Instant,
    ) -> io::Result<u64> {
        let transfer = self
            .transfers
            .get_mut(&key)
            .ok_or_else(|| io::Error::other("unknown transfer"))?;
        if transfer.failed || transfer.sealed || data.is_empty() || data.len() > 64 * 1024 {
            return Err(io::Error::other("invalid chunk or transfer state"));
        }
        let digest: [u8; 32] = Sha256::digest(data).into();
        if transfer.last == Some((offset, data.len(), digest)) {
            return Ok(transfer.offset);
        }
        let end = offset
            .checked_add(u64::try_from(data.len()).map_err(io::Error::other)?)
            .ok_or_else(|| io::Error::other("chunk overflow"))?;
        if offset != transfer.offset || end > transfer.total {
            return Err(io::Error::other("unexpected chunk offset"));
        }
        transfer.failed = true;
        transfer.file.write_all(data)?;
        transfer.hash.update(data);
        transfer.offset = end;
        transfer.last = Some((offset, data.len(), digest));
        transfer.progress = now;
        transfer.failed = false;
        Ok(end)
    }

    /// Seal complete bytes after digest verification. This is not canonical-object
    /// validation, durable replication or snapshot installation.
    /// # Errors
    /// Rejects incomplete or corrupt objects; flush failure poisons the object.
    pub fn finish(&mut self, key: TransferKey) -> io::Result<()> {
        self.finish_validated(key, |_, _| Ok(()))
    }

    /// Seal only after the caller validates canonical bytes and header identity.
    pub(crate) fn finish_validated(
        &mut self,
        key: TransferKey,
        validate: impl FnOnce(&mut File, u64) -> io::Result<()>,
    ) -> io::Result<()> {
        let ticket = self.prepare_validation(key)?.validate(validate)?;
        self.complete_validation(key, ticket)
    }

    pub(crate) fn prepare_validation(&mut self, key: TransferKey) -> io::Result<ValidationTicket> {
        let transfer = self
            .transfers
            .get_mut(&key)
            .ok_or_else(|| io::Error::other("unknown transfer"))?;
        if Arc::strong_count(&transfer.validation) != 1
            || (transfer.failed
                && !transfer
                    .validation
                    .load(std::sync::atomic::Ordering::SeqCst))
            || transfer.offset != transfer.total
            || <[u8; 32]>::from(transfer.hash.clone().finalize()) != transfer.digest
        {
            return Err(io::Error::other("incomplete or corrupt transfer"));
        }
        transfer.failed = true;
        transfer
            .validation
            .store(false, std::sync::atomic::Ordering::SeqCst);
        transfer.file.flush()?;
        // switchy_fs handles cannot be duplicated, so validation reads through
        // an independent read-only handle opened in the owned directory, and
        // only if it is provably the same single-link inode this transfer wrote.
        let file = transfer.directory.dir().open_file_at(
            transfer.name.as_str(),
            &OpenOptions::new().read(true).nofollow(true),
        )?;
        let reopened = file.metadata()?;
        validate_owned_file(&reopened)?;
        if !reopened.is_same_file(&transfer.file.metadata()?) {
            return Err(io::Error::other("staged transfer file was replaced"));
        }
        Ok(ValidationTicket {
            file,
            total: transfer.total,
            identity: Arc::clone(&transfer.validation),
        })
    }

    // Callers must revalidate authority before consuming a successfully validated ticket.
    pub(crate) fn complete_validation(
        &mut self,
        key: TransferKey,
        ticket: ValidatedTicket,
    ) -> io::Result<()> {
        let transfer = self
            .transfers
            .get_mut(&key)
            .ok_or_else(|| io::Error::other("unknown transfer"))?;
        if !Arc::ptr_eq(&ticket.0.identity, &transfer.validation) {
            return Err(io::Error::other("stale validation ticket"));
        }
        transfer.sealed = true;
        transfer.failed = false;
        drop(ticket);
        Ok(())
    }

    /// Read only sealed bytes in bounded chunks for canonical validation/consumption.
    /// # Errors
    /// Rejects unsealed objects or reads larger than 64 KiB.
    pub fn read(&mut self, key: TransferKey, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        let transfer = self
            .transfers
            .get_mut(&key)
            .ok_or_else(|| io::Error::other("unknown transfer"))?;
        if !transfer.sealed
            || transfer.failed
            || buffer.len() > 64 * 1024
            || offset > transfer.total
        {
            return Err(io::Error::other("invalid staged read"));
        }
        transfer.file.seek(SeekFrom::Start(offset))?;
        transfer.file.read(buffer)
    }

    /// Borrow the exact immutable sealed object under the caller's exclusive
    /// staging owner. The borrowed reader cannot outlive header binding checks.
    /// # Errors
    /// Rejects unknown, unsealed or mismatched transfers.
    pub fn sealed_reader(
        &mut self,
        key: TransferKey,
        binding: [u8; 32],
    ) -> io::Result<(&mut File, u64)> {
        self.validate_binding(key, binding)?;
        let transfer = self
            .transfers
            .get_mut(&key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown transfer"))?;
        if !transfer.sealed || transfer.failed {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "snapshot is not sealed",
            ));
        }
        transfer.file.rewind()?;
        Ok((&mut transfer.file, transfer.total))
    }

    /// # Errors
    /// Failed deletion keeps the object and its reservation for a later retry.
    pub fn abort(&mut self, key: TransferKey) -> io::Result<()> {
        if let Some(transfer) = self.transfers.get(&key) {
            if Arc::strong_count(&transfer.validation) != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "validation still owns the transfer reservation",
                ));
            }
            transfer
                .directory
                .dir()
                .remove_file_at(transfer.name.as_str())?;
        }
        self.transfers.remove(&key);
        Ok(())
    }

    /// Expire only on explicit timer events; replays and begin retries do not renew.
    /// # Errors
    /// Reports cleanup failure without releasing the failed reservation.
    pub fn expire(&mut self, now: Instant) -> io::Result<()> {
        let expired = self
            .transfers
            .iter()
            .filter(|(_, transfer)| {
                Arc::strong_count(&transfer.validation) == 1
                    && now.saturating_duration_since(transfer.progress) >= Duration::from_secs(30)
            })
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        for key in expired {
            self.abort(key)?;
        }
        Ok(())
    }

    /// # Errors
    /// Reports incomplete cleanup; caller must stop admission before shutdown.
    pub fn clear(&mut self) -> io::Result<()> {
        let keys = self.transfers.keys().copied().collect::<Vec<_>>();
        for key in keys {
            self.abort(key)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn private_root() -> tempfile::TempDir {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        tempfile::tempdir_in(parent).unwrap()
    }

    #[test]
    fn restart_recovers_owned_transfers_but_preserves_unknown_files() {
        let root = private_root();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = recover_directory(root.path()).unwrap();
        let mut staging = TransferStaging::new(8, 16).unwrap();
        let key = TransferKey {
            peer: [1; 32],
            incarnation: Uuid::nil(),
            sequence: 1,
        };
        staging
            .begin(&directory, key, [0; 32], 4, [0; 32], Instant::now())
            .unwrap();
        drop(staging);
        drop(directory);
        std::fs::write(root.path().join("unknown"), b"preserve").unwrap();
        assert!(recover_directory(root.path()).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 3);
        std::fs::remove_file(root.path().join("unknown")).unwrap();
        drop(recover_directory(root.path()).unwrap());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        std::fs::write(root.path().join(OWNER_FILE), b"corrupt").unwrap();
        assert!(recover_directory(root.path()).is_err());
        assert_eq!(
            std::fs::read(root.path().join(OWNER_FILE)).unwrap(),
            b"corrupt"
        );
    }

    #[test]
    fn validation_ticket_retains_reservation_until_dropped() {
        let root = private_root();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = recover_directory(root.path()).unwrap();
        let mut staging = TransferStaging::new(4, 4).unwrap();
        let key = TransferKey {
            peer: [1; 32],
            incarnation: Uuid::nil(),
            sequence: 1,
        };
        let now = Instant::now();
        staging
            .begin(
                &directory,
                key,
                [0; 32],
                4,
                Sha256::digest(b"data").into(),
                now,
            )
            .unwrap();
        staging.chunk(key, 0, b"data", now).unwrap();
        let ticket = staging.prepare_validation(key).unwrap();
        assert_eq!(
            staging.abort(key).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(staging.clear().is_err());
        staging.expire(now + Duration::from_secs(31)).unwrap();
        assert!(staging.transfers.contains_key(&key));
        let other = TransferKey { sequence: 2, ..key };
        assert!(
            staging
                .begin(&directory, other, [0; 32], 4, [0; 32], now)
                .is_err()
        );
        assert!(staging.chunk(key, 0, b"data", now).is_err());
        drop(ticket);
        staging.abort(key).unwrap();
        staging
            .begin(&directory, other, [0; 32], 4, [0; 32], now)
            .unwrap();
    }

    #[test]
    fn successful_validation_can_retry_after_unpublished_ticket_is_dropped() {
        let root = private_root();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = recover_directory(root.path()).unwrap();
        let mut staging = TransferStaging::new(4, 4).unwrap();
        let key = TransferKey {
            peer: [1; 32],
            incarnation: Uuid::nil(),
            sequence: 1,
        };
        let now = Instant::now();
        staging
            .begin(
                &directory,
                key,
                [0; 32],
                4,
                Sha256::digest(b"data").into(),
                now,
            )
            .unwrap();
        staging.chunk(key, 0, b"data", now).unwrap();
        let ticket = staging
            .prepare_validation(key)
            .unwrap()
            .validate(|_, _| Ok(()))
            .unwrap();
        assert!(staging.prepare_validation(key).is_err());
        drop(ticket); // Final authority admission timed out without sealing.
        assert!(staging.read(key, 0, &mut [0; 4]).is_err());
        assert!(staging.chunk(key, 0, b"data", now).is_err());
        let retry = staging.prepare_validation(key).unwrap();
        assert!(
            retry
                .validate(|_, _| Err(io::Error::other("invalid")))
                .is_err()
        );
        assert!(staging.prepare_validation(key).is_err());
        staging.abort(key).unwrap();
    }

    #[test]
    fn canonical_validation_failure_never_seals_or_allows_reads() {
        let root = private_root();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = recover_directory(root.path()).unwrap();
        let mut staging = TransferStaging::new(8, 16).unwrap();
        let key = TransferKey {
            peer: [1; 32],
            incarnation: Uuid::nil(),
            sequence: 1,
        };
        let now = Instant::now();
        staging
            .begin(
                &directory,
                key,
                [0; 32],
                4,
                Sha256::digest(b"bad!").into(),
                now,
            )
            .unwrap();
        staging.chunk(key, 0, b"bad!", now).unwrap();
        assert!(
            staging
                .finish_validated(key, |file, total| {
                    crate::consensus_storage::decode_owned_entry_reader(
                        file,
                        usize::try_from(total).unwrap(),
                    )
                    .map(|_| ())
                    .map_err(io::Error::other)
                })
                .is_err()
        );
        assert!(!staging.transfers[&key].sealed);
        assert!(staging.read(key, 0, &mut [0; 4]).is_err());
        assert!(staging.finish(key).is_err());
        staging.abort(key).unwrap();
    }

    #[test]
    fn startup_rejects_symlink_and_writable_ancestors() {
        let root = private_root();
        let root = root.path().canonicalize().unwrap();
        let parent = root.join("parent");
        let target = parent.join("staging");
        std::fs::create_dir(&parent).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(open_directory(&target).is_err());
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        open_directory(&target).unwrap();
        let link = root.join("alias");
        std::os::unix::fs::symlink(&parent, &link).unwrap();
        assert!(open_directory(&link.join("staging")).is_err());
        assert!(open_directory(Path::new("relative")).is_err());
    }

    #[test]
    fn directory_ownership_is_exclusive_across_rename_and_clones() {
        let root = private_root();
        let original = root.path().join("original");
        let moved = root.path().join("moved");
        std::fs::create_dir(&original).unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = open_directory(&original).unwrap();
        assert!(open_directory(&original).is_err());
        let retained = directory.clone();
        std::fs::rename(&original, &moved).unwrap();
        drop(directory);
        assert!(open_directory(&moved).is_err());
        drop(retained);
        open_directory(&moved).unwrap();
    }

    #[test]
    fn directory_replacement_cannot_redirect_creation_or_cleanup() {
        let root = private_root();
        let original = root.path().join("original");
        let moved = root.path().join("moved");
        std::fs::create_dir(&original).unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = open_directory(&original).unwrap();
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        let mut staging = TransferStaging::new(8, 16).unwrap();
        let key = TransferKey {
            peer: [1; 32],
            incarnation: Uuid::nil(),
            sequence: 1,
        };
        staging
            .begin(
                &directory,
                key,
                [2; 32],
                4,
                Sha256::digest(b"abcd").into(),
                Instant::now(),
            )
            .unwrap();
        let name = staging.transfers[&key].name.clone();
        std::fs::write(original.join(&name), b"unrelated").unwrap();
        assert!(moved.join(&name).exists());
        staging.abort(key).unwrap();
        assert!(!moved.join(&name).exists());
        assert_eq!(std::fs::read(original.join(name)).unwrap(), b"unrelated");
    }

    #[test]
    fn reservations_replays_sealing_and_expiry() {
        let root = private_root();
        let mut staging = TransferStaging::new(8, 16).unwrap();
        let key = TransferKey {
            peer: [1; 32],
            incarnation: Uuid::nil(),
            sequence: 1,
        };
        let now = Instant::now();
        let digest = Sha256::digest(b"abcd").into();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = open_directory(root.path()).unwrap();
        staging
            .begin(&directory, key, [2; 32], 4, digest, now)
            .unwrap();
        assert_eq!(staging.chunk(key, 0, b"ab", now).unwrap(), 2);
        assert!(staging.finish(key).is_err());
        assert_eq!(
            staging
                .chunk(key, 0, b"ab", now + Duration::from_secs(20))
                .unwrap(),
            2
        );
        assert!(staging.chunk(key, 0, b"ax", now).is_err());
        staging.chunk(key, 2, b"cd", now).unwrap();
        staging.finish(key).unwrap();
        let mut bytes = [0; 4];
        assert_eq!(staging.read(key, 0, &mut bytes).unwrap(), 4);
        assert_eq!(&bytes, b"abcd");
        assert!(staging.chunk(key, 2, b"cd", now).is_err());
        staging.expire(now + Duration::from_secs(30)).unwrap();
        assert!(staging.transfers.is_empty());
        // Only the owner marker remains.
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn staged_names_are_deterministic_and_recoverable() {
        let key = TransferKey {
            peer: [7; 32],
            incarnation: Uuid::from_u128(9),
            sequence: 3,
        };
        assert_eq!(key.file_name(), key.file_name());
        assert_ne!(
            key.file_name(),
            TransferKey { sequence: 4, ..key }.file_name()
        );
        assert_ne!(
            key.file_name(),
            TransferKey {
                incarnation: Uuid::from_u128(10),
                ..key
            }
            .file_name()
        );
        let root = private_root();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = recover_directory(root.path()).unwrap();
        let mut staging = TransferStaging::new(8, 16).unwrap();
        staging
            .begin(&directory, key, [0; 32], 4, [0; 32], Instant::now())
            .unwrap();
        assert!(root.path().join(key.file_name()).is_file());
        drop(staging);
        drop(directory);
        drop(recover_directory(root.path()).unwrap());
        assert!(!root.path().join(key.file_name()).exists());
    }

    #[test]
    fn recovery_rejects_linked_or_excessive_leftovers_without_deleting() {
        let root = private_root();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        drop(recover_directory(root.path()).unwrap());
        let name = |sequence| {
            TransferKey {
                peer: [1; 32],
                incarnation: Uuid::nil(),
                sequence,
            }
            .file_name()
        };
        let outside = private_root();
        let target = outside.path().join("target");
        std::fs::write(&target, b"outside").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        // A well-named symbolic link is never followed or deleted.
        std::os::unix::fs::symlink(&target, root.path().join(name(1))).unwrap();
        assert!(recover_directory(root.path()).is_err());
        assert!(root.path().join(name(1)).symlink_metadata().is_ok());
        std::fs::remove_file(root.path().join(name(1))).unwrap();

        // A well-named hard link to another file is rejected (nlink != 1).
        std::fs::hard_link(&target, root.path().join(name(2))).unwrap();
        assert!(recover_directory(root.path()).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"outside");
        std::fs::remove_file(root.path().join(name(2))).unwrap();

        // More leftovers than an owner can create are rejected before deletion.
        for sequence in 0..=8 {
            let path = root.path().join(name(sequence));
            std::fs::write(&path, b"").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(recover_directory(root.path()).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 10);
        std::fs::remove_file(root.path().join(name(8))).unwrap();
        drop(recover_directory(root.path()).unwrap());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
