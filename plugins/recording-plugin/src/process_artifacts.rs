//! Derived process artifacts from canonical recording segments, never a PTY authority.
//!
//! `output.pty` concatenates only selected `PaneOutputRaw` bytes, without decoding.
//! `events.jsonl` contains the unchanged protocol envelopes; serde encodes binary
//! fields as JSON arrays of unsigned bytes (not lossy UTF-8 strings).
//! `output.txt` is a readable transcript, not a terminal screen: ANSI CSI and
//! string controls (OSC/DCS/APC/PM/SOS) are stripped across frame boundaries,
//! CR moves to the beginning of the current line, BS moves back one Unicode
//! scalar, LF commits the line, and TAB is retained. UTF-8 is decoded across
//! frames, replacing malformed/incomplete sequences with U+FFFD. Overwrites
//! operate on Unicode scalars, not display cells or grapheme clusters. Other
//! C0 controls and DEL are omitted. Each pane has independent parser state;
//! unterminated lines are emitted in pane-ID order at EOF. This is deliberately
//! not a terminal emulator. `screen.txt` is supplied by authoritative capture.
//!
//! Publication requires a fresh output directory in a caller-controlled parent.
//! A private sibling staging directory is synced and renamed only after every
//! artifact succeeds. No success is returned for partial or truncated input.

use anyhow::{Context, Result, bail, ensure};
use bmux_recording_protocol::{
    RECORDING_FORMAT_VERSION, RecordingEventEnvelope, RecordingEventKind, RecordingPayload,
    RecordingSummary,
};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

pub(crate) type ProcessRecordingEvent =
    RecordingEventEnvelope<bmux_ipc::Event, bmux_ipc::ErrorCode>;

#[derive(Clone, Debug, Default)]
pub(crate) struct ProcessArtifactScope {
    pub session: Option<Uuid>,
    pub pane: Option<Uuid>,
    /// The current canonical envelope cannot represent execution identity.
    /// Any non-None value is rejected, rather than approximated with a pane ID.
    pub execution: Option<Uuid>,
}

