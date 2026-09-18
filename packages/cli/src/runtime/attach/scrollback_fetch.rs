//! One bounded history read at a time, independent of the input loop.
use bmux_attach_pipeline::{CapturedHistoryAnchor, PaneScrollbackWindow, ScrollbackPin};
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    Oldest,
    Newest,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub boundary: Option<Boundary>,
    pub session: Uuid,
    pub pane: Uuid,
    pub pin: Option<ScrollbackPin>,
    pub offset: usize,
    pub width: usize,
    pub rows: usize,
    pub total: Option<u64>,
    pub anchor: Option<CapturedHistoryAnchor>,
    pub delta: isize,
}

impl Request {
    pub(super) fn projection(self) -> (usize, Option<CapturedHistoryAnchor>, isize) {
        match self.boundary {
            Some(Boundary::Oldest) => (
                self.width,
                self.pin
                    .and_then(|pin| pin.capture)
                    .map(|capture| CapturedHistoryAnchor {
                        capture_id: capture.identity,
                        line_index: 0,
                        column: 0,
                    }),
                0,
            ),
            Some(Boundary::Newest) => (self.width, None, 0),
            None => (self.width, self.anchor, self.delta),
        }
    }

    pub(super) fn neighbors(self, window: &PaneScrollbackWindow) -> Vec<Self> {
        if self.pin.is_none() || self.rows == 0 {
            return Vec::new();
        }
        let Some(anchor) = window.row_anchors.last().copied() else {
            return Vec::new();
        };
        let mut requests = Vec::with_capacity(2);
        for offset in [
            window
                .scrollback_offset
                .saturating_add(self.rows)
                .min(window.max_scrollback_offset),
            window.scrollback_offset.saturating_sub(self.rows),
        ] {
            if offset == window.scrollback_offset {
                continue;
            }
            let distance =
                isize::try_from(offset.abs_diff(window.scrollback_offset)).unwrap_or(isize::MAX);
            requests.push(Self {
                boundary: None,
                offset,
                anchor: Some(anchor),
                delta: if offset > window.scrollback_offset {
                    distance
                } else {
                    -distance
                },
                ..self
            });
        }
        requests
    }

    /// Bound queued work by both screens and distance, closest first. This is
    /// speculative capture hydration, never an alternate navigation authority.
    pub(super) fn warm_range(self, window: &PaneScrollbackWindow) -> Vec<Self> {
        let mut requests = self.neighbors(window);
        let Some(anchor) = window.row_anchors.last().copied() else {
            return requests;
        };
        if self.pin.is_none() || self.rows == 0 {
            return requests;
        }
        for screens in 2_usize..=16 {
            let distance = self.rows.saturating_mul(screens);
            if distance > 2048 {
                break;
            }
            for offset in [
                window
                    .scrollback_offset
                    .saturating_add(distance)
                    .min(window.max_scrollback_offset),
                window.scrollback_offset.saturating_sub(distance),
            ] {
                if offset == window.scrollback_offset
                    || requests.iter().any(|request| request.offset == offset)
                {
                    continue;
                }
                let delta = isize::try_from(offset.abs_diff(window.scrollback_offset))
                    .unwrap_or(isize::MAX);
                requests.push(Self {
                    boundary: None,
                    offset,
                    anchor: Some(anchor),
                    delta: if offset > window.scrollback_offset {
                        delta
                    } else {
                        -delta
                    },
                    ..self
                });
            }
        }
        requests
    }

    /// Navigation may supersede presentation without invalidating immutable
    /// content. Mutable live windows cannot use this admission shortcut.
    pub(super) fn reusable_capture(
        self,
        session: Uuid,
        pin: Option<ScrollbackPin>,
        geometry: Option<(usize, usize)>,
    ) -> bool {
        self.pin.is_some()
            && self.session == session
            && self.pin == pin
            && geometry == Some((self.width, self.rows))
    }
}

