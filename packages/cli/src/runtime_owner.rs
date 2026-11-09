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
    driver: Option<DriverThread>,
}
struct DriverThread {
    join: std::thread::JoinHandle<()>,
    // Disconnected only after the driver destroyed the runtime (or panicked).
    finished: std::sync::mpsc::Receiver<()>,
    pins: std::sync::Weak<RuntimePin>,
}
/// First notice while shutdown waits for retained execution pins.
const RELEASE_NOTICE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);
/// Repeat interval for subsequent notices.
const RELEASE_NOTICE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Primary runtime shutdown did not complete.
#[derive(Debug)]
pub enum RuntimeShutdownError {
    /// The lifetime driver thread panicked.
    Panicked,
    /// Retained execution owners outlived the shutdown bound. The runtime was
    /// abandoned, not destroyed: no code is unloaded and no referenced memory
    /// is freed; it is reclaimed only by process exit.
    Abandoned {
        waited: std::time::Duration,
        retained_pins: usize,
        /// Every unfinished plugin instance and its attributed holders.
        unfinished: Vec<bmux_plugin_runtime::OutstandingHolders>,
    },
}

impl std::fmt::Display for RuntimeShutdownError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Panicked => formatter.write_str("primary runtime driver panicked"),
            Self::Abandoned {
                waited,
                retained_pins,
                unfinished,
            } => {
                write!(
                    formatter,
                    "primary runtime abandoned after {}s with {retained_pins} retained runtime owner pin(s); {} unfinished plugin instance(s)",
                    waited.as_secs(),
                    unfinished.len()
                )?;
                for report in unfinished {
                    write!(formatter, "\n  blocked by {report}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for RuntimeShutdownError {}

impl DriverThread {
    /// Wait for actual runtime destruction for at most `bound`, never detaching
    /// cleanup early. Waits past the notice threshold are reported. When the
    /// bound expires, the retained owners are named (plugin instance, scope,
    /// creating site, label) and the runtime is abandoned rather than awaited
    /// forever; it is never destroyed while still referenced.
    fn join(self, bound: std::time::Duration) -> Result<(), RuntimeShutdownError> {
        let started = std::time::Instant::now();
        let mut notice_at = RELEASE_NOTICE_AFTER;
        loop {
            let remaining = notice_at.min(bound).saturating_sub(started.elapsed());
            match self.finished.recv_timeout(remaining) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if started.elapsed() >= bound => {
                    let error = RuntimeShutdownError::Abandoned {
                        waited: started.elapsed(),
                        retained_pins: self.pins.strong_count(),
                        unfinished: bmux_plugin_runtime::unfinished_instances(),
                    };
                    tracing::error!(%error, "primary runtime shutdown bound exceeded");
                    // Dropping the join handle detaches the driver thread, which
                    // keeps owning the runtime until process exit.
                    return Err(error);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let retained = self.pins.strong_count();
                    let waited = started.elapsed().as_secs();
                    tracing::warn!(
                        retained_pins = retained,
                        waited_secs = waited,
                        bound_secs = bound.as_secs(),
                        "primary runtime shutdown waiting for retained execution owners"
                    );
                    eprintln!(
                        "bmux: shutdown has waited {waited}s (bound {}s) for {retained} retained runtime owner pin(s); unresolved plugin cleanup keeps the primary runtime alive",
                        bound.as_secs()
                    );
                    notice_at = started.elapsed() + RELEASE_NOTICE_INTERVAL;
                }
            }
        }
        self.join.join().map_err(|_| RuntimeShutdownError::Panicked)
    }
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
        let (finished_tx, finished) = std::sync::mpsc::channel::<()>();
        let join = std::thread::Builder::new()
            .name("bmux-runtime-owner".into())
            .spawn(move || {
                // Multi-thread runtime workers continue driving all accepted
                // work while this non-runtime thread waits for owner release.
                let _ = released.recv();
                drop(runtime);
                drop(finished_tx);
            })?;
        let owner = RuntimeOwner(Arc::new(RuntimePin {
            handle,
            _keepalive: keepalive,
        }));
        let pins = Arc::downgrade(&owner.0);
        Ok(Self {
            owner: Some(owner),
            driver: Some(DriverThread {
                join,
                finished,
                pins,
            }),
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
    /// reactor shutdown, bounded by [`bmux_plugin_runtime::teardown_timeout`].
    /// Outstanding cleanup cannot be reported as shutdown.
    /// # Errors
    /// Returns an error if the lifetime driver panicked or retained owners
    /// outlived the bound (the error names them).
    pub fn shutdown(self) -> Result<(), RuntimeShutdownError> {
        self.shutdown_within(bmux_plugin_runtime::teardown_timeout())
    }

    /// [`Self::shutdown`] with an explicit bound.
    /// # Errors
    /// Returns an error if the lifetime driver panicked or retained owners
    /// outlived `bound` (the error names them).
    ///
    /// # Panics
    /// Panics if the driver's private thread invariant is broken. Shutdown
    /// consumes the driver, so it cannot be called twice through the public API.
    pub fn shutdown_within(
        mut self,
        bound: std::time::Duration,
    ) -> Result<(), RuntimeShutdownError> {
        self.owner.take();
        self.driver
            .take()
            .expect("runtime driver is open")
            .join(bound)
    }
}
impl Drop for RuntimeDriver {
    fn drop(&mut self) {
        self.owner.take();
        if let Some(driver) = self.driver.take() {
            // The root must be dropped outside async execution. An unresolved
            // retained execution blocks shutdown up to the bound, never unloads.
            if let Err(error) = driver.join(bmux_plugin_runtime::teardown_timeout()) {
                eprintln!("bmux: {error}");
            }
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
