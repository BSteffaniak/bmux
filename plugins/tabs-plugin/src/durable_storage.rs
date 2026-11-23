//! Tabs arrangement transactions span read/modify/write, not just file publication.
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

static ROOT: OnceLock<PathBuf> = OnceLock::new();
const CONTRACT: &[u8] = b"bmux.tabs/arrangements-writer-v2\n";
const CONTRACT_FILE: &str = "bmux.tabs.arrangements.writer";
const PERSONAL_VIEWS_CONTRACT: &[u8] = b"bmux.tabs/personal-views-writer-v1\n";
const PERSONAL_VIEWS_CONTRACT_FILE: &str = "bmux.tabs.personal-views.writer";

#[cfg(not(test))]
pub fn configure(data_dir: &str) -> Result<(), String> {
    validate_data_dir(data_dir)?;
    let root = Path::new(data_dir).join("plugin-storage");
    if ROOT.get_or_init(|| root.clone()) != &root {
        return Err("tabs durable installation changed inside one plugin instance".into());
    }
    Ok(())
}

// Unit service callers use in-memory storage and synthetic `/data` roots;
// lock behavior is tested directly with real temporary roots below.
#[cfg(test)]
pub fn configure(data_dir: &str) -> Result<(), String> {
    validate_data_dir(data_dir)
}

fn validate_data_dir(data_dir: &str) -> Result<(), String> {
    if !Path::new(data_dir).is_absolute() {
        return Err("tabs durable data directory must be nonempty and absolute".into());
    }
    Ok(())
}

/// Which on-disk writer contract a lock enforces: the contract file name and
/// the exact bytes every compatible writer agrees on.
struct WriterContract {
    file: &'static str,
    bytes: &'static [u8],
}

const ARRANGEMENTS_WRITER: WriterContract = WriterContract {
    file: CONTRACT_FILE,
    bytes: CONTRACT,
};

pub struct ArrangementMutation {
    local: Mutex<()>,
    name: &'static str,
    contract: WriterContract,
}
impl ArrangementMutation {
    pub const fn new() -> Self {
        Self {
            local: Mutex::new(()),
            name: "bmux.tabs.arrangements.lock",
            contract: ARRANGEMENTS_WRITER,
        }
    }

    pub const fn transfer() -> Self {
        Self {
            local: Mutex::new(()),
            name: "bmux.tabs.transfer.lock",
            contract: ARRANGEMENTS_WRITER,
        }
    }

    /// Serializes personal-view repository transactions. Independent of the
    /// shared arrangement lock and writer contract, so personal documents can
    /// evolve their format without touching the shared order's writers.
    pub const fn personal_views() -> Self {
        Self {
            local: Mutex::new(()),
            name: "bmux.tabs.personal-views.lock",
            contract: WriterContract {
                file: PERSONAL_VIEWS_CONTRACT_FILE,
                bytes: PERSONAL_VIEWS_CONTRACT,
            },
        }
    }

    pub fn lock(&self) -> Result<ArrangementGuard<'_>, String> {
        let local = self.local.lock().map_err(|_| "tab order lock poisoned")?;
        let Some(root) = ROOT.get() else {
            // Unit tests use fake callers without activating an installation.
            #[cfg(test)]
            return Ok(ArrangementGuard {
                _local: local,
                _file: None,
            });
            #[cfg(not(test))]
            return Err("tabs durable storage has not been activated".into());
        };
        let file = acquire_named(root, self.name, &self.contract, Duration::from_secs(2))?;
        Ok(ArrangementGuard {
            _local: local,
            _file: Some(file),
        })
    }
}

pub struct ArrangementGuard<'a> {
    _local: MutexGuard<'a, ()>,
    _file: Option<File>,
}

#[cfg(test)]
fn acquire(root: &Path, timeout: Duration) -> Result<File, String> {
    acquire_named(
        root,
        "bmux.tabs.arrangements.lock",
        &ARRANGEMENTS_WRITER,
        timeout,
    )
}

fn acquire_named(
    root: &Path,
    name: &str,
    writer: &WriterContract,
    timeout: Duration,
) -> Result<File, String> {
    std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(name))
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(format!(
                    "tabs arrangement authority busy or unavailable: {error}"
                ));
            }
        }
    }
    let contract = root.join(writer.file);
    match std::fs::read(&contract) {
        Ok(bytes) if bytes == writer.bytes => {}
        Ok(_) => {
            return Err(format!(
                "unsupported or corrupt tabs writer contract {}",
                writer.file
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bmux_plugin::filesystem::publish_new(&contract, writer.bytes)
                .map_err(|error| error.to_string())?;
        }
        Err(error) => return Err(error.to_string()),
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_handles_exclude_until_transaction_release() {
        let root =
            std::env::temp_dir().join(format!("bmux-tabs-authority-{}", uuid::Uuid::new_v4()));
        let first = acquire(&root, Duration::ZERO).unwrap();
        assert!(acquire(&root, Duration::ZERO).is_err());
        drop(first);
        let next = acquire(&root, Duration::ZERO).unwrap();
        drop(next);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn independently_opened_transactions_preserve_every_read_modify_write() {
        let root = std::env::temp_dir().join(format!("bmux-tabs-writers-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let state = root.join("counter");
        std::fs::write(&state, "0").unwrap();
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let root = root.clone();
                let state = state.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        let _transaction = acquire(&root, Duration::from_secs(2)).unwrap();
                        let value: u32 = std::fs::read_to_string(&state).unwrap().parse().unwrap();
                        std::fs::write(&state, (value + 1).to_string()).unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(std::fs::read_to_string(state).unwrap(), "80");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn personal_view_lock_has_its_own_lock_and_writer_contract() {
        let root =
            std::env::temp_dir().join(format!("bmux-tabs-personal-{}", uuid::Uuid::new_v4()));
        let personal = ArrangementMutation::personal_views();
        let held = acquire_named(&root, personal.name, &personal.contract, Duration::ZERO).unwrap();
        // The shared arrangement authority stays available while a personal
        // transaction runs, and each keeps its own contract file.
        drop(acquire(&root, Duration::ZERO).unwrap());
        assert!(acquire_named(&root, personal.name, &personal.contract, Duration::ZERO).is_err());
        drop(held);
        assert_eq!(
            std::fs::read(root.join(PERSONAL_VIEWS_CONTRACT_FILE)).unwrap(),
            PERSONAL_VIEWS_CONTRACT
        );
        assert_eq!(std::fs::read(root.join(CONTRACT_FILE)).unwrap(), CONTRACT);
        std::fs::write(root.join(PERSONAL_VIEWS_CONTRACT_FILE), b"future").unwrap();
        assert!(acquire_named(&root, personal.name, &personal.contract, Duration::ZERO).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incompatible_contract_is_not_replaced() {
        let root =
            std::env::temp_dir().join(format!("bmux-tabs-contract-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let contract = root.join("bmux.tabs.arrangements.writer");
        std::fs::write(&contract, b"future-writer").unwrap();
        assert!(acquire(&root, Duration::ZERO).is_err());
        assert_eq!(std::fs::read(contract).unwrap(), b"future-writer");
        std::fs::remove_dir_all(root).unwrap();
    }
}