pub enum FetchError {
    Unavailable,
    Failed(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("capture viewport unavailable"),
            Self::Failed(error) => f.write_str(error),
        }
    }
}
impl std::fmt::Debug for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::error::Error for FetchError {}
impl From<String> for FetchError {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}
impl From<&str> for FetchError {
    fn from(error: &str) -> Self {
        Self::Failed(error.into())
    }
}

pub(super) enum Outcome {
    Ready(PaneScrollbackWindow),
    Unavailable,
    Failed(String),
}

pub(super) struct Fetch {
    pub prefetch: bool,
    pub request: Request,
    pub task: tokio::task::JoinHandle<Outcome>,
}

impl Drop for Fetch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) async fn fetch(
    client: bmux_plugin::AsyncServiceClient,
    request: Request,
    cache: std::sync::Arc<tokio::sync::Mutex<crate::pane_runtime_client::CapturedHistoryCache>>,
) -> Outcome {
    let mut client = client.with_backpressure();
    match fetch_with_client(&mut client, request, cache).await {
        Ok(window) => Outcome::Ready(window),
        Err(FetchError::Unavailable) => Outcome::Unavailable,
        Err(FetchError::Failed(error)) => Outcome::Failed(error),
    }
}

pub async fn fetch_with_client(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    request: Request,
    cache: std::sync::Arc<tokio::sync::Mutex<crate::pane_runtime_client::CapturedHistoryCache>>,
) -> Result<PaneScrollbackWindow, FetchError> {
    let original = cache.lock().await.clone();
    let mut working = original.clone();
    let result = fetch_owned(client, request, &mut working).await;
    if result.is_ok() {
        let mut resident = cache.lock().await;
        if resident.same_source(&original) {
            *resident = working;
        }
    }
    result
}

