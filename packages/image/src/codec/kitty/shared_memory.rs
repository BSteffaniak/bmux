//! Opt-in Linux Kitty POSIX shared-memory transport, using safe standard APIs.
//!
//! The terminal unlinks consumed objects. ACK, timeout, abort and final drop
//! unlink our remaining objects. SIGKILL/power loss before terminal consumption
//! can leave objects behind: there is deliberately no prefix-based scavenger.
//! Each independent connection must construct its own `Transport`; clones share
//! leases and failure generation so speculative compositor clones cannot unlink
//! a still-pending upload or retain residency after an asynchronous rejection.
//!
//! Threat model: trust processes running as the same UID and root. Linux's
//! sticky /dev/shm plus exclusive 0600 files protects against other UIDs; random
//! single-component names reduce accidental collisions. The inode check before
//! unlink detects an already-replaced path, but check/unlink is NOT atomic and
//! does not protect against a concurrent same-UID replacement. Safe std has no
//! conditional identity-unlink primitive. A private directory cannot contain a
//! POSIX SHM object name: the protocol permits only the leading slash. Names here
//! are 60 bytes including that slash, below Linux's NAME_MAX limit of 255.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{KittyReplyStatus, base64_encode, parse_reply_frame};
use crate::model::KittyFormat;

const MAX_PENDING: usize = 16;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct Lease {
    #[cfg(target_os = "linux")]
    path: std::path::PathBuf,
    #[cfg(target_os = "linux")]
    identity: (u64, u64),
    #[cfg(target_os = "linux")]
    _file: std::fs::File,
    name: String,
    bytes: usize,
}

impl Lease {
    #[cfg(target_os = "linux")]
    fn create(data: &[u8]) -> io::Result<Self> {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let mut random = [0u8; 24];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let name = format!("/bmux-kitty-{suffix}");
        let path = std::path::Path::new("/dev/shm").join(&name[1..]);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let metadata = file.metadata()?;
        let mut lease = Self {
            path,
            identity: (metadata.dev(), metadata.ino()),
            _file: file,
            name,
            bytes: data.len(),
        };
        lease._file.write_all(data)?;
        Ok(lease)
    }

