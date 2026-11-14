//! Cluster-owned durable mutation coordination; no daemon-wide lifetime claim.
use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

static MUTATION: Mutex<()> = Mutex::new(());
const CONTRACT: &[u8] = b"bmux.cluster/durable-writer-v1\n";

pub fn configure(data_dir: &str) -> Result<(), String> {
    validate_data_dir(data_dir)
}

fn validate_data_dir(data_dir: &str) -> Result<(), String> {
    if !Path::new(data_dir).is_absolute() {
        return Err("cluster durable data directory must be nonempty and absolute".into());
    }
    Ok(())
}

thread_local! {
    // Nested synchronous membership readers can migrate under the same lease.
    // Rc retains the actual fence until the last nested guard drops, even when
    // callers explicitly release guards out of lexical order.
    static ACTIVE: RefCell<Weak<MutationLease>> = const { RefCell::new(Weak::new()) };
}

struct MutationLease {
    _local: MutexGuard<'static, ()>,
    _file: Option<File>,
    root: Option<PathBuf>,
}

pub struct MutationGuard {
    _lease: Rc<MutationLease>,
}

pub fn mutation_at(root: Option<&Path>) -> Result<MutationGuard, String> {
    let active_lease = ACTIVE.with(|active| active.borrow().upgrade());
    if let Some(lease) = active_lease {
        if lease.root.as_deref() != root {
            return Err("nested cluster mutation changed storage authority".into());
        }
        return Ok(MutationGuard { _lease: lease });
    }
    let local = MUTATION
        .lock()
        .map_err(|_| "cluster storage lock poisoned")?;
    let file = match root {
        Some(root) => Some(acquire(root, Duration::from_secs(2))?),
        #[cfg(test)]
        None => None,
        #[cfg(not(test))]
        None => return Err("cluster durable storage has not been activated".into()),
    };
    let lease = Rc::new(MutationLease {
        _local: local,
        _file: file,
        root: root.map(Path::to_path_buf),
    });
    ACTIVE.with(|active| *active.borrow_mut() = Rc::downgrade(&lease));
    Ok(MutationGuard { _lease: lease })
}

fn acquire(root: &Path, timeout: Duration) -> Result<File, String> {
    std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("bmux.cluster.mutation.lock"))
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
                    "cluster durable authority busy or unavailable: {error}"
                ));
            }
        }
    }
    let contract = root.join("bmux.cluster.writer");
    match std::fs::read(&contract) {
        Ok(bytes) if bytes == CONTRACT => {}
        Ok(_) => return Err("unsupported or corrupt cluster writer contract".into()),
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
    fn nested_mutation_cannot_switch_installations() {
        let directory =
            std::env::temp_dir().join(format!("bmux-cluster-scope-{}", uuid::Uuid::new_v4()));
        let other = directory.join("other");
        let outer = mutation_at(Some(&directory)).unwrap();
        assert!(mutation_at(Some(&other)).is_err());
        let same = mutation_at(Some(&directory)).unwrap();
        drop(same);
        drop(outer);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn mutation_ownership_releases_after_full_transaction() {
        let root =
            std::env::temp_dir().join(format!("bmux-cluster-authority-{}", uuid::Uuid::new_v4()));
        let mutation = acquire(&root, Duration::ZERO).unwrap();
        assert!(acquire(&root, Duration::ZERO).is_err());
        drop(mutation);
        let next = acquire(&root, Duration::ZERO).unwrap();
        drop(next);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn reentrant_read_side_migration_retains_outer_authority() {
        let outer = mutation_at(None).unwrap();
        let inner = mutation_at(None).unwrap();
        drop(outer);
        assert!(MUTATION.try_lock().is_err());
        drop(inner);
        assert!(ACTIVE.with(|active| active.borrow().upgrade().is_none()));
    }

    #[test]
    fn corrupt_contract_is_never_reinitialized() {
        let root =
            std::env::temp_dir().join(format!("bmux-cluster-contract-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let contract = root.join("bmux.cluster.writer");
        std::fs::write(&contract, b"corrupt").unwrap();
        assert!(acquire(&root, Duration::ZERO).is_err());
        assert_eq!(std::fs::read(contract).unwrap(), b"corrupt");
        std::fs::remove_dir_all(root).unwrap();
    }
}