impl ProcessArtifactScope {
    fn matches(&self, event: &ProcessRecordingEvent) -> bool {
        self.session.is_none_or(|id| event.session_id == Some(id))
            && self.pane.is_none_or(|id| event.pane_id == Some(id))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessArtifactOptions {
    pub scope: ProcessArtifactScope,
    pub max_frame_bytes: usize,
    pub max_events: u64,
    pub max_line_chars: usize,
    pub max_panes: usize,
    /// Per artifact, not per frame. Exceeding a limit is an error, never truncation.
    pub max_artifact_bytes: u64,
    /// Existing exact-view GIF exporter, only valid for an unscoped recording.
    /// It retains the existing display-track/profile semantics. Its renderer
    /// currently loads the display track; this option does not promise streaming.
    pub gif_fps: Option<u32>,
}

impl Default for ProcessArtifactOptions {
    fn default() -> Self {
        Self {
            scope: ProcessArtifactScope::default(),
            max_frame_bytes: 16 * 1024 * 1024,
            max_events: 1_000_000,
            max_line_chars: 1024 * 1024,
            max_panes: 128,
            max_artifact_bytes: 1024 * 1024 * 1024,
            gif_fps: None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ProcessArtifactReport {
    pub selected_events: u64,
    pub output_bytes: u64,
    pub artifacts: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct Manifest {
    summary: RecordingSummary,
}

/// Export a stopped canonical recording to a new, atomically published directory.
/// `screen` must be authoritative capture bytes already scoped by the caller.
/// Missing screen capture creates no screen artifact. No execution metadata is
/// invented: JSON preserves all metadata actually present in the envelope.
pub(crate) fn export_process_artifacts(
    recording_dir: &Path,
    output_dir: &Path,
    options: &ProcessArtifactOptions,
    screen: Option<&[u8]>,
) -> Result<ProcessArtifactReport> {
    export_process_artifacts_selected(recording_dir, output_dir, options, screen, true, true, true)
}

// Interactive callers select derived artifacts without changing the historical
// headless export surface. GIF selection remains options.gif_fps.
#[allow(
    clippy::too_many_arguments,
    reason = "explicit private format selection preserves the existing headless options contract"
)]
pub(crate) fn export_process_artifacts_selected(
    recording_dir: &Path,
    output_dir: &Path,
    options: &ProcessArtifactOptions,
    screen: Option<&[u8]>,
    include_pty: bool,
    include_text: bool,
    include_events: bool,
) -> Result<ProcessArtifactReport> {
    validate_options(options)?;
    let summary = read_summary(recording_dir)?;
    ensure!(
        summary.ended_epoch_ms.is_some(),
        "recording must be stopped before artifact export"
    );
    ensure!(
        !output_dir.try_exists()?,
        "artifact directory already exists"
    );
    let parent = output_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(
        output_dir.file_name().is_some(),
        "artifact directory requires a file name"
    );
    let staging = Staging::create(parent)?;
    let mut raw = artifact_writer(&staging.path.join("output.pty"), options.max_artifact_bytes)?;
    let mut text = artifact_writer(&staging.path.join("output.txt"), options.max_artifact_bytes)?;
    let mut events = artifact_writer(
        &staging.path.join("events.jsonl"),
        options.max_artifact_bytes,
    )?;
    let mut report = export_segments(
        recording_dir,
        &summary,
        options,
        &mut raw,
        &mut text,
        &mut events,
    )?;
    finish_writer(raw)?;
    finish_writer(text)?;
    finish_writer(events)?;
    let mut names = Vec::new();
    if recording_dir.join("capture-format.json").try_exists()? {
        for name in ["capture-format.json", "capture.events", "viewport.json"] {
            let bytes = fs::read(recording_dir.join(name))?;
            let mut writer = artifact_writer(&staging.path.join(name), options.max_artifact_bytes)?;
            writer.write_all(&bytes)?;
            finish_writer(writer)?;
            names.push(name);
        }
    }
    for (selected, name) in [
        (include_pty, "output.pty"),
        (include_text, "output.txt"),
        (include_events, "events.jsonl"),
    ] {
        if selected {
            names.push(name);
        } else {
            fs::remove_file(staging.path.join(name))?;
        }
    }
    if let Some(screen) = screen {
        let mut writer =
            artifact_writer(&staging.path.join("screen.txt"), options.max_artifact_bytes)?;
        writer.write_all(screen)?;
        finish_writer(writer)?;
        names.push("screen.txt");
    }
    if let Some(fps) = options.gif_fps {
        let gif_path = staging.path.join("output.gif");
        crate::export::export_recording_gif_for_recording_dir(
            recording_dir,
            summary.id,
            gif_path.to_str().context("GIF path is not UTF-8")?,
            fps,
        )?;
        let file = File::open(&gif_path)?;
        ensure!(
            file.metadata()?.len() <= options.max_artifact_bytes,
            "GIF artifact limit exceeded"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.sync_all()?;
        names.push("output.gif");
    }
    File::open(&staging.path)?.sync_all()?;
    ensure!(
        !output_dir.try_exists()?,
        "artifact directory appeared during export"
    );
    publish_directory_noreplace(&staging.path, output_dir)?;
    File::open(parent)?
        .sync_all()
        .context("syncing artifact parent after publication")?;
    report.artifacts = names.iter().map(|name| output_dir.join(name)).collect();
    Ok(report)
}

fn publish_directory_noreplace(source: &Path, target: &Path) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        let target = std::ffi::CString::new(target.as_os_str().as_bytes())?;
        // Both paths point into the caller-owned export parent. The kernel
        // atomically rejects any competing destination, including empty dirs.
        #[cfg(target_os = "macos")]
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("publishing artifacts without replacement");
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (source, target);
        bail!("atomic no-replace artifact publication is unsupported on this platform")
    }
}

#[cfg(test)]
mod publication_tests {
    #[test]
    fn no_replace_publication_rejects_competing_empty_directory() {
        let root = std::env::temp_dir().join(format!("bmux-no-replace-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("staging");
        let target = root.join("published");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("evidence"), b"owned").unwrap();
        std::fs::create_dir(&target).unwrap();
        assert!(super::publish_directory_noreplace(&source, &target).is_err());
        assert!(source.join("evidence").exists());
        assert!(std::fs::read_dir(&target).unwrap().next().is_none());
    }
}

fn export_segments(
    recording_dir: &Path,
    summary: &RecordingSummary,
    options: &ProcessArtifactOptions,
    raw: &mut impl Write,
    text: &mut impl Write,
    events: &mut impl Write,
) -> Result<ProcessArtifactReport> {
    let mut report = ProcessArtifactReport::default();
    let mut transcripts = BTreeMap::<Option<Uuid>, Transcript>::new();
    let mut seen_segments = HashSet::new();
    let mut count = 0_u64;
    let mut segment_bytes = 0_u64;
    for segment in &summary.segments {
        validate_segment(segment)?;
        ensure!(
            seen_segments.insert(segment),
            "duplicate recording segment: {segment}"
        );
        let path = recording_dir.join(segment);
        ensure!(
            fs::symlink_metadata(&path)?.file_type().is_file(),
            "segment must be a regular file: {}",
            path.display()
        );
        let file =
            File::open(&path).with_context(|| format!("opening segment {}", path.display()))?;
        segment_bytes = segment_bytes
            .checked_add(file.metadata()?.len())
            .context("segment byte count overflow")?;
        let mut reader = BufReader::new(file);
        while let Some(event) = read_event(&mut reader, options.max_frame_bytes)? {
            count = count.checked_add(1).context("event count overflow")?;
            ensure!(
                count <= options.max_events,
                "recording event limit exceeded"
            );
            if !options.scope.matches(&event) {
                continue;
            }
            serde_json::to_writer(&mut *events, &event).context("serializing recording event")?;
            events.write_all(b"\n")?;
            report.selected_events += 1;
            if event.kind != RecordingEventKind::PaneOutputRaw {
                continue;
            }
            let RecordingPayload::Bytes { data } = &event.payload else {
                bail!("PaneOutputRaw event {} has a non-byte payload", event.seq);
            };
            raw.write_all(data)?;
            report.output_bytes = report
                .output_bytes
                .checked_add(u64::try_from(data.len())?)
                .context("output byte count overflow")?;
            let pane_count = transcripts.len();
            let transcript = match transcripts.entry(event.pane_id) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    ensure!(
                        pane_count < options.max_panes,
                        "transcript pane limit exceeded"
                    );
                    entry.insert(Transcript::new(options.max_line_chars))
                }
            };
            transcript.feed(data, text)?;
        }
    }
    verify_canonical_totals(summary, count, segment_bytes)?;
    for transcript in transcripts.values_mut() {
        transcript.finish(text)?;
    }
    Ok(report)
}

pub(crate) fn verify_canonical_totals(
    summary: &RecordingSummary,
    events: u64,
    bytes: u64,
) -> Result<()> {
    ensure!(
        events == summary.event_count,
        "canonical event count does not match committed manifest"
    );
    ensure!(
        bytes == summary.total_segment_bytes,
        "canonical segment byte count does not match committed manifest"
    );
    Ok(())
}

fn validate_options(options: &ProcessArtifactOptions) -> Result<()> {
    ensure!(
        options.scope.execution.is_none(),
        "canonical recording envelope has no execution identity; execution filtering is unsupported"
    );
    ensure!(
        options.max_frame_bytes > 0
            && options.max_events > 0
            && options.max_line_chars > 0
            && options.max_panes > 0
            && options.max_artifact_bytes > 0,
        "artifact limits must be positive"
    );
    if let Some(fps) = options.gif_fps {
        ensure!(fps > 0, "GIF FPS must be positive");
        ensure!(
            options.scope.session.is_none() && options.scope.pane.is_none(),
            "scoped process GIF requires an explicitly selected authoritative display track; existing exporter is recording-view only"
        );
    }
    Ok(())
}

fn read_summary(recording_dir: &Path) -> Result<RecordingSummary> {
    const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    File::open(recording_dir.join("manifest.json"))?
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        u64::try_from(bytes.len())? <= MAX_MANIFEST_BYTES,
        "recording manifest limit exceeded"
    );
    let manifest: Manifest =
        serde_json::from_slice(&bytes).context("invalid recording manifest")?;
    ensure!(
        manifest.summary.format_version == RECORDING_FORMAT_VERSION,
        "unsupported recording version {}; expected {}",
        manifest.summary.format_version,
        RECORDING_FORMAT_VERSION
    );
    ensure!(
        !manifest.summary.segments.is_empty(),
        "recording has no canonical segments"
    );
    Ok(manifest.summary)
}

fn validate_segment(segment: &str) -> Result<()> {
    let mut components = Path::new(segment).components();
    ensure!(
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none(),
        "unsafe recording segment path: {segment}"
    );
    Ok(())
}

/// Read exactly one length-prefixed protocol frame; never allocate from an
/// unchecked on-disk length and never accept a partial trailing frame.
fn read_event(reader: &mut impl Read, limit: usize) -> Result<Option<ProcessRecordingEvent>> {
    let mut prefix = [0_u8; 4];
    if reader.read(&mut prefix[..1])? == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut prefix[1..])
        .context("truncated recording frame prefix")?;
    let length = usize::try_from(u32::from_le_bytes(prefix))?;
    ensure!(
        length > 0 && length <= limit,
        "recording frame length {length} exceeds limit or is zero"
    );
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .context("truncated recording frame")?;
    bmux_codec::from_bytes(&bytes)
        .map(Some)
        .context("invalid recording event frame")
}

