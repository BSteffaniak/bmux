use super::{MIN_PANE_COLS, MIN_PANE_ROWS, PaneProcess, PaneRuntime, PaneState};
use crate::pane::{PaneId, Rect};
use crate::pty::extract_filtered_output;
use crate::runtime::terminal_protocol::{
    ProtocolProfile, SharedProtocolTraceBuffer, TerminalProtocolEngine,
};
use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use tracing::debug;
use vt100::Parser as VtParser;

pub(super) fn spawn_pane(
    pane_id: PaneId,
    shell: &str,
    pane_term: &str,
    protocol_profile: ProtocolProfile,
    title: String,
    pane_inner: Rect,
    startup_deadline: Instant,
    user_input_seen: Arc<AtomicBool>,
    protocol_trace: Option<SharedProtocolTraceBuffer>,
) -> Result<PaneRuntime> {
    let state = Arc::new(PaneState {
        parser: Mutex::new(VtParser::new(
            pane_inner.height.max(MIN_PANE_ROWS),
            pane_inner.width.max(MIN_PANE_COLS),
            10_000,
        )),
        dirty: AtomicBool::new(true),
    });

    Ok(PaneRuntime {
        title: title.clone(),
        shell: shell.to_string(),
        process: Some(spawn_pane_process(
            shell,
            pane_term,
            protocol_profile,
            pane_id,
            title,
            pane_inner,
            startup_deadline,
            user_input_seen,
            Arc::clone(&state),
            protocol_trace,
        )?),
        state,
        closed: false,
        exit_code: None,
    })
}