async fn fetch_owned(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    request: Request,
    cache: &mut crate::pane_runtime_client::CapturedHistoryCache,
) -> Result<PaneScrollbackWindow, FetchError> {
    let rows = request.rows;
    if let Some(pin) = request.pin {
        let mut last_error = None;
        // Retain nearby older rows for local scrolling and direction reversals.
        // The image service bounds projections to 256 rows; oversized terminals
        // keep the exact-viewport path rather than truncating visible content.
        // Resolve visible navigation first. A larger speculative viewport has
        // different boundary semantics and must not clamp the user's target.
        for count in [rows] {
            let offset = if request.boundary == Some(Boundary::Newest) {
                0
            } else {
                request.offset
            };
            let result = crate::pane_runtime_client::captured_history_window_cached(
                client,
                (request.session, request.pane, pin),
                offset,
                count,
                request.projection(),
                cache,
            )
            .await;
            match result {
                Ok(crate::pane_runtime_client::CapturedWindowOutcome::Window(mut window)) => {
                    if let Some(origin) = window.row_anchors.last() {
                        cache.prepare_resident_index(request.width, origin.line_index);
                    }
                    if let Some(images) = cache.cached_images(request.width, &window.row_anchors) {
                        window.images = images;
                        return Ok(window);
                    }
                    let origin = window.row_anchors.first().ok_or("missing capture origin")?;
                    let reply = bmux_pane_runtime_plugin_api::attach_runtime_state::client::attach_history_images_v3(
                        client, request.session, request.pane, pin.pin_id, origin.capture_id,
                        u16::try_from(request.width).map_err(|error| error.to_string())?,
                        origin.line_index,
                        u32::try_from(origin.column).map_err(|error| error.to_string())?,
                        u16::try_from(window.rows.len()).map_err(|error| error.to_string())?,
                    ).await.map_err(|error| error.to_string())?
                        .map_err(|error| format!("historical images: {error:?}"))?;
                    if reply.capture_id != origin.capture_id
                        || reply.encoded.len() > 8 * 1024 * 1024
                    {
                        return Err("invalid historical image response".into());
                    }
                    window.images = serde_json::from_slice(&reply.encoded)
                        .map_err(|error| error.to_string())?;
                    cache.retain_images(request.width, &window.row_anchors, &window.images);
                    return Ok(window);
                }
                Ok(crate::pane_runtime_client::CapturedWindowOutcome::Unavailable) => {}
                Err(error) => {
                    // Failed/cancelled assembly must not leave a partial tail
                    // index to be mistaken for a complete cached capture.
                    *cache = crate::pane_runtime_client::CapturedHistoryCache::default();
                    last_error = Some(error.to_string());
                }
            }
        }
        // A capture-bound request must not degrade to a text-only physical
        // snapshot: that would acknowledge a complete viewport while losing its
        // graphics and logical origin. Legacy reads are only for unpinned views.
        return Err(last_error.map_or(FetchError::Unavailable, FetchError::Failed));
    }
    let windows = crate::pane_runtime_client::attach_pane_grid_window_state_streaming(
        client,
        request.session,
        vec![crate::pane_runtime_client::PaneGridWindowRequest {
            pane_id: request.pane,
            scrollback_offset: request.offset,
            rows,
            anchor_total_scrolled_rows: request.total,
            pin_id: request.pin.map(|pin| pin.pin_id),
        }],
    )
    .await
    .map_err(|error| error.to_string())?;
    let window = windows
        .into_iter()
        .find(|window| window.pane_id == request.pane)
        .ok_or("history window missing")?;
    let decoded: bmux_terminal_grid::GridSnapshot =
        serde_json::from_slice(&window.encoded).map_err(|error| error.to_string())?;
    let grid = bmux_terminal_grid::TerminalGrid::from_snapshot(
        &decoded,
        bmux_terminal_grid::GridLimits::default(),
    )
    .map_err(|error| error.to_string())?;
    Ok(PaneScrollbackWindow {
        images: Vec::new(),
        projection_width: usize::from(decoded.width),
        row_anchors: Vec::new(),
        palette: grid.palette().clone(),
        scrollback_offset: window.scrollback_offset,
        max_scrollback_offset: window.max_scrollback_offset,
        total_scrolled_rows: window.total_scrolled_rows,
        rows: grid.display_rows(0, decoded.rows.len()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefetch_is_bounded_bidirectional_and_capture_only() {
        let request = Request {
            boundary: None,
            session: Uuid::new_v4(),
            pane: Uuid::new_v4(),
            pin: Some(ScrollbackPin {
                capture: None,
                pin_id: 1,
                total_scrolled_rows: 100,
                max_scrollback_offset: 100,
                stream_end: 0,
                created_epoch_secs: 0,
            }),
            offset: 40,
            width: 80,
            rows: 20,
            total: None,
            anchor: None,
            delta: 0,
        };
        let window = PaneScrollbackWindow {
            images: Vec::new(),
            projection_width: 80,
            row_anchors: vec![CapturedHistoryAnchor {
                capture_id: Uuid::new_v4(),
                line_index: 50,
                column: 0,
            }],
            palette: bmux_terminal_grid::StylePalette::default(),
            scrollback_offset: 40,
            max_scrollback_offset: 100,
            total_scrolled_rows: 100,
            rows: Vec::new(),
        };
        let neighbors = request.neighbors(&window);
        assert_eq!(neighbors.len(), 2);
        assert_eq!((neighbors[0].offset, neighbors[0].delta), (60, 20));
        assert_eq!((neighbors[1].offset, neighbors[1].delta), (20, -20));
        let warm = request.warm_range(&window);
        assert!(warm.len() <= 32);
        assert_eq!(warm[0].offset, 60);
        assert!(warm.iter().all(|item| item.offset <= 100));
        let distinct = warm
            .iter()
            .map(|item| item.offset)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(distinct.len(), warm.len());
        assert!(
            Request { rows: 0, ..request }
                .warm_range(&window)
                .is_empty()
        );
        assert!(
            Request {
                pin: None,
                ..request
            }
            .neighbors(&window)
            .is_empty()
        );
    }

    #[test]
    fn superseded_navigation_reuses_only_the_same_immutable_capture() {
        let pin = ScrollbackPin {
            capture: None,
            pin_id: 1,
            total_scrolled_rows: 100,
            max_scrollback_offset: 100,
            stream_end: 100,
            created_epoch_secs: 0,
        };
        let request = Request {
            boundary: None,
            session: Uuid::new_v4(),
            pane: Uuid::new_v4(),
            pin: Some(pin),
            offset: 10,
            width: 80,
            rows: 24,
            total: None,
            anchor: None,
            delta: 0,
        };
        assert!(request.reusable_capture(request.session, Some(pin), Some((80, 24))));
        let moved = Request {
            offset: 30,
            ..request
        };
        assert!(moved.reusable_capture(request.session, Some(pin), Some((80, 24))));
        assert!(!request.reusable_capture(Uuid::new_v4(), Some(pin), Some((80, 24))));
        assert!(!request.reusable_capture(request.session, None, Some((80, 24))));
        assert!(!request.reusable_capture(
            request.session,
            Some(ScrollbackPin { pin_id: 2, ..pin }),
            Some((80, 24))
        ));
        assert!(!request.reusable_capture(request.session, Some(pin), Some((40, 24))));
        assert!(
            !Request {
                pin: None,
                ..request
            }
            .reusable_capture(request.session, None, Some((80, 24)))
        );
    }

    #[tokio::test]
    async fn unavailable_capture_never_falls_back_to_text_only_snapshot() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let client = bmux_plugin::AsyncServiceClient::bind(1, sender).unwrap();
        let request = Request {
            boundary: None,
            session: Uuid::new_v4(),
            pane: Uuid::new_v4(),
            pin: Some(ScrollbackPin {
                capture: None,
                pin_id: 1,
                total_scrolled_rows: 100,
                max_scrollback_offset: 100,
                stream_end: 100,
                created_epoch_secs: 0,
            }),
            offset: 10,
            width: 80,
            rows: 20,
            total: Some(100),
            anchor: None,
            delta: 0,
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fetch(client, request, std::sync::Arc::default()),
        )
        .await
        .expect("must reject without attempting legacy service");
        assert!(matches!(result, Outcome::Unavailable));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn slow_history_service_does_not_block_the_caller_and_drop_cancels() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let client = bmux_plugin::AsyncServiceClient::bind(1, sender).unwrap();
        let request = Request {
            boundary: None,
            session: Uuid::new_v4(),
            pane: Uuid::new_v4(),
            pin: None,
            offset: 10,
            width: 80,
            rows: 20,
            total: Some(100),
            anchor: None,
            delta: 0,
        };
        let cache = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::pane_runtime_client::CapturedHistoryCache::default(),
        ));
        let request = Request {
            boundary: None,
            pin: Some(ScrollbackPin {
                capture: Some(bmux_attach_pipeline::ScrollbackCapture {
                    identity: Uuid::new_v4(),
                    lines: 50,
                    truncated: false,
                    width: 80,
                    height: 24,
                }),
                pin_id: 1,
                total_scrolled_rows: 50,
                max_scrollback_offset: 50,
                stream_end: 0,
                created_epoch_secs: 0,
            }),
            ..request
        };
        let fetch = Fetch {
            prefetch: false,
            request,
            task: tokio::spawn(fetch(client, request, cache.clone())),
        };
        let held_request = receiver.recv().await.unwrap();
        assert!(
            cache.try_lock().is_ok(),
            "resident content must remain readable during history IPC"
        );
        assert!(!fetch.task.is_finished());
        drop(fetch);
        tokio::task::yield_now().await;
        assert!(held_request.is_cancelled());
    }
}