struct LimitedWriter {
    inner: BufWriter<File>,
    remaining: u64,
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let length = u64::try_from(bytes.len()).map_err(std::io::Error::other)?;
        if length > self.remaining {
            return Err(std::io::Error::other("artifact byte limit exceeded"));
        }
        let written = self.inner.write(bytes)?;
        self.remaining -= u64::try_from(written).map_err(std::io::Error::other)?;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn artifact_writer(path: &Path, limit: u64) -> Result<LimitedWriter> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(LimitedWriter {
        inner: BufWriter::new(options.open(path)?),
        remaining: limit,
    })
}

fn finish_writer(mut writer: LimitedWriter) -> Result<()> {
    writer.flush()?;
    writer.inner.get_ref().sync_all()?;
    Ok(())
}

struct Staging {
    path: PathBuf,
}

impl Staging {
    fn create(parent: &Path) -> Result<Self> {
        let path = parent.join(format!(".bmux-artifacts-{}", Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .context("creating private artifact staging directory")?;
        Ok(Self { path })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        // Only this newly created private directory, never the caller's output.
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Copy)]
enum Escape {
    Text,
    Start,
    Csi,
    String,
    StringTerminator,
    Intermediate,
}

struct Transcript {
    escape: Escape,
    utf8: Vec<u8>,
    line: Vec<char>,
    cursor: usize,
    limit: usize,
}

impl Transcript {
    fn new(limit: usize) -> Self {
        Self {
            escape: Escape::Text,
            utf8: Vec::with_capacity(4),
            line: Vec::new(),
            cursor: 0,
            limit,
        }
    }

    fn feed(&mut self, bytes: &[u8], writer: &mut impl Write) -> Result<()> {
        for &byte in bytes {
            match self.escape {
                Escape::Start => {
                    self.escape = match byte {
                        b'[' => Escape::Csi,
                        b']' | b'P' | b'_' | b'^' | b'X' => Escape::String,
                        0x20..=0x2f => Escape::Intermediate,
                        _ => Escape::Text,
                    };
                }
                Escape::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.escape = Escape::Text;
                    }
                }
                Escape::String => {
                    if byte == 0x07 {
                        self.escape = Escape::Text;
                    } else if byte == 0x1b {
                        self.escape = Escape::StringTerminator;
                    }
                }
                Escape::StringTerminator => {
                    self.escape = if byte == b'\\' {
                        Escape::Text
                    } else if byte == 0x1b {
                        Escape::StringTerminator
                    } else {
                        Escape::String
                    };
                }
                Escape::Intermediate => {
                    if (0x30..=0x7e).contains(&byte) {
                        self.escape = Escape::Text;
                    }
                }
                Escape::Text => self.text_byte(byte, writer)?,
            }
        }
        Ok(())
    }

