//! Managed blocking crossterm input sources.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use bmux_tui::crossterm::read_event;
use bmux_tui::event::Event;
use tokio::sync::mpsc;

use crate::program::Program;
use crate::runtime::RuntimeHandle;

/// Managed bounded terminal event stream.
///
/// Crossterm input is a blocking operating-system read. Shutdown marks the source closed, but does
/// not join a thread currently blocked in the backend. The detached thread observes shutdown after
/// the next event or backend error. Bounded channel admission backpressures the reader rather than
/// allocating without limit.
pub struct ManagedTerminalInput {
    receiver: mpsc::Receiver<Result<Option<Event>, std::io::Error>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ManagedTerminalInput {
    /// Start a blocking terminal reader with the requested bounded event capacity.
    #[must_use]
    pub fn start(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                let result = read_event();
                let terminal = result.is_err();
                if sender.blocking_send(result).is_err() || terminal {
                    break;
                }
            }
        });
        Self {
            receiver,
            shutdown,
            thread: Some(thread),
        }
    }

    /// Create a deterministic bounded input stream from already available events.
    ///
    /// This is intended for headless tests and embedding environments that already decoded terminal
    /// input. Capacity is exactly the number of supplied entries, normalized to one when empty.
    ///
    /// # Panics
    ///
    /// Panics if the pre-sized channel cannot accept every supplied event.
    #[must_use]
    pub fn from_events(
        events: impl IntoIterator<Item = Result<Option<Event>, std::io::Error>>,
    ) -> Self {
        let events = events.into_iter().collect::<Vec<_>>();
        let (sender, receiver) = mpsc::channel(events.len().max(1));
        for event in events {
            sender
                .try_send(event)
                .expect("pre-sized deterministic terminal input channel");
        }
        drop(sender);
        Self {
            receiver,
            shutdown: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    /// Receive the next terminal event or reader error.
    pub async fn recv(&mut self) -> Option<Result<Option<Event>, std::io::Error>> {
        self.receiver.recv().await
    }

    /// Request reader shutdown.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Return the configured channel capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.receiver.max_capacity()
    }
}

impl Drop for ManagedTerminalInput {
    fn drop(&mut self) {
        self.request_shutdown();
        self.receiver.close();
        let _detached = self.thread.take();
    }
}

/// Managed terminal input reader forwarding events directly into a runtime.
///
/// Crossterm input is a blocking operating-system read. Shutdown marks the source closed, but does
/// not join a thread currently blocked in the backend. The detached thread observes shutdown after
/// the next event or backend error.
pub struct TerminalInput {
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TerminalInput {
    /// Start a blocking input reader using the current Tokio runtime to await bounded admission.
    /// Backend errors are converted to an application message by `map_error`.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    #[must_use]
    pub fn start<P>(
        handle: RuntimeHandle<P::Message>,
        map_error: impl Fn(std::io::Error) -> P::Message + Send + 'static,
    ) -> Self
    where
        P: Program,
    {
        Self::start_with::<P>(handle, map_error, read_event)
    }

    fn start_with<P>(
        handle: RuntimeHandle<P::Message>,
        map_error: impl Fn(std::io::Error) -> P::Message + Send + 'static,
        mut read: impl FnMut() -> std::io::Result<Option<Event>> + Send + 'static,
    ) -> Self
    where
        P: Program,
    {
        let runtime = tokio::runtime::Handle::current();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match read() {
                    Ok(Some(
                        event @ (Event::Resize(_)
                        | Event::Mouse(bmux_tui::event::MouseEvent {
                            kind: bmux_tui::event::MouseEventKind::Move,
                            ..
                        })),
                    )) => {
                        if handle.send_latest_terminal(event).is_err() {
                            break;
                        }
                    }
                    Ok(Some(event)) => {
                        if runtime.block_on(handle.send_terminal(event)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _result = runtime.block_on(handle.send(map_error(error)));
                        break;
                    }
                }
            }
        });
        Self {
            shutdown,
            thread: Some(thread),
        }
    }

    /// Request reader shutdown.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        self.request_shutdown();
        let _detached = self.thread.take();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::{HeadlessPresenter, Program, Runtime, RuntimeConfig, RuntimeEvent, Update};
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::event::Event;

    use super::{ManagedTerminalInput, TerminalInput};

    #[tokio::test]
    async fn terminal_input_forwards_key_events_into_runtime() {
        struct ExitProgram;

        impl Program for ExitProgram {
            type Message = ();
            type Error = std::convert::Infallible;

            fn update(
                &mut self,
                event: RuntimeEvent<Self::Message>,
            ) -> Result<Update<Self::Message>, Self::Error> {
                Ok(match event {
                    RuntimeEvent::Terminal(Event::Key(stroke))
                        if stroke.key == KeyCode::Char('q') =>
                    {
                        Update::exit()
                    }
                    RuntimeEvent::Terminal(_)
                    | RuntimeEvent::Message(())
                    | RuntimeEvent::Timer(_) => Update::none(),
                })
            }
        }

        let (runtime, handle) = Runtime::new(
            ExitProgram,
            HeadlessPresenter::default(),
            RuntimeConfig::default(),
        );
        let events = Arc::new(Mutex::new(VecDeque::from([
            Ok(Some(Event::Key(KeyStroke::simple(KeyCode::Char('q'))))),
            Ok(None),
        ])));
        let reader_events = Arc::clone(&events);
        let read_called = Arc::new(AtomicBool::new(false));
        let reader_called = Arc::clone(&read_called);
        let input = TerminalInput::start_with::<ExitProgram>(
            handle,
            |_| (),
            move || {
                reader_called.store(true, Ordering::Relaxed);
                reader_events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_front()
                    .unwrap_or(Ok(None))
            },
        );

        let wait_started = std::time::Instant::now();
        while !read_called.load(Ordering::Relaxed) {
            assert!(wait_started.elapsed() < Duration::from_secs(1));
            tokio::task::yield_now().await;
        }

        let output = tokio::time::timeout(Duration::from_secs(1), runtime.run())
            .await
            .expect("key event should exit runtime")
            .unwrap_or_else(|_| panic!("infallible runtime failed"));
        drop(input);

        assert!(output.stats.updates_completed >= 1);
    }

    #[test]
    fn managed_input_normalizes_zero_capacity() {
        let input = ManagedTerminalInput::start(0);
        assert_eq!(input.capacity(), 1);
    }
}
