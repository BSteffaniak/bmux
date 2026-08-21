//! Managed asynchronous Crossterm input sources.

use bmux_tui::crossterm::CrosstermEventStream;
use bmux_tui::event::Event;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::program::Program;
use crate::runtime::RuntimeHandle;

/// Managed bounded terminal event stream.
pub struct ManagedTerminalInput {
    receiver: mpsc::Receiver<Result<Event, std::io::Error>>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl ManagedTerminalInput {
    /// Start an asynchronous terminal reader with the requested bounded event capacity.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    #[must_use]
    pub fn start(capacity: usize) -> Self {
        Self::start_stream(capacity, CrosstermEventStream::new())
    }

    fn start_stream<S>(capacity: usize, mut events: S) -> Self
    where
        S: futures_util::Stream<Item = Result<Event, std::io::Error>> + Send + Unpin + 'static,
    {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let (shutdown, mut shutdown_receiver) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                let result = tokio::select! {
                    biased;
                    changed = shutdown_receiver.changed() => {
                        if changed.is_err() || *shutdown_receiver.borrow() {
                            break;
                        }
                        continue;
                    }
                    result = futures_util::StreamExt::next(&mut events) => match result {
                        Some(result) => result,
                        None => break,
                    },
                };
                let terminal = result.is_err();
                let sent = tokio::select! {
                    biased;
                    changed = shutdown_receiver.changed() => {
                        if changed.is_err() || *shutdown_receiver.borrow() {
                            false
                        } else {
                            continue;
                        }
                    }
                    sent = sender.send(result) => sent.is_ok(),
                };
                if !sent || terminal {
                    break;
                }
            }
        });
        Self {
            receiver,
            shutdown,
            task: Some(task),
        }
    }

    /// Create a deterministic bounded input stream from already available events.
    ///
    /// Capacity is exactly the number of supplied entries, normalized to one when empty.
    ///
    /// # Panics
    ///
    /// Panics if the pre-sized channel cannot accept every supplied event.
    #[must_use]
    pub fn from_events(
        events: impl IntoIterator<Item = Result<Option<Event>, std::io::Error>>,
    ) -> Self {
        let events = events
            .into_iter()
            .filter_map(Result::transpose)
            .collect::<Vec<_>>();
        let (sender, receiver) = mpsc::channel(events.len().max(1));
        for event in events {
            sender
                .try_send(event)
                .expect("pre-sized deterministic terminal input channel");
        }
        drop(sender);
        let (shutdown, _) = watch::channel(false);
        Self {
            receiver,
            shutdown,
            task: None,
        }
    }

    /// Receive the next terminal event or reader error.
    pub async fn recv(&mut self) -> Option<Result<Option<Event>, std::io::Error>> {
        self.receiver.recv().await.map(|event| event.map(Some))
    }

    /// Request reader shutdown.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Request shutdown and await complete event-reader cancellation.
    pub async fn shutdown(&mut self) {
        self.request_shutdown();
        self.receiver.close();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
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
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Managed terminal input reader forwarding events directly into a runtime.
pub struct TerminalInput {
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl TerminalInput {
    /// Start an event-driven input reader forwarding into bounded runtime admission.
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
        Self::start_stream::<P, _>(handle, map_error, CrosstermEventStream::new())
    }

    fn start_stream<P, S>(
        handle: RuntimeHandle<P::Message>,
        map_error: impl Fn(std::io::Error) -> P::Message + Send + 'static,
        mut events: S,
    ) -> Self
    where
        P: Program,
        S: futures_util::Stream<Item = Result<Event, std::io::Error>> + Send + Unpin + 'static,
    {
        let (shutdown, mut shutdown_receiver) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    biased;
                    changed = shutdown_receiver.changed() => {
                        if changed.is_err() || *shutdown_receiver.borrow() {
                            break;
                        }
                        continue;
                    }
                    result = futures_util::StreamExt::next(&mut events) => match result {
                        Some(Ok(event)) => event,
                        Some(Err(error)) => {
                            let message = map_error(error);
                            let _ = handle.send(message).await;
                            break;
                        }
                        None => break,
                    },
                };
                if matches!(
                    event,
                    Event::Resize(_)
                        | Event::Mouse(bmux_tui::event::MouseEvent {
                            kind: bmux_tui::event::MouseEventKind::Move,
                            ..
                        })
                ) {
                    if handle.send_latest_terminal(event).is_err() {
                        break;
                    }
                    continue;
                }
                let admitted = tokio::select! {
                    biased;
                    changed = shutdown_receiver.changed() => {
                        !(changed.is_err() || *shutdown_receiver.borrow())
                    }
                    result = handle.send_terminal(event) => result.is_ok(),
                };
                if !admitted {
                    break;
                }
            }
        });
        Self {
            shutdown,
            task: Some(task),
        }
    }

    /// Request reader shutdown.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Request shutdown and await complete event-reader cancellation.
    pub async fn shutdown(&mut self) {
        self.request_shutdown();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        self.request_shutdown();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::event::Event;
    use futures_util::StreamExt;

    use crate::{HeadlessPresenter, Program, Runtime, RuntimeConfig, RuntimeEvent, Update};

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
        let events =
            futures_util::stream::iter([Ok(Event::Key(KeyStroke::simple(KeyCode::Char('q'))))])
                .chain(futures_util::stream::pending());
        let mut input = TerminalInput::start_stream::<ExitProgram, _>(handle, |_| (), events);

        let output = tokio::time::timeout(Duration::from_secs(1), runtime.run())
            .await
            .expect("key event should exit runtime")
            .unwrap_or_else(|_| panic!("infallible runtime failed"));
        input.shutdown().await;

        assert!(output.stats.updates_completed >= 1);
    }

    #[tokio::test]
    async fn terminal_input_shutdown_cancels_pending_event_wait() {
        let (runtime, handle) = Runtime::new(
            ExitNeverProgram,
            HeadlessPresenter::default(),
            RuntimeConfig::default(),
        );
        let mut input = TerminalInput::start_stream::<ExitNeverProgram, _>(
            handle,
            |_| (),
            futures_util::stream::pending(),
        );

        tokio::time::timeout(Duration::from_secs(1), input.shutdown())
            .await
            .expect("shutdown should cancel pending event wait");
        drop(runtime);
    }

    struct ExitNeverProgram;

    impl Program for ExitNeverProgram {
        type Message = ();
        type Error = std::convert::Infallible;

        fn update(
            &mut self,
            _event: RuntimeEvent<Self::Message>,
        ) -> Result<Update<Self::Message>, Self::Error> {
            Ok(Update::none())
        }
    }

    #[test]
    fn managed_input_normalizes_zero_capacity() {
        let input = ManagedTerminalInput::from_events([]);
        assert_eq!(input.capacity(), 1);
    }
}
