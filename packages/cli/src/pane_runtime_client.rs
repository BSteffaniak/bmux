use bmux_attach_image_protocol::AttachPaneImageDelta;
use bmux_attach_layout_protocol::{
    AttachPaneChunk, AttachPaneInputMode, AttachPaneMouseProtocol, AttachScene, PaneLayoutNode,
    PaneSummary,
};
use bmux_attach_token_state::AttachGrant;
use bmux_client::{
    AttachLayoutState, AttachOpenInfo, AttachPaneSnapshotState, AttachSnapshotState, ClientError,
    PaneOutputBatchResult,
};
use bmux_context_state::ContextSelector;
use bmux_ipc::ErrorCode;
use bmux_pane_runtime_plugin_api::{
    attach_runtime_commands as AttachCommands, attach_runtime_state as AttachState,
    pane_runtime_commands as PaneCommands, pane_runtime_state as PaneState,
};
use bmux_session_models::SessionSelector;
use std::future::Future;
use uuid::Uuid;

type ClientResult<T> = bmux_client::Result<T>;

#[allow(
    dead_code,
    reason = "structured attach hydration is wired in follow-up phases"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneGridSnapshotResult {
    pub pane_id: Uuid,
    pub stream_end: u64,
    pub encoded: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneGridDeltaResult {
    pub pane_id: Uuid,
    pub base_revision: u64,
    pub revision: u64,
    pub desynced: bool,
    pub encoded: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneGridWindowRequest {
    pub pane_id: Uuid,
    pub scrollback_offset: usize,
    pub rows: usize,
    pub anchor_total_scrolled_rows: Option<u64>,
    pub pin_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneGridWindowResult {
    pub pane_id: Uuid,
    pub scrollback_offset: usize,
    pub max_scrollback_offset: usize,
    pub total_scrolled_rows: u64,
    pub anchor_delta_rows: usize,
    pub anchor_clamped: bool,
    pub stream_end: u64,
    pub encoded: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneScrollbackPinResult {
    pub capture: bmux_attach_pipeline::ScrollbackCapture,
    pub pane_id: Uuid,
    pub pin_id: u64,
    pub total_scrolled_rows: u64,
    pub max_scrollback_offset: usize,
    pub stream_end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneScrollbackRuleMetadata {
    pub name: Option<String>,
    pub shell: String,
    pub active_command: Option<String>,
}

#[allow(
    dead_code,
    reason = "structured attach hydration is wired in follow-up phases"
)]
pub async fn attach_pane_grid_snapshot_state(
    client: &mut bmux_client::BmuxClient,
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    max_rows_per_pane: usize,
) -> ClientResult<Vec<PaneGridSnapshotResult>> {
    let max_rows_per_pane = u32::try_from(max_rows_per_pane).unwrap_or(u32::MAX);
    match AttachState::client::attach_pane_grid_snapshot_state(
        client,
        session_id,
        pane_ids,
        max_rows_per_pane,
    )
    .await
    {
        Ok(Ok(state)) => Ok(state
            .snapshots
            .into_iter()
            .map(|snapshot| PaneGridSnapshotResult {
                pane_id: snapshot.pane_id,
                stream_end: snapshot.stream_end,
                encoded: snapshot.encoded,
            })
            .collect()),
        Ok(Err(err)) => typed_server_error("attach-pane-grid-snapshot-state", err),
        Err(err) => typed_dispatch_error("attach-pane-grid-snapshot-state", err),
    }
}

#[allow(
    dead_code,
    reason = "structured attach hydration is wired in follow-up phases"
)]
pub async fn attach_pane_grid_snapshot_state_streaming(
    client: &mut bmux_client::StreamingBmuxClient,
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    max_rows_per_pane: usize,
) -> ClientResult<Vec<PaneGridSnapshotResult>> {
    let max_rows_per_pane = u32::try_from(max_rows_per_pane).unwrap_or(u32::MAX);
    match AttachState::client::attach_pane_grid_snapshot_state(
        client,
        session_id,
        pane_ids,
        max_rows_per_pane,
    )
    .await
    {
        Ok(Ok(state)) => Ok(state
            .snapshots
            .into_iter()
            .map(|snapshot| PaneGridSnapshotResult {
                pane_id: snapshot.pane_id,
                stream_end: snapshot.stream_end,
                encoded: snapshot.encoded,
            })
            .collect()),
        Ok(Err(err)) => typed_server_error("attach-pane-grid-snapshot-state", err),
        Err(err) => typed_dispatch_error("attach-pane-grid-snapshot-state", err),
    }
}

pub async fn attach_pane_grid_window_state_streaming(
    client: &mut bmux_client::StreamingBmuxClient,
    session_id: Uuid,
    windows: Vec<PaneGridWindowRequest>,
) -> ClientResult<Vec<PaneGridWindowResult>> {
    let windows = windows
        .into_iter()
        .map(|window| AttachState::PaneGridWindowRequest {
            pane_id: window.pane_id,
            scrollback_offset: u32::try_from(window.scrollback_offset).unwrap_or(u32::MAX),
            rows: u32::try_from(window.rows).unwrap_or(u32::MAX),
            anchor_total_scrolled_rows: window.anchor_total_scrolled_rows,
            pin_id: window.pin_id,
        })
        .collect::<Vec<_>>();
    match AttachState::client::attach_pane_grid_window_state(client, session_id, windows).await {
        Ok(Ok(state)) => Ok(state
            .windows
            .into_iter()
            .map(|window| PaneGridWindowResult {
                pane_id: window.pane_id,
                scrollback_offset: window.scrollback_offset as usize,
                max_scrollback_offset: window.max_scrollback_offset as usize,
                total_scrolled_rows: window.total_scrolled_rows,
                anchor_delta_rows: window.anchor_delta_rows as usize,
                anchor_clamped: window.anchor_clamped,
                stream_end: window.stream_end,
                encoded: window.encoded,
            })
            .collect()),
        Ok(Err(err)) => typed_server_error("attach-pane-grid-window-state", err),
        Err(err) => typed_dispatch_error("attach-pane-grid-window-state", err),
    }
}

pub async fn pane_scrollback_rule_metadata_streaming(
    client: &mut bmux_client::StreamingBmuxClient,
    session_id: Uuid,
    pane_id: Uuid,
) -> ClientResult<PaneScrollbackRuleMetadata> {
    match PaneState::client::get_pane(client, session_id, pane_id).await {
        Ok(Ok(pane)) => Ok(PaneScrollbackRuleMetadata {
            name: pane.name,
            shell: pane.shell,
            active_command: pane.active_command,
        }),
        Ok(Err(err)) => typed_server_error("get-pane", err),
        Err(err) => typed_dispatch_error("get-pane", err),
    }
}

/// Assemble a requested logical line from an existing immutable capture.
/// The caller owns the pin lifetime; failures never substitute a new capture.
pub async fn fetch_captured_history_line(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session_id: Uuid,
    capture: &AttachState::HistoryCaptureV1,
    line_index: u32,
    remaining: &mut usize,
    styles: &mut Vec<bmux_terminal_grid::Style>,
    requests_left: &mut usize,
) -> ClientResult<bmux_terminal_grid::HistoryLineAssembly> {
    use bmux_terminal_grid::{HistoryLineAssembly, HistorySlice, HistorySliceEnd};
    if u64::from(line_index) >= capture.history_line_count {
        return typed_server_error("history-line", "line outside capture");
    }
    let mut line = HistoryLineAssembly::new(
        capture.capture_id.as_u128(),
        line_index as usize,
        *remaining,
        capture.history_truncated && line_index == 0,
    );
    while line.completed().is_none() {
        *requests_left = requests_left
            .checked_sub(1)
            .ok_or_else(|| history_decode_error(&"history window request limit"))?;
        let offset =
            u32::try_from(line.next_offset()).map_err(|error| history_decode_error(&error))?;
        let reply = AttachState::client::attach_history_slice_v1(
            client,
            session_id,
            capture.pin.pane_id,
            capture.pin.pin_id,
            capture.capture_id,
            line_index,
            offset,
            256,
            4096,
        )
        .await
        .map_err(|error| history_decode_error(&error))?
        .map_err(|error| history_decode_error(&format!("{error:?}")))?;
        // Bound the complete JSON input before deserializing. Each cell is then
        // admitted individually; do not deserialize an unbounded Vec first.
        if reply.encoded.len() > 128 * 1024 {
            return Err(history_decode_error(&"oversized history reply"));
        }
        let cells = decode_history_cells(&reply.encoded, styles, remaining)?;
        let end = match reply.end_kind {
            AttachState::HistoryEnd::Continue => HistorySliceEnd::Continue,
            AttachState::HistoryEnd::HardBreak => HistorySliceEnd::HardBreak,
            AttachState::HistoryEnd::Open => HistorySliceEnd::Open,
        };
        let next = usize::try_from(reply.next_cell_offset)
            .map_err(|error| history_decode_error(&error))?;
        line.append(
            capture.capture_id.as_u128(),
            line_index as usize,
            offset as usize,
            &HistorySlice {
                cells: &cells,
                next_cell_offset: next,
                end,
            },
        )
        .map_err(|error| history_decode_error(&format!("{error:?}")))?;
    }
    Ok(line)
}

#[cfg(test)]
mod history_tests {
    #[test]
    fn tail_projection_charges_shared_budget_before_mutating_window() {
        use bmux_terminal_grid::{
            Cell, HistoryLineAssembly, HistorySlice, HistorySliceEnd, StyleId,
        };
        let mut line = HistoryLineAssembly::new(1, 0, 4096, false);
        line.append(
            1,
            0,
            0,
            &HistorySlice {
                cells: &[Cell::new("X".to_owned(), StyleId(0), 1)],
                next_cell_offset: 1,
                end: HistorySliceEnd::HardBreak,
            },
        )
        .unwrap();
        let mut budget = 4096;
        let mut output = Vec::new();
        super::append_tail_projection(&line, 8, 1, &mut budget, &mut output).unwrap();
        let charged = 4096 - budget;
        assert!(charged > 0);
        budget = charged - 1;
        assert!(super::append_tail_projection(&line, 8, 1, &mut budget, &mut output).is_err());
        assert_eq!(budget, charged - 1);
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].cells()[0].text(), "X");
    }

    struct TailClient(usize);
    impl bmux_plugin_sdk::TypedDispatchClient for TailClient {
        async fn invoke_service_raw(
            &mut self,
            _capability: &str,
            _kind: bmux_ipc::InvokeServiceKind,
            _interface: &str,
            operation: &str,
            _payload: Vec<u8>,
        ) -> bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>> {
            use super::AttachState::{HistoryEnd, HistoryFetchError, HistorySliceV1};
            if operation == "attach-history-slice-v1" {
                return Ok(
                    bmux_plugin_sdk::encode_service_message(&Ok::<_, HistoryFetchError>(
                        HistorySliceV1 {
                            encoded: serde_json::to_vec(&[(
                                "P",
                                1_u8,
                                bmux_terminal_grid::Style::default(),
                            )])
                            .unwrap(),
                            next_cell_offset: 1,
                            end_kind: HistoryEnd::Open,
                        },
                    ))
                    .unwrap(),
                );
            }
            assert_eq!(operation, "attach-main-row-slice-v1");
            let (text, next, end) = match self.0 {
                0 => ("X", 1, HistoryEnd::HardBreak),
                1 => ("A", 1, HistoryEnd::Continue),
                2 => ("B", 2, HistoryEnd::Open),
                3 => ("C", 1, HistoryEnd::HardBreak),
                _ => panic!("unexpected fetch"),
            };
            self.0 += 1;
            Ok(
                bmux_plugin_sdk::encode_service_message(&Ok::<_, HistoryFetchError>(
                    HistorySliceV1 {
                        encoded: serde_json::to_vec(&[(
                            text,
                            1_u8,
                            bmux_terminal_grid::Style::default(),
                        )])
                        .unwrap(),
                        next_cell_offset: next,
                        end_kind: end,
                    },
                ))
                .unwrap(),
            )
        }
    }

    #[tokio::test]
    async fn tail_joins_paginated_wrapped_rows_with_capture_width_padding() {
        let pin = bmux_attach_pipeline::ScrollbackPin {
            capture: Some(bmux_attach_pipeline::ScrollbackCapture {
                identity: uuid::Uuid::new_v4(),
                lines: 1,
                truncated: false,
                width: 4,
                height: 3,
            }),
            pin_id: 1,
            total_scrolled_rows: 1,
            max_scrollback_offset: 1,
            stream_end: 0,
            created_epoch_secs: 0,
        };
        let first = super::captured_tail_window(
            &mut TailClient(0),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            2,
            1,
            8,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            first.rows[0]
                .cells()
                .iter()
                .map(bmux_terminal_grid::Cell::text)
                .collect::<String>(),
            "PX"
        );
        let initial = super::captured_history_window_outcome(
            &mut TailClient(0),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            3,
            1,
            (8, None, 0),
        )
        .await
        .unwrap();
        let super::CapturedWindowOutcome::Window(initial) = initial else {
            panic!("pending entry should resolve");
        };
        assert_eq!(
            bmux_terminal_grid::row_text(&initial.rows[0], 8),
            "PX      "
        );
        let refreshed = super::captured_history_window_outcome(
            &mut TailClient(0),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            3,
            1,
            (8, initial.row_anchors.last().copied(), 0),
        )
        .await
        .unwrap();
        let super::CapturedWindowOutcome::Window(refreshed) = refreshed else {
            panic!("pending refresh should resolve");
        };
        assert_eq!(initial.rows, refreshed.rows);
        assert_eq!(initial.row_anchors, refreshed.row_anchors);
        let mut client = TailClient(0);
        let window = super::captured_tail_window(
            &mut client,
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            0,
            1,
            8,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(client.0, 4);
        assert_eq!(
            window.rows[0]
                .cells()
                .iter()
                .map(bmux_terminal_grid::Cell::text)
                .collect::<String>(),
            "AB  C"
        );
    }

    #[tokio::test]
    async fn entry_resolves_physical_boundary_without_fetching_target_or_later_rows() {
        let capture = super::AttachState::HistoryCaptureV1 {
            width: 4,
            height: 3,
            capture_id: uuid::Uuid::new_v4(),
            history_line_count: 1,
            history_truncated: false,
            pin: super::AttachState::PaneScrollbackPin {
                pane_id: uuid::Uuid::new_v4(),
                pin_id: 1,
                total_scrolled_rows: 1,
                max_scrollback_offset: 1,
                stream_end: 0,
            },
        };
        let mut client = TailClient(0);
        // Pending P, hard-ended X, then paginated wrapped AB: the last physical
        // row begins at column four regardless of whether it contains any cells.
        let anchor = super::resolve_tail_entry(
            &mut client,
            uuid::Uuid::new_v4(),
            &capture,
            0,
            &mut 65536,
            &mut Vec::new(),
            &mut 8,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(client.0, 3);
        assert_eq!(anchor.line_index, 1);
        assert_eq!(anchor.column, 4);
        assert_eq!(anchor.capture_id, capture.capture_id);
    }

    struct EmptySuffixClient(usize);
    impl bmux_plugin_sdk::TypedDispatchClient for EmptySuffixClient {
        async fn invoke_service_raw(
            &mut self,
            _capability: &str,
            _kind: bmux_ipc::InvokeServiceKind,
            _interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>> {
            use super::AttachState::{HistoryEnd, HistoryFetchError, HistorySliceV1};
            #[derive(serde::Deserialize)]
            struct RowRequest {
                session_id: uuid::Uuid,
                pane_id: uuid::Uuid,
                pin_id: u64,
                capture_id: uuid::Uuid,
                line_index: u32,
                cell_offset: u32,
                max_columns: u32,
                max_text_bytes: u32,
            }
            assert_eq!(operation, "attach-main-row-slice-v1");
            let request: RowRequest = bmux_plugin_sdk::decode_service_message(&payload).unwrap();
            let _ = (
                request.session_id,
                request.pane_id,
                request.pin_id,
                request.capture_id,
                request.cell_offset,
                request.max_columns,
                request.max_text_bytes,
            );
            let empty = request.line_index == 1;
            self.0 += 1;
            let cells = if empty {
                vec![]
            } else {
                vec![("A", 1_u8, bmux_terminal_grid::Style::default())]
            };
            Ok(
                bmux_plugin_sdk::encode_service_message(&Ok::<_, HistoryFetchError>(
                    HistorySliceV1 {
                        encoded: serde_json::to_vec(&cells).unwrap(),
                        next_cell_offset: u64::from(!empty),
                        end_kind: if empty {
                            HistoryEnd::HardBreak
                        } else {
                            HistoryEnd::Open
                        },
                    },
                ))
                .unwrap(),
            )
        }
    }

    #[tokio::test]
    async fn empty_suffix_entry_and_refresh_preserve_boundary() {
        let pin = bmux_attach_pipeline::ScrollbackPin {
            capture: Some(bmux_attach_pipeline::ScrollbackCapture {
                identity: uuid::Uuid::new_v4(),
                lines: 0,
                truncated: false,
                width: 4,
                height: 2,
            }),
            pin_id: 1,
            total_scrolled_rows: 0,
            max_scrollback_offset: 0,
            stream_end: 0,
            created_epoch_secs: 0,
        };
        let window = super::captured_history_window(
            &mut EmptySuffixClient(0),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            0,
            1,
            (4, None, 0),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(window.row_anchors[0].column, 4);
        assert_eq!(bmux_terminal_grid::row_text(&window.rows[0], 4), "    ");
        let refreshed = super::captured_history_window(
            &mut EmptySuffixClient(1),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            0,
            1,
            (4, window.row_anchors.last().copied(), 0),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(window.rows, refreshed.rows);
        assert_eq!(window.row_anchors, refreshed.row_anchors);
        for width in [2, 8] {
            let resized = super::captured_history_window(
                &mut EmptySuffixClient(1),
                uuid::Uuid::new_v4(),
                uuid::Uuid::new_v4(),
                pin,
                0,
                1,
                (width, window.row_anchors.last().copied(), 0),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                resized.row_anchors[0].capture_id,
                window.row_anchors[0].capture_id
            );
            assert_eq!(
                resized.row_anchors[0].column,
                if width == 2 { 4 } else { 0 }
            );
        }
        let backward = super::captured_history_window(
            &mut EmptySuffixClient(1),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            1,
            1,
            (2, window.row_anchors.last().copied(), 1),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(backward.row_anchors[0].column, 2);
        assert_eq!(bmux_terminal_grid::row_text(&backward.rows[0], 2), "  ");
    }

    #[tokio::test]
    async fn decoded_line_cache_reuses_admitted_assembly_without_budget_charge() {
        let capture = super::AttachState::HistoryCaptureV1 {
            width: 2,
            height: 2,
            capture_id: uuid::Uuid::new_v4(),
            history_line_count: 1,
            history_truncated: false,
            pin: super::AttachState::PaneScrollbackPin {
                pane_id: uuid::Uuid::new_v4(),
                pin_id: 1,
                total_scrolled_rows: 1,
                max_scrollback_offset: 1,
                stream_end: 0,
            },
        };
        let mut cache = super::CapturedLineCache::default();
        let mut remaining = 65536;
        let mut styles = Vec::new();
        let mut requests = 8;
        let session = uuid::Uuid::new_v4();
        let first = cache
            .resolve(
                &mut HistoryClient,
                session,
                &capture,
                0,
                (&mut remaining, &mut styles, &mut requests),
            )
            .await
            .unwrap()
            .unwrap();
        let admitted = (remaining, requests, styles.len());
        let second = cache
            .resolve(
                &mut HistoryClient,
                session,
                &capture,
                0,
                (&mut remaining, &mut styles, &mut requests),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(admitted, (remaining, requests, styles.len()));
    }

    struct HistoryClient;
    impl bmux_plugin_sdk::TypedDispatchClient for HistoryClient {
        async fn invoke_service_raw(
            &mut self,
            _capability: &str,
            _kind: bmux_ipc::InvokeServiceKind,
            _interface: &str,
            operation: &str,
            _payload: Vec<u8>,
        ) -> bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>> {
            assert!(matches!(
                operation,
                "attach-history-slice-v1" | "attach-main-row-slice-v1"
            ));
            let reply = super::AttachState::HistorySliceV1 {
                encoded: serde_json::to_vec(&vec![
                    ("A", 1_u8, bmux_terminal_grid::Style::default()),
                    ("B", 1_u8, bmux_terminal_grid::Style::default()),
                ])
                .unwrap(),
                next_cell_offset: 2,
                end_kind: super::AttachState::HistoryEnd::HardBreak,
            };
            Ok(bmux_plugin_sdk::encode_service_message(&Ok::<
                _,
                super::AttachState::HistoryFetchError,
            >(reply))
            .unwrap())
        }
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "sequential fetch, resize and navigation assertions share one immutable capture"
    )]
    async fn captured_window_fetches_and_projects_history_without_live_tail() {
        let pin = bmux_attach_pipeline::ScrollbackPin {
            capture: Some(bmux_attach_pipeline::ScrollbackCapture {
                identity: uuid::Uuid::new_v4(),
                lines: 1,
                truncated: false,
                width: 2,
                height: 2,
            }),
            pin_id: 1,
            total_scrolled_rows: 1,
            max_scrollback_offset: 1,
            stream_end: 9,
            created_epoch_secs: 0,
        };
        let window = super::captured_history_window(
            &mut HistoryClient,
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            2,
            1,
            (2, None, 0),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(bmux_terminal_grid::row_text(&window.rows[0], 2), "AB");
        assert_eq!(window.scrollback_offset, 2);
        let anchor = window.content_anchor(0, 1).unwrap();
        assert_eq!(anchor.capture_id, pin.capture.unwrap().identity);
        assert_eq!(anchor.line_index, 0);
        assert_eq!(anchor.column, 1);
        let narrow = super::captured_history_window(
            &mut HistoryClient,
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            2,
            2,
            (1, None, 0),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(narrow.projection_width, 1);
        assert_eq!(bmux_terminal_grid::row_text(&narrow.rows[0], 1), "A");
        assert_eq!(bmux_terminal_grid::row_text(&narrow.rows[1], 1), "B");
        assert_eq!(narrow.content_anchor(1, 0), Some(anchor));
        let anchored = super::captured_history_window(
            &mut HistoryClient,
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            2,
            1,
            (1, window.row_anchors.last().copied(), 0),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(bmux_terminal_grid::row_text(&anchored.rows[0], 1), "A");
        let scrolled = super::captured_history_window(
            &mut HistoryClient,
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            3,
            1,
            (1, narrow.row_anchors.last().copied(), 1),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(bmux_terminal_grid::row_text(&scrolled.rows[0], 1), "A");
        let returned = super::captured_history_window(
            &mut HistoryClient,
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            2,
            1,
            (1, scrolled.row_anchors.last().copied(), -1),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(bmux_terminal_grid::row_text(&returned.rows[0], 1), "B");
        assert_eq!(returned.row_anchors.last(), narrow.row_anchors.last());
        let boundary = super::captured_history_window_outcome(
            &mut HistoryClient,
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            1,
            1,
            (1, returned.row_anchors.last().copied(), -1),
        )
        .await
        .unwrap();
        let super::CapturedWindowOutcome::Window(boundary) = boundary else {
            panic!("tail should retain a logical anchor");
        };
        assert_eq!(boundary.row_anchors[0].line_index, 1);
        assert_eq!(bmux_terminal_grid::row_text(&boundary.rows[0], 1), "A");
        let refreshed = super::captured_history_window_outcome(
            &mut HistoryClient,
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            1,
            1,
            (1, boundary.row_anchors.last().copied(), -1),
        )
        .await
        .unwrap();
        let super::CapturedWindowOutcome::Window(refreshed) = refreshed else {
            panic!("tail anchor must resolve again");
        };
        assert_eq!(bmux_terminal_grid::row_text(&refreshed.rows[0], 1), "B");
        assert_eq!(refreshed.row_anchors[0].line_index, 1);
        let overshoot = super::captured_history_window(
            &mut HistoryClient,
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            0,
            1,
            (1, refreshed.row_anchors.last().copied(), -100),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(overshoot.row_anchors[0].line_index, 2);
        assert_eq!(overshoot.row_anchors[0].column, 1);
        assert_eq!(bmux_terminal_grid::row_text(&overshoot.rows[0], 1), "B");
        let entered = super::captured_history_window(
            &mut HistoryClient,
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            1,
            1,
            (2, None, 0),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(entered.row_anchors[0].line_index, 1);
        let refreshed = super::captured_history_window(
            &mut HistoryClient,
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            pin,
            1,
            1,
            (2, entered.row_anchors.last().copied(), 0),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(entered.rows, refreshed.rows);
        assert_eq!(entered.row_anchors, refreshed.row_anchors);
    }

    #[test]
    fn decoding_charges_cells_and_rejects_exhausted_budget() {
        let style = bmux_terminal_grid::Style::default();
        let encoded = serde_json::to_vec(&vec![("界", 2_u8, style)]).unwrap();
        let mut styles = Vec::new();
        let mut budget = 1024;
        let cells = super::decode_history_cells(&encoded, &mut styles, &mut budget).unwrap();
        assert_eq!(cells[0].text(), "界");
        assert_eq!(styles, vec![style]);
        assert!(budget < 1024);
        assert!(super::decode_history_cells(&encoded, &mut styles, &mut 0).is_err());
    }
}

/// History-only windows use local width with capture-relative scroll offsets.
/// Live-tail overlap stays on the pinned snapshot path until joins are modeled.
#[allow(
    clippy::too_many_lines,
    reason = "bounded fetch, projection and budget accounting form one ordered window assembly workflow"
)]
pub enum CapturedWindowOutcome {
    Window(bmux_attach_pipeline::PaneScrollbackWindow),
    Unavailable,
}

/// Request-local memoization for immutable capture reads only. It never survives
/// a window refresh and never caches mutations or transport failures.
struct CachedCaptureReply {
    capability: String,
    interface: String,
    operation: String,
    payload: Vec<u8>,
    reply: Vec<u8>,
}

struct CaptureReadCache<'a, C> {
    client: &'a mut C,
    replies: Vec<CachedCaptureReply>,
    remaining: usize,
}

impl<C: bmux_plugin_sdk::TypedDispatchClient> bmux_plugin_sdk::TypedDispatchClient
    for CaptureReadCache<'_, C>
{
    async fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: bmux_ipc::InvokeServiceKind,
        interface: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>> {
        let cacheable = kind == bmux_ipc::InvokeServiceKind::Query
            && matches!(
                operation,
                "attach-history-slice-v1" | "attach-main-row-slice-v1"
            );
        if cacheable
            && let Some(entry) = self.replies.iter().find(|entry| {
                entry.capability == capability
                    && entry.interface == interface
                    && entry.operation == operation
                    && entry.payload == payload
            })
        {
            return Ok(entry.reply.clone());
        }
        let key = if cacheable && payload.len() <= self.remaining && self.replies.len() < 256 {
            Some(payload.clone())
        } else {
            None
        };
        let reply = self
            .client
            .invoke_service_raw(capability, kind, interface, operation, payload)
            .await?;
        if let Some(key) = key {
            let charge = key
                .len()
                .saturating_add(reply.len())
                .saturating_add(capability.len())
                .saturating_add(interface.len())
                .saturating_add(operation.len())
                .saturating_add(std::mem::size_of::<(
                    String,
                    String,
                    String,
                    Vec<u8>,
                    Vec<u8>,
                )>());
            if charge <= self.remaining && self.replies.try_reserve_exact(1).is_ok() {
                self.remaining -= charge;
                self.replies.push(CachedCaptureReply {
                    capability: capability.to_owned(),
                    interface: interface.to_owned(),
                    operation: operation.to_owned(),
                    payload: key,
                    reply: reply.clone(),
                });
            }
        }
        Ok(reply)
    }
}

#[cfg(test)]
pub async fn captured_history_window(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session_id: Uuid,
    pane_id: Uuid,
    pin: bmux_attach_pipeline::ScrollbackPin,
    offset: usize,
    rows: usize,
    projection: (
        usize,
        Option<bmux_attach_pipeline::CapturedHistoryAnchor>,
        isize,
    ),
) -> ClientResult<Option<bmux_attach_pipeline::PaneScrollbackWindow>> {
    Ok(
        match captured_history_window_outcome(
            client, session_id, pane_id, pin, offset, rows, projection,
        )
        .await?
        {
            CapturedWindowOutcome::Window(window) => Some(window),
            CapturedWindowOutcome::Unavailable => None,
        },
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "bounded navigation and assembly share one request and byte budget"
)]
pub async fn captured_history_window_outcome(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session_id: Uuid,
    pane_id: Uuid,
    pin: bmux_attach_pipeline::ScrollbackPin,
    offset: usize,
    rows: usize,
    projection: (
        usize,
        Option<bmux_attach_pipeline::CapturedHistoryAnchor>,
        isize,
    ),
) -> ClientResult<CapturedWindowOutcome> {
    let mut cached_client = CaptureReadCache {
        client,
        replies: Vec::new(),
        remaining: 256 * 1024,
    };
    let client = &mut cached_client;
    let mut decoded = CapturedLineCache::default();
    let (width, mut bottom_anchor, local_delta) = projection;
    let mut local_skip = usize::try_from(local_delta).unwrap_or(0);
    let Some(meta) = pin.capture else {
        return Ok(CapturedWindowOutcome::Unavailable);
    };
    if rows == 0 || rows > 256 || meta.width == 0 || width == 0 || width > 4096 {
        return Ok(CapturedWindowOutcome::Unavailable);
    }
    let capture = AttachState::HistoryCaptureV1 {
        width: meta.width,
        height: meta.height,
        capture_id: meta.identity,
        history_line_count: meta.lines,
        history_truncated: meta.truncated,
        pin: AttachState::PaneScrollbackPin {
            pane_id,
            pin_id: pin.pin_id,
            total_scrolled_rows: pin.total_scrolled_rows,
            max_scrollback_offset: u32::try_from(pin.max_scrollback_offset).unwrap_or(u32::MAX),
            stream_end: pin.stream_end,
        },
    };
    if bottom_anchor.is_some_and(|anchor| {
        anchor.capture_id != meta.identity
            || u64::from(anchor.line_index) >= meta.lines + u64::from(meta.height)
    }) {
        return Ok(CapturedWindowOutcome::Unavailable);
    }
    let mut remaining: usize = 2 * 1024 * 1024;
    let mut requests_left = 256;
    let mut styles = Vec::new();
    if bottom_anchor.is_none() && offset < usize::from(meta.height) {
        bottom_anchor = resolve_tail_entry(
            client,
            session_id,
            &capture,
            offset,
            &mut remaining,
            &mut styles,
            &mut requests_left,
        )
        .await?;
        if bottom_anchor.is_none() {
            return Ok(CapturedWindowOutcome::Unavailable);
        }
    }
    if local_delta < 0 {
        let Some(mut anchor) = bottom_anchor else {
            return Ok(CapturedWindowOutcome::Unavailable);
        };
        let mut advance = local_delta.unsigned_abs();
        let mut last_valid = None;
        loop {
            let Some(line) = decoded
                .resolve(
                    client,
                    session_id,
                    &capture,
                    anchor.line_index,
                    (&mut remaining, &mut styles, &mut requests_left),
                )
                .await?
            else {
                let Some(last) = last_valid else {
                    return Ok(CapturedWindowOutcome::Unavailable);
                };
                bottom_anchor = Some(last);
                break;
            };
            let row = line
                .row_for_column(width, anchor.column)
                .ok_or_else(|| history_decode_error(&"unavailable navigation anchor"))?;
            let available = line.projected_rows(width).saturating_sub(row + 1);
            if advance <= available {
                anchor.column = line
                    .column_for_row(width, row + advance)
                    .ok_or_else(|| history_decode_error(&"unavailable navigation destination"))?;
                bottom_anchor = Some(anchor);
                break;
            }
            anchor.column = line
                .column_for_row(width, row + available)
                .ok_or_else(|| history_decode_error(&"unavailable final capture row"))?;
            last_valid = Some(anchor);
            advance -= available + 1;
            let Some(next) = anchor
                .line_index
                .checked_add(1)
                .filter(|index| u64::from(*index) < meta.lines + u64::from(meta.height))
            else {
                bottom_anchor = last_valid;
                break;
            };
            anchor.line_index = next;
            anchor.column = 0;
        }
    }
    let scan_end = bottom_anchor.map_or(meta.lines, |anchor| u64::from(anchor.line_index) + 1);
    let mut skip = if bottom_anchor.is_some() {
        0
    } else {
        offset - usize::from(meta.height)
    };
    let mut selected = Vec::new();
    let mut anchors = Vec::new();
    anchors
        .try_reserve_exact(rows)
        .map_err(|error| history_decode_error(&error))?;
    remaining = remaining
        .checked_sub(rows.saturating_mul(std::mem::size_of::<
            bmux_attach_pipeline::CapturedHistoryAnchor,
        >()))
        .ok_or_else(|| history_decode_error(&"anchor budget exhausted"))?;
    selected
        .try_reserve_exact(rows)
        .map_err(|error| history_decode_error(&error))?;
    // Bound scanning independently of bytes (empty lines still cost requests).
    for index in (0..scan_end).rev().take(256) {
        let index = u32::try_from(index).map_err(|error| history_decode_error(&error))?;
        let mut line = if bottom_anchor.is_some() {
            let Some(line) = decoded
                .resolve(
                    client,
                    session_id,
                    &capture,
                    index,
                    (&mut remaining, &mut styles, &mut requests_left),
                )
                .await?
            else {
                return Ok(CapturedWindowOutcome::Unavailable);
            };
            line
        } else {
            std::sync::Arc::new(
                fetch_captured_history_line(
                    client,
                    session_id,
                    &capture,
                    index,
                    &mut remaining,
                    &mut styles,
                    &mut requests_left,
                )
                .await?,
            )
        };
        let count = line.projected_rows(usize::from(meta.width));
        if skip >= count {
            skip -= count;
            continue;
        }
        // Resolve a pending prefix through the same complete line used on refresh.
        // Its last captured row remains the boundary, not the appended main tail.
        let pending_column = if bottom_anchor.is_none()
            && matches!(
                line.completed(),
                Some((_, bmux_terminal_grid::HistorySliceEnd::Open))
            ) {
            let column = line
                .column_for_row(usize::from(meta.width), count - skip - 1)
                .ok_or_else(|| history_decode_error(&"unavailable pending boundary"))?;
            line = decoded
                .resolve(
                    client,
                    session_id,
                    &capture,
                    index,
                    (&mut remaining, &mut styles, &mut requests_left),
                )
                .await?
                .ok_or_else(|| history_decode_error(&"unavailable joined pending line"))?;
            Some(column)
        } else {
            None
        };
        // Offset remains in capture rows. Resolve the exclusive lower boundary
        // through content coordinates before selecting locally projected rows.
        let end = if let Some(column) = pending_column {
            line.row_for_column(width, column)
                .ok_or_else(|| history_decode_error(&"unavailable joined boundary"))?
                + 1
        } else if let Some(anchor) = bottom_anchor.filter(|anchor| anchor.line_index == index) {
            line.row_for_column(width, anchor.column)
                .ok_or_else(|| history_decode_error(&"unavailable viewport anchor"))?
                + 1
        } else if skip == 0 {
            line.projected_rows(width)
        } else {
            let column = line
                .column_for_row(usize::from(meta.width), count - skip)
                .ok_or_else(|| history_decode_error(&"unavailable capture boundary"))?;
            line.row_for_column(width, column)
                .ok_or_else(|| history_decode_error(&"unavailable projection boundary"))?
        };
        let skipped = local_skip.min(end);
        local_skip -= skipped;
        let end = end - skipped;
        let start = end.saturating_sub(rows - selected.len());
        let projected = project_captured_range(&line, width, start..end, &mut remaining)?;
        for row in (start..end).rev() {
            let column = line
                .column_for_row(width, row)
                .ok_or_else(|| history_decode_error(&"unavailable logical row origin"))?;
            anchors.push(bmux_attach_pipeline::CapturedHistoryAnchor {
                capture_id: meta.identity,
                line_index: index,
                column,
            });
        }
        selected.extend(projected.into_iter().rev());
        skip = 0;
        if selected.len() == rows {
            break;
        }
    }
    if selected.len() != rows {
        return Ok(CapturedWindowOutcome::Unavailable);
    }
    selected.reverse();
    anchors.reverse();
    Ok(CapturedWindowOutcome::Window(
        bmux_attach_pipeline::PaneScrollbackWindow {
            projection_width: width,
            row_anchors: anchors,
            palette: bmux_terminal_grid::StylePalette::from_styles(styles),
            rows: selected,
            scrollback_offset: offset,
            max_scrollback_offset: pin.max_scrollback_offset,
            total_scrolled_rows: pin.total_scrolled_rows,
        },
    ))
}

/// Project captured tail lines without resizing the replica.
/// The first logical line includes any pending prefix from the same capture.
#[cfg(test)]
pub async fn captured_tail_window(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session: Uuid,
    pane: Uuid,
    pin: bmux_attach_pipeline::ScrollbackPin,
    offset: usize,
    rows: usize,
    width: usize,
) -> ClientResult<Option<bmux_attach_pipeline::PaneScrollbackWindow>> {
    use bmux_terminal_grid::{HistoryLineAssembly, HistorySlice, HistorySliceEnd};
    let Some(meta) = pin.capture else {
        return Ok(None);
    };
    if width == 0
        || width > 4096
        || rows == 0
        || rows > 256
        || offset >= usize::from(meta.height)
        || usize::from(meta.height).saturating_sub(offset) > 256
    {
        return Ok(None);
    }
    let mut budget = 2 * 1024 * 1024;
    let mut styles = Vec::new();
    let mut output = Vec::new();
    let end_row = usize::from(meta.height) - offset;
    // Fetch forward so a row is never mistaken for a logical-line start.
    // The pending-history prefix is fetched under the same pin before row zero.
    let mut requests_left = 512_usize;
    let mut line = captured_tail_prefix(
        client,
        session,
        pane,
        pin,
        &mut budget,
        &mut styles,
        &mut requests_left,
    )
    .await?;
    let mut line_index = 0;
    for row in 0..end_row {
        let mut source_offset = 0_u32;
        loop {
            requests_left = requests_left
                .checked_sub(1)
                .ok_or_else(|| history_decode_error(&"tail request budget exhausted"))?;
            let reply = AttachState::client::attach_main_row_slice_v1(
                client,
                session,
                pane,
                pin.pin_id,
                meta.identity,
                u32::try_from(row).map_err(|error| history_decode_error(&error))?,
                source_offset,
                256,
                4096,
            )
            .await
            .map_err(|error| history_decode_error(&error))?
            .map_err(|error| history_decode_error(&format!("{error:?}")))?;
            let cells =
                decode_tail_slice(&reply, source_offset, meta.width, &mut styles, &mut budget)?;
            let next = tail_next_column(line.next_offset(), &cells)?;
            let end = if matches!(reply.end_kind, AttachState::HistoryEnd::HardBreak) {
                HistorySliceEnd::HardBreak
            } else if row + 1 == end_row && matches!(reply.end_kind, AttachState::HistoryEnd::Open)
            {
                HistorySliceEnd::Open
            } else {
                HistorySliceEnd::Continue
            };
            line.append(
                meta.identity.as_u128(),
                line_index,
                line.next_offset(),
                &HistorySlice {
                    cells: &cells,
                    next_cell_offset: next,
                    end,
                },
            )
            .map_err(|error| history_decode_error(&format!("{error:?}")))?;
            source_offset = u32::try_from(reply.next_cell_offset)
                .map_err(|error| history_decode_error(&error))?;
            if !matches!(reply.end_kind, AttachState::HistoryEnd::Continue) {
                break;
            }
        }
        if line.completed().is_some() {
            append_tail_projection(&line, width, rows, &mut budget, &mut output)?;
            line_index = row + 1;
            line = HistoryLineAssembly::new(meta.identity.as_u128(), line_index, budget, false);
        }
    }
    if output.len() != rows {
        return Ok(None);
    }
    Ok(Some(bmux_attach_pipeline::PaneScrollbackWindow {
        projection_width: width,
        row_anchors: Vec::new(),
        palette: bmux_terminal_grid::StylePalette::from_styles(styles),
        rows: output,
        scrollback_offset: offset,
        max_scrollback_offset: pin.max_scrollback_offset,
        total_scrolled_rows: pin.total_scrolled_rows,
    }))
}

/// Convert a main-screen capture row into a capture-wide logical anchor.
async fn resolve_tail_entry(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session: Uuid,
    capture: &AttachState::HistoryCaptureV1,
    offset: usize,
    budget: &mut usize,
    styles: &mut Vec<bmux_terminal_grid::Style>,
    requests: &mut usize,
) -> ClientResult<Option<bmux_attach_pipeline::CapturedHistoryAnchor>> {
    use bmux_terminal_grid::HistorySliceEnd;
    let mut index =
        u32::try_from(capture.history_line_count).map_err(|error| history_decode_error(&error))?;
    let mut prefix_columns = 0;
    if index > 0 {
        let last = fetch_captured_history_line(
            client,
            session,
            capture,
            index - 1,
            budget,
            styles,
            requests,
        )
        .await?;
        if matches!(last.completed(), Some((_, HistorySliceEnd::Open))) {
            index -= 1;
            prefix_columns = last.next_offset();
        }
    }
    let target = usize::from(capture.height)
        .checked_sub(offset + 1)
        .ok_or_else(|| history_decode_error(&"invalid tail entry"))?;
    for row in 0..=target {
        if row == target {
            return Ok(Some(bmux_attach_pipeline::CapturedHistoryAnchor {
                capture_id: capture.capture_id,
                line_index: index,
                column: prefix_columns,
            }));
        }
        let mut cell_offset = 0;
        let end = loop {
            *requests = requests
                .checked_sub(1)
                .ok_or_else(|| history_decode_error(&"entry request limit"))?;
            let reply = AttachState::client::attach_main_row_slice_v1(
                client,
                session,
                capture.pin.pane_id,
                capture.pin.pin_id,
                capture.capture_id,
                u32::try_from(row).map_err(|error| history_decode_error(&error))?,
                cell_offset,
                256,
                4096,
            )
            .await
            .map_err(|error| history_decode_error(&error))?
            .map_err(|error| history_decode_error(&format!("{error:?}")))?;
            // Validate every slice, even though entry only needs row boundaries.
            decode_tail_slice(&reply, cell_offset, capture.width, styles, budget)?;
            cell_offset = u32::try_from(reply.next_cell_offset)
                .map_err(|error| history_decode_error(&error))?;
            if !matches!(reply.end_kind, AttachState::HistoryEnd::Continue) {
                break reply.end_kind;
            }
        };
        if matches!(end, AttachState::HistoryEnd::HardBreak) {
            index = index
                .checked_add(1)
                .ok_or_else(|| history_decode_error(&"tail index overflow"))?;
            prefix_columns = 0;
        } else {
            prefix_columns = prefix_columns
                .checked_add(usize::from(capture.width))
                .ok_or_else(|| history_decode_error(&"entry column overflow"))?;
        }
    }
    Ok(None)
}

#[derive(Default)]
struct CapturedLineCache {
    lines: Vec<(u32, std::sync::Arc<bmux_terminal_grid::HistoryLineAssembly>)>,
}

impl CapturedLineCache {
    async fn resolve(
        &mut self,
        client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
        session: Uuid,
        capture: &AttachState::HistoryCaptureV1,
        index: u32,
        budgets: (&mut usize, &mut Vec<bmux_terminal_grid::Style>, &mut usize),
    ) -> ClientResult<Option<std::sync::Arc<bmux_terminal_grid::HistoryLineAssembly>>> {
        if let Some((_, line)) = self.lines.iter().find(|(key, _)| *key == index) {
            return Ok(Some(std::sync::Arc::clone(line)));
        }
        let (remaining, styles, requests) = budgets;
        if self.lines.len() >= 256 {
            return Err(history_decode_error(&"decoded line cache limit"));
        }
        let charge = std::mem::size_of::<bmux_terminal_grid::HistoryLineAssembly>()
            + 4 * std::mem::size_of::<usize>()
            + std::mem::size_of::<u32>();
        *remaining = remaining
            .checked_sub(charge)
            .ok_or_else(|| history_decode_error(&"decoded line metadata budget"))?;
        let Some(line) = fetch_captured_content_line(
            client, session, capture, index, remaining, styles, requests,
        )
        .await?
        else {
            return Ok(None);
        };
        self.lines
            .try_reserve_exact(1)
            .map_err(|error| history_decode_error(&error))?;
        let line = std::sync::Arc::new(line);
        self.lines.push((index, std::sync::Arc::clone(&line)));
        Ok(Some(line))
    }
}

/// Resolve a capture-wide logical index, including the pending history/main join.
#[allow(
    clippy::too_many_lines,
    reason = "ordered prefix and paginated main-row assembly share capture identity and mutable admission budgets"
)]
async fn fetch_captured_content_line(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session: Uuid,
    capture: &AttachState::HistoryCaptureV1,
    index: u32,
    budget: &mut usize,
    styles: &mut Vec<bmux_terminal_grid::Style>,
    requests: &mut usize,
) -> ClientResult<Option<bmux_terminal_grid::HistoryLineAssembly>> {
    use bmux_terminal_grid::{HistoryLineAssembly, HistorySlice, HistorySliceEnd};
    let mut logical = usize::try_from(capture.history_line_count)
        .map_err(|error| history_decode_error(&error))?;
    let mut prefix = None;
    if logical > 0 {
        let last = u32::try_from(logical - 1).map_err(|error| history_decode_error(&error))?;
        if index < last {
            return fetch_captured_history_line(
                client, session, capture, index, budget, styles, requests,
            )
            .await
            .map(Some);
        }
        let history =
            fetch_captured_history_line(client, session, capture, last, budget, styles, requests)
                .await?;
        if matches!(history.completed(), Some((_, HistorySliceEnd::Open))) {
            logical -= 1;
            prefix = Some(history);
        } else if index == last {
            return Ok(Some(history));
        }
    }
    let mut line = HistoryLineAssembly::new(
        capture.capture_id.as_u128(),
        logical,
        *budget,
        capture.history_truncated && logical == 0,
    );
    if let Some(prefix) = prefix {
        let (cells, _) = prefix
            .completed()
            .ok_or_else(|| history_decode_error(&"unfinished prefix"))?;
        line.append(
            capture.capture_id.as_u128(),
            logical,
            0,
            &HistorySlice {
                cells,
                next_cell_offset: prefix.next_offset(),
                end: HistorySliceEnd::Continue,
            },
        )
        .map_err(|error| history_decode_error(&format!("{error:?}")))?;
    }
    line.retain_trailing_cells();
    for row in 0..capture.height {
        let mut offset = 0;
        loop {
            *requests = requests
                .checked_sub(1)
                .ok_or_else(|| history_decode_error(&"content request limit"))?;
            let reply = AttachState::client::attach_main_row_slice_v1(
                client,
                session,
                capture.pin.pane_id,
                capture.pin.pin_id,
                capture.capture_id,
                u32::from(row),
                offset,
                256,
                4096,
            )
            .await
            .map_err(|error| history_decode_error(&error))?
            .map_err(|error| history_decode_error(&format!("{error:?}")))?;
            let mut cells = decode_tail_slice(&reply, offset, capture.width, styles, budget)?;
            if offset == 0
                && reply.next_cell_offset == 0
                && cells.is_empty()
                && matches!(reply.end_kind, AttachState::HistoryEnd::HardBreak)
            {
                pad_wrapped_tail(&mut cells, 1, budget, styles)?;
            }
            line.retain_trailing_cells();
            let end = match reply.end_kind {
                AttachState::HistoryEnd::HardBreak => HistorySliceEnd::HardBreak,
                AttachState::HistoryEnd::Open if row + 1 == capture.height => HistorySliceEnd::Open,
                _ => HistorySliceEnd::Continue,
            };
            line.append(
                capture.capture_id.as_u128(),
                logical,
                line.next_offset(),
                &HistorySlice {
                    cells: &cells,
                    next_cell_offset: tail_next_column(line.next_offset(), &cells)?,
                    end,
                },
            )
            .map_err(|error| history_decode_error(&format!("{error:?}")))?;
            offset = u32::try_from(reply.next_cell_offset)
                .map_err(|error| history_decode_error(&error))?;
            if !matches!(reply.end_kind, AttachState::HistoryEnd::Continue) {
                break;
            }
        }
        if line.completed().is_some() {
            if logical == index as usize {
                return Ok(Some(line));
            }
            logical += 1;
            line = HistoryLineAssembly::new(capture.capture_id.as_u128(), logical, *budget, false);
        }
    }
    Ok(None)
}

fn tail_next_column(offset: usize, cells: &[bmux_terminal_grid::Cell]) -> ClientResult<usize> {
    cells
        .iter()
        .try_fold(offset, |offset, cell| {
            offset.checked_add(usize::from(cell.width()))
        })
        .ok_or_else(|| history_decode_error(&"tail column overflow"))
}

#[cfg(test)]
async fn captured_tail_prefix(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session: Uuid,
    pane: Uuid,
    pin: bmux_attach_pipeline::ScrollbackPin,
    budget: &mut usize,
    styles: &mut Vec<bmux_terminal_grid::Style>,
    requests_left: &mut usize,
) -> ClientResult<bmux_terminal_grid::HistoryLineAssembly> {
    use bmux_terminal_grid::{HistoryLineAssembly, HistorySlice, HistorySliceEnd};
    let meta = pin
        .capture
        .ok_or_else(|| history_decode_error(&"missing capture"))?;
    let mut line = HistoryLineAssembly::new(
        meta.identity.as_u128(),
        0,
        *budget,
        meta.truncated && meta.lines == 0,
    );
    if meta.lines == 0 {
        return Ok(line);
    }
    let capture = AttachState::HistoryCaptureV1 {
        width: meta.width,
        height: meta.height,
        capture_id: meta.identity,
        history_line_count: meta.lines,
        history_truncated: meta.truncated,
        pin: AttachState::PaneScrollbackPin {
            pane_id: pane,
            pin_id: pin.pin_id,
            total_scrolled_rows: pin.total_scrolled_rows,
            max_scrollback_offset: u32::try_from(pin.max_scrollback_offset).unwrap_or(u32::MAX),
            stream_end: pin.stream_end,
        },
    };
    let index = u32::try_from(meta.lines - 1).map_err(|error| history_decode_error(&error))?;
    let prefix = fetch_captured_history_line(
        client,
        session,
        &capture,
        index,
        budget,
        styles,
        requests_left,
    )
    .await?;
    if let Some((cells, HistorySliceEnd::Open)) = prefix.completed() {
        // Reopen only the pending prefix, never a completed hard-ended history line.
        line = HistoryLineAssembly::new(
            meta.identity.as_u128(),
            0,
            *budget,
            meta.truncated && index == 0,
        );
        line.append(
            meta.identity.as_u128(),
            0,
            0,
            &HistorySlice {
                cells,
                next_cell_offset: prefix.next_offset(),
                end: HistorySliceEnd::Continue,
            },
        )
        .map_err(|error| history_decode_error(&format!("{error:?}")))?;
    }
    Ok(line)
}

#[cfg(test)]
fn append_tail_projection(
    line: &bmux_terminal_grid::HistoryLineAssembly,
    width: usize,
    rows: usize,
    budget: &mut usize,
    output: &mut Vec<bmux_terminal_grid::PhysicalRow>,
) -> ClientResult<()> {
    let count = line.projected_rows(width);
    let projected = project_captured_range(line, width, count.saturating_sub(rows)..count, budget)?;
    output
        .try_reserve_exact(projected.len())
        .map_err(|error| history_decode_error(&error))?;
    output.extend(projected);
    if output.len() > rows {
        output.drain(..output.len() - rows);
    }
    Ok(())
}

fn project_captured_range(
    line: &bmux_terminal_grid::HistoryLineAssembly,
    width: usize,
    range: std::ops::Range<usize>,
    budget: &mut usize,
) -> ClientResult<Vec<bmux_terminal_grid::PhysicalRow>> {
    use bmux_terminal_grid::{Cell, PhysicalRow};
    let selected = range.end.saturating_sub(range.start);
    // Charge before projection, including temporary and retained row/cell arrays.
    // Deliberately retain charges for discarded rows: the whole fetch has a
    // cumulative allocation allowance, not a fresh allowance for every line.
    let text = line
        .completed()
        .ok_or_else(|| history_decode_error(&"unfinished tail line"))?
        .0
        .iter()
        .try_fold(0_usize, |bytes, cell| bytes.checked_add(cell.text().len()))
        .ok_or_else(|| history_decode_error(&"tail text overflow"))?;
    let charge = selected
        .checked_mul(width)
        .and_then(|cells| cells.checked_mul(std::mem::size_of::<Cell>() + 1))
        .and_then(|bytes| {
            bytes.checked_add(selected.checked_mul(std::mem::size_of::<PhysicalRow>())?)
        })
        .and_then(|bytes| bytes.checked_add(text))
        .and_then(|bytes| bytes.checked_mul(2))
        .ok_or_else(|| history_decode_error(&"tail projection overflow"))?;
    let remaining = budget
        .checked_sub(charge)
        .ok_or_else(|| history_decode_error(&"tail projection budget exhausted"))?;
    let projected = line
        .project(width, range, charge)
        .map_err(|error| history_decode_error(&format!("{error:?}")))?;
    *budget = remaining;
    Ok(projected)
}

fn decode_tail_slice(
    reply: &AttachState::HistorySliceV1,
    offset: u32,
    width: u16,
    styles: &mut Vec<bmux_terminal_grid::Style>,
    budget: &mut usize,
) -> ClientResult<Vec<bmux_terminal_grid::Cell>> {
    if reply.encoded.len() > 128 * 1024 {
        return Err(history_decode_error(&"oversized tail reply"));
    }
    let mut cells = decode_history_cells(&reply.encoded, styles, budget)?;
    let next = cells.iter().try_fold(u64::from(offset), |offset, cell| {
        offset.checked_add(u64::from(cell.width()))
    });
    if next != Some(reply.next_cell_offset)
        || reply.next_cell_offset > u64::from(width)
        || (matches!(reply.end_kind, AttachState::HistoryEnd::Continue)
            && reply.next_cell_offset == u64::from(offset))
    {
        return Err(history_decode_error(&"invalid tail continuation"));
    }
    if matches!(reply.end_kind, AttachState::HistoryEnd::Open) {
        let padding = usize::from(width)
            - usize::try_from(reply.next_cell_offset)
                .map_err(|error| history_decode_error(&error))?;
        pad_wrapped_tail(&mut cells, padding, budget, styles)?;
    }
    Ok(cells)
}

fn pad_wrapped_tail(
    cells: &mut Vec<bmux_terminal_grid::Cell>,
    padding: usize,
    budget: &mut usize,
    styles: &mut Vec<bmux_terminal_grid::Style>,
) -> ClientResult<()> {
    use bmux_terminal_grid::{Cell, Style, StyleId};
    let charge = padding
        .checked_mul(2 * (std::mem::size_of::<Cell>() + 1))
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Style>()))
        .ok_or_else(|| history_decode_error(&"tail padding overflow"))?;
    *budget = budget
        .checked_sub(charge)
        .ok_or_else(|| history_decode_error(&"tail padding budget"))?;
    let index = if let Some(index) = styles.iter().position(|style| *style == Style::default()) {
        index
    } else {
        styles
            .try_reserve_exact(1)
            .map_err(|error| history_decode_error(&error))?;
        styles.push(Style::default());
        styles.len() - 1
    };
    let style = StyleId(u32::try_from(index).map_err(|error| history_decode_error(&error))?);
    cells
        .try_reserve_exact(padding)
        .map_err(|error| history_decode_error(&error))?;
    cells.extend((0..padding).map(|_| Cell::new(" ".to_owned(), style, 1)));
    Ok(())
}

fn history_decode_error(error: &impl std::fmt::Display) -> ClientError {
    ClientError::ServerError {
        code: ErrorCode::Internal,
        message: format!("captured history: {error}"),
    }
}

fn decode_history_cells(
    encoded: &[u8],
    styles: &mut Vec<bmux_terminal_grid::Style>,
    remaining: &mut usize,
) -> ClientResult<Vec<bmux_terminal_grid::Cell>> {
    use serde::de::{Error, SeqAccess, Visitor};
    struct Cells<'a> {
        styles: &'a mut Vec<bmux_terminal_grid::Style>,
        remaining: &'a mut usize,
    }
    impl<'de> Visitor<'de> for Cells<'_> {
        type Value = Vec<bmux_terminal_grid::Cell>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("bounded history cells")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            use bmux_terminal_grid::{Cell, Style, StyleId};
            let mut cells = Vec::new();
            while let Some((text, width, style)) = seq.next_element::<(String, u8, Style)>()? {
                if cells.len() >= 256 || !(1..=2).contains(&width) || text.len() > 4096 {
                    return Err(A::Error::custom("invalid history cell limits"));
                }
                // Charge both temporary decoded and retained copies, plus a
                // conservative palette slot even when the style already exists.
                let charge =
                    2 * (std::mem::size_of::<Cell>() + text.len()) + std::mem::size_of::<Style>();
                *self.remaining = self
                    .remaining
                    .checked_sub(charge)
                    .ok_or_else(|| A::Error::custom("history budget exhausted"))?;
                let index =
                    if let Some(index) = self.styles.iter().position(|entry| *entry == style) {
                        index
                    } else {
                        self.styles.try_reserve_exact(1).map_err(A::Error::custom)?;
                        self.styles.push(style);
                        self.styles.len() - 1
                    };
                cells.try_reserve_exact(1).map_err(A::Error::custom)?;
                cells.push(Cell::new(
                    text,
                    StyleId(u32::try_from(index).map_err(A::Error::custom)?),
                    width,
                ));
            }
            Ok(cells)
        }
    }
    let mut decoder = serde_json::Deserializer::from_slice(encoded);
    let cells = serde::Deserializer::deserialize_seq(&mut decoder, Cells { styles, remaining })
        .map_err(|error| history_decode_error(&error))?;
    decoder
        .end()
        .map_err(|error| history_decode_error(&error))?;
    Ok(cells)
}

pub async fn attach_pane_scrollback_pin_streaming(
    client: &mut bmux_client::StreamingBmuxClient,
    session_id: Uuid,
    pane_id: Uuid,
) -> ClientResult<PaneScrollbackPinResult> {
    match AttachState::client::attach_history_capture_v1(client, session_id, pane_id).await {
        Ok(Ok(captured)) => {
            let pin = captured.pin;
            Ok(PaneScrollbackPinResult {
                capture: bmux_attach_pipeline::ScrollbackCapture {
                    identity: captured.capture_id,
                    lines: captured.history_line_count,
                    truncated: captured.history_truncated,
                    width: captured.width,
                    height: captured.height,
                },
                pane_id: pin.pane_id,
                pin_id: pin.pin_id,
                total_scrolled_rows: pin.total_scrolled_rows,
                max_scrollback_offset: pin.max_scrollback_offset as usize,
                stream_end: pin.stream_end,
            })
        }
        Ok(Err(err)) => typed_server_error("attach-pane-scrollback-pin", err),
        Err(err) => typed_dispatch_error("attach-pane-scrollback-pin", err),
    }
}

pub async fn attach_pane_scrollback_unpin_streaming(
    client: &mut bmux_client::StreamingBmuxClient,
    session_id: Uuid,
    pane_id: Uuid,
    pin_id: u64,
) -> ClientResult<bool> {
    match AttachState::client::attach_pane_scrollback_unpin(client, session_id, pane_id, pin_id)
        .await
    {
        Ok(Ok(ack)) => Ok(ack.released),
        Ok(Err(err)) => typed_server_error("attach-pane-scrollback-unpin", err),
        Err(err) => typed_dispatch_error("attach-pane-scrollback-unpin", err),
    }
}

pub async fn attach_pane_grid_delta_state_streaming(
    client: &mut bmux_client::StreamingBmuxClient,
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    base_revisions: Vec<u64>,
    max_batches_per_pane: usize,
) -> ClientResult<Vec<PaneGridDeltaResult>> {
    let max_batches_per_pane = u32::try_from(max_batches_per_pane).unwrap_or(u32::MAX);
    match AttachState::client::attach_pane_grid_delta_state(
        client,
        session_id,
        pane_ids,
        base_revisions,
        max_batches_per_pane,
    )
    .await
    {
        Ok(Ok(state)) => Ok(state
            .deltas
            .into_iter()
            .map(|delta| PaneGridDeltaResult {
                pane_id: delta.pane_id,
                base_revision: delta.base_revision,
                revision: delta.revision,
                desynced: delta.desynced,
                encoded: delta.encoded,
            })
            .collect()),
        Ok(Err(err)) => typed_server_error("attach-pane-grid-delta-state", err),
        Err(err) => typed_dispatch_error("attach-pane-grid-delta-state", err),
    }
}

#[derive(serde::Deserialize)]
struct LayoutPayload {
    panes: Vec<PaneSummary>,
    layout_root: PaneLayoutNode,
    scene: AttachScene,
    zoomed: bool,
}

#[derive(serde::Deserialize)]
struct SnapshotLayoutPayload {
    panes: Vec<PaneSummary>,
    layout_root: PaneLayoutNode,
    scene: AttachScene,
}

pub trait BmuxPaneRuntimeClientExt {
    fn attach_grant(
        &mut self,
        selector: SessionSelector,
    ) -> impl Future<Output = ClientResult<AttachGrant>> + Send;

    fn attach_context_grant(
        &mut self,
        selector: ContextSelector,
    ) -> impl Future<Output = ClientResult<AttachGrant>> + Send;

    fn retarget_attach_context(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> impl Future<Output = ClientResult<AttachOpenInfo>> + Send;

    #[allow(
        clippy::too_many_arguments,
        reason = "the four independent viewport edges mirror the generated typed attach contract; a second wrapper DTO would obscure that boundary"
    )]
    fn retarget_attach_context_with_insets(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
        top_inset: u16,
        right_inset: u16,
        bottom_inset: u16,
        left_inset: u16,
    ) -> impl Future<Output = ClientResult<AttachOpenInfo>> + Send;

    fn open_attach_stream_info(
        &mut self,
        grant: &AttachGrant,
    ) -> impl Future<Output = ClientResult<AttachOpenInfo>> + Send;

    fn detach(&mut self) -> impl Future<Output = ClientResult<()>> + Send;

    fn set_attach_policy(
        &mut self,
        allow_detach: bool,
    ) -> impl Future<Output = ClientResult<()>> + Send;

    fn attach_input(
        &mut self,
        session_id: Uuid,
        data: Vec<u8>,
    ) -> impl Future<Output = ClientResult<usize>> + Send;

    fn pane_direct_input(
        &mut self,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> impl Future<Output = ClientResult<usize>> + Send;

    fn attach_set_viewport(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> impl Future<Output = ClientResult<(u16, u16)>> + Send;

    #[allow(
        clippy::too_many_arguments,
        reason = "the four independent viewport edges mirror the generated typed attach contract; a second wrapper DTO would obscure that boundary"
    )]
    fn attach_set_viewport_with_insets(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
        top_inset: u16,
        right_inset: u16,
        bottom_inset: u16,
        left_inset: u16,
    ) -> impl Future<Output = ClientResult<(u16, u16)>> + Send;

    fn attach_layout(
        &mut self,
        session_id: Uuid,
    ) -> impl Future<Output = ClientResult<AttachLayoutState>> + Send;

    fn attach_pane_output_batch(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> impl Future<Output = ClientResult<PaneOutputBatchResult>> + Send;

    // Used by the attach runtime binary path; `cargo check -p bmux_cli` checks the
    // library target separately and reports this trait item as otherwise unused.
    #[allow(dead_code)]
    fn attach_pane_images(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        since_sequences: Vec<u64>,
    ) -> impl Future<Output = ClientResult<Vec<AttachPaneImageDelta>>> + Send;

    fn attach_snapshot(
        &mut self,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> impl Future<Output = ClientResult<AttachSnapshotState>> + Send;

    fn attach_pane_snapshot(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes_per_pane: usize,
    ) -> impl Future<Output = ClientResult<AttachPaneSnapshotState>> + Send;
}

impl BmuxPaneRuntimeClientExt for bmux_client::BmuxClient {
    async fn attach_grant(&mut self, selector: SessionSelector) -> ClientResult<AttachGrant> {
        match AttachCommands::client::attach_session(
            self,
            pane_runtime_session_selector(selector),
            true,
        )
        .await
        {
            Ok(Ok(grant)) => Ok(AttachGrant {
                attach_token: grant.token,
                session_id: grant.session_id,
                context_id: grant.context_id,
                expires_at_epoch_ms: grant.expires_epoch_ms,
            }),
            Ok(Err(err)) => typed_server_error("attach-session", err),
            Err(err) => typed_dispatch_error("attach-session", err),
        }
    }

    async fn attach_context_grant(
        &mut self,
        selector: ContextSelector,
    ) -> ClientResult<AttachGrant> {
        match AttachCommands::client::attach_context(
            self,
            pane_runtime_context_selector(selector),
            true,
        )
        .await
        {
            Ok(Ok(grant)) => Ok(AttachGrant {
                attach_token: grant.token,
                session_id: grant.session_id,
                context_id: grant.context_id,
                expires_at_epoch_ms: grant.expires_epoch_ms,
            }),
            Ok(Err(err)) => typed_server_error("attach-context", err),
            Err(err) => typed_dispatch_error("attach-context", err),
        }
    }

    async fn retarget_attach_context(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientResult<AttachOpenInfo> {
        self.retarget_attach_context_with_insets(context_id, cols, rows, 0, 0, 0, 0)
            .await
    }

    async fn retarget_attach_context_with_insets(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
        top_inset: u16,
        right_inset: u16,
        bottom_inset: u16,
        left_inset: u16,
    ) -> ClientResult<AttachOpenInfo> {
        match AttachCommands::client::attach_retarget_context(
            self,
            context_id,
            true,
            cols,
            rows,
            top_inset,
            right_inset,
            bottom_inset,
            left_inset,
            cell_pixel_width(),
            cell_pixel_height(),
        )
        .await
        {
            Ok(Ok(ready)) => Ok(AttachOpenInfo {
                context_id: ready.context_id,
                session_id: ready.session_id,
                can_write: ready.can_write,
            }),
            Ok(Err(err)) => typed_server_error("attach-retarget-context", err),
            Err(err) => typed_dispatch_error("attach-retarget-context", err),
        }
    }

    async fn open_attach_stream_info(
        &mut self,
        grant: &AttachGrant,
    ) -> ClientResult<AttachOpenInfo> {
        match AttachCommands::client::attach_open(self, grant.session_id, grant.attach_token).await
        {
            Ok(Ok(ready)) => Ok(AttachOpenInfo {
                context_id: ready.context_id,
                session_id: ready.session_id,
                can_write: ready.can_write,
            }),
            Ok(Err(err)) => typed_server_error("attach-open", err),
            Err(err) => typed_dispatch_error("attach-open", err),
        }
    }

    async fn detach(&mut self) -> ClientResult<()> {
        match AttachCommands::client::detach(self).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => typed_server_error("detach", err),
            Err(err) => typed_dispatch_error("detach", err),
        }
    }

    async fn set_attach_policy(&mut self, allow_detach: bool) -> ClientResult<()> {
        match AttachCommands::client::set_client_attach_policy(self, allow_detach).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => typed_server_error("set-client-attach-policy", err),
            Err(err) => typed_dispatch_error("set-client-attach-policy", err),
        }
    }

    async fn attach_input(&mut self, session_id: Uuid, data: Vec<u8>) -> ClientResult<usize> {
        match AttachCommands::client::attach_input(self, session_id, data).await {
            Ok(Ok(accepted)) => Ok(accepted.bytes as usize),
            Ok(Err(err)) => typed_server_error("attach-input", err),
            Err(err) => typed_dispatch_error("attach-input", err),
        }
    }

    async fn pane_direct_input(
        &mut self,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> ClientResult<usize> {
        let bytes_len = data.len();
        match PaneCommands::client::pane_direct_input(self, session_id, pane_id, data).await {
            Ok(Ok(_ack)) => Ok(bytes_len),
            Ok(Err(err)) => typed_server_error("pane-direct-input", err),
            Err(err) => typed_dispatch_error("pane-direct-input", err),
        }
    }

    async fn attach_set_viewport(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientResult<(u16, u16)> {
        self.attach_set_viewport_with_insets(session_id, cols, rows, 0, 0, 0, 0)
            .await
    }

    async fn attach_set_viewport_with_insets(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
        top_inset: u16,
        right_inset: u16,
        bottom_inset: u16,
        left_inset: u16,
    ) -> ClientResult<(u16, u16)> {
        match AttachCommands::client::attach_set_viewport(
            self,
            session_id,
            cols,
            rows,
            top_inset,
            right_inset,
            bottom_inset,
            left_inset,
            cell_pixel_width(),
            cell_pixel_height(),
        )
        .await
        {
            Ok(Ok(set)) => Ok((set.cols, set.rows)),
            Ok(Err(err)) => typed_server_error("attach-set-viewport", err),
            Err(err) => typed_dispatch_error("attach-set-viewport", err),
        }
    }

    async fn attach_layout(&mut self, session_id: Uuid) -> ClientResult<AttachLayoutState> {
        match AttachState::client::attach_layout_state(self, session_id).await {
            Ok(Ok(layout)) => decode_attach_layout(&layout),
            Ok(Err(err)) => typed_server_error("attach-layout-state", err),
            Err(err) => typed_dispatch_error("attach-layout-state", err),
        }
    }

    async fn attach_pane_output_batch(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> ClientResult<PaneOutputBatchResult> {
        let max_bytes_u32 = u32::try_from(max_bytes).unwrap_or(u32::MAX);
        match AttachState::client::attach_pane_output_batch(
            self,
            session_id,
            pane_ids,
            max_bytes_u32,
        )
        .await
        {
            Ok(Ok(batch)) => Ok(PaneOutputBatchResult {
                chunks: batch
                    .chunks
                    .into_iter()
                    .map(pane_chunk_from_record)
                    .collect(),
                output_still_pending: batch.output_still_pending,
            }),
            Ok(Err(err)) => typed_server_error("attach-pane-output-batch", err),
            Err(err) => typed_dispatch_error("attach-pane-output-batch", err),
        }
    }

    async fn attach_pane_images(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        since_sequences: Vec<u64>,
    ) -> ClientResult<Vec<AttachPaneImageDelta>> {
        match AttachState::client::attach_pane_images(self, session_id, pane_ids, since_sequences)
            .await
        {
            Ok(Ok(images)) => serde_json::from_slice::<Vec<AttachPaneImageDelta>>(&images.encoded)
                .map_err(|e| ClientError::ServerError {
                    code: ErrorCode::Internal,
                    message: format!("decode pane-images deltas: {e}"),
                }),
            Ok(Err(err)) => typed_server_error("attach-pane-images", err),
            Err(err) => typed_dispatch_error("attach-pane-images", err),
        }
    }

    async fn attach_snapshot(
        &mut self,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> ClientResult<AttachSnapshotState> {
        let max_bytes_u32 = u32::try_from(max_bytes_per_pane).unwrap_or(u32::MAX);
        match AttachState::client::attach_snapshot_state(self, session_id, max_bytes_u32).await {
            Ok(Ok(snap)) => decode_attach_snapshot(snap),
            Ok(Err(err)) => typed_server_error("attach-snapshot-state", err),
            Err(err) => typed_dispatch_error("attach-snapshot-state", err),
        }
    }

    async fn attach_pane_snapshot(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes_per_pane: usize,
    ) -> ClientResult<AttachPaneSnapshotState> {
        let max_bytes_u32 = u32::try_from(max_bytes_per_pane).unwrap_or(u32::MAX);
        match AttachState::client::attach_pane_snapshot_state(
            self,
            session_id,
            pane_ids,
            max_bytes_u32,
        )
        .await
        {
            Ok(Ok(snap)) => Ok(AttachPaneSnapshotState {
                chunks: snap
                    .chunks
                    .into_iter()
                    .map(pane_chunk_from_record)
                    .collect(),
                pane_mouse_protocols: snap
                    .pane_mouse_protocols
                    .iter()
                    .map(pane_mouse_from_record)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                pane_input_modes: snap
                    .pane_input_modes
                    .iter()
                    .map(pane_input_mode_from_record)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            }),
            Ok(Err(err)) => typed_server_error("attach-pane-snapshot-state", err),
            Err(err) => typed_dispatch_error("attach-pane-snapshot-state", err),
        }
    }
}

fn one_way_typed_service_request<E>(payload: Vec<u8>) -> bmux_ipc::Request
where
    E: bmux_plugin_sdk::TypedServiceEndpoint,
{
    bmux_ipc::Request::InvokeService {
        capability: E::CAPABILITY.as_str().to_string(),
        kind: E::KIND,
        interface_id: E::INTERFACE_ID.as_str().to_string(),
        operation: E::OPERATION.as_str().to_string(),
        payload,
    }
}

#[allow(dead_code)]
pub trait StreamingAttachInputExt {
    fn send_one_way_attach_input(
        &mut self,
        session_id: Uuid,
        data: Vec<u8>,
    ) -> impl Future<Output = ClientResult<()>> + Send;

    fn send_one_way_pane_direct_input(
        &mut self,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> impl Future<Output = ClientResult<()>> + Send;
}

#[allow(dead_code)]
impl StreamingAttachInputExt for bmux_client::StreamingBmuxClient {
    async fn send_one_way_attach_input(
        &mut self,
        session_id: Uuid,
        data: Vec<u8>,
    ) -> ClientResult<()> {
        let typed_payload =
            bmux_plugin_sdk::encode_service_message(&AttachCommands::client::AttachInputRequest {
                session_id,
                data,
            })
            .map_err(|error| ClientError::ServerError {
                code: bmux_ipc::ErrorCode::Internal,
                message: format!("encoding attach-input payload: {error}"),
            })?;
        self.send_one_way(one_way_typed_service_request::<
            AttachCommands::client::AttachInputEndpoint,
        >(typed_payload))
            .await
    }

    async fn send_one_way_pane_direct_input(
        &mut self,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> ClientResult<()> {
        let typed_payload = bmux_plugin_sdk::encode_service_message(
            &PaneCommands::client::PaneDirectInputRequest {
                session_id,
                pane_id,
                data,
            },
        )
        .map_err(|error| ClientError::ServerError {
            code: bmux_ipc::ErrorCode::Internal,
            message: format!("encoding pane-direct-input payload: {error}"),
        })?;
        self.send_one_way(one_way_typed_service_request::<
            PaneCommands::client::PaneDirectInputEndpoint,
        >(typed_payload))
            .await
    }
}

impl BmuxPaneRuntimeClientExt for bmux_client::StreamingBmuxClient {
    async fn attach_grant(&mut self, selector: SessionSelector) -> ClientResult<AttachGrant> {
        match AttachCommands::client::attach_session(
            self,
            pane_runtime_session_selector(selector),
            true,
        )
        .await
        {
            Ok(Ok(grant)) => Ok(AttachGrant {
                attach_token: grant.token,
                session_id: grant.session_id,
                context_id: grant.context_id,
                expires_at_epoch_ms: grant.expires_epoch_ms,
            }),
            Ok(Err(err)) => typed_server_error("attach-session", err),
            Err(err) => typed_dispatch_error("attach-session", err),
        }
    }

    async fn attach_context_grant(
        &mut self,
        selector: ContextSelector,
    ) -> ClientResult<AttachGrant> {
        match AttachCommands::client::attach_context(
            self,
            pane_runtime_context_selector(selector),
            true,
        )
        .await
        {
            Ok(Ok(grant)) => Ok(AttachGrant {
                attach_token: grant.token,
                session_id: grant.session_id,
                context_id: grant.context_id,
                expires_at_epoch_ms: grant.expires_epoch_ms,
            }),
            Ok(Err(err)) => typed_server_error("attach-context", err),
            Err(err) => typed_dispatch_error("attach-context", err),
        }
    }

    async fn retarget_attach_context(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientResult<AttachOpenInfo> {
        self.retarget_attach_context_with_insets(context_id, cols, rows, 0, 0, 0, 0)
            .await
    }

    async fn retarget_attach_context_with_insets(
        &mut self,
        context_id: Uuid,
        cols: u16,
        rows: u16,
        top_inset: u16,
        right_inset: u16,
        bottom_inset: u16,
        left_inset: u16,
    ) -> ClientResult<AttachOpenInfo> {
        match AttachCommands::client::attach_retarget_context(
            self,
            context_id,
            true,
            cols,
            rows,
            top_inset,
            right_inset,
            bottom_inset,
            left_inset,
            cell_pixel_width(),
            cell_pixel_height(),
        )
        .await
        {
            Ok(Ok(ready)) => Ok(AttachOpenInfo {
                context_id: ready.context_id,
                session_id: ready.session_id,
                can_write: ready.can_write,
            }),
            Ok(Err(err)) => typed_server_error("attach-retarget-context", err),
            Err(err) => typed_dispatch_error("attach-retarget-context", err),
        }
    }

    async fn open_attach_stream_info(
        &mut self,
        grant: &AttachGrant,
    ) -> ClientResult<AttachOpenInfo> {
        match AttachCommands::client::attach_open(self, grant.session_id, grant.attach_token).await
        {
            Ok(Ok(ready)) => Ok(AttachOpenInfo {
                context_id: ready.context_id,
                session_id: ready.session_id,
                can_write: ready.can_write,
            }),
            Ok(Err(err)) => typed_server_error("attach-open", err),
            Err(err) => typed_dispatch_error("attach-open", err),
        }
    }

    async fn detach(&mut self) -> ClientResult<()> {
        match AttachCommands::client::detach(self).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => typed_server_error("detach", err),
            Err(err) => typed_dispatch_error("detach", err),
        }
    }

    async fn set_attach_policy(&mut self, allow_detach: bool) -> ClientResult<()> {
        match AttachCommands::client::set_client_attach_policy(self, allow_detach).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => typed_server_error("set-client-attach-policy", err),
            Err(err) => typed_dispatch_error("set-client-attach-policy", err),
        }
    }

    async fn attach_input(&mut self, session_id: Uuid, data: Vec<u8>) -> ClientResult<usize> {
        match AttachCommands::client::attach_input(self, session_id, data).await {
            Ok(Ok(accepted)) => Ok(accepted.bytes as usize),
            Ok(Err(err)) => typed_server_error("attach-input", err),
            Err(err) => typed_dispatch_error("attach-input", err),
        }
    }

    async fn pane_direct_input(
        &mut self,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> ClientResult<usize> {
        let bytes_len = data.len();
        match PaneCommands::client::pane_direct_input(self, session_id, pane_id, data).await {
            Ok(Ok(_ack)) => Ok(bytes_len),
            Ok(Err(err)) => typed_server_error("pane-direct-input", err),
            Err(err) => typed_dispatch_error("pane-direct-input", err),
        }
    }

    async fn attach_set_viewport(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientResult<(u16, u16)> {
        self.attach_set_viewport_with_insets(session_id, cols, rows, 0, 0, 0, 0)
            .await
    }

    async fn attach_set_viewport_with_insets(
        &mut self,
        session_id: Uuid,
        cols: u16,
        rows: u16,
        top_inset: u16,
        right_inset: u16,
        bottom_inset: u16,
        left_inset: u16,
    ) -> ClientResult<(u16, u16)> {
        match AttachCommands::client::attach_set_viewport(
            self,
            session_id,
            cols,
            rows,
            top_inset,
            right_inset,
            bottom_inset,
            left_inset,
            cell_pixel_width(),
            cell_pixel_height(),
        )
        .await
        {
            Ok(Ok(set)) => Ok((set.cols, set.rows)),
            Ok(Err(err)) => typed_server_error("attach-set-viewport", err),
            Err(err) => typed_dispatch_error("attach-set-viewport", err),
        }
    }

    async fn attach_layout(&mut self, session_id: Uuid) -> ClientResult<AttachLayoutState> {
        match AttachState::client::attach_layout_state(self, session_id).await {
            Ok(Ok(layout)) => decode_attach_layout(&layout),
            Ok(Err(err)) => typed_server_error("attach-layout-state", err),
            Err(err) => typed_dispatch_error("attach-layout-state", err),
        }
    }

    async fn attach_pane_output_batch(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> ClientResult<PaneOutputBatchResult> {
        let max_bytes_u32 = u32::try_from(max_bytes).unwrap_or(u32::MAX);
        match AttachState::client::attach_pane_output_batch(
            self,
            session_id,
            pane_ids,
            max_bytes_u32,
        )
        .await
        {
            Ok(Ok(batch)) => Ok(PaneOutputBatchResult {
                chunks: batch
                    .chunks
                    .into_iter()
                    .map(pane_chunk_from_record)
                    .collect(),
                output_still_pending: batch.output_still_pending,
            }),
            Ok(Err(err)) => typed_server_error("attach-pane-output-batch", err),
            Err(err) => typed_dispatch_error("attach-pane-output-batch", err),
        }
    }

    async fn attach_pane_images(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        since_sequences: Vec<u64>,
    ) -> ClientResult<Vec<AttachPaneImageDelta>> {
        match AttachState::client::attach_pane_images(self, session_id, pane_ids, since_sequences)
            .await
        {
            Ok(Ok(images)) => serde_json::from_slice::<Vec<AttachPaneImageDelta>>(&images.encoded)
                .map_err(|e| ClientError::ServerError {
                    code: ErrorCode::Internal,
                    message: format!("decode pane-images deltas: {e}"),
                }),
            Ok(Err(err)) => typed_server_error("attach-pane-images", err),
            Err(err) => typed_dispatch_error("attach-pane-images", err),
        }
    }

    async fn attach_snapshot(
        &mut self,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> ClientResult<AttachSnapshotState> {
        let max_bytes_u32 = u32::try_from(max_bytes_per_pane).unwrap_or(u32::MAX);
        match AttachState::client::attach_snapshot_state(self, session_id, max_bytes_u32).await {
            Ok(Ok(snap)) => decode_attach_snapshot(snap),
            Ok(Err(err)) => typed_server_error("attach-snapshot-state", err),
            Err(err) => typed_dispatch_error("attach-snapshot-state", err),
        }
    }

    async fn attach_pane_snapshot(
        &mut self,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes_per_pane: usize,
    ) -> ClientResult<AttachPaneSnapshotState> {
        let max_bytes_u32 = u32::try_from(max_bytes_per_pane).unwrap_or(u32::MAX);
        match AttachState::client::attach_pane_snapshot_state(
            self,
            session_id,
            pane_ids,
            max_bytes_u32,
        )
        .await
        {
            Ok(Ok(snap)) => Ok(AttachPaneSnapshotState {
                chunks: snap
                    .chunks
                    .into_iter()
                    .map(pane_chunk_from_record)
                    .collect(),
                pane_mouse_protocols: snap
                    .pane_mouse_protocols
                    .iter()
                    .map(pane_mouse_from_record)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                pane_input_modes: snap
                    .pane_input_modes
                    .iter()
                    .map(pane_input_mode_from_record)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            }),
            Ok(Err(err)) => typed_server_error("attach-pane-snapshot-state", err),
            Err(err) => typed_dispatch_error("attach-pane-snapshot-state", err),
        }
    }
}

fn decode_attach_layout(layout: &AttachState::AttachLayout) -> ClientResult<AttachLayoutState> {
    let payload: LayoutPayload =
        serde_json::from_slice(&layout.encoded).map_err(|e| ClientError::ServerError {
            code: ErrorCode::Internal,
            message: format!("decode attach-layout payload: {e}"),
        })?;
    Ok(AttachLayoutState {
        context_id: layout.context_id,
        session_id: layout.session_id,
        focused_pane_id: layout.focused_pane_id,
        panes: payload.panes,
        layout_root: payload.layout_root,
        scene: payload.scene,
        zoomed: payload.zoomed,
    })
}

fn decode_attach_snapshot(snap: AttachState::AttachSnapshot) -> ClientResult<AttachSnapshotState> {
    let layout: SnapshotLayoutPayload =
        serde_json::from_slice(&snap.layout_encoded).map_err(|e| ClientError::ServerError {
            code: ErrorCode::Internal,
            message: format!("decode attach-snapshot layout payload: {e}"),
        })?;
    Ok(AttachSnapshotState {
        context_id: snap.context_id,
        session_id: snap.session_id,
        focused_pane_id: snap.focused_pane_id,
        panes: layout.panes,
        layout_root: layout.layout_root,
        scene: layout.scene,
        chunks: snap
            .chunks
            .into_iter()
            .map(pane_chunk_from_record)
            .collect(),
        pane_mouse_protocols: snap
            .pane_mouse_protocols
            .iter()
            .map(pane_mouse_from_record)
            .collect::<std::result::Result<Vec<_>, _>>()?,
        pane_input_modes: snap
            .pane_input_modes
            .iter()
            .map(pane_input_mode_from_record)
            .collect::<std::result::Result<Vec<_>, _>>()?,
        zoomed: snap.zoomed,
    })
}

fn pane_chunk_from_record(chunk: AttachState::PaneChunk) -> AttachPaneChunk {
    AttachPaneChunk {
        pane_id: chunk.pane_id,
        data: chunk.data,
        stream_start: chunk.stream_start,
        stream_end: chunk.stream_end,
        stream_gap: chunk.stream_gap,
        sync_update_active: chunk.sync_update_active,
    }
}

fn pane_mouse_from_record(
    mouse: &AttachState::PaneMouseProtocol,
) -> ClientResult<AttachPaneMouseProtocol> {
    let protocol =
        serde_json::from_slice(&mouse.encoded).map_err(|e| ClientError::ServerError {
            code: ErrorCode::Internal,
            message: format!("decode pane mouse-protocol record: {e}"),
        })?;
    Ok(AttachPaneMouseProtocol {
        pane_id: mouse.pane_id,
        protocol,
    })
}

fn pane_input_mode_from_record(
    mode: &AttachState::PaneInputMode,
) -> ClientResult<AttachPaneInputMode> {
    let decoded = serde_json::from_slice(&mode.encoded).map_err(|e| ClientError::ServerError {
        code: ErrorCode::Internal,
        message: format!("decode pane input-mode record: {e}"),
    })?;
    Ok(AttachPaneInputMode {
        pane_id: mode.pane_id,
        mode: decoded,
    })
}

fn pane_runtime_session_selector(selector: SessionSelector) -> AttachCommands::SessionSelector {
    match selector {
        SessionSelector::ById(id) => AttachCommands::SessionSelector {
            id: Some(id),
            name: None,
        },
        SessionSelector::ByName(name) => AttachCommands::SessionSelector {
            id: None,
            name: Some(name),
        },
    }
}

fn pane_runtime_context_selector(selector: ContextSelector) -> AttachCommands::ContextSelector {
    match selector {
        ContextSelector::ById(id) => AttachCommands::ContextSelector {
            id: Some(id),
            name: None,
        },
        ContextSelector::ByName(name) => AttachCommands::ContextSelector {
            id: None,
            name: Some(name),
        },
    }
}

fn typed_server_error<T>(operation: &str, err: impl std::fmt::Debug) -> ClientResult<T> {
    Err(ClientError::ServerError {
        code: ErrorCode::Internal,
        message: format!("{operation} failed: {err:?}"),
    })
}

fn typed_dispatch_error<T>(operation: &str, err: impl std::fmt::Display) -> ClientResult<T> {
    Err(ClientError::ServerError {
        code: ErrorCode::Internal,
        message: format!("{operation} typed dispatch failed: {err}"),
    })
}

#[cfg(unix)]
fn cell_pixel_width() -> u16 {
    let (w, _) = cell_pixel_size_from_ioctl();
    w
}

#[cfg(unix)]
fn cell_pixel_height() -> u16 {
    let (_, h) = cell_pixel_size_from_ioctl();
    h
}

#[cfg(unix)]
fn cell_pixel_size_from_ioctl() -> (u16, u16) {
    use std::os::unix::io::AsRawFd;

    #[repr(C)]
    #[allow(clippy::struct_field_names)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: u64 = 0x4008_7468;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const TIOCGWINSZ: u64 = 0x5413;

    let fd = std::io::stdout().as_raw_fd();
    let mut ws = std::mem::MaybeUninit::<Winsize>::uninit();
    let ret = unsafe {
        unsafe extern "C" {
            fn ioctl(fd: i32, request: u64, ...) -> i32;
        }
        ioctl(fd, TIOCGWINSZ, ws.as_mut_ptr())
    };
    if ret != 0 {
        return (0, 0);
    }
    let ws = unsafe { ws.assume_init() };
    if ws.ws_col == 0 || ws.ws_row == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return (0, 0);
    }
    (ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row)
}

#[cfg(windows)]
fn cell_pixel_width() -> u16 {
    let (w, _) = cell_pixel_size_from_console();
    w
}

#[cfg(windows)]
fn cell_pixel_height() -> u16 {
    let (_, h) = cell_pixel_size_from_console();
    h
}

#[cfg(windows)]
fn cell_pixel_size_from_console() -> (u16, u16) {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct Coord {
        X: i16,
        Y: i16,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct ConsoleFontInfoEx {
        cbSize: u32,
        nFont: u32,
        dwFontSize: Coord,
        FontFamily: u32,
        FontWeight: u32,
        FaceName: [u16; 32],
    }

    unsafe {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            GetCurrentConsoleFontEx, GetStdHandle, STD_OUTPUT_HANDLE,
        };

        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return (0, 0);
        }
        let mut info = ConsoleFontInfoEx {
            cbSize: std::mem::size_of::<ConsoleFontInfoEx>() as u32,
            nFont: 0,
            dwFontSize: Coord { X: 0, Y: 0 },
            FontFamily: 0,
            FontWeight: 0,
            FaceName: [0; 32],
        };
        if GetCurrentConsoleFontEx(handle, 0, std::ptr::addr_of_mut!(info).cast()) == 0 {
            return (0, 0);
        }
        (
            u16::try_from(info.dwFontSize.X).unwrap_or(0),
            u16::try_from(info.dwFontSize.Y).unwrap_or(0),
        )
    }
}

#[cfg(not(any(unix, windows)))]
const fn cell_pixel_width() -> u16 {
    0
}

#[cfg(not(any(unix, windows)))]
const fn cell_pixel_height() -> u16 {
    0
}
