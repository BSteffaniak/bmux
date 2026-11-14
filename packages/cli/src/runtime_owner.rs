//! Ownership of the selected primary reactor, independent of command observers.
use std::sync::Arc;

/// A real runtime-owner pin, not proof of any caller or resource permission.
#[derive(Clone, Debug)]
pub struct RuntimeOwner(Arc<RuntimePin>);
#[derive(Debug)]
struct RuntimePin {
    handle: tokio::runtime::Handle,
    // The driver owns Runtime while any sender exists. Dropping the last sender
    // wakes the driver; Runtime is destroyed there, outside an async context.
    _keepalive: std::sync::mpsc::Sender<()>,
}

/// Root owner joins actual primary-runtime destruction. Never detaches cleanup.
pub struct RuntimeDriver {
    owner: Option<RuntimeOwner>,
    driver: Option<std::thread::JoinHandle<()>>,
}
impl RuntimeDriver {
    /// Transfer an already configured primary runtime to its lifetime driver.
    /// No second runtime is constructed.
    /// # Errors
    /// Returns an error if the lifetime driver cannot be started.
    pub fn new(runtime: tokio::runtime::Runtime) -> std::io::Result<Self> {
        if runtime.handle().runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return Err(std::io::Error::other(
                "primary runtime ownership requires a multi-thread driver",
            ));
        }
        let handle = runtime.handle().clone();
        let (keepalive, released) = std::sync::mpsc::channel();
        let driver = std::thread::Builder::new()
            .name("bmux-runtime-owner".into())
            .spawn(move || {
                // Multi-thread runtime workers continue driving all accepted
                // work while this non-runtime thread waits for owner release.
                let _ = released.recv();
                drop(runtime);
            })?;
        Ok(Self {
            owner: Some(RuntimeOwner(Arc::new(RuntimePin {
                handle,
                _keepalive: keepalive,
            }))),
            driver: Some(driver),
        })
    }
    /// Clone the actual runtime owner while this driver is open.
    ///
    /// # Panics
    /// Panics if the driver's private owner invariant is broken. Public methods
    /// cannot produce that state: shutdown consumes the driver.
    #[must_use]
    pub fn owner(&self) -> RuntimeOwner {
        self.owner.as_ref().expect("runtime driver is open").clone()
    }
    /// Release the root and wait for every retained execution pin, then actual
    /// reactor shutdown. Outstanding cleanup cannot be reported as shutdown.
    /// # Errors
    /// Returns an error if the lifetime driver panicked.
    ///
    /// # Panics
    /// Panics if the driver's private thread invariant is broken. Shutdown
    /// consumes the driver, so it cannot be called twice through the public API.
    pub fn shutdown(mut self) -> std::thread::Result<()> {
        self.owner.take();
        self.driver.take().expect("runtime driver is open").join()
    }
}
impl Drop for RuntimeDriver {
    fn drop(&mut self) {
        self.owner.take();
        if let Some(driver) = self.driver.take() {
            // The root must be dropped outside async execution. An unresolved
            // retained execution deliberately blocks shutdown, never unloads.
            let _ = driver.join();
        }
    }
}
#[derive(Debug, Clone)]
pub(crate) struct WeakRuntimeOwner(std::sync::Weak<RuntimePin>);
impl WeakRuntimeOwner {
    pub(crate) fn upgrade(&self) -> Option<RuntimeOwner> {
        self.0.upgrade().map(RuntimeOwner)
    }
}
impl RuntimeOwner {
    pub(crate) fn downgrade(&self) -> WeakRuntimeOwner {
        WeakRuntimeOwner(Arc::downgrade(&self.0))
    }
    #[must_use]
    pub fn handle(&self) -> &tokio::runtime::Handle {
        &self.0.handle
    }
    #[must_use]
    pub fn pin(&self) -> Arc<dyn std::any::Any + Send + Sync> {
        Arc::new(self.clone())
    }
    /// Correlate with the selected runtime before entering the CLI future.
    pub(crate) async fn run<T>(&self, work: impl std::future::Future<Output = T>) -> T {
        SELECTED_RUNTIME.scope(self.clone(), work).await
    }
}
tokio::task_local! {
    static SELECTED_RUNTIME: RuntimeOwner;
}
#[cfg(test)]
mod tests;

pub(crate) fn selected() -> Option<RuntimeOwner> {
    SELECTED_RUNTIME.try_with(Clone::clone).ok()
}
