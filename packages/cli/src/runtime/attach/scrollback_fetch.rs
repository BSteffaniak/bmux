//! One bounded history read at a time, independent of the input loop.
use bmux_attach_pipeline::{CapturedHistoryAnchor, PaneScrollbackWindow, ScrollbackPin};
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Request {
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

pub(super) struct Fetch {
    pub request: Request,
    pub task: tokio::task::JoinHandle<Result<PaneScrollbackWindow, String>>,
}

impl Drop for Fetch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) async fn fetch(
    mut client: bmux_plugin::AsyncServiceClient,
    request: Request,
    cache: std::sync::Arc<tokio::sync::Mutex<crate::pane_runtime_client::CapturedHistoryCache>>,
) -> Result<PaneScrollbackWindow, String> {
    let mut cache = cache.lock().await;
    let rows = request.rows;
    if let Some(pin) = request.pin {
        let mut last_error = None;
        for count in [rows, request.rows] {
            let result = crate::pane_runtime_client::captured_history_window_cached(
                &mut client,
                (request.session, request.pane, pin),
                request.offset,
                count,
                (request.width, request.anchor, request.delta),
                &mut cache,
            )
            .await;
            match result {
                Ok(crate::pane_runtime_client::CapturedWindowOutcome::Window(mut window)) => {
                    let origin = window.row_anchors.first().ok_or("missing capture origin")?;
                    let reply = bmux_pane_runtime_plugin_api::attach_runtime_state::client::attach_history_images_v3(
                        &mut client, request.session, request.pane, pin.pin_id, origin.capture_id,
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
        if request.anchor.is_some() {
            return Err(last_error.unwrap_or_else(|| "captured history window unavailable".into()));
        }
    }
    drop(cache);
    let windows = crate::pane_runtime_client::attach_pane_grid_window_state_streaming(
        &mut client,
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

    #[tokio::test]
    async fn slow_history_service_does_not_block_the_caller_and_drop_cancels() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let client = bmux_plugin::AsyncServiceClient::bind(1, sender).unwrap();
        let request = Request {
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
        let fetch = Fetch {
            request,
            task: tokio::spawn(fetch(client, request, std::sync::Arc::default())),
        };
        let held_request = receiver.recv().await.unwrap();
        assert!(!fetch.task.is_finished());
        drop(fetch);
        tokio::task::yield_now().await;
        assert!(held_request.is_cancelled());
    }
}