    #[cfg(not(target_os = "linux"))]
    fn create(_data: &[u8]) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Kitty SHM requires Linux",
        ))
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            // No namespace scan. Skip an observed replacement; this is not an
            // atomic identity/unlink operation (see the same-UID threat model).
            // A consumed object normally was already unlinked by the terminal.
            if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
                && metadata.is_file()
                && (metadata.dev(), metadata.ino()) == self.identity
            {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

#[derive(Debug)]
struct Pending {
    lease: Lease,
    deadline: Instant,
    probe: bool,
    generation: u64,
}

#[derive(Debug, Default)]
struct State {
    enabled: bool,
    pending: BTreeMap<u32, Pending>,
    generation: u64,
    high_water_id: u32,
    repair_stream: bool,
}

/// Connection-local shared transport. Default is direct-only; no environment
/// variable is accepted as proof of terminal support or namespace accessibility.
#[derive(Clone, Debug, Default)]
pub struct Transport(Arc<Mutex<State>>);

impl Transport {
    pub(crate) fn allocate_id(&self, previous: u32) -> io::Result<u32> {
        let mut state = self.0.lock().expect("Kitty SHM mutex poisoned");
        let id = previous
            .max(state.high_water_id)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Kitty image ID exhausted"))?;
        state.high_water_id = id;
        Ok(id)
    }
    /// Failure generation, used to invalidate speculative host residency.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.0.lock().expect("Kitty SHM mutex poisoned").generation
    }

    /// Explicitly opt in and send a load-only shared-memory capability query.
    /// A matching successful reply is required before uploads select SHM.
    /// Returns false if SHM setup is unavailable, without writing any bytes.
    ///
    /// # Errors
    /// * Output failure after setup. No direct fallback may be appended; repair
    ///   the terminal stream. Pending commands retain leases until ACK/deadline.
    pub fn begin_probe(&self, out: &mut impl Write, id: u32, now: Instant) -> io::Result<bool> {
        self.expire(now);
        // Re-probing cannot revoke an earlier command's readable data. Existing
        // requests remain until their ACK/deadline, and old probe ACKs cannot
        // re-enable a failed generation.
        self.invalidate();
        self.repair_stream(out)?;
        self.send(out, id, KittyFormat::Rgb, &[0, 0, 0], (1, 1), now, true)
    }

    /// Transmit through SHM only after successful negotiation and within bounds.
    /// Returns false before output when direct fallback is safe.
    ///
    /// # Errors
    /// * Output failure; never append direct fallback to a partial APC command.
    pub(crate) fn upload(
        &self,
        out: &mut impl Write,
        id: u32,
        format: KittyFormat,
        data: &[u8],
        size: (u32, u32),
        now: Instant,
    ) -> io::Result<bool> {
        self.send(out, id, format, data, size, now, false)
    }

    #[allow(clippy::too_many_arguments)] // One complete wire upload and its lease deadline.
    fn send(
        &self,
        out: &mut impl Write,
        id: u32,
        format: KittyFormat,
        data: &[u8],
        size: (u32, u32),
        now: Instant,
        probe: bool,
    ) -> io::Result<bool> {
        let mut state = self.0.lock().expect("Kitty SHM mutex poisoned");
        let bytes: usize = state.pending.values().map(|p| p.lease.bytes).sum();
        if (!probe && !state.enabled)
            || state.pending.len() >= MAX_PENDING
            || data.len() > MAX_BYTES.saturating_sub(bytes)
            || state.pending.contains_key(&id)
        {
            return Ok(false);
        }
        let Ok(lease) = Lease::create(data) else {
            return Ok(false);
        };
        let format = match format {
            KittyFormat::Rgb => 24,
            KittyFormat::Rgba => 32,
            KittyFormat::Png => 100,
        };
        let action = if probe { 'q' } else { 't' };
        // Once SHM output has been staged, every later attempt terminates any
        // uncertain APC. Writing into a Vec cannot discharge this obligation.
        state.repair_stream = true;
        let command = format!(
            "\x1b\\\x1b_Ga={action},t=s,i={id},f={format},s={},v={},S={},q=0;{}\x1b\\",
            size.0,
            size.1,
            data.len(),
            base64_encode(lease.name.as_bytes())
        );
        let generation = state.generation;
        state.pending.insert(
            id,
            Pending {
                lease,
                deadline: now + DEADLINE,
                probe,
                generation,
            },
        );
        if let Err(error) = out.write_all(command.as_bytes()) {
            // This command might itself have partially reached the host. Keep
            // its lease as well as older commands until their own ACK/deadline.
            state.repair_stream = true;
            state.enabled = false;
            state.generation = state.generation.wrapping_add(1);
            return Err(error);
        }
        Ok(true)
    }

    /// Claim only complete, timely replies for this connection's outstanding IDs.
    /// Unmatched/expired frames remain owned by the runtime input router.
    #[must_use]
    pub fn handle_reply(&self, frame: &[u8], now: Instant) -> bool {
        self.expire(now);
        let Some(reply) = parse_reply_frame(frame) else {
            return false;
        };
        if reply.placement_id.is_some() {
            return false;
        }
        let mut state = self.0.lock().expect("Kitty SHM mutex poisoned");
        let Some(pending) = state.pending.remove(&reply.image_id) else {
            return false;
        };
        if reply.status == KittyReplyStatus::Ok {
            if pending.probe && pending.generation == state.generation {
                state.enabled = true;
            }
        } else {
            state.enabled = false;
            state.generation = state.generation.wrapping_add(1);
        }
        true
    }

    /// Release expired objects, disable SHM and invalidate uncertain residency.
    pub fn expire(&self, now: Instant) {
        let mut state = self.0.lock().expect("Kitty SHM mutex poisoned");
        if state.pending.values().any(|p| p.deadline <= now) {
            // Expiration of one upload cannot revoke a younger upload's lease.
            state.pending.retain(|_, pending| pending.deadline > now);
            state.enabled = false;
            state.generation = state.generation.wrapping_add(1);
        }
    }

    /// Terminate an uncertain partial APC before any repair or fallback output.
    /// Retain this obligation across failed attempts and candidate clones.
    pub(crate) fn repair_stream(&self, out: &mut impl Write) -> io::Result<()> {
        let state = self.0.lock().expect("Kitty SHM mutex poisoned");
        if state.repair_stream {
            out.write_all(b"\x1b\\")?;
        }
        Ok(())
    }

    /// Disable future SHM and invalidate residency without revoking any command
    /// the terminal may still be reading. Used for candidate output/flush errors.
    pub(crate) fn invalidate(&self) {
        let mut state = self.0.lock().expect("Kitty SHM mutex poisoned");
        if state.enabled || !state.pending.is_empty() {
            state.enabled = false;
            state.generation = state.generation.wrapping_add(1);
        }
    }

    /// Release this connection's objects and disable SHM. Cloned candidates see
    /// the failure generation. No foreign object or orphan namespace is scanned.
    pub fn abort(&self) {
        let mut state = self.0.lock().expect("Kitty SHM mutex poisoned");
        if state.enabled || !state.pending.is_empty() {
            state.repair_stream |= !state.pending.is_empty();
            state.pending.clear();
            state.enabled = false;
            state.generation = state.generation.wrapping_add(1);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn enabled() -> (Transport, Instant) {
        let transport = Transport::default();
        let now = Instant::now();
        assert!(transport.begin_probe(&mut Vec::new(), 1, now).unwrap());
        assert!(transport.handle_reply(b"\x1b_Gi=1;OK\x1b\\", now));
        (transport, now)
    }

    #[test]
    fn partial_candidate_upload_does_not_revoke_previously_pending_upload() {
        struct Fail;
        impl Write for Fail {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("candidate write failed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (transport, now) = enabled();
        assert!(
            transport
                .upload(&mut Vec::new(), 2, KittyFormat::Png, b"prior", (1, 1), now)
                .unwrap()
        );
        let path = transport.0.lock().unwrap().pending[&2].lease.path.clone();
        let candidate = transport.clone();
        assert!(
            candidate
                .upload(&mut Fail, 3, KittyFormat::Png, b"candidate", (1, 1), now)
                .is_err()
        );
        drop(candidate);
        assert!(
            path.exists(),
            "candidate failure must retain prior host-readable bytes"
        );
        assert!(transport.handle_reply(b"\x1b_Gi=2;OK\x1b\\", now));
        assert!(!path.exists());
    }

    #[test]
    fn lease_drop_skips_stable_replacement_without_scanning_other_names() {
        let lease = Lease::create(b"original").unwrap();
        let path = lease.path.clone();
        // This replacement is also test-owned. Holding the original file open
        // ensures the replacement cannot have the original inode by reuse.
        let replacement = Lease::create(b"replacement").unwrap();
        let replacement_path = replacement.path.clone();
        std::fs::rename(&replacement_path, &path).unwrap();
        drop(lease);
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        drop(replacement);
        assert!(
            path.exists(),
            "neither lease owns the replaced pathname identity"
        );
        std::fs::remove_file(path).unwrap(); // Exact test-owned replacement only.
    }

    #[test]
    fn older_timeout_or_error_cannot_revoke_younger_pending_upload() {
        for reject in [false, true] {
            let (transport, now) = enabled();
            assert!(
                transport
                    .upload(&mut Vec::new(), 2, KittyFormat::Png, b"older", (1, 1), now)
                    .unwrap()
            );
            let younger = now + Duration::from_secs(1);
            assert!(
                transport
                    .upload(
                        &mut Vec::new(),
                        3,
                        KittyFormat::Png,
                        b"younger",
                        (1, 1),
                        younger
                    )
                    .unwrap()
            );
            let path = transport.0.lock().unwrap().pending[&3].lease.path.clone();
            if reject {
                assert!(transport.handle_reply(b"\x1b_Gi=2;EIO: rejected\x1b\\", younger));
            } else {
                transport.expire(now + DEADLINE);
            }
            assert!(path.exists());
            assert!(transport.handle_reply(b"\x1b_Gi=3;OK\x1b\\", now + DEADLINE));
            assert!(!path.exists());
        }
    }

    #[test]
    fn invalidated_probe_ack_cannot_enable_new_uploads() {
        let transport = Transport::default();
        let now = Instant::now();
        assert!(transport.begin_probe(&mut Vec::new(), 1, now).unwrap());
        transport.invalidate();
        assert!(transport.handle_reply(b"\x1b_Gi=1;OK\x1b\\", now));
        let mut output = Vec::new();
        assert!(
            !transport
                .upload(&mut output, 2, KittyFormat::Png, b"x", (1, 1), now)
                .unwrap()
        );
        assert!(output.is_empty());
    }

    #[test]
    fn count_and_byte_bounds_fall_back_before_writing() {
        let (transport, now) = enabled();
        let data = vec![7; MAX_BYTES / MAX_PENDING];
        for id in 2..=17 {
            assert!(
                transport
                    .upload(&mut Vec::new(), id, KittyFormat::Png, &data, (1, 1), now)
                    .unwrap()
            );
        }
        let mut output = Vec::new();
        assert!(
            !transport
                .upload(&mut output, 18, KittyFormat::Png, b"x", (1, 1), now)
                .unwrap()
        );
        assert!(output.is_empty());
        let paths: Vec<_> = transport
            .0
            .lock()
            .unwrap()
            .pending
            .values()
            .map(|p| p.lease.path.clone())
            .collect();
        transport.abort();
        assert!(paths.iter().all(|p| !p.exists()));
        assert!(
            !transport
                .upload(&mut output, 19, KittyFormat::Png, b"x", (1, 1), now)
                .unwrap()
        );
        assert!(output.is_empty());
        // Byte budget is independent of pending count.
        let (transport, now) = enabled();
        assert!(
            !transport
                .upload(
                    &mut output,
                    2,
                    KittyFormat::Png,
                    &vec![0; MAX_BYTES + 1],
                    (1, 1),
                    now
                )
                .unwrap()
        );
        assert!(output.is_empty());
    }

    #[test]
    fn upload_error_expires_all_leases_and_isolates_connections() {
        let (transport, now) = enabled();
        let (other, _) = enabled();
        assert!(
            transport
                .upload(&mut Vec::new(), 2, KittyFormat::Png, b"one", (1, 1), now)
                .unwrap()
        );
        let path = transport.0.lock().unwrap().pending[&2].lease.path.clone();
        assert!(!other.handle_reply(b"\x1b_Gi=2;OK\x1b\\", now));
        assert!(path.exists());
        let generation = transport.generation();
        assert!(transport.handle_reply(b"\x1b_Gi=2;EIO: cannot read\x1b\\", now));
        assert!(!path.exists());
        assert_ne!(transport.generation(), generation);
        assert!(!transport.handle_reply(b"\x1b_Gi=2;OK\x1b\\", now));
        assert!(
            other
                .upload(&mut Vec::new(), 2, KittyFormat::Png, b"two", (1, 1), now)
                .unwrap()
        );
    }

    #[test]
    fn timed_out_probe_never_enables_and_ids_survive_candidate_forks() {
        let transport = Transport::default();
        let now = Instant::now();
        let first = transport.allocate_id(0).unwrap();
        assert!(transport.begin_probe(&mut Vec::new(), first, now).unwrap());
        let path = transport.0.lock().unwrap().pending[&first]
            .lease
            .path
            .clone();
        let fork = transport.clone();
        assert!(fork.allocate_id(0).unwrap() > first);
        assert!(!transport.handle_reply(
            format!("\x1b_Gi={first};OK\x1b\\").as_bytes(),
            now + DEADLINE
        ));
        assert!(!path.exists());
        let mut output = Vec::new();
        assert!(
            !transport
                .upload(&mut output, 3, KittyFormat::Png, b"x", (1, 1), now)
                .unwrap()
        );
        assert!(output.is_empty());
    }
}
