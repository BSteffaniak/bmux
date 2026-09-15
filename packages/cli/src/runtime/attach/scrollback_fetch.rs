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
) -> Result<PaneScrollbackWindow, String> {
    let rows = request.rows.saturating_add(32).min(256).max(request.rows);
    if let Some(pin) = request.pin {
        for count in [rows, request.rows] {
            let result = crate::pane_runtime_client::captured_history_window_outcome(
                &mut client,
                request.session,
                request.pane,
                pin,
                request.offset,
                count,
                (request.width, request.anchor, request.delta),
            )
            .await
            .map_err(|error| error.to_string())?;
            if let crate::pane_runtime_client::CapturedWindowOutcome::Window(window) = result {
                return Ok(window);
            }
        }
        if request.anchor.is_some() {
            return Err("captured history window unavailable".into());
        }
    }
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
            task: tokio::spawn(fetch(client, request)),
        };
        let held_request = receiver.recv().await.unwrap();
        assert!(!fetch.task.is_finished());
        drop(fetch);
        tokio::task::yield_now().await;
        assert!(held_request.is_cancelled());
    }
}
