//! Managed asynchronous Crossterm input sources.

use bmux_tui::crossterm::CrosstermEventStream;
use bmux_tui::event::Event;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::program::Program;
use crate::runtime::RuntimeHandle;

type PrivateRouter<M> = (
    super::private_input::PrivateInputGate,
    Box<dyn Fn(super::private_input::PrivateInputRoute) -> M + Send>,
);

/// Bounded retrievable opaque packets for a single input connection.
#[cfg(target_os = "linux")]
#[derive(Clone, Default)]
pub struct OpaqueInputQueue(std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<Vec<u8>>>>);
#[cfg(target_os = "linux")]
impl OpaqueInputQueue {
    /// Retain unchanged bytes, rejecting overflow instead of dropping old packets.
    /// # Errors
    /// Returns a budget error at 64 packets or 64 KiB total.
    pub fn push(&self, bytes: Vec<u8>) -> std::io::Result<()> {
        let mut queue = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.len() >= 64
            || queue
                .iter()
                .map(Vec::len)
                .sum::<usize>()
                .saturating_add(bytes.len())
                > 65536
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                "opaque input queue budget exceeded",
            ));
        }
        queue.push_back(bytes);
        drop(queue);
        Ok(())
    }
    /// Whether no opaque packets remain retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Retrieve the oldest unmatched packet unchanged.
    pub fn take(&self) -> Option<Vec<u8>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }
}

/// Bounded opt-in packet reader. No Crossterm reader is constructed on this path.
#[cfg(target_os = "linux")]
pub struct ManagedRawInput {
    receiver: mpsc::Receiver<std::io::Result<bmux_tui::crossterm::input::LinuxInputPacket>>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}