    fn text_byte(&mut self, byte: u8, writer: &mut impl Write) -> Result<()> {
        if byte < 0x20 || byte == 0x7f {
            self.flush_utf8()?;
            match byte {
                0x1b => self.escape = Escape::Start,
                b'\r' => self.cursor = 0,
                0x08 => self.cursor = self.cursor.saturating_sub(1),
                b'\n' => self.commit(writer, true)?,
                b'\t' => self.put('\t')?,
                _ => {}
            }
        } else {
            self.utf8.push(byte);
            loop {
                match std::str::from_utf8(&self.utf8) {
                    Ok(text) => {
                        let character = text.chars().next().context("empty UTF-8 buffer")?;
                        self.utf8.clear();
                        self.put(character)?;
                        break;
                    }
                    Err(error) if error.error_len().is_none() => break,
                    Err(error) => {
                        let invalid = error.error_len().context("missing invalid UTF-8 length")?;
                        self.utf8.drain(..invalid);
                        self.put('\u{fffd}')?;
                        if self.utf8.is_empty() {
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn flush_utf8(&mut self) -> Result<()> {
        if !self.utf8.is_empty() {
            self.utf8.clear();
            self.put('\u{fffd}')?;
        }
        Ok(())
    }

    fn put(&mut self, character: char) -> Result<()> {
        if self.cursor < self.line.len() {
            self.line[self.cursor] = character;
        } else {
            ensure!(
                self.line.len() < self.limit,
                "transcript line limit exceeded"
            );
            self.line.push(character);
        }
        self.cursor += 1;
        Ok(())
    }

    fn commit(&mut self, writer: &mut impl Write, newline: bool) -> Result<()> {
        for character in &self.line {
            writer.write_all(character.encode_utf8(&mut [0; 4]).as_bytes())?;
        }
        if newline {
            writer.write_all(b"\n")?;
        }
        self.line.clear();
        self.cursor = 0;
        Ok(())
    }

    fn finish(&mut self, writer: &mut impl Write) -> Result<()> {
        self.flush_utf8()?;
        self.commit(writer, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_recording_protocol::{RecordingProfile, write_frame};
    use std::io::Cursor;

    fn event(seq: u64, session: Uuid, pane: Uuid, bytes: &[u8]) -> ProcessRecordingEvent {
        ProcessRecordingEvent {
            seq,
            mono_ns: seq * 1_000,
            wall_epoch_ms: seq * 10,
            session_id: Some(session),
            pane_id: Some(pane),
            client_id: None,
            kind: RecordingEventKind::PaneOutputRaw,
            payload: RecordingPayload::Bytes {
                data: bytes.to_vec(),
            },
        }
    }

    fn fixture(events: &[ProcessRecordingEvent]) -> Result<(Staging, RecordingSummary)> {
        let dir = Staging::create(&std::env::temp_dir())?;
        let mut summary = RecordingSummary {
            id: Uuid::new_v4(),
            name: None,
            format_version: RECORDING_FORMAT_VERSION,
            session_id: None,
            capture_input: false,
            profile: RecordingProfile::Full,
            event_kinds: vec![RecordingEventKind::PaneOutputRaw],
            started_epoch_ms: 0,
            ended_epoch_ms: Some(100),
            event_count: u64::try_from(events.len())?,
            payload_bytes: 0,
            path: dir.path.to_string_lossy().into_owned(),
            segments: vec!["events_0.bin".into()],
            total_segment_bytes: 0,
        };
        let mut segment = File::create(dir.path.join("events_0.bin"))?;
        for event in events {
            write_frame(&mut segment, event).map_err(|error| anyhow::anyhow!(error.to_string()))?;
        }
        summary.total_segment_bytes = segment.metadata()?.len();
        fs::write(
            dir.path.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({"summary": summary}))?,
        )?;
        Ok((dir, summary))
    }

    #[test]
    fn whole_event_loss_and_manifest_mismatches_never_publish() -> Result<()> {
        let session = Uuid::new_v4();
        let pane = Uuid::new_v4();
        let events = [
            event(1, session, pane, b"FIRST"),
            event(2, session, pane, b"LAST"),
        ];
        for corruption in 0..3 {
            let (source, mut summary) = fixture(&events)?;
            match corruption {
                0 => {
                    let mut file = File::create(source.path.join("events_0.bin"))?;
                    write_frame(&mut file, &events[0])
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                }
                1 => summary.event_count += 1,
                _ => summary.total_segment_bytes += 1,
            }
            fs::write(
                source.path.join("manifest.json"),
                serde_json::to_vec(&serde_json::json!({"summary":summary}))?,
            )?;
            let output = source.path.join("rejected");
            assert!(
                export_process_artifacts(
                    &source.path,
                    &output,
                    &ProcessArtifactOptions::default(),
                    None
                )
                .is_err()
            );
            assert!(!output.exists());
        }
        Ok(())
    }

    #[test]
    fn exact_bytes_and_binary_json_metadata() -> Result<()> {
        let session = Uuid::new_v4();
        let pane = Uuid::new_v4();
        let bytes = b"\0\xff\x1b[31mhi\r\n";
        let expected = event(7, session, pane, bytes);
        let (source, _) = fixture(std::slice::from_ref(&expected))?;
        let output = source.path.join("artifacts");
        let report = export_process_artifacts(
            &source.path,
            &output,
            &ProcessArtifactOptions::default(),
            Some(b"authoritative screen"),
        )?;
        assert_eq!(report.selected_events, 1);
        assert_eq!(report.output_bytes, u64::try_from(bytes.len())?);
        assert_eq!(fs::read(output.join("output.pty"))?, bytes);
        assert_eq!(
            fs::read(output.join("screen.txt"))?,
            b"authoritative screen"
        );
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("events.jsonl"))?)?;
        assert_eq!(
            json["payload"]["bytes"]["data"],
            serde_json::json!([0, 255, 27, 91, 51, 49, 109, 104, 105, 13, 10])
        );
        assert_eq!(json["seq"], 7);
        assert_eq!(json["mono_ns"], 7000);
        assert_eq!(json["wall_epoch_ms"], 70);
        let actual: ProcessRecordingEvent = serde_json::from_value(json)?;
        assert_eq!(actual, expected);
        assert!(
            export_process_artifacts(
                &source.path,
                &output,
                &ProcessArtifactOptions::default(),
                None
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn mixed_panes_and_session_scope() -> Result<()> {
        let session = Uuid::new_v4();
        let pane = Uuid::new_v4();
        let other = Uuid::new_v4();
        let data = [
            event(1, session, pane, b"chosen"),
            event(2, session, other, b"other\n"),
            event(3, Uuid::new_v4(), pane, b"wrong session"),
        ];
        let (source, _) = fixture(&data)?;
        let options = ProcessArtifactOptions {
            scope: ProcessArtifactScope {
                session: Some(session),
                pane: Some(pane),
                execution: None,
            },
            ..ProcessArtifactOptions::default()
        };
        let output = source.path.join("selected");
        let report = export_process_artifacts(&source.path, &output, &options, None)?;
        assert_eq!(report.selected_events, 1);
        assert_eq!(fs::read(output.join("output.pty"))?, b"chosen");
        assert_eq!(fs::read(output.join("output.txt"))?, b"chosen");
        let mut transcripts = [Transcript::new(100), Transcript::new(100)];
        let mut text = Vec::new();
        transcripts[0].feed(b"first\x1b[", &mut text)?;
        transcripts[1].feed(b"second\n", &mut text)?;
        transcripts[0].feed(b"31m!\n", &mut text)?;
        assert_eq!(text, b"second\nfirst!\n");
        Ok(())
    }

    #[test]
    fn split_ansi_unicode_cr_backspace_and_invalid_utf8() -> Result<()> {
        let mut transcript = Transcript::new(100);
        let mut output = Vec::new();
        for bytes in [
            b"\x1b[3".as_slice(),
            b"1mabc\rZ\x08Q\n\x1b]title",
            b"\x1b\\\xe2",
            b"\x82\xac\n\xff\xc3",
            b"\xa9\xc2",
        ] {
            transcript.feed(bytes, &mut output)?;
        }
        transcript.finish(&mut output)?;
        assert_eq!(String::from_utf8(output)?, "Qbc\n€\n�é�");
        Ok(())
    }

    #[test]
    fn rejects_invalid_truncated_and_oversized_frames() -> Result<()> {
        assert!(read_event(&mut Cursor::new(vec![1, 0]), 16).is_err());
        assert!(read_event(&mut Cursor::new(vec![4, 0, 0, 0, 1]), 16).is_err());
        assert!(read_event(&mut Cursor::new(vec![1, 0, 0, 0, 255]), 16).is_err());
        assert!(read_event(&mut Cursor::new(u32::MAX.to_le_bytes()), 16).is_err());
        assert!(read_event(&mut Cursor::new([0; 4]), 16).is_err());
        assert!(read_event(&mut Cursor::new(Vec::<u8>::new()), 16)?.is_none());
        Ok(())
    }

    #[test]
    fn truncated_frames_never_publish_artifacts_or_change_manifest() -> Result<()> {
        use bmux_recording_protocol::{DisplayTrackEnvelope, DisplayTrackEvent};

        let (source, _) = fixture(&[event(1, Uuid::new_v4(), Uuid::new_v4(), b"exact bytes")])?;
        let manifest = fs::read(source.path.join("manifest.json"))?;
        let segment_path = source.path.join("events_0.bin");
        let canonical = fs::read(&segment_path)?;
        let output = source.path.join("artifacts");
        // A complete raw frame followed by an incomplete prefix or payload must
        // fail publication even though some selected bytes were staged already.
        for trailing in [vec![1, 0], vec![4, 0, 0, 0, 1]] {
            let mut truncated = canonical.clone();
            truncated.extend_from_slice(&trailing);
            fs::write(&segment_path, truncated)?;
            assert!(
                export_process_artifacts(
                    &source.path,
                    &output,
                    &ProcessArtifactOptions::default(),
                    None
                )
                .is_err()
            );
            assert!(!output.exists());
            assert_eq!(fs::read(source.path.join("manifest.json"))?, manifest);
        }
        fs::write(&segment_path, &canonical)?;
        let track_path = source.path.join(format!("display-{}.bin", Uuid::new_v4()));
        let display = DisplayTrackEnvelope {
            mono_ns: 1,
            event: DisplayTrackEvent::Resize { cols: 80, rows: 24 },
        };
        let mut frame = Vec::new();
        write_frame(&mut frame, &display).map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let options = ProcessArtifactOptions {
            gif_fps: Some(10),
            ..ProcessArtifactOptions::default()
        };
        for partial in [&frame[..2], &frame[..frame.len() - 1]] {
            let mut truncated = frame.clone();
            truncated.extend_from_slice(partial);
            fs::write(&track_path, truncated)?;
            let error = export_process_artifacts(&source.path, &output, &options, None)
                .expect_err("incomplete display track must reject the entire artifact export");
            assert!(error.to_string().contains("refusing partial export"));
            assert!(!output.exists());
            assert_eq!(fs::read(&segment_path)?, canonical);
            assert_eq!(fs::read(source.path.join("manifest.json"))?, manifest);
        }
        Ok(())
    }

    #[test]
    fn limits_and_bad_version_do_not_publish() -> Result<()> {
        let (source, mut summary) =
            fixture(&[event(1, Uuid::new_v4(), Uuid::new_v4(), b"long output")])?;
        let output = source.path.join("output");
        for options in [
            ProcessArtifactOptions {
                max_frame_bytes: 1,
                ..ProcessArtifactOptions::default()
            },
            ProcessArtifactOptions {
                max_line_chars: 1,
                ..ProcessArtifactOptions::default()
            },
            ProcessArtifactOptions {
                max_artifact_bytes: 1,
                ..ProcessArtifactOptions::default()
            },
        ] {
            assert!(export_process_artifacts(&source.path, &output, &options, None).is_err());
            assert!(!output.exists());
        }
        summary.format_version = RECORDING_FORMAT_VERSION + 1;
        fs::write(
            source.path.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({"summary": summary}))?,
        )?;
        assert!(
            export_process_artifacts(
                &source.path,
                &output,
                &ProcessArtifactOptions::default(),
                None
            )
            .is_err()
        );
        assert!(!output.exists());
        Ok(())
    }

    #[test]
    fn pane_event_and_string_state_are_bounded() -> Result<()> {
        let session = Uuid::new_v4();
        let (source, _) = fixture(&[
            event(1, session, Uuid::new_v4(), b"a"),
            event(2, session, Uuid::new_v4(), b"b"),
        ])?;
        let output = source.path.join("bounded");
        for options in [
            ProcessArtifactOptions {
                max_events: 1,
                ..ProcessArtifactOptions::default()
            },
            ProcessArtifactOptions {
                max_panes: 1,
                ..ProcessArtifactOptions::default()
            },
        ] {
            assert!(export_process_artifacts(&source.path, &output, &options, None).is_err());
            assert!(!output.exists());
        }
        let mut transcript = Transcript::new(1);
        let mut text = Vec::new();
        transcript.feed(b"\x1b]", &mut text)?;
        for _ in 0..1000 {
            transcript.feed(b"ignored string payload", &mut text)?;
        }
        transcript.feed(b"\x07x", &mut text)?;
        assert_eq!(transcript.line.len(), 1);
        assert!(transcript.utf8.is_empty());
        transcript.finish(&mut text)?;
        assert_eq!(text, b"x");
        Ok(())
    }

    #[test]
    fn unsafe_paths_execution_scope_and_scoped_gif_are_rejected() {
        for path in ["../events.bin", "/events.bin", "nested/events.bin", "", "."] {
            assert!(validate_segment(path).is_err());
        }
        let mut options = ProcessArtifactOptions::default();
        options.scope.execution = Some(Uuid::new_v4());
        assert!(validate_options(&options).is_err());
        options.scope.execution = None;
        options.scope.pane = Some(Uuid::new_v4());
        options.gif_fps = Some(10);
        assert!(validate_options(&options).is_err());
    }
}