pub(super) fn spawn_pane_process(
    shell: &str,
    pane_term: &str,
    protocol_profile: ProtocolProfile,
    pane_id: PaneId,
    title: String,
    pane_inner: Rect,
    startup_deadline: Instant,
    user_input_seen: Arc<AtomicBool>,
    state: Arc<PaneState>,
    protocol_trace: Option<SharedProtocolTraceBuffer>,
) -> Result<PaneProcess> {
    let pty_system = native_pty_system();
    let pty_pair = pty_system
        .openpty(PtySize {
            rows: pane_inner.height.max(MIN_PANE_ROWS),
            cols: pane_inner.width.max(MIN_PANE_COLS),
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open pane PTY")?;

    let mut command = CommandBuilder::new(shell);
    command.env("TERM", pane_term);
    let child = pty_pair
        .slave
        .spawn_command(command)
        .context("failed to spawn shell in pane")?;
    drop(pty_pair.slave);

    {
        let mut parser = state.parser.lock().expect("pane parser mutex poisoned");
        parser.screen_mut().set_size(
            pane_inner.height.max(MIN_PANE_ROWS),
            pane_inner.width.max(MIN_PANE_COLS),
        );
    }
    state.dirty.store(true, Ordering::Relaxed);

    let mut reader = pty_pair
        .master
        .try_clone_reader()
        .context("failed to clone pane PTY reader")?;
    let writer = pty_pair
        .master
        .take_writer()
        .context("failed to open pane PTY writer")?;
    let writer = Arc::new(Mutex::new(writer));

    let state_for_thread = Arc::clone(&state);
    let writer_for_thread = Arc::clone(&writer);
    let output_thread = thread::Builder::new()
        .name(format!("bmux-pane-output-{title}"))
        .spawn(move || -> Result<()> {
            let mut buffer = [0_u8; 8192];
            let mut pending = Vec::new();
            let mut protocol_engine = if let Some(trace) = protocol_trace {
                TerminalProtocolEngine::with_trace(protocol_profile, pane_id.0, trace)
            } else {
                TerminalProtocolEngine::new(protocol_profile)
            };

            loop {
                let bytes_read = reader
                    .read(&mut buffer)
                    .context("failed reading pane PTY output")?;
                if bytes_read == 0 {
                    break;
                }

                pending.extend_from_slice(&buffer[..bytes_read]);
                let startup_guard_active =
                    !user_input_seen.load(Ordering::Relaxed) && Instant::now() < startup_deadline;

                let (output, dropped_exit_sequence) =
                    extract_filtered_output(&mut pending, startup_guard_active);

                if dropped_exit_sequence {
                    debug!("Dropped startup alt-screen exit sequence from pane output");
                }

                if output.is_empty() {
                    continue;
                }

                let mut parser = state_for_thread
                    .parser
                    .lock()
                    .expect("pane parser mutex poisoned");
                parser.process(&output);
                let cursor_pos = parser.screen().cursor_position();
                state_for_thread.dirty.store(true, Ordering::Relaxed);
                drop(parser);

                let reply = protocol_engine.process_output(&output, cursor_pos);
                if !reply.is_empty() {
                    let mut writer = writer_for_thread
                        .lock()
                        .expect("pane PTY writer mutex poisoned");
                    writer
                        .write_all(&reply)
                        .and_then(|_| writer.flush())
                        .context("failed writing terminal protocol reply to pane")?;
                }
            }

            Ok(())
        })
        .context("failed to spawn pane output thread")?;

    Ok(PaneProcess {
        master: pty_pair.master,
        writer,
        child,
        output_thread: Some(output_thread),
    })
}

pub(super) fn refresh_exit_codes(panes: &mut BTreeMap<PaneId, PaneRuntime>) -> Result<()> {
    for pane in panes.values_mut() {
        let Some(process) = pane.process.as_mut() else {
            continue;
        };

        if let Some(status) = process
            .child
            .try_wait()
            .context("failed to poll pane shell status")?
        {
            pane.exit_code = Some(super::exit_code_from_u32(status.exit_code()));
            stop_pane_process(pane, false)?;
            pane.closed = false;
            pane.state.dirty.store(true, Ordering::Relaxed);
        }
    }

    Ok(())
}

pub(super) fn stop_pane_process(pane: &mut PaneRuntime, kill: bool) -> Result<()> {
    if let Some(mut process) = pane.process.take() {
        if kill {
            let _ = process.child.kill();
        }

        let _ = process.child.wait();

        if let Some(output_thread) = process.output_thread.take() {
            match output_thread.join() {
                Ok(result) => result.context("PTY output thread failed")?,
                Err(_) => return Err(anyhow::anyhow!("PTY output thread panicked")),
            }
        }
    }

    Ok(())
}

pub(super) fn pane_is_running(pane: &PaneRuntime) -> bool {
    pane.process.is_some()
}

pub(super) fn any_running_panes(panes: &BTreeMap<PaneId, PaneRuntime>) -> bool {
    panes.values().any(pane_is_running)
}

pub(super) fn first_running_pane_id(
    pane_order: &[PaneId],
    panes: &BTreeMap<PaneId, PaneRuntime>,
) -> Option<PaneId> {
    pane_order
        .iter()
        .find(|pane_id| panes.get(pane_id).is_some_and(pane_is_running))
        .copied()
}

pub(super) fn next_focusable_pane_id(
    pane_order: &[PaneId],
    panes: &BTreeMap<PaneId, PaneRuntime>,
    current: PaneId,
) -> PaneId {
    if pane_order.is_empty() {
        return current;
    }

    let current_index = pane_order
        .iter()
        .position(|pane_id| *pane_id == current)
        .unwrap_or(0);

    for offset in 1..=pane_order.len() {
        let index = (current_index + offset) % pane_order.len();
        let candidate = pane_order[index];
        if panes.get(&candidate).is_some_and(pane_is_running) {
            return candidate;
        }
    }

    current
}

pub(super) fn resize_panes(
    panes: &mut BTreeMap<PaneId, PaneRuntime>,
    pane_rects: &BTreeMap<PaneId, Rect>,
) -> Result<()> {
    for (pane_id, pane) in panes.iter_mut() {
        let Some(rect) = pane_rects.get(pane_id) else {
            continue;
        };
        let inner = rect.inner();

        if let Some(process) = pane.process.as_mut() {
            process
                .master
                .resize(PtySize {
                    rows: inner.height.max(MIN_PANE_ROWS),
                    cols: inner.width.max(MIN_PANE_COLS),
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("failed to resize pane PTY")?;
        }

        let mut parser = pane
            .state
            .parser
            .lock()
            .expect("pane parser mutex poisoned");
        parser.screen_mut().set_size(
            inner.height.max(MIN_PANE_ROWS),
            inner.width.max(MIN_PANE_COLS),
        );
        pane.state.dirty.store(true, Ordering::Relaxed);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::TerminalProtocolEngine;
    use crate::runtime::terminal_protocol::ProtocolProfile;
    use vt100::Parser as VtParser;

    struct ReplayFixture {
        profile: ProtocolProfile,
        chunks: Vec<Vec<u8>>,
        expected_reply: Vec<u8>,
        expected_render_contains: String,
    }

    fn process_chunks(profile: ProtocolProfile, chunks: &[&[u8]]) -> (String, Vec<u8>) {
        let mut parser = VtParser::new(10, 40, 100);
        let mut engine = TerminalProtocolEngine::new(profile);
        let mut replies = Vec::new();

        for chunk in chunks {
            parser.process(chunk);
            let cursor = parser.screen().cursor_position();
            replies.extend(engine.process_output(chunk, cursor));
        }

        (parser.screen().contents().to_string(), replies)
    }

    fn process_fixture(fixture: &ReplayFixture) -> (String, Vec<u8>) {
        let chunks: Vec<&[u8]> = fixture.chunks.iter().map(Vec::as_slice).collect();
        process_chunks(fixture.profile, &chunks)
    }

    fn parse_fixture(source: &str) -> ReplayFixture {
        let mut profile = ProtocolProfile::Conservative;
        let mut chunks = Vec::new();
        let mut expected_reply = Vec::new();
        let mut expected_render_contains = String::new();

        for raw_line in source.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some(value) = line.strip_prefix("PROFILE:") {
                profile = match value.trim() {
                    "bmux" => ProtocolProfile::Bmux,
                    "xterm" => ProtocolProfile::Xterm,
                    "screen" => ProtocolProfile::Screen,
                    _ => ProtocolProfile::Conservative,
                };
                continue;
            }

            if let Some(value) = line.strip_prefix("CHUNK:") {
                chunks.push(unescape(value.trim()));
                continue;
            }

            if let Some(value) = line.strip_prefix("EXPECT_REPLY:") {
                expected_reply = unescape(value.trim());
                continue;
            }

            if let Some(value) = line.strip_prefix("EXPECT_RENDER_CONTAINS:") {
                expected_render_contains = value.trim().to_string();
            }
        }

        ReplayFixture {
            profile,
            chunks,
            expected_reply,
            expected_render_contains,
        }
    }

    fn unescape(value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let bytes = value.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                match bytes[i + 1] {
                    b'x' if i + 3 < bytes.len() => {
                        let hi = bytes[i + 2] as char;
                        let lo = bytes[i + 3] as char;
                        let hex = format!("{hi}{lo}");
                        if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                            out.push(byte);
                            i += 4;
                            continue;
                        }
                    }
                    b'n' => {
                        out.push(b'\n');
                        i += 2;
                        continue;
                    }
                    b'r' => {
                        out.push(b'\r');
                        i += 2;
                        continue;
                    }
                    b't' => {
                        out.push(b'\t');
                        i += 2;
                        continue;
                    }
                    b'\\' => {
                        out.push(b'\\');
                        i += 2;
                        continue;
                    }
                    _ => {}
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        out
    }

    #[test]
    fn mixed_output_and_queries_keeps_rendered_text_contiguous() {
        let (contents, replies) = process_chunks(
            ProtocolProfile::Xterm,
            &[b"hello ", b"\x1b[5n", b"world", b"\x1b[>c"],
        );

        assert!(contents.contains("hello world"));
        assert_eq!(replies, b"\x1b[0n\x1b[>0;115;0c");
    }

    #[test]
    fn split_query_sequences_preserve_text_and_reply_once() {
        let (contents, replies) = process_chunks(
            ProtocolProfile::Screen,
            &[b"ab", b"\x1b", b"[", b"?25$p", b"cd"],
        );

        assert!(contents.contains("abcd"));
        assert_eq!(replies, b"\x1b[?25;1$y");
    }

    #[test]
    fn replays_fish_startup_fixture() {
        let fixture = parse_fixture(include_str!("fixtures/fish_startup.trace"));
        let (contents, replies) = process_fixture(&fixture);
        assert!(contents.contains(&fixture.expected_render_contains));
        assert_eq!(replies, fixture.expected_reply);
    }

    #[test]
    fn replays_vim_startup_fixture() {
        let fixture = parse_fixture(include_str!("fixtures/vim_startup.trace"));
        let (contents, replies) = process_fixture(&fixture);
        assert!(contents.contains(&fixture.expected_render_contains));
        assert_eq!(replies, fixture.expected_reply);
    }

    #[test]
    fn replays_fzf_startup_fixture() {
        let fixture = parse_fixture(include_str!("fixtures/fzf_startup.trace"));
        let (contents, replies) = process_fixture(&fixture);
        assert!(contents.contains(&fixture.expected_render_contains));
        assert_eq!(replies, fixture.expected_reply);
    }
}
