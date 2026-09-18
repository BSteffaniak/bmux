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
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
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
    styles: &mut std::sync::Arc<Vec<bmux_terminal_grid::Style>>,
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
            &mut std::sync::Arc::new(Vec::new()),
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
        let mut styles = std::sync::Arc::new(Vec::new());
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

    #[tokio::test]
    async fn small_capture_assembles_each_screen_row_once() {
        struct CountingClient(usize);
        impl bmux_plugin_sdk::TypedDispatchClient for CountingClient {
            async fn invoke_service_raw(
                &mut self,
                capability: &str,
                kind: bmux_ipc::InvokeServiceKind,
                interface: &str,
                operation: &str,
                payload: Vec<u8>,
            ) -> bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>> {
                self.0 += 1;
                HistoryClient
                    .invoke_service_raw(capability, kind, interface, operation, payload)
                    .await
            }
        }
        let mut client = CountingClient(0);
        let pin = bmux_attach_pipeline::ScrollbackPin {
            capture: Some(bmux_attach_pipeline::ScrollbackCapture {
                identity: uuid::Uuid::new_v4(),
                lines: 0,
                truncated: false,
                width: 2,
                height: 60,
            }),
            pin_id: 1,
            total_scrolled_rows: 0,
            max_scrollback_offset: 0,
            stream_end: 0,
            created_epoch_secs: 0,
        };
        let mut cache = super::CapturedHistoryCache::default();
        let identity = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), pin);
        let outcome = super::captured_history_window_cached(
            &mut client,
            identity,
            0,
            60,
            (2, None, 0),
            &mut cache,
        )
        .await
        .unwrap();
        let super::CapturedWindowOutcome::Window(window) = outcome else {
            panic!("missing window")
        };
        assert_eq!(window.rows.len(), 60);
        assert_eq!(client.0, 60);
        let remaining = cache.remaining;
        for _ in 0..10 {
            let next = super::captured_history_window_cached(
                &mut client,
                identity,
                0,
                60,
                (2, None, 0),
                &mut cache,
            )
            .await
            .unwrap();
            assert!(matches!(next, super::CapturedWindowOutcome::Window(_)));
        }
        assert_eq!(client.0, 60, "warm fetches must not read services");
        assert_eq!(
            cache.remaining, remaining,
            "projection must not consume retained budget"
        );
        cache.retain_images(2, &window.row_anchors, &[]);
        cache.prepare_resident_index(2, 30);
        let admitted = cache.remaining;
        for offset in 0..40 {
            let indexed = cache
                .indexed_window(
                    offset,
                    20,
                    (
                        2,
                        window.row_anchors.last().copied(),
                        isize::try_from(offset).unwrap(),
                    ),
                )
                .unwrap();
            let local = cache
                .resident_window(identity, offset, 20, (2, None, 0))
                .unwrap();
            assert_eq!(local.rows.len(), 20);
            assert_eq!(indexed.rows, local.rows);
            assert_eq!(indexed.row_anchors, local.row_anchors);
        }
        assert_eq!(cache.remaining, admitted);
        assert!(
            cache
                .resident_window(identity, 0, 20, (1, None, 0))
                .is_none(),
            "unknown resized image coverage must fetch"
        );
        assert_eq!(client.0, 60);
        assert_resident_under_pressure(&mut cache, identity, &window);
        assert_partial_index(&mut cache, &window);
        assert_capture_replacement(&mut cache, identity);
    }

    fn assert_capture_replacement(
        cache: &mut super::CapturedHistoryCache,
        identity: (uuid::Uuid, uuid::Uuid, bmux_attach_pipeline::ScrollbackPin),
    ) {
        assert!(cache.matches_capture(identity));
        cache.prepare(uuid::Uuid::new_v4(), identity.1, identity.2);
        assert!(!cache.matches_capture(identity));
        assert!(cache.decoded.lines.is_empty());
    }

    fn assert_resident_under_pressure(
        cache: &mut super::CapturedHistoryCache,
        identity: (uuid::Uuid, uuid::Uuid, bmux_attach_pipeline::ScrollbackPin),
        window: &bmux_attach_pipeline::PaneScrollbackWindow,
    ) {
        let remaining = cache.remaining;
        let revision = cache.revision.clone();
        cache.remaining = 0;
        let local = cache
            .resident_window(identity, 0, 20, (2, window.row_anchors.last().copied(), 0))
            .unwrap();
        assert_eq!(local.rows.len(), 20);
        assert_eq!(cache.remaining, 0);
        assert!(std::sync::Arc::ptr_eq(&revision, &cache.revision));
        let mut replaced = identity;
        replaced.2.pin_id += 1;
        assert!(
            cache
                .resident_window(replaced, 0, 20, (2, window.row_anchors.last().copied(), 0))
                .is_none()
        );
        cache.remaining = remaining;
    }

    fn assert_partial_index(
        cache: &mut super::CapturedHistoryCache,
        window: &bmux_attach_pipeline::PaneScrollbackWindow,
    ) {
        cache.indexed = None;
        std::sync::Arc::make_mut(&mut cache.decoded.lines).retain(|line, _| *line >= 20);
        cache.prepare_resident_index(2, 40);
        assert_eq!(cache.indexed.as_ref().unwrap().first_line, 20);
        cache.prepare_resident_index(1, 40);
        assert_eq!(cache.indexed.as_ref().unwrap().width, 1);
        assert_eq!(
            cache.indexed.as_ref().unwrap().content.row_count(),
            Some(80)
        );
        cache.prepare_resident_index(0, 40);
        assert_eq!(cache.indexed.as_ref().unwrap().width, 1);
        cache.prepare_resident_index(2, 40);
        let partial = cache
            .indexed_window(0, 20, (2, window.row_anchors.last().copied(), 0))
            .unwrap();
        assert_eq!(partial.row_anchors[0].line_index, 40);
        assert!(
            cache
                .indexed_window(50, 20, (2, window.row_anchors.last().copied(), 50))
                .is_none()
        );
    }

    #[tokio::test]
    async fn captured_tail_boundary_retries_do_not_duplicate_decoded_lines() {
        let capture = super::AttachState::HistoryCaptureV1 {
            width: 2,
            height: 50,
            capture_id: uuid::Uuid::new_v4(),
            history_line_count: 0,
            history_truncated: false,
            pin: super::AttachState::PaneScrollbackPin {
                pane_id: uuid::Uuid::new_v4(),
                pin_id: 1,
                total_scrolled_rows: 0,
                max_scrollback_offset: 0,
                stream_end: 0,
            },
        };
        let mut cache = super::CapturedLineCache::default();
        let mut remaining = 2 * 1024 * 1024;
        let mut styles = std::sync::Arc::new(Vec::new());
        let session = uuid::Uuid::new_v4();
        // Incrementally resolving tail lines used to retain each preceding
        // prefix again, exceeding 256 entries with only 23 source lines.
        let mut requests = 50;
        for index in 0..50 {
            assert!(
                cache
                    .resolve(
                        &mut HistoryClient,
                        session,
                        &capture,
                        index,
                        (&mut remaining, &mut styles, &mut requests)
                    )
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(cache.lines.len(), index as usize + 1);
        }
        assert_eq!(requests, 0, "exactly one request per source row");
        assert!(
            cache
                .resolve(
                    &mut HistoryClient,
                    session,
                    &capture,
                    50,
                    (&mut remaining, &mut styles, &mut requests)
                )
                .await
                .unwrap()
                .is_none()
        );
        let admitted = remaining;
        for _ in 0..100 {
            let mut requests = 0;
            assert!(
                cache
                    .resolve(
                        &mut HistoryClient,
                        session,
                        &capture,
                        50,
                        (&mut remaining, &mut styles, &mut requests)
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                cache
                    .resolve(
                        &mut HistoryClient,
                        session,
                        &capture,
                        49,
                        (&mut remaining, &mut styles, &mut requests)
                    )
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(cache.lines.len(), 50);
        assert_eq!(remaining, admitted);
    }

    #[tokio::test]
    async fn decoded_cache_retains_more_than_256_short_lines_within_byte_budget() {
        let capture = super::AttachState::HistoryCaptureV1 {
            width: 2,
            height: 50,
            capture_id: uuid::Uuid::new_v4(),
            history_line_count: 1024,
            history_truncated: false,
            pin: super::AttachState::PaneScrollbackPin {
                pane_id: uuid::Uuid::new_v4(),
                pin_id: 1,
                total_scrolled_rows: 1024,
                max_scrollback_offset: 1024,
                stream_end: 0,
            },
        };
        let mut cache = super::CapturedLineCache::default();
        let mut remaining = 16 * 1024 * 1024;
        let mut styles = std::sync::Arc::new(Vec::new());
        let session = uuid::Uuid::new_v4();
        for index in 0..1024 {
            let mut requests = 2;
            assert!(
                cache
                    .resolve(
                        &mut HistoryClient,
                        session,
                        &capture,
                        index,
                        (&mut remaining, &mut styles, &mut requests)
                    )
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(cache.lines.len(), 1024);
        let resident_identity = capture.capture_id;
        let mut replica = super::CapturedHistoryCache {
            decoded: cache.clone(),
            remaining: 0,
            ..super::CapturedHistoryCache::default()
        };
        replica.evict_distant(500);
        assert!(replica.decoded.lines.contains_key(&500));
        assert!(!replica.decoded.lines.contains_key(&0));
        assert!(replica.remaining > 4 * 1024 * 1024);
        assert_eq!(capture.capture_id, resident_identity);
        assert_eq!(
            cache.lines.len(),
            1024,
            "eviction must isolate worker snapshots"
        );
        let admitted = remaining;
        for index in (0..1024).rev() {
            let mut requests = 0;
            assert!(
                cache
                    .resolve(
                        &mut HistoryClient,
                        session,
                        &capture,
                        index,
                        (&mut remaining, &mut styles, &mut requests)
                    )
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(remaining, admitted);
        assert!(remaining > 0);
    }

    #[test]
    fn empty_image_coverage_is_local_but_never_crosses_capture_or_width() {
        let id = uuid::Uuid::new_v4();
        let anchors = (0..50)
            .map(|line_index| bmux_attach_pipeline::CapturedHistoryAnchor {
                capture_id: id,
                line_index,
                column: 0,
            })
            .collect::<Vec<_>>();
        let mut cache = super::CapturedHistoryCache::default();
        cache.retain_images(80, &anchors, &[]);
        for start in 0..30 {
            assert!(
                cache
                    .cached_images(80, &anchors[start..start + 20])
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(cache.cached_images(40, &anchors[..20]).is_none());
        let mut other = anchors.clone();
        other[0].capture_id = uuid::Uuid::new_v4();
        assert!(cache.cached_images(80, &other[..20]).is_none());
        assert!(cache.cached_images(80, &[]).is_none());
    }

    #[test]
    fn decoded_image_coverage_preserves_payload_and_bounds_eviction() {
        let mut cache = super::CapturedHistoryCache::default();
        let anchor = bmux_attach_pipeline::CapturedHistoryAnchor {
            capture_id: uuid::Uuid::new_v4(),
            line_index: 0,
            column: 0,
        };
        let image = bmux_attach_image_protocol::AttachPaneImage {
            id: 7,
            protocol: bmux_attach_image_protocol::AttachImageProtocol::Sixel,
            compression: bmux_attach_image_protocol::CompressionId::None,
            raw_data: vec![1, 2, 3],
            position_row: 0,
            position_col: 1,
            cell_rows: 1,
            cell_cols: 2,
            pixel_width: 16,
            pixel_height: 16,
        };
        cache.retain_images(80, &[anchor], std::slice::from_ref(&image));
        assert_eq!(cache.cached_images(80, &[anchor]).unwrap(), vec![image]);
        assert!(cache.cached_images(40, &[anchor]).is_none());
        for line_index in 1..=256 {
            cache.retain_images(
                80,
                &[bmux_attach_pipeline::CapturedHistoryAnchor {
                    line_index,
                    ..anchor
                }],
                &[],
            );
        }
        assert!(cache.cached_images(80, &[anchor]).is_none());
        assert_eq!(cache.image_coverage.len(), 256);
        assert_eq!(
            cache.image_bytes,
            cache
                .image_coverage
                .iter()
                .map(|entry| entry.bytes)
                .sum::<usize>()
        );
        assert!(cache.image_bytes <= 8 * 1024 * 1024);
    }

    #[test]
    #[ignore = "manual resident navigation benchmark; run with --ignored --nocapture"]
    fn resident_history_navigation_benchmark() {
        use bmux_terminal_grid::{
            Cell, HistoryLineAssembly, HistorySlice, HistorySliceEnd, StyleId,
        };
        let mut cache = super::CapturedHistoryCache::default();
        let capture = uuid::Uuid::new_v4();
        let pin = bmux_attach_pipeline::ScrollbackPin {
            capture: Some(bmux_attach_pipeline::ScrollbackCapture {
                identity: capture,
                lines: 10_000,
                truncated: false,
                width: 80,
                height: 24,
            }),
            pin_id: 1,
            total_scrolled_rows: 10_000,
            max_scrollback_offset: 10_000,
            stream_end: 0,
            created_epoch_secs: 0,
        };
        let identity = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), pin);
        cache.prepare(identity.0, identity.1, identity.2);
        for index in 0..10_000_u32 {
            let text = format!("row-{index:05} abcdefghijklmnopqrstuvwxyz");
            let cells = text
                .chars()
                .map(|ch| Cell::new(ch.to_string(), StyleId::DEFAULT, 1))
                .collect::<Vec<_>>();
            let mut line =
                HistoryLineAssembly::new(capture.as_u128(), index as usize, 65536, false);
            line.append(
                capture.as_u128(),
                index as usize,
                0,
                &HistorySlice {
                    cells: &cells,
                    next_cell_offset: cells.len(),
                    end: HistorySliceEnd::HardBreak,
                },
            )
            .unwrap();
            std::sync::Arc::make_mut(&mut cache.history).insert(index, std::sync::Arc::new(line));
        }
        let anchors = (4900..5100)
            .map(|line_index| bmux_attach_pipeline::CapturedHistoryAnchor {
                capture_id: capture,
                line_index,
                column: 0,
            })
            .collect::<Vec<_>>();
        cache.retain_images(80, &anchors, &[]);
        let started = std::time::Instant::now();
        cache.prepare_resident_index(80, 5000);
        let build = started.elapsed();
        let mut samples = Vec::with_capacity(2000);
        for step in 0..2000 {
            let distance = step % 100;
            let started = std::time::Instant::now();
            let window = cache
                .resident_window(
                    identity,
                    distance,
                    24,
                    (
                        80,
                        anchors.last().copied(),
                        isize::try_from(distance).unwrap(),
                    ),
                )
                .unwrap();
            std::hint::black_box(&window);
            samples.push(started.elapsed().as_nanos());
            assert_eq!(window.rows.len(), 24);
            assert_eq!(
                window.row_anchors.last().unwrap().line_index,
                5099 - u32::try_from(distance).unwrap()
            );
        }
        samples.sort_unstable();
        eprintln!(
            "resident_history lines=10000 width=80 rows=24 samples=2000 index_build_us={} p50_ns={} p95_ns={} p99_ns={} max_ns={}",
            build.as_micros(),
            samples[1000],
            samples[1900],
            samples[1980],
            samples[1999]
        );
    }

    #[test]
    fn adjacent_empty_image_ranges_cover_navigation_without_rpc() {
        let capture = uuid::Uuid::new_v4();
        let anchors = (0..60)
            .map(|line_index| bmux_attach_pipeline::CapturedHistoryAnchor {
                capture_id: capture,
                line_index,
                column: 0,
            })
            .collect::<Vec<_>>();
        let mut cache = super::CapturedHistoryCache::default();
        cache.retain_images(80, &anchors[..20], &[]);
        cache.retain_images(80, &anchors[20..40], &[]);
        cache.retain_images(80, &anchors[40..], &[]);
        for start in 0..=40 {
            assert_eq!(
                cache.cached_images(80, &anchors[start..start + 20]),
                Some(Vec::new())
            );
        }
        assert!(cache.cached_images(40, &anchors[10..30]).is_none());
        cache.image_coverage.remove(1);
        assert!(cache.cached_images(80, &anchors[10..30]).is_none());
        let other = [bmux_attach_pipeline::CapturedHistoryAnchor {
            capture_id: uuid::Uuid::new_v4(),
            ..anchors[0]
        }];
        assert!(cache.cached_images(80, &other).is_none());
    }

    #[tokio::test]
    async fn worker_snapshot_shares_line_index_until_mutation() {
        let capture = super::AttachState::HistoryCaptureV1 {
            width: 2,
            height: 2,
            capture_id: uuid::Uuid::new_v4(),
            history_line_count: 10,
            history_truncated: false,
            pin: super::AttachState::PaneScrollbackPin {
                pane_id: uuid::Uuid::new_v4(),
                pin_id: 1,
                total_scrolled_rows: 10,
                max_scrollback_offset: 10,
                stream_end: 0,
            },
        };
        let mut resident = super::CapturedLineCache::default();
        let mut bytes = 65536;
        let mut styles = std::sync::Arc::new(Vec::new());
        let mut requests = 10;
        let session = uuid::Uuid::new_v4();
        resident
            .resolve(
                &mut HistoryClient,
                session,
                &capture,
                0,
                (&mut bytes, &mut styles, &mut requests),
            )
            .await
            .unwrap();
        let mut worker = resident.clone();
        assert!(std::sync::Arc::ptr_eq(&resident.lines, &worker.lines));
        worker
            .resolve(
                &mut HistoryClient,
                session,
                &capture,
                0,
                (&mut bytes, &mut styles, &mut requests),
            )
            .await
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&resident.lines, &worker.lines));
        worker
            .resolve(
                &mut HistoryClient,
                session,
                &capture,
                1,
                (&mut bytes, &mut styles, &mut requests),
            )
            .await
            .unwrap();
        assert_eq!(resident.lines.len(), 1);
        assert_eq!(worker.lines.len(), 2);
        assert!(!std::sync::Arc::ptr_eq(&resident.lines, &worker.lines));
        assert!(std::sync::Arc::ptr_eq(
            &resident.lines[&0],
            &worker.lines[&0]
        ));
    }

    #[test]
    fn worker_publication_rejects_same_source_revision_changes() {
        let mut resident = super::CapturedHistoryCache::default();
        let original = resident.clone();
        let mut first = original.clone();
        first.remaining = 1234;
        assert!(resident.publish_if_current(&original, first));
        let mut obsolete = original.clone();
        obsolete.remaining = 9876;
        assert!(!resident.publish_if_current(&original, obsolete));
        assert_eq!(resident.remaining, 1234);
        let before_reset = resident.clone();
        resident = super::CapturedHistoryCache::default();
        assert!(!resident.publish_if_current(&before_reset, before_reset.clone()));
    }

    #[test]
    fn resident_probe_miss_preserves_cache_and_admission_revision() {
        let mut cache = super::CapturedHistoryCache::default();
        let pin = bmux_attach_pipeline::ScrollbackPin {
            capture: Some(bmux_attach_pipeline::ScrollbackCapture {
                identity: uuid::Uuid::new_v4(),
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
        };
        let identity = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), pin);
        cache.prepare(identity.0, identity.1, pin);
        let before = cache.clone();
        for _ in 0..100 {
            assert!(
                cache
                    .resident_window(identity, 0, 24, (80, None, 0))
                    .is_none()
            );
        }
        assert!(std::sync::Arc::ptr_eq(&before.revision, &cache.revision));
        assert!(std::sync::Arc::ptr_eq(
            &before.decoded.lines,
            &cache.decoded.lines
        ));
        assert_eq!(cache.remaining, before.remaining);
        assert_eq!(cache.decoded.end, before.decoded.end);
        assert_eq!(cache.decoded.tail_resume, before.decoded.tail_resume);
        assert_eq!(cache.tail_loaded, before.tail_loaded);
        assert_eq!(cache.styles, before.styles);
    }

    #[test]
    fn decoding_existing_styles_does_not_detach_shared_palette() {
        let style = bmux_terminal_grid::Style::default();
        let mut styles = std::sync::Arc::new(vec![style]);
        let published = styles.clone();
        let encoded = serde_json::to_vec(&vec![("a", 1_u8, style)]).unwrap();
        super::decode_history_cells(&encoded, &mut styles, &mut 65536).unwrap();
        assert!(std::sync::Arc::ptr_eq(&styles, &published));
        let bold = bmux_terminal_grid::Style {
            bold: true,
            ..style
        };
        let encoded = serde_json::to_vec(&vec![("b", 1_u8, bold)]).unwrap();
        super::decode_history_cells(&encoded, &mut styles, &mut 65536).unwrap();
        assert!(!std::sync::Arc::ptr_eq(&styles, &published));
        assert_eq!(published.as_slice(), &[style]);
        assert_eq!(styles.as_slice(), &[style, bold]);
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
        let mut styles = std::sync::Arc::new(Vec::new());
        let mut budget = 1024;
        let cells = super::decode_history_cells(&encoded, &mut styles, &mut budget).unwrap();
        assert_eq!(cells[0].text(), "界");
        assert_eq!(styles.as_slice(), &[style]);
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

/// No transport capability: a cache miss suspends until the local probe is
/// dropped. The asynchronous worker remains responsible for admission and I/O.
struct ResidentOnly;
impl bmux_plugin_sdk::TypedDispatchClient for ResidentOnly {
    async fn invoke_service_raw(
        &mut self,
        _capability: &str,
        _kind: bmux_ipc::InvokeServiceKind,
        _interface: &str,
        _operation: &str,
        _payload: Vec<u8>,
    ) -> bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>> {
        std::future::pending().await
    }
}

struct CapturedImageCoverage {
    width: usize,
    anchors: Vec<bmux_attach_pipeline::CapturedHistoryAnchor>,
    images: Vec<bmux_attach_image_protocol::AttachPaneImage>,
    bytes: usize,
}

struct ResidentIndex {
    width: usize,
    first_line: u32,
    end_line: u32,
    content: bmux_terminal_grid::ContentProjection,
    bytes: usize,
}

#[derive(Default, Clone)]
pub struct CapturedHistoryCache {
    revision: std::sync::Arc<()>,
    indexed: Option<std::sync::Arc<ResidentIndex>>,
    image_coverage: std::collections::VecDeque<std::sync::Arc<CapturedImageCoverage>>,
    image_bytes: usize,
    identity: Option<(Uuid, Uuid, bmux_attach_pipeline::ScrollbackPin)>,
    decoded: CapturedLineCache,
    styles: std::sync::Arc<Vec<bmux_terminal_grid::Style>>,
    remaining: usize,
    tail_loaded: bool,
    history: std::sync::Arc<
        std::collections::BTreeMap<u32, std::sync::Arc<bmux_terminal_grid::HistoryLineAssembly>>,
    >,
}

impl CapturedHistoryCache {
    /// Conservative admission charge: decoded allocations are charged by the
    /// decoder; derived index allocations are measured after construction.
    pub fn retained_charge(&self) -> usize {
        (if self.identity.is_some() {
            (16_usize * 1024 * 1024).saturating_sub(self.remaining)
        } else {
            0
        })
        .saturating_add(self.image_bytes)
        .saturating_add(self.indexed.as_ref().map_or(0, |index| index.bytes))
        .saturating_add(std::mem::size_of::<Self>())
    }

    /// Publish only against the exact snapshot used to start background work.
    /// An allocation identity avoids counter wrap and same-source reset ABA.
    pub fn publish_if_current(&mut self, original: &Self, mut updated: Self) -> bool {
        if self.identity != original.identity
            || !std::sync::Arc::ptr_eq(&self.revision, &original.revision)
        {
            return false;
        }
        updated.revision = std::sync::Arc::new(());
        *self = updated;
        true
    }

    fn changed(&mut self) {
        self.revision = std::sync::Arc::new(());
    }

    #[cfg(test)]
    pub fn matches_capture(
        &self,
        identity: (Uuid, Uuid, bmux_attach_pipeline::ScrollbackPin),
    ) -> bool {
        self.identity == Some(identity)
    }

    /// Index a bounded contiguous resident range around the requested anchor.
    /// Gaps remain explicit; partial residency never renumbers source identities.
    pub fn prepare_resident_index(&mut self, width: usize, line: u32) {
        if self.indexed.as_ref().is_some_and(|index| {
            index.width == width && (index.first_line..index.end_line).contains(&line)
        }) {
            return;
        }
        let Some((_, _, pin)) = self.identity else {
            return;
        };
        let Some(meta) = pin.capture else {
            return;
        };
        let resident = |index: u32| {
            self.decoded
                .lines
                .get(&index)
                .or_else(|| self.history.get(&index))
        };
        if resident(line).is_none() {
            return;
        }
        let mut first = line;
        let mut end = line.saturating_add(1);
        // Metadata scans are bounded independently of cell and byte budgets.
        while line - first < 2048 && first > 0 && resident(first - 1).is_some() {
            first -= 1;
        }
        while end - line < 2048 && resident(end).is_some() {
            end += 1;
        }
        if let Some(index) = self.indexed.as_mut().and_then(std::sync::Arc::get_mut)
            && index.first_line == first
            && index.end_line == end
        {
            // Reflow the retained canonical source; changing presentation width
            // does not require cloning source cells again. prepare is atomic on
            // failure, so the old width remains usable until replacement succeeds.
            if index
                .content
                .prepare(
                    width,
                    bmux_terminal_grid::ContentBudget {
                        cells: 1_000_000,
                        bytes: 32 * 1024 * 1024,
                    },
                )
                .is_ok()
            {
                index.width = width;
                index.bytes = index.content.retained_bytes();
            }
            return;
        }
        let mut lines = Vec::new();
        for index in first..end {
            let Some(line) = self
                .decoded
                .lines
                .get(&index)
                .or_else(|| self.history.get(&index))
            else {
                return;
            };
            let Some((cells, end)) = line.completed() else {
                return;
            };
            lines.push((cells, end == bmux_terminal_grid::HistorySliceEnd::Open));
        }
        let budget = bmux_terminal_grid::ContentBudget {
            cells: 1_000_000,
            bytes: 32 * 1024 * 1024,
        };
        let Ok(mut content) = bmux_terminal_grid::ContentProjection::from_lines(
            meta.identity.as_u128(),
            0,
            lines,
            first != 0,
            meta.truncated,
            budget,
        ) else {
            return;
        };
        if content.prepare(width, budget).is_ok() {
            self.indexed = Some(std::sync::Arc::new(ResidentIndex {
                width,
                first_line: first,
                end_line: end,
                bytes: content.retained_bytes(),
                content,
            }));
        }
    }

    fn indexed_window(
        &self,
        offset: usize,
        rows: usize,
        projection: (
            usize,
            Option<bmux_attach_pipeline::CapturedHistoryAnchor>,
            isize,
        ),
    ) -> Option<bmux_attach_pipeline::PaneScrollbackWindow> {
        let (_, _, pin) = self.identity?;
        let resident = self.indexed.as_ref()?;
        let width = resident.width;
        let index = &resident.content;
        if width != projection.0 || rows == 0 || rows > 256 {
            return None;
        }
        let anchor = projection.1?;
        let bottom = index
            .resolve(bmux_terminal_grid::ContentAnchor {
                capture: anchor.capture_id.as_u128(),
                line: anchor.line_index.checked_sub(resident.first_line)? as usize,
                column: anchor.column,
            })?
            .checked_add(1)?;
        let total = index.row_count()?;
        let requested_end = if projection.2 >= 0 {
            bottom.saturating_sub(projection.2.unsigned_abs())
        } else {
            bottom.saturating_add(projection.2.unsigned_abs())
        };
        if requested_end < rows || requested_end > total {
            return None;
        }
        let end = requested_end;
        let content = index
            .window(end.saturating_sub(rows)..end, 2 * 1024 * 1024)
            .ok()?;
        let anchors = content
            .anchors
            .iter()
            .map(|anchor| {
                Some(bmux_attach_pipeline::CapturedHistoryAnchor {
                    capture_id: Uuid::from_u128(anchor.capture),
                    line_index: u32::try_from(anchor.line)
                        .ok()?
                        .checked_add(resident.first_line)?,
                    column: anchor.column,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let images = self.cached_images(width, &anchors)?;
        Some(bmux_attach_pipeline::PaneScrollbackWindow {
            images,
            projection_width: width,
            row_anchors: anchors,
            palette: bmux_terminal_grid::StylePalette::from_shared_styles(self.styles.clone()),
            scrollback_offset: offset,
            max_scrollback_offset: pin.max_scrollback_offset,
            total_scrolled_rows: pin.total_scrolled_rows,
            rows: content.rows,
        })
    }

    /// Poll the shared resolver using resident data only. Missing content yields
    /// immediately: this caller cannot send transport requests or await I/O.
    pub fn resident_window(
        &mut self,
        identity: (Uuid, Uuid, bmux_attach_pipeline::ScrollbackPin),
        offset: usize,
        rows: usize,
        projection: (
            usize,
            Option<bmux_attach_pipeline::CapturedHistoryAnchor>,
            isize,
        ),
    ) -> Option<bmux_attach_pipeline::PaneScrollbackWindow> {
        use futures_util::FutureExt;
        if self.identity != Some(identity) {
            return None;
        }
        if let Some(window) = self.indexed_window(offset, rows, projection) {
            return Some(window);
        }
        if self.remaining < 4 * 1024 * 1024 {
            // Allocation pressure cannot invalidate an existing indexed view.
            // Only a miss needs the worker's admission/eviction path.
            return None;
        }
        // Probe against a copy-on-write transaction. A missing text/image
        // range must not leave partial assembly or uncharged metadata resident.
        let mut working = self.clone();
        let outcome = captured_history_window_cached(
            &mut ResidentOnly,
            identity,
            offset,
            rows,
            projection,
            &mut working,
        )
        .now_or_never();
        let Some(Ok(CapturedWindowOutcome::Window(mut window))) = outcome else {
            return None;
        };
        window.images = working.cached_images(projection.0, &window.row_anchors)?;
        if !std::sync::Arc::ptr_eq(&self.decoded.lines, &working.decoded.lines)
            || !std::sync::Arc::ptr_eq(&self.history, &working.history)
            || self.styles.len() != working.styles.len()
            || self.decoded.end != working.decoded.end
            || self.decoded.tail_resume != working.decoded.tail_resume
            || self.tail_loaded != working.tail_loaded
            || self.remaining != working.remaining
        {
            working.changed();
            *self = working;
        }
        Some(window)
    }

    /// A cached empty response proves absence over every subrange. Nonempty
    /// image responses are reusable only at the exact origin and geometry.
    pub fn cached_images(
        &self,
        width: usize,
        anchors: &[bmux_attach_pipeline::CapturedHistoryAnchor],
    ) -> Option<Vec<bmux_attach_image_protocol::AttachPaneImage>> {
        if anchors.is_empty() {
            return None;
        }
        self.image_coverage
            .iter()
            .rev()
            .find_map(|entry| {
                if entry.width != width {
                    return None;
                }
                let exact = entry.anchors == anchors;
                let empty = entry.images.is_empty();
                if !(exact
                    || (empty
                        && entry
                            .anchors
                            .windows(anchors.len())
                            .any(|range| range == anchors)))
                {
                    return None;
                }
                Some(entry.images.clone())
            })
            .or_else(|| {
                // Every requested physical source row must be covered at this width.
                // Do not infer empty coverage from a neighboring row or from another
                // capture; anchors carry their capture identity in equality checks.
                anchors
                    .iter()
                    .all(|anchor| {
                        self.image_coverage.iter().any(|entry| {
                            entry.width == width
                                && entry.images.is_empty()
                                && entry
                                    .anchors
                                    .binary_search_by_key(
                                        &(anchor.capture_id, anchor.line_index, anchor.column),
                                        |candidate| {
                                            (
                                                candidate.capture_id,
                                                candidate.line_index,
                                                candidate.column,
                                            )
                                        },
                                    )
                                    .is_ok()
                        })
                    })
                    .then(Vec::new)
            })
    }

    pub fn retain_images(
        &mut self,
        width: usize,
        anchors: &[bmux_attach_pipeline::CapturedHistoryAnchor],
        images: &[bmux_attach_image_protocol::AttachPaneImage],
    ) {
        const LIMIT: usize = 8 * 1024 * 1024;
        let bytes = images.iter().fold(
            std::mem::size_of_val(images)
                .saturating_add(std::mem::size_of_val(anchors))
                .saturating_add(std::mem::size_of::<CapturedImageCoverage>()),
            |bytes, image| bytes.saturating_add(image.raw_data.len()),
        );
        if bytes > LIMIT
            || anchors.is_empty()
            || anchors.windows(2).any(|pair| {
                (pair[0].capture_id, pair[0].line_index, pair[0].column)
                    >= (pair[1].capture_id, pair[1].line_index, pair[1].column)
            })
        {
            return;
        }
        while self.image_bytes.saturating_add(bytes) > LIMIT || self.image_coverage.len() >= 256 {
            if let Some(old) = self.image_coverage.pop_front() {
                self.image_bytes -= old.bytes;
            }
        }
        self.changed();
        self.image_bytes += bytes;
        self.image_coverage
            .push_back(std::sync::Arc::new(CapturedImageCoverage {
                width,
                anchors: anchors.to_vec(),
                images: images.to_vec(),
                bytes,
            }));
    }

    fn evict_distant(&mut self, center: u32) {
        let mut radius = 256_u32;
        loop {
            let range = center.saturating_sub(radius)..=center.saturating_add(radius);
            std::sync::Arc::make_mut(&mut self.decoded.lines)
                .retain(|line, _| range.contains(line));
            std::sync::Arc::make_mut(&mut self.history).retain(|line, _| range.contains(line));
            // Dropped prefix lines must remain reloadable; tail discovery cannot
            // claim complete residency after eviction.
            self.tail_loaded = false;
            self.decoded.tail_resume = None;
            self.decoded.end = None;
            let mut bytes = self
                .styles
                .capacity()
                .saturating_mul(std::mem::size_of::<bmux_terminal_grid::Style>());
            for line in self.decoded.lines.values().chain(self.history.values()) {
                bytes = bytes
                    .saturating_add(line.retained_bytes().saturating_mul(2))
                    .saturating_add(HISTORY_INDEX_ENTRY_BYTES);
            }
            self.remaining = (16_usize * 1024 * 1024).saturating_sub(bytes);
            if self.remaining >= 4 * 1024 * 1024 || radius == 0 {
                break;
            }
            radius /= 2;
        }
        self.changed();
    }

    fn prepare(&mut self, session: Uuid, pane: Uuid, pin: bmux_attach_pipeline::ScrollbackPin) {
        let identity = (session, pane, pin);
        // One capture per attachment worker, with a fixed retained allocation
        // allowance. Eviction is reconstructible and never changes the pin.
        if self.identity != Some(identity) {
            *self = Self {
                identity: Some(identity),
                remaining: 16 * 1024 * 1024,
                ..Self::default()
            };
        }
    }
}

#[cfg(test)]
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
    captured_history_window_cached(
        client,
        (session_id, pane_id, pin),
        offset,
        rows,
        projection,
        &mut CapturedHistoryCache::default(),
    )
    .await
}

#[allow(
    clippy::too_many_lines,
    reason = "bounded navigation and assembly share capture state"
)]
pub async fn captured_history_window_cached(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    identity: (Uuid, Uuid, bmux_attach_pipeline::ScrollbackPin),
    offset: usize,
    rows: usize,
    projection: (
        usize,
        Option<bmux_attach_pipeline::CapturedHistoryAnchor>,
        isize,
    ),
    cache: &mut CapturedHistoryCache,
) -> ClientResult<CapturedWindowOutcome> {
    let (session_id, pane_id, pin) = identity;
    cache.prepare(session_id, pane_id, pin);
    if cache.remaining < 4 * 1024 * 1024 {
        let center = projection.1.map_or_else(
            || {
                pin.capture.map_or(0, |capture| {
                    u32::try_from(capture.lines).unwrap_or(u32::MAX)
                })
            },
            |anchor| anchor.line_index,
        );
        cache.evict_distant(center);
    }
    let mut cached_client = CaptureReadCache {
        client,
        replies: Vec::new(),
        remaining: 256 * 1024,
    };
    let client = &mut cached_client;
    let decoded = &mut cache.decoded;
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
    let remaining = &mut cache.remaining;
    let mut requests_left = 256;
    let mut projection_budget: usize = 2 * 1024 * 1024;
    let styles = &mut cache.styles;
    if bottom_anchor.is_none() && offset < usize::from(meta.height) {
        // Assemble the captured screen once. Resolving subsequent viewport
        // lines reuses the assemblies and their palette instead of rescanning
        // and decoding every prefix for each logical line.
        let end = u32::try_from(meta.lines + u64::from(meta.height))
            .map_err(|error| history_decode_error(&error))?;
        if !cache.tail_loaded {
            decoded
                .resolve(
                    client,
                    session_id,
                    &capture,
                    end,
                    (remaining, styles, &mut requests_left),
                )
                .await?;
            cache.tail_loaded = true;
        }
        let mut skip = offset;
        for (index, line) in decoded.lines.iter().rev() {
            let count = line.projected_rows(usize::from(meta.width));
            if skip >= count {
                skip -= count;
                continue;
            }
            bottom_anchor = Some(bmux_attach_pipeline::CapturedHistoryAnchor {
                capture_id: meta.identity,
                line_index: *index,
                column: line
                    .column_for_row(usize::from(meta.width), count - skip - 1)
                    .ok_or_else(|| history_decode_error(&"invalid tail entry"))?,
            });
            break;
        }
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
                    (remaining, styles, &mut requests_left),
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
    projection_budget = projection_budget
        .checked_sub(rows.saturating_mul(std::mem::size_of::<
            bmux_attach_pipeline::CapturedHistoryAnchor,
        >()))
        .ok_or_else(|| history_decode_error(&"anchor budget exhausted"))?;
    selected
        .try_reserve_exact(rows)
        .map_err(|error| history_decode_error(&error))?;
    // Bound scanning independently of bytes (empty lines still cost requests).
    let mut reached_oldest = false;
    for index in (0..scan_end).rev().take(256) {
        reached_oldest = index == 0;
        let index = u32::try_from(index).map_err(|error| history_decode_error(&error))?;
        let mut line = if bottom_anchor.is_some() {
            let Some(line) = decoded
                .resolve(
                    client,
                    session_id,
                    &capture,
                    index,
                    (remaining, styles, &mut requests_left),
                )
                .await?
            else {
                return Ok(CapturedWindowOutcome::Unavailable);
            };
            line
        } else if let Some(line) = cache.history.get(&index) {
            std::sync::Arc::clone(line)
        } else {
            let line = std::sync::Arc::new(
                fetch_captured_history_line(
                    client,
                    session_id,
                    &capture,
                    index,
                    remaining,
                    styles,
                    &mut requests_left,
                )
                .await?,
            );
            *remaining = remaining
                .checked_sub(HISTORY_INDEX_ENTRY_BYTES)
                .ok_or_else(|| history_decode_error(&"history cache metadata budget"))?;
            std::sync::Arc::make_mut(&mut cache.history)
                .insert(index, std::sync::Arc::clone(&line));
            line
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
                    (remaining, styles, &mut requests_left),
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
        let projected = project_captured_range(&line, width, start..end, &mut projection_budget)?;
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
    let mut resolved_offset = offset;
    if selected.len() != rows && reached_oldest {
        // A boundary is a navigation result, not a missing capture. Resolve
        // the oldest viewport forward so a taller projection cannot request
        // rows before the beginning of the capture.
        let deficit = rows - selected.len();
        resolved_offset = offset.saturating_sub(deficit.saturating_add(local_skip));
        selected.clear();
        anchors.clear();
        let mut index = 0_u32;
        while selected.len() < rows && u64::from(index) < meta.lines + u64::from(meta.height) {
            let Some(line) = decoded
                .resolve(
                    client,
                    session_id,
                    &capture,
                    index,
                    (remaining, styles, &mut requests_left),
                )
                .await?
            else {
                break;
            };
            let count = line.projected_rows(width).min(rows - selected.len());
            let projected = project_captured_range(&line, width, 0..count, &mut projection_budget)?;
            for row in 0..count {
                anchors.push(bmux_attach_pipeline::CapturedHistoryAnchor {
                    capture_id: meta.identity,
                    line_index: index,
                    column: line
                        .column_for_row(width, row)
                        .ok_or_else(|| history_decode_error(&"invalid boundary row"))?,
                });
            }
            selected.extend(projected);
            index += 1;
        }
        // A capture shorter than the viewport is valid; only actual source
        // rows have anchors. Display padding is not fabricated source content.
        selected.reverse();
        anchors.reverse();
    }
    if selected.len() != rows && !reached_oldest {
        return Ok(CapturedWindowOutcome::Unavailable);
    }
    selected.reverse();
    anchors.reverse();
    Ok(CapturedWindowOutcome::Window(
        bmux_attach_pipeline::PaneScrollbackWindow {
            images: Vec::new(),
            projection_width: width,
            row_anchors: anchors,
            palette: bmux_terminal_grid::StylePalette::from_shared_styles(styles.clone()),
            rows: selected,
            scrollback_offset: resolved_offset,
            max_scrollback_offset: if reached_oldest {
                resolved_offset
            } else {
                pin.max_scrollback_offset
            },
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
    let mut styles = std::sync::Arc::new(Vec::new());
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
        images: Vec::new(),
        projection_width: width,
        row_anchors: Vec::new(),
        palette: bmux_terminal_grid::StylePalette::from_shared_styles(styles),
        rows: output,
        scrollback_offset: offset,
        max_scrollback_offset: pin.max_scrollback_offset,
        total_scrolled_rows: pin.total_scrolled_rows,
    }))
}

/// Convert a main-screen capture row into a capture-wide logical anchor.
#[cfg(test)]
async fn resolve_tail_entry(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    session: Uuid,
    capture: &AttachState::HistoryCaptureV1,
    offset: usize,
    budget: &mut usize,
    styles: &mut std::sync::Arc<Vec<bmux_terminal_grid::Style>>,
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

// Conservative per-entry tree-node allowance, including sparsely occupied nodes.
const HISTORY_INDEX_ENTRY_BYTES: usize = 256;

#[derive(Default, Clone)]
struct CapturedLineCache {
    /// First logical index beyond the immutable captured screen.
    end: Option<u32>,
    /// Resume only at a completed logical-line boundary. Cancelled partial
    /// assembly never advances this cursor.
    tail_resume: Option<(u16, usize)>,
    lines: std::sync::Arc<
        std::collections::BTreeMap<u32, std::sync::Arc<bmux_terminal_grid::HistoryLineAssembly>>,
    >,
}

impl CapturedLineCache {
    async fn resolve(
        &mut self,
        client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
        session: Uuid,
        capture: &AttachState::HistoryCaptureV1,
        index: u32,
        budgets: (
            &mut usize,
            &mut std::sync::Arc<Vec<bmux_terminal_grid::Style>>,
            &mut usize,
        ),
    ) -> ClientResult<Option<std::sync::Arc<bmux_terminal_grid::HistoryLineAssembly>>> {
        if self.end.is_some_and(|end| index >= end) {
            return Ok(None);
        }
        if let Some(line) = self.lines.get(&index) {
            return Ok(Some(std::sync::Arc::clone(line)));
        }
        let (remaining, styles, requests) = budgets;
        // Admission is byte-budgeted below, including line metadata. A fixed
        // entry count rejects cheap short lines despite ample memory headroom.
        let charge = std::mem::size_of::<bmux_terminal_grid::HistoryLineAssembly>()
            + HISTORY_INDEX_ENTRY_BYTES;
        *remaining = remaining
            .checked_sub(charge)
            .ok_or_else(|| history_decode_error(&"decoded line metadata budget"))?;
        let Some(line) = fetch_captured_content_line(
            client,
            session,
            capture,
            index,
            (remaining, styles, requests),
            self,
        )
        .await?
        else {
            // The scan reached the end of this immutable capture. Remember the
            // exact boundary, not the potentially distant requested index.
            self.end = self
                .lines
                .last_key_value()
                .map(|(key, _)| key.saturating_add(1));
            return Ok(None);
        };
        std::sync::Arc::make_mut(&mut self.lines)
            .entry(index)
            .or_insert_with(|| std::sync::Arc::clone(&line));
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
    budgets: (
        &mut usize,
        &mut std::sync::Arc<Vec<bmux_terminal_grid::Style>>,
        &mut usize,
    ),
    cache: &mut CapturedLineCache,
) -> ClientResult<Option<std::sync::Arc<bmux_terminal_grid::HistoryLineAssembly>>> {
    use bmux_terminal_grid::{HistoryLineAssembly, HistorySlice, HistorySliceEnd};
    let CapturedLineCache {
        lines, tail_resume, ..
    } = cache;
    let (budget, styles, requests) = budgets;
    let mut logical = usize::try_from(capture.history_line_count)
        .map_err(|error| history_decode_error(&error))?;
    let mut prefix = None;
    let resume = tail_resume.filter(|(_, logical)| index as usize >= *logical);
    if let Some((_, next_logical)) = resume {
        logical = next_logical;
    }
    if resume.is_none() && logical > 0 {
        let last = u32::try_from(logical - 1).map_err(|error| history_decode_error(&error))?;
        if index < last {
            return fetch_captured_history_line(
                client, session, capture, index, budget, styles, requests,
            )
            .await
            .map(|line| Some(std::sync::Arc::new(line)));
        }
        let history =
            fetch_captured_history_line(client, session, capture, last, budget, styles, requests)
                .await?;
        if matches!(history.completed(), Some((_, HistorySliceEnd::Open))) {
            logical -= 1;
            prefix = Some(history);
        } else if index == last {
            return Ok(Some(std::sync::Arc::new(history)));
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
    for row in resume.map_or(0, |(row, _)| row)..capture.height {
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
            let key = u32::try_from(logical).map_err(|error| history_decode_error(&error))?;
            if let Some(cached) = lines.get(&key) {
                *tail_resume = Some((row + 1, logical + 1));
                if logical == index as usize {
                    return Ok(Some(std::sync::Arc::clone(cached)));
                }
                logical += 1;
                line =
                    HistoryLineAssembly::new(capture.capture_id.as_u128(), logical, *budget, false);
                continue;
            }
            let charge = std::mem::size_of::<HistoryLineAssembly>() + HISTORY_INDEX_ENTRY_BYTES;
            *budget = budget
                .checked_sub(charge)
                .ok_or_else(|| history_decode_error(&"decoded line metadata budget"))?;
            let completed = std::sync::Arc::new(line);
            std::sync::Arc::make_mut(lines).insert(key, std::sync::Arc::clone(&completed));
            *tail_resume = Some((row + 1, logical + 1));
            if logical == index as usize {
                return Ok(Some(completed));
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
    styles: &mut std::sync::Arc<Vec<bmux_terminal_grid::Style>>,
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
    // Charge only text intersecting the requested projection, not the entire
    // logical line (which may be much larger than this viewport).
    let start_column = line.column_for_row(width, range.start).unwrap_or(0);
    let end_column = line.column_for_row(width, range.end).unwrap_or(usize::MAX);
    let mut column = 0_usize;
    let text = line
        .completed()
        .ok_or_else(|| history_decode_error(&"unfinished tail line"))?
        .0
        .iter()
        .try_fold(0_usize, |bytes, cell| {
            let start = column;
            column = column.saturating_add(usize::from(cell.width()));
            bytes.checked_add(if column > start_column && start < end_column {
                cell.text().len()
            } else {
                0
            })
        })
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
    styles: &mut std::sync::Arc<Vec<bmux_terminal_grid::Style>>,
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
    styles: &mut std::sync::Arc<Vec<bmux_terminal_grid::Style>>,
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
        std::sync::Arc::make_mut(styles)
            .try_reserve_exact(1)
            .map_err(|error| history_decode_error(&error))?;
        std::sync::Arc::make_mut(styles).push(Style::default());
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
    styles: &mut std::sync::Arc<Vec<bmux_terminal_grid::Style>>,
    remaining: &mut usize,
) -> ClientResult<Vec<bmux_terminal_grid::Cell>> {
    use serde::de::{Error, SeqAccess, Visitor};
    struct Cells<'a> {
        styles: &'a mut std::sync::Arc<Vec<bmux_terminal_grid::Style>>,
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
                let existing_style = self.styles.iter().position(|entry| *entry == style);
                // Temporary and retained cell copies coexist during assembly;
                // palette storage is charged only when a new slot is retained.
                let charge = 2 * (std::mem::size_of::<Cell>() + text.len())
                    + if existing_style.is_none() {
                        std::mem::size_of::<Style>()
                    } else {
                        0
                    };
                *self.remaining = self
                    .remaining
                    .checked_sub(charge)
                    .ok_or_else(|| A::Error::custom("history budget exhausted"))?;
                let index = if let Some(index) = existing_style {
                    index
                } else {
                    let styles = std::sync::Arc::make_mut(self.styles);
                    styles.try_reserve_exact(1).map_err(A::Error::custom)?;
                    styles.push(style);
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
