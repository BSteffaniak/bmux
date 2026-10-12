//! Complete raw-event replay using BMUX's canonical streaming terminal parser.
//! Every recorded output event contributes a frame; this is not sampled polling.
use anyhow::{Context, Result, ensure};
use bmux_recording_protocol::{
    DisplayTrackEnvelope, DisplayTrackEvent, RecordingEventEnvelope, RecordingEventKind,
    RecordingPayload, write_frame,
};
use std::io::Read;
use std::path::Path;
use uuid::Uuid;

pub(crate) fn replay(directory: &Path, recording_id: Uuid, cols: u16, rows: u16) -> Result<String> {
    if directory.join("capture-format.json").try_exists()? {
        return replay_ordered(directory, recording_id, cols, rows);
    }
    let client_id = Uuid::nil();
    let (mut output, mut stream) = open_replay_stream(recording_id, cols, rows)?;
    let mut canonical_raw = Vec::new();
    let mut last = 0;
    let mut output_origin = None;
    let manifest: crate::recording_runtime::Manifest =
        serde_json::from_slice(&std::fs::read(directory.join("manifest.json"))?)?;
    ensure!(
        manifest.summary.id == recording_id && manifest.summary.ended_epoch_ms.is_some(),
        "replay requires finalized canonical recording"
    );
    ensure!(
        manifest.summary.segments.len() <= 4096,
        "replay segment limit exceeded"
    );
    ensure!(
        manifest.summary.format_version == bmux_recording_protocol::RECORDING_FORMAT_VERSION,
        "unsupported canonical recording version"
    );
    let mut events = 0_u64;
    let mut segment_bytes = 0_u64;
    let mut seen = std::collections::HashSet::new();
    for segment in &manifest.summary.segments {
        ensure!(seen.insert(segment), "duplicate canonical segment");
        ensure!(
            std::path::Path::new(segment).components().count() == 1
                && segment.starts_with("events_")
                && std::path::Path::new(segment)
                    .extension()
                    .is_some_and(|extension| extension
                        .as_encoded_bytes()
                        .eq_ignore_ascii_case(b"bin")),
            "invalid canonical segment locator"
        );
        let path = directory.join(segment);
        ensure!(
            std::fs::symlink_metadata(&path)?.file_type().is_file(),
            "canonical segment must be a regular file"
        );
        let file = std::fs::File::open(path)?;
        segment_bytes = segment_bytes
            .checked_add(file.metadata()?.len())
            .context("segment byte count overflow")?;
        let mut reader = std::io::BufReader::new(file);
        loop {
            let Some(event) = read_replay_event(&mut reader)? else {
                break;
            };
            events += 1;
            ensure!(events <= 1_000_000, "replay event limit exceeded");
            last = output_origin.map_or(0, |origin| event.mono_ns.saturating_sub(origin));
            if event.kind == RecordingEventKind::PaneOutputRaw
                && let RecordingPayload::Bytes { data } = event.payload
            {
                let origin = *output_origin.get_or_insert(event.mono_ns);
                last = event.mono_ns.saturating_sub(origin);
                ensure!(
                    canonical_raw.len().saturating_add(data.len()) <= 64 * 1024 * 1024,
                    "canonical output projection exceeds bound"
                );
                canonical_raw.extend_from_slice(&data);
                stream.process(&data);
                frame(
                    &mut output,
                    &DisplayTrackEnvelope {
                        mono_ns: last,
                        event: DisplayTrackEvent::FrameBytes {
                            data: bmux_terminal_grid::full_screen_repaint_bytes(stream.grid()),
                        },
                    },
                )?;
                ensure!(
                    output.len() <= 64 * 1024 * 1024,
                    "replay display limit exceeded; refusing incomplete timeline"
                );
            }
        }
    }
    crate::process_artifacts::verify_canonical_totals(&manifest.summary, events, segment_bytes)?;
    let text = bmux_terminal_grid::visible_text_trimmed(stream.grid(), 0, usize::from(rows));
    publish_replay(
        directory,
        client_id,
        last,
        &mut output,
        &canonical_raw,
        &text,
    )?;
    Ok(text)
}

