//! Tabs arrangement transactions span read/modify/write, not just file publication.
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

static ROOT: OnceLock<PathBuf> = OnceLock::new();
const CONTRACT: &[u8] = b"bmux.tabs/arrangements-writer-v2\n";

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

pub struct ArrangementMutation {
    local: Mutex<()>,
    name: &'static str,
}
impl ArrangementMutation {
    pub const fn new() -> Self {
        Self {
            local: Mutex::new(()),
            name: "bmux.tabs.arrangements.lock",
        }
    }

    pub const fn transfer() -> Self {
        Self {
            local: Mutex::new(()),
            name: "bmux.tabs.transfer.lock",
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
        let file = acquire_named(root, self.name, Duration::from_secs(2))?;
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
    acquire_named(root, "bmux.tabs.arrangements.lock", timeout)
}

fn acquire_named(root: &Path, name: &str, timeout: Duration) -> Result<File, String> {
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
    let contract = root.join("bmux.tabs.arrangements.writer");
    match std::fs::read(&contract) {
        Ok(bytes) if bytes == CONTRACT => {}
        Ok(_) => return Err("unsupported or corrupt tabs arrangement writer contract".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bmux_plugin::filesystem::publish_new(&contract, CONTRACT)
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
