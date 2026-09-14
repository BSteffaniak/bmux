//! Reentrant shared borrows for synchronous plugin-to-plugin service calls.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::RwLock;

thread_local! {
    static READS: RefCell<BTreeMap<usize, *const ()>> = const { RefCell::new(BTreeMap::new()) };
}

struct ActiveRead(usize);
impl Drop for ActiveRead {
    fn drop(&mut self) {
        READS.with(|reads| {
            reads.borrow_mut().remove(&self.0);
        });
    }
}

/// Execute against a shared instance. Nested calls on this thread borrow the
/// already-held read guard, so a queued event writer cannot deadlock re-entry.
/// Other threads still acquire the ordinary lock and writers remain exclusive.
pub(super) fn with_service_read<P: 'static, R>(
    instance: &'static RwLock<P>,
    call: impl FnOnce(&P) -> R,
) -> Result<R, ()> {
    let key = std::ptr::from_ref(instance).addr();
    let existing = READS.with(|reads| reads.borrow().get(&key).copied());
    if let Some(pointer) = existing {
        // SAFETY: the entry exists only during the outer call below, while its
        // read guard is alive. TLS excludes other threads; the lock address
        // identifies the same static RwLock<P>. The callback cannot retain a
        // reference beyond this borrow. Unwinding removes the entry before the
        // guard drops, and only shared references are ever exposed.
        return Ok(call(unsafe { &*pointer.cast::<P>() }));
    }
    let guard = instance.read().map_err(|_| ())?;
    READS.with(|reads| {
        reads
            .borrow_mut()
            .insert(key, std::ptr::from_ref(&*guard).cast());
    });
    let _active = ActiveRead(key);
    Ok(call(&guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    #[test]
    fn nested_read_completes_while_writer_waits() {
        let instance: &'static RwLock<i32> = Box::leak(Box::new(RwLock::new(7)));
        let waiting = Arc::new(AtomicBool::new(false));
        let writer = with_service_read(instance, |value| {
            let waiting_writer = waiting.clone();
            let writer = std::thread::spawn(move || {
                waiting_writer.store(true, Ordering::Release);
                *instance.write().unwrap() = 9;
            });
            while !waiting.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            std::thread::sleep(Duration::from_millis(20));
            assert_eq!(
                with_service_read(instance, |nested| *nested).unwrap(),
                *value
            );
            writer
        })
        .unwrap();
        writer.join().unwrap();
        assert_eq!(*instance.read().unwrap(), 9);
    }
}