fn replay_ordered(directory: &Path, recording_id: Uuid, cols: u16, rows: u16) -> Result<String> {
    use bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::{
        TerminalCaptureEvent, TerminalCaptureEventKind, TerminalCaptureFormat,
    };
    let _: TerminalCaptureFormat =
        serde_json::from_slice(&std::fs::read(directory.join("capture-format.json"))?)?;
    let bytes = std::fs::read(directory.join("capture.events"))?;
    ensure!(
        bytes.len() <= 256 * 1024 * 1024,
        "ordered capture exceeds bound"
    );
    let (mut output, mut stream) = open_replay_stream(recording_id, cols, rows)?;
    let mut reader = std::io::Cursor::new(bytes);
    let mut raw = Vec::new();
    let mut last = 0;
    while reader.position() < u64::try_from(reader.get_ref().len())? {
        let mut length = [0; 4];
        reader.read_exact(&mut length)?;
        let length = usize::try_from(u32::from_le_bytes(length))?;
        ensure!(length <= 65536, "ordered capture frame exceeds bound");
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes)?;
        let event: TerminalCaptureEvent = serde_json::from_slice(&bytes)?;
        ensure!(event.mono_ns >= last, "ordered capture time regressed");
        last = event.mono_ns;
        match event.event {
            TerminalCaptureEventKind::Output { data } => {
                ensure!(
                    raw.len().saturating_add(data.len()) <= 64 * 1024 * 1024,
                    "ordered raw exceeds bound"
                );
                raw.extend_from_slice(&data);
                stream.process(&data);
            }
            TerminalCaptureEventKind::Resize { cols, rows } => {
                ensure!(
                    cols > 0 && rows > 0 && cols <= 1000 && rows <= 1000,
                    "invalid captured viewport"
                );
                stream
                    .resize(cols, rows)
                    .map_err(|error| anyhow::anyhow!("replay resize: {error:?}"))?;
                frame(
                    &mut output,
                    &DisplayTrackEnvelope {
                        mono_ns: last,
                        event: DisplayTrackEvent::Resize { cols, rows },
                    },
                )?;
            }
        }
        frame(
            &mut output,
            &DisplayTrackEnvelope {
                mono_ns: last,
                event: DisplayTrackEvent::FrameBytes {
                    data: bmux_terminal_grid::full_screen_repaint_bytes(stream.grid()),
                },
            },
        )?;
        ensure!(
            output.len() <= 64 * 1024 * 1024,
            "ordered display exceeds bound"
        );
    }
    ensure!(
        std::fs::read(directory.join("output.live"))? == raw,
        "ordered capture differs from canonical raw projection"
    );
    let text = bmux_terminal_grid::visible_text(stream.grid(), 0, stream.grid().height());
    publish_replay(directory, Uuid::nil(), last, &mut output, &raw, &text)?;
    Ok(text)
}

fn open_replay_stream(
    recording_id: Uuid,
    cols: u16,
    rows: u16,
) -> Result<(Vec<u8>, bmux_terminal_grid::TerminalGridStream)> {
    let mut output = Vec::new();
    frame(
        &mut output,
        &DisplayTrackEnvelope {
            mono_ns: 0,
            event: DisplayTrackEvent::StreamOpened {
                client_id: Uuid::nil(),
                recording_id,
                cell_width_px: Some(8),
                cell_height_px: Some(16),
                window_width_px: None,
                window_height_px: None,
                terminal_profile: None,
            },
        },
    )?;
    frame(
        &mut output,
        &DisplayTrackEnvelope {
            mono_ns: 0,
            event: DisplayTrackEvent::Resize { cols, rows },
        },
    )?;
    let stream = bmux_terminal_grid::TerminalGridStream::new(
        cols,
        rows,
        bmux_terminal_grid::GridLimits { scrollback_rows: 0 },
    )
    .map_err(|error| anyhow::anyhow!("replay grid: {error:?}"))?;
    Ok((output, stream))
}

