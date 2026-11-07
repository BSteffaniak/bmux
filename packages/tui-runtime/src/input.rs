//! Managed asynchronous terminal input sources.
//!
//! On Unix every source shares one reader: [`RawInput`] on its own thread,
//! blocked in event-driven waits (input, `SIGWINCH`, or a shutdown wake) and
//! handing packets to async consumers through bounded channels. Other
//! platforms read through Crossterm.

use bmux_tui::crossterm::CrosstermEventStream;
use bmux_tui::event::Event;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::program::Program;
use crate::runtime::RuntimeHandle;
#[cfg(unix)]
use bmux_tui::crossterm::input::{InputWaker, RawInput, RawInputPacket};

/// Event tag delivered as [`Event::User`] when [`ManagedTerminalInput::terminal_colors`]
/// changes after the terminal answers [`bmux_tui::capabilities::TerminalColorReport::QUERY`].
pub const TERMINAL_COLORS_EVENT: &str = "bmux.terminal-colors";

/// Latest terminal color replies observed by a reader.
type TerminalColorState =
    std::sync::Arc<std::sync::Mutex<bmux_tui::capabilities::TerminalColorReport>>;

/// Shared, read-only view of a reader's terminal color replies.
///
/// Programs keep a clone and read it when a [`TERMINAL_COLORS_EVENT`]
/// notification arrives. It stays empty until the application writes
/// [`bmux_tui::capabilities::TerminalColorReport::QUERY`] and the terminal answers.
#[derive(Debug, Clone, Default)]
pub struct TerminalColorsHandle(TerminalColorState);

impl TerminalColorsHandle {
    /// Latest terminal-reported colors.
    #[must_use]
    pub fn get(&self) -> bmux_tui::capabilities::TerminalColorReport {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A [`RawInput`] on its own thread, blocked in [`RawInput::wait`] and handing
/// each packet to a delivery callback. Stopping wakes the wait, so shutdown is
/// prompt without any polling; dropping stops and joins the thread, releasing
/// terminal input ownership before returning.
#[cfg(unix)]
struct RawReader {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    waker: InputWaker,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl RawReader {
    /// Read until `deliver` returns `false` or the reader stops. Errors are
    /// delivered and end the reader.
    ///
    /// # Panics
    ///
    /// Panics if the operating system cannot start a thread.
    fn spawn(
        mut reader: RawInput,
        mut deliver: impl FnMut(std::io::Result<RawInputPacket>) -> bool + Send + 'static,
    ) -> Self {
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waker = reader.waker();
        let stopped = std::sync::Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("bmux-terminal-input".to_owned())
            .spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    match reader.wait() {
                        Ok(Some(packet)) => {
                            if !deliver(Ok(packet)) {
                                break;
                            }
                        }
                        // Woken: check whether to stop.
                        Ok(None) => {}
                        Err(error) => {
                            deliver(Err(error));
                            break;
                        }
                    }
                }
            })
            .expect("spawn terminal input thread");
        Self {
            stop,
            waker,
            thread: Some(thread),
        }
    }

    fn request_stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        self.waker.wake();
    }

    /// Stop and await the thread without blocking the async runtime.
    async fn shutdown(&mut self) {
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}