#[cfg(target_os = "linux")]
impl ManagedRawInput {
    fn start(capacity: usize) -> std::io::Result<Self> {
        let reader = bmux_tui::crossterm::input::RawInput::open_stdout_tty()?;
        Ok(Self::from_reader(reader, capacity))
    }
    /// Adopt an exclusive raw reader, retaining the same bounded admission contract.
    #[must_use]
    pub fn from_reader(mut reader: bmux_tui::crossterm::input::RawInput, capacity: usize) -> Self {
        // Explicit cap bounds queued opaque bytes even for untrusted capacities.
        let (sender, receiver) = mpsc::channel(capacity.clamp(1, 256));
        let (shutdown, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                if *stopped.borrow() {
                    break;
                }
                let packet = match reader.read_ready(std::time::Instant::now()) {
                    Ok(Some(packet)) => Ok(packet),
                    Ok(None) => {
                        tokio::select! { biased; _ = stopped.changed() => break,
                        () = tokio::time::sleep(std::time::Duration::from_millis(2)) => {} }
                        continue;
                    }
                    Err(error) => Err(error),
                };
                let terminal = packet.is_err();
                tokio::select! { biased; _ = stopped.changed() => break,
                result = sender.send(packet) => if result.is_err() { break; } }
                if terminal {
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
    /// Receive an ordinary event or unchanged opaque packet, in read order.
    pub async fn recv(
        &mut self,
    ) -> Option<std::io::Result<bmux_tui::crossterm::input::LinuxInputPacket>> {
        self.receiver.recv().await
    }
    /// Stop admission and await reader ownership release, even under channel backpressure.
    pub async fn shutdown(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
#[cfg(target_os = "linux")]
impl Drop for ManagedRawInput {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Managed bounded terminal event stream.
pub struct ManagedTerminalInput {
    receiver: mpsc::Receiver<Result<Event, std::io::Error>>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl ManagedTerminalInput {
    /// Open an opt-in sole Linux raw packet owner. Legacy constructors are unchanged.
    /// Never combine this owner with another terminal reader.
    /// # Errors
    /// Returns tty identity/open errors.
    #[cfg(target_os = "linux")]
    pub fn start_raw_packets(capacity: usize) -> std::io::Result<ManagedRawInput> {
        ManagedRawInput::start(capacity)
    }

    /// Start an asynchronous terminal reader with the requested bounded event capacity.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    #[must_use]
    pub fn start(capacity: usize) -> Self {
        #[cfg(target_os = "linux")]
        {
            let (sender, receiver) = mpsc::channel(capacity.max(1));
            let (shutdown, mut stopped) = watch::channel(false);
            let task = tokio::spawn(async move {
                let mut reader = match bmux_tui::crossterm::input::RawInput::open_stdout_tty() {
                    Ok(reader) => reader,
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                };
                loop {
                    if *stopped.borrow() {
                        break;
                    }
                    match reader.read_ready(std::time::Instant::now()) {
                        Ok(Some(packet)) => {
                            if let Some(event) = packet.into_event() {
                                tokio::select! { biased; _ = stopped.changed() => break,
                                result = sender.send(Ok(event)) => if result.is_err() { break; } }
                            }
                        }
                        Ok(None) => {
                            tokio::select! { biased; _ = stopped.changed() => break,
                            () = tokio::time::sleep(std::time::Duration::from_millis(2)) => {} }
                        }
                        Err(error) => {
                            let _ = sender.send(Err(error)).await;
                            break;
                        }
                    }
                }
            });
            Self {
                receiver,
                shutdown,
                task: Some(task),
            }
        }
        #[cfg(not(target_os = "linux"))]
        Self::start_stream(capacity, CrosstermEventStream::new())
    }

    /// Linux single-reader implementation is used by production; stream injection
    /// remains available for non-Linux backends and deterministic runtime tests.
    #[cfg(not(target_os = "linux"))]
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
        #[cfg(target_os = "linux")]
        {
            let reader = bmux_tui::crossterm::input::RawInput::open_stdout_tty();
            let events = Box::pin(futures_util::stream::unfold(
                Some(reader),
                |state| async move {
                    let mut reader = match state? {
                        Ok(reader) => reader,
                        Err(error) => return Some((Err(error), None)),
                    };
                    loop {
                        match reader.read_ready(std::time::Instant::now()) {
                            Ok(Some(packet)) => {
                                if let Some(event) = packet.into_event() {
                                    return Some((Ok(event), Some(Ok(reader))));
                                }
                            }
                            Ok(None) => {
                                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                            }
                            Err(error) => return Some((Err(error), None)),
                        }
                    }
                },
            ));
            Self::start_stream::<P, _>(handle, map_error, events)
        }
        #[cfg(not(target_os = "linux"))]
        Self::start_stream::<P, _>(handle, map_error, CrosstermEventStream::new())
    }

    /// Start private-capable ingress. `gate` routes before ordinary runtime admission.
    /// `map_private` must consume secret outcomes without persistence, tracing or cloning.
    /// Capture suppression must cover enclosing terminals; absent support fails closed.
    ///
    /// # Panics
    /// Panics when called outside a Tokio runtime.
    #[must_use]
    pub fn start_private<P>(
        handle: RuntimeHandle<P::Message>,
        map_error: impl Fn(std::io::Error) -> P::Message + Send + 'static,
        gate: super::private_input::PrivateInputGate,
        map_private: impl Fn(super::private_input::PrivateInputRoute) -> P::Message + Send + 'static,
    ) -> Self
    where
        P: Program,
    {
        Self::start_routed_stream::<P, _>(
            handle,
            map_error,
            CrosstermEventStream::new(),
            Some((gate, Box::new(map_private))),
        )
    }

    fn start_stream<P, S>(
        handle: RuntimeHandle<P::Message>,
        map_error: impl Fn(std::io::Error) -> P::Message + Send + 'static,
        events: S,
    ) -> Self
    where
        P: Program,
        S: futures_util::Stream<Item = Result<Event, std::io::Error>> + Send + Unpin + 'static,
    {
        Self::start_routed_stream::<P, S>(handle, map_error, events, None)
    }

    fn start_routed_stream<P, S>(
        handle: RuntimeHandle<P::Message>,
        map_error: impl Fn(std::io::Error) -> P::Message + Send + 'static,
        mut events: S,
        private: Option<PrivateRouter<P::Message>>,
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
                let event = if let Some((gate, map_private)) = &private {
                    match gate.route(event) {
                        super::private_input::PrivateInputRoute::Public(event) => event,
                        super::private_input::PrivateInputRoute::Consumed => continue,
                        outcome => {
                            if handle.send(map_private(outcome)).await.is_err() {
                                break;
                            }
                            continue;
                        }
                    }
                } else {
                    event
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