fn publish_replay(
    directory: &Path,
    client_id: Uuid,
    last: u64,
    output: &mut Vec<u8>,
    canonical_raw: &[u8],
    text: &str,
) -> Result<()> {
    let live = directory.join("output.live");
    if live.try_exists()? {
        let file = std::fs::File::open(&live)?;
        ensure!(
            file.metadata()?.len() == u64::try_from(canonical_raw.len())?,
            "canonical replay length differs from retained live evidence"
        );
        let mut retained = Vec::new();
        file.take(u64::try_from(canonical_raw.len())?.saturating_add(1))
            .read_to_end(&mut retained)?;
        ensure!(
            retained == canonical_raw,
            "canonical replay differs from retained live evidence"
        );
    }
    frame(
        output,
        &DisplayTrackEnvelope {
            mono_ns: last,
            event: DisplayTrackEvent::StreamClosed,
        },
    )?;
    crate::durable::write_atomic(&directory.join(format!("display-{client_id}.bin")), output)?;
    crate::durable::write_atomic(&directory.join("output.live"), canonical_raw)?;
    crate::durable::write_atomic(&directory.join("screen.txt"), text.as_bytes())?;
    Ok(())
}

fn read_replay_event(
    reader: &mut impl Read,
) -> Result<Option<RecordingEventEnvelope<bmux_ipc::Event, bmux_ipc::ErrorCode>>> {
    let mut prefix = [0; 4];
    if reader.read(&mut prefix[..1])? == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut prefix[1..])
        .context("truncated replay prefix")?;
    let size = usize::try_from(u32::from_le_bytes(prefix))?;
    ensure!(
        size > 0 && size <= 4 * 1024 * 1024,
        "replay frame limit exceeded"
    );
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    let event: RecordingEventEnvelope<bmux_ipc::Event, bmux_ipc::ErrorCode> =
        bmux_codec::from_bytes(&bytes)?;
    Ok(Some(event))
}

fn frame(writer: &mut Vec<u8>, value: &DisplayTrackEnvelope) -> Result<()> {
    write_frame(writer, value).map_err(|error| anyhow::anyhow!("display encoding: {error}"))
}

#[cfg(test)]
mod ordered_replay_tests {
    use super::*;
    #[test]
    fn ordered_stream_replays_resize_time_and_final_geometry() {
        use bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::{
            TerminalCaptureEvent, TerminalCaptureEventKind, TerminalCaptureFormat,
        };
        let root = std::env::temp_dir().join(format!("bmux-ordered-replay-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let events = [
            TerminalCaptureEvent {
                mono_ns: 10,
                event: TerminalCaptureEventKind::Output {
                    data: b"before".to_vec(),
                },
            },
            TerminalCaptureEvent {
                mono_ns: 50_000_000,
                event: TerminalCaptureEventKind::Resize {
                    cols: 101,
                    rows: 29,
                },
            },
            TerminalCaptureEvent {
                mono_ns: 70_000_000,
                event: TerminalCaptureEventKind::Output {
                    data: b"\r\nafter".to_vec(),
                },
            },
        ];
        let mut capture = Vec::new();
        for event in events {
            let bytes = serde_json::to_vec(&event).unwrap();
            capture.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_le_bytes());
            capture.extend_from_slice(&bytes);
        }
        std::fs::write(
            root.join("capture-format.json"),
            serde_json::to_vec(&TerminalCaptureFormat::OrderedEventsV1).unwrap(),
        )
        .unwrap();
        std::fs::write(root.join("capture.events"), capture).unwrap();
        std::fs::write(root.join("output.live"), b"before\r\nafter").unwrap();
        let text = replay(&root, Uuid::new_v4(), 80, 24).unwrap();
        assert!(text.contains("after"));
        let display = std::fs::read(root.join(format!("display-{}.bin", Uuid::nil()))).unwrap();
        let frames =
            bmux_recording_protocol::read_frames::<DisplayTrackEnvelope>(&display).unwrap();
        assert_eq!(frames.bytes_remaining, 0);
        let mut found = false;
        for event in frames.frames {
            if matches!(
                event.event,
                DisplayTrackEvent::Resize {
                    cols: 101,
                    rows: 29
                }
            ) {
                assert_eq!(event.mono_ns, 50_000_000);
                found = true;
            }
        }
        assert!(found);
    }
}

#[cfg(test)]
include!("process_replay_tests.rs");