#[cfg(unix)]
impl Drop for RawReader {
    fn drop(&mut self) {
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Decoded terminal events from the stdout tty through a [`RawReader`].
#[cfg(unix)]
struct RawEventStream {
    receiver: mpsc::Receiver<Result<Event, std::io::Error>>,
    _reader: Option<RawReader>,
}

#[cfg(unix)]
impl RawEventStream {
    /// Open the stdout tty, or yield the failure as the stream's only item.
    ///
    /// With `colors`, terminal color replies update that state and surface as a
    /// [`TERMINAL_COLORS_EVENT`] notification; without it they are discarded
    /// like other opaque input.
    fn open(capacity: usize, colors: Option<TerminalColorState>) -> Self {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        match RawInput::open_stdout_tty() {
            Ok(reader) => {
                let reader = RawReader::spawn(reader, move |packet| match packet {
                    Ok(RawInputPacket::Opaque(frame))
                        if colors
                            .as_ref()
                            .is_some_and(|colors| observe_colors(colors, &frame)) =>
                    {
                        sender
                            .blocking_send(Ok(Event::User(TERMINAL_COLORS_EVENT.to_owned())))
                            .is_ok()
                    }
                    Ok(packet) => packet
                        .into_event()
                        .is_none_or(|event| sender.blocking_send(Ok(event)).is_ok()),
                    Err(error) => {
                        let _ = sender.blocking_send(Err(error));
                        false
                    }
                });
                Self {
                    receiver,
                    _reader: Some(reader),
                }
            }
            Err(error) => {
                let _ = sender.try_send(Err(error));
                Self {
                    receiver,
                    _reader: None,
                }
            }
        }
    }
}

/// Record a color reply; `true` only when the frame was a reply that changed the report.
///
/// Recognized replies that leave the report unchanged are still consumed (never
/// treated as input) but suppress the redundant notification.
#[cfg(unix)]
fn observe_colors(colors: &TerminalColorState, frame: &[u8]) -> bool {
    let mut report = colors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = *report;
    report.observe(frame) && *report != before
}

#[cfg(unix)]
impl futures_util::Stream for RawEventStream {
    type Item = Result<Event, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

#[cfg(unix)]
impl Drop for RawEventStream {
    fn drop(&mut self) {
        // Fail any pending delivery so the reader thread can be joined.
        self.receiver.close();
    }
}

type PrivateRouter<M> = (
    super::private_input::PrivateInputGate,
    Box<dyn Fn(super::private_input::PrivateInputRoute) -> M + Send>,
);

/// Bounded retrievable opaque packets for a single input connection.
#[cfg(unix)]
#[derive(Clone, Default)]
pub struct OpaqueInputQueue(std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<Vec<u8>>>>);
#[cfg(unix)]
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
#[cfg(unix)]
pub struct ManagedRawInput {
    receiver: mpsc::Receiver<std::io::Result<RawInputPacket>>,
    reader: Option<RawReader>,
}
#[cfg(unix)]
impl ManagedRawInput {
    fn start(capacity: usize) -> std::io::Result<Self> {
        let reader = RawInput::open_stdout_tty()?;
        Ok(Self::from_reader(reader, capacity))
    }
    /// Adopt an exclusive raw reader, retaining the same bounded admission contract.
    ///
    /// # Panics
    ///
    /// Panics if the operating system cannot start the reader thread.
    #[must_use]
    pub fn from_reader(reader: RawInput, capacity: usize) -> Self {
        // Explicit cap bounds queued opaque bytes even for untrusted capacities.
        let (sender, receiver) = mpsc::channel(capacity.clamp(1, 256));
        let reader = RawReader::spawn(reader, move |packet| {
            let terminal = packet.is_err();
            sender.blocking_send(packet).is_ok() && !terminal
        });
        Self {
            receiver,
            reader: Some(reader),
        }
    }
    /// Receive an ordinary event or unchanged opaque packet, in read order.
    pub async fn recv(&mut self) -> Option<std::io::Result<RawInputPacket>> {
        self.receiver.recv().await
    }
    /// Stop admission and await reader ownership release, even under channel backpressure.
    pub async fn shutdown(&mut self) {
        self.receiver.close();
        if let Some(mut reader) = self.reader.take() {
            reader.shutdown().await;
        }
    }
}
#[cfg(unix)]
impl Drop for ManagedRawInput {
    fn drop(&mut self) {
        // Fail any pending delivery; the reader then stops and joins.
        self.receiver.close();
    }
}

/// Managed bounded terminal event stream.
pub struct ManagedTerminalInput {
    receiver: mpsc::Receiver<Result<Event, std::io::Error>>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    colors: TerminalColorState,
}

impl ManagedTerminalInput {
    /// Open an opt-in sole raw packet owner. Legacy constructors are unchanged.
    /// Never combine this owner with another terminal reader.
    /// # Errors
    /// Returns tty identity/open errors.
    #[cfg(unix)]
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
        let colors = TerminalColorState::default();
        #[cfg(unix)]
        let events = RawEventStream::open(capacity, Some(std::sync::Arc::clone(&colors)));
        #[cfg(not(unix))]
        let events = CrosstermEventStream::new();
        Self::start_stream(capacity, events, colors)
    }

    /// Latest terminal-reported colors.
    ///
    /// Empty until the application writes
    /// [`bmux_tui::capabilities::TerminalColorReport::QUERY`] and the terminal
    /// answers; each change is announced by an [`Event::User`] carrying
    /// [`TERMINAL_COLORS_EVENT`]. Terminals that ignore the query leave every
    /// field `None`.
    #[must_use]
    pub fn terminal_colors(&self) -> bmux_tui::capabilities::TerminalColorReport {
        *self
            .colors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn start_stream<S>(capacity: usize, mut events: S, colors: TerminalColorState) -> Self
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
            colors,
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
            colors: TerminalColorState::default(),
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
    colors: TerminalColorsHandle,
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
        Self::start_with_colors::<P>(handle, map_error, TerminalColorsHandle::default())
    }

    /// Start like [`Self::start`], recording terminal color replies into `colors`.
    ///
    /// Lets applications create the shared handle before the runtime owns their
    /// program state.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    #[must_use]
    pub fn start_with_colors<P>(
        handle: RuntimeHandle<P::Message>,
        map_error: impl Fn(std::io::Error) -> P::Message + Send + 'static,
        colors: TerminalColorsHandle,
    ) -> Self
    where
        P: Program,
    {
        // Bounded like runtime admission: the reader waits rather than buffering.
        #[cfg(unix)]
        let events = RawEventStream::open(64, Some(std::sync::Arc::clone(&colors.0)));
        #[cfg(not(unix))]
        let events = CrosstermEventStream::new();
        let mut input = Self::start_stream::<P, _>(handle, map_error, events);
        input.colors = colors;
        input
    }

    /// Shared view of terminal color replies, announced by [`TERMINAL_COLORS_EVENT`].
    #[must_use]
    pub fn terminal_colors(&self) -> TerminalColorsHandle {
        self.colors.clone()
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
            colors: TerminalColorsHandle::default(),
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

    #[cfg(unix)]
    #[test]
    fn color_replies_notify_only_when_the_report_changes() {
        let colors = super::TerminalColorState::default();
        let reply = b"\x1b]11;rgb:0000/0000/0000\x1b\\";
        assert!(super::observe_colors(&colors, reply));
        assert!(!super::observe_colors(&colors, reply), "unchanged reply");
        assert!(!super::observe_colors(&colors, b"\x1b]52;c;aGk=\x07"));
        assert!(super::observe_colors(
            &colors,
            b"\x1b]11;rgb:ffff/ffff/ffff\x07"
        ));
        let report = *colors.lock().unwrap();
        assert_eq!(
            report.background.map(|color| [color.r, color.g, color.b]),
            Some([255; 3])
        );
    }

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
