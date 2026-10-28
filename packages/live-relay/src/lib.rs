//! Single-channel BMWP relay. Only authority transitions touch durable storage.
use bmux_web_renderer::{BrowserFrame, decode_binary, encode_binary};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::PathBuf,
    sync::{Arc, OnceLock},
};
use uuid::Uuid;

pub mod ingest_reason;
pub use ingest_reason::IngestReason;

pub const MAX_BYTES: usize = 8 * 1024 * 1024;
pub const LEASE_MS: u64 = 15_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Offline,
    Starting,
    Live,
    Paused,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Authority {
    pub version: u8,
    pub epoch: Option<Uuid>,
    pub publisher_id: Option<Uuid>,
    pub fence: u64,
    pub state: Phase,
    pub expires_ms: u64,
}
impl Default for Authority {
    fn default() -> Self {
        Self {
            version: 1,
            epoch: None,
            publisher_id: None,
            fence: 0,
            state: Phase::Offline,
            expires_ms: 0,
        }
    }
}
impl Authority {
    pub fn active(&self, now: u64) -> bool {
        self.epoch.is_some() && self.expires_ms > now
    }
    pub fn owns(&self, epoch: Uuid, fence: u64, now: u64) -> bool {
        self.active(now) && self.epoch == Some(epoch) && self.fence == fence
    }
    fn advance(&mut self) -> io::Result<()> {
        self.fence = self
            .fence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("fence exhausted"))?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct Delivery {
    pub frame: Arc<BrowserFrame>,
    full: Arc<OnceLock<Result<axum::body::Bytes, &'static str>>>,
    pub patch: axum::body::Bytes,
    pub base: Option<(String, u64)>,
}
impl Delivery {
    pub fn snapshot(&self) -> Result<axum::body::Bytes, &'static str> {
        self.full
            .get_or_init(|| {
                let bytes = encode_binary(&self.frame).map_err(|_| "encode")?;
                if bytes.len() > MAX_BYTES {
                    return Err("snapshot size");
                }
                Ok(bytes.into())
            })
            .clone()
    }
}
#[derive(Clone)]
pub struct View {
    pub authority: Authority,
    pub delivery: Option<Delivery>,
}
pub struct Relay {
    pub authority: Authority,
    pub delivery: Option<Delivery>,
    path: PathBuf,
    poisoned: bool,
}
impl Relay {
    pub fn open(path: PathBuf, now: u64) -> io::Result<Self> {
        let saved = match File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(4097).read_to_end(&mut bytes)?;
                Ok(bytes)
            }
            Err(error) => Err(error),
        };
        let authority: Authority = match saved {
            Ok(bytes) if bytes.len() <= 4096 => {
                serde_json::from_slice(&bytes).map_err(io::Error::other)?
            }
            Ok(_) => return Err(io::Error::other("oversized authority file")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Authority::default(),
            Err(e) => return Err(e),
        };
        if authority.version != 1
            || (authority.epoch.is_some() != authority.publisher_id.is_some())
            || (authority.state == Phase::Offline) != authority.epoch.is_none()
        {
            return Err(io::Error::other("invalid authority file"));
        }
        let mut relay = Self {
            authority,
            delivery: None,
            path,
            poisoned: false,
        };
        let mut next = relay.authority.clone();
        next.advance()?;
        if !next.active(now) {
            next.epoch = None;
            next.publisher_id = None;
            next.state = Phase::Offline;
            next.expires_ms = 0;
        } else if next.state == Phase::Live {
            next.state = Phase::Starting;
        }
        relay.commit(next, true)?;
        Ok(relay)
    }
    fn commit(&mut self, next: Authority, clear: bool) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("authority storage poisoned"));
        }
        let result = self.persist(&next);
        if let Err(error) = result {
            self.poisoned = true;
            self.authority.epoch = None;
            self.authority.publisher_id = None;
            self.authority.state = Phase::Offline;
            self.delivery = None;
            return Err(error);
        }
        self.authority = next;
        if clear {
            self.delivery = None;
        }
        Ok(())
    }
    fn persist(&self, next: &Authority) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("authority path"))?;
        fs::create_dir_all(parent)?;
        let temp = self.path.with_extension("tmp");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(&serde_json::to_vec(next)?)?;
        file.sync_all()?;
        fs::rename(&temp, &self.path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
    pub fn view(&self) -> View {
        View {
            authority: self.authority.clone(),
            delivery: self.delivery.clone(),
        }
    }
    pub fn expire(&mut self, now: u64) -> io::Result<bool> {
        if self.poisoned {
            return Err(io::Error::other("authority storage poisoned"));
        }
        if self.authority.epoch.is_some() && !self.authority.active(now) {
            let mut next = self.authority.clone();
            next.advance()?;
            next.epoch = None;
            next.publisher_id = None;
            next.state = Phase::Offline;
            next.expires_ms = 0;
            self.commit(next, true)?;
            return Ok(true);
        }
        Ok(false)
    }
    pub fn start(&mut self, publisher: Uuid, now: u64) -> io::Result<bool> {
        self.expire(now)?;
        if self.authority.active(now) {
            return Ok(false);
        }
        let mut next = self.authority.clone();
        next.advance()?;
        next.epoch = Some(Uuid::new_v4());
        next.publisher_id = Some(publisher);
        next.state = Phase::Starting;
        next.expires_ms = now.saturating_add(LEASE_MS);
        self.commit(next, true)?;
        Ok(true)
    }
    pub fn control(&mut self, action: &str, now: u64) -> io::Result<bool> {
        let mut next = self.authority.clone();
        match action {
            "heartbeat" => {}
            "pause" => {
                next.advance()?;
                next.state = Phase::Paused;
            }
            "resume" if next.state == Phase::Paused => {
                next.advance()?;
                next.state = Phase::Starting;
            }
            "stop" => {
                next.advance()?;
                next.state = Phase::Offline;
                next.epoch = None;
                next.publisher_id = None;
            }
            _ => return Ok(false),
        }
        next.expires_ms = if next.epoch.is_some() {
            now.saturating_add(LEASE_MS)
        } else {
            0
        };
        self.commit(next, action != "heartbeat")?;
        Ok(true)
    }
    pub fn ingest(&mut self, bytes: axum::body::Bytes, fresh: bool) -> Result<(), &'static str> {
        self.ingest_detailed(bytes, fresh)
            .map_err(|reason| match reason {
                IngestReason::Oversize | IngestReason::PoisonedAuthority => {
                    "size or poisoned authority"
                }
                IngestReason::Resources => "resources",
                IngestReason::ZeroRevision => "zero revision",
                IngestReason::StaleRevision => "revision",
                _ => "repair",
            })
    }
    /// Typed local diagnostics; publisher ACK/repair wire envelopes remain unchanged.
    pub fn ingest_detailed(
        &mut self,
        bytes: axum::body::Bytes,
        fresh: bool,
    ) -> Result<(), IngestReason> {
        if bytes.len() > MAX_BYTES {
            return Err(IngestReason::Oversize);
        }
        if self.poisoned {
            return Err(IngestReason::PoisonedAuthority);
        }
        let base = if fresh {
            None
        } else {
            self.delivery.as_ref().map(|d| d.frame.as_ref())
        };
        let resources = base
            .map(bmux_web_renderer::extract_resources)
            .transpose()
            .map_err(|_| IngestReason::Resources)?
            .unwrap_or_default();
        let frame = bmux_web_renderer::decode_binary_with_resources(&bytes, base, &resources)
            .map_err(IngestReason::decode)?;
        if frame.revision() == 0 {
            return Err(IngestReason::ZeroRevision);
        }
        if let Some(old) = &self.delivery
            && old.frame.generation() == frame.generation()
            && frame.revision() <= old.frame.revision()
        {
            return Err(IngestReason::StaleRevision);
        }
        // Detect full versus delta through the canonical decoder, not a parallel wire parser.
        let is_full = decode_binary(&bytes, None).is_ok();
        let full = Arc::new(OnceLock::new());
        if is_full {
            let _ = full.set(Ok(bytes.clone()));
        }
        let base = if is_full {
            None
        } else {
            base.map(|f| (f.generation().to_owned(), f.revision()))
        };
        self.delivery = Some(Delivery {
            frame: Arc::new(frame),
            full,
            patch: bytes,
            base,
        });
        // Live is derived from volatile frame admission; never persist a frame revision.
        self.authority.state = Phase::Live;
        Ok(())
    }
}
