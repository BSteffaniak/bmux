//! BMUX-authored deterministic terminal workloads. Bytes and protocol acknowledgements
//! are not evidence that a terminal has visibly presented a frame.
use crate::model::{BenchResult, CaseSpec, Geometry, Workload};
use sha2::{Digest, Sha256};

pub(crate) const MIN_BYTES: usize = 4096;
pub(crate) const MAX_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const SUITES: &[&str] = &["realistic", "throughput", "unicode", "tui", "edge"];
const MAX_OPERATIONS: usize = 4096;
// Fixed pacing duration, independent of throughput-oriented byte budgets.
const PACED_OPERATIONS: usize = 16;
const MAX_CHUNKS: usize = 16_384;
// Charge the 64-bit Vec layout on all supported targets so admission and
// workload hashes do not vary with 32-bit versus 64-bit host metadata size.
const CHUNK_OVERHEAD: usize = 24;

type Definition = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    bool,
);
const DEFINITIONS: [Definition; 16] = [
    (
        "plain-logs",
        "throughput",
        "Plain logs",
        "Timestamped ASCII application logs isolate scrolling and text ingestion.",
        "Inspect parser fast paths, scrollback retention, and output backpressure.",
        "realistic",
        false,
    ),
    (
        "ansi-build",
        "throughput",
        "ANSI build output",
        "BMUX-authored compiler-style messages mix SGR changes with scrolling.",
        "Inspect SGR parsing, styled runs, and scrolling damage.",
        "realistic",
        false,
    ),
    (
        "color-flood",
        "throughput",
        "Color style flood",
        "RC-inspired, independently authored truecolor style churn stresses dense attributes.",
        "Inspect attribute interning, escape parsing, and render batching.",
        "throughput",
        false,
    ),
    (
        "progress",
        "realistic",
        "Progress replacement",
        "Carriage-return and erase-line updates replace a bounded progress display.",
        "Inspect line invalidation and coalescing of repeated updates.",
        "realistic",
        false,
    ),
    (
        "tui-sparse",
        "tui",
        "Sparse TUI",
        "A few positioned cells change without repainting the whole screen.",
        "Inspect sparse damage tracking and retained compositor overhead.",
        "realistic",
        false,
    ),
    (
        "tui-dense",
        "tui",
        "Dense TUI",
        "Each operation repaints every row of a synthetic dashboard.",
        "Inspect full-frame damage, cell diffing, and presentation batching.",
        "tui",
        false,
    ),
    (
        "scroll-region",
        "tui",
        "Scroll regions",
        "A bounded interior region scrolls while outer rows remain retained.",
        "Inspect region scrolling, coordinate transforms, and damage boundaries.",
        "edge",
        false,
    ),
    (
        "unicode-combining",
        "unicode",
        "Combining text",
        "Decomposed accents exercise grapheme assembly independently of ASCII throughput.",
        "Inspect grapheme storage and combining-mark invalidation.",
        "unicode",
        false,
    ),
    (
        "unicode-cjk",
        "unicode",
        "CJK wrapping",
        "Wide CJK text crosses the configured right margin.",
        "Inspect display-cell widths, wrap state, and wide-cell continuation.",
        "unicode",
        false,
    ),
    (
        "unicode-emoji",
        "unicode",
        "Emoji wrapping",
        "Emoji variation and joined sequences cross the configured right margin.",
        "Inspect grapheme segmentation and terminal-specific width policy; no visual correctness is inferred.",
        "unicode",
        false,
    ),
    (
        "writes-small",
        "throughput",
        "Small writes",
        "The same log stream as writes-large is delivered in small complete records.",
        "Compare against writes-large to isolate syscall and queue overhead.",
        "edge",
        false,
    ),
    (
        "writes-large",
        "throughput",
        "Large writes",
        "The same log stream as writes-small is batched into larger writes.",
        "Compare against writes-small to isolate batching from parsing cost.",
        "throughput",
        false,
    ),
    (
        "split-sequences",
        "edge",
        "Split UTF-8 and escapes",
        "Intentional byte boundaries interrupt both UTF-8 and CSI sequences.",
        "Inspect incremental parser state across transport reads.",
        "edge",
        false,
    ),
    (
        "paced-updates",
        "realistic",
        "Paced updates",
        "Sparse progress updates comprise 16 frames separated by explicit 2 ms pacing; byte budgets never increase frame count.",
        "Inspect scheduling and coalescing; elapsed time includes intentional pacing, not visible-frame latency.",
        "edge",
        false,
    ),
    (
        "frames-unsynchronized",
        "tui",
        "Unbatched frame control",
        "Control member of the opt-in synchronized-update pair.",
        "Compare only with frames-synchronized under the same geometry and budget.",
        "tui",
        true,
    ),
    (
        "frames-synchronized",
        "tui",
        "Synchronized frame control",
        "The paired dashboard uses explicit synchronized-update begin/end delimiters.",
        "Investigate synchronized-update handling; accepted bytes do not prove visible atomic frames.",
        "tui",
        true,
    ),
];

/// Catalog order is stable and doubles as the canonical selection order.
#[must_use]
pub fn catalog() -> Vec<CaseSpec> {
    DEFINITIONS.into_iter().map(case_spec).collect()
}

fn case_spec(
    (id, category, title, description, diagnostic, suite, requires_sync): Definition,
) -> CaseSpec {
    let mut suites = vec![category.to_owned()];
    if category != suite {
        suites.push(suite.to_owned());
    }
    CaseSpec {
        id: id.to_owned(),
        revision: if id == "paced-updates" { 3 } else { 2 },
        category: category.to_owned(),
        title: title.to_owned(),
        description: if uses_alternate_screen(id) {
            format!(
                "{description} Uses an isolated alternate screen; run only from the main screen, never inside an existing alternate-screen application. Synchronized-update cases additionally require that mode to be inactive on entry. DECSC/DECRC preserves attributes but overwrites the terminal saved-cursor slot. Requires default origin mode and scroll margins on entry."
            )
        } else {
            format!(
                "{description} Writes on the main screen without changing screen or cursor-visibility modes. Run from the main screen with default origin mode and scroll margins. DECSC/DECRC preserves attributes but overwrites the terminal saved-cursor slot; scrolling output remains in history."
            )
        },
        diagnostic: format!(
            "{diagnostic} Investigative entry point (hypothesis, not attribution): {}.",
            if matches!(category, "unicode" | "edge") || requires_sync {
                "packages/cli/src/runtime/terminal_protocol.rs"
            } else {
                "packages/cli/src/runtime/hosted_output.rs"
            }
        ),
        suites,
        requires_sync,
    }
}

fn uses_alternate_screen(id: &str) -> bool {
    matches!(
        id,
        "tui-sparse"
            | "tui-dense"
            | "scroll-region"
            | "frames-unsynchronized"
            | "frames-synchronized"
    )
}

pub(crate) fn validate_geometry(geometry: Geometry) -> BenchResult<()> {
    if !(2..=512).contains(&geometry.columns)
        || !(2..=256).contains(&geometry.rows)
        || usize::from(geometry.columns) * usize::from(geometry.rows) > 65_536
    {
        return Err("geometry must be 2..512 columns, 2..256 rows, and at most 65536 cells".into());
    }
    Ok(())
}

/// Generate complete logical operations only. Budget includes owned byte buffers
/// and chunk-vector metadata, not merely the bytes written to the terminal.
/// Budget must be 4096..16777216 bytes; large geometries may require more.
/// Callers must satisfy the screen/mode restrictions in the case description.
/// There is no truncation of setup, teardown, UTF-8, or logical CSI operations
/// (except the intentionally fragmented transport chunks of split-sequences).
pub fn generate(
    id: &str,
    geometry: Geometry,
    seed: u64,
    max_bytes: usize,
) -> BenchResult<Workload> {
    validate_geometry(geometry)?;
    if !(MIN_BYTES..=MAX_BYTES).contains(&max_bytes) {
        return Err("workload budget must be 4096..16777216 bytes".into());
    }
    let definition = DEFINITIONS
        .iter()
        .find(|definition| definition.0 == id)
        .ok_or_else(|| format!("unknown case: {id}"))?;
    // Reserve fixed space for the one selected specification, hash state and
    // result metadata; never materialize the complete catalog during generation.
    const FIXED_OVERHEAD: usize = 2048;
    if max_bytes < FIXED_OVERHEAD {
        return Err("workload budget must cover fixed metadata".into());
    }
    let spec = case_spec(*definition);
    let alternate = uses_alternate_screen(id);
    // Save rendition and cursor on the caller's screen. No visibility toggles:
    // the benchmark never owns the caller's cursor-visibility mode.
    let mut setup = b"\x1b7".to_vec();
    if alternate {
        setup.extend_from_slice(b"\x1b[?1049h\x1b[2J\x1b[H");
    }
    setup.extend_from_slice(b"\x1b[0m");
    let mut teardown = Vec::new();
    if id == "frames-synchronized" {
        teardown.extend_from_slice(b"\x1b[?2026l");
    }
    if id == "scroll-region" {
        teardown.extend_from_slice(b"\x1b[r");
    }
    if alternate {
        teardown.extend_from_slice(b"\x1b[?1049l");
    }
    teardown.extend_from_slice(b"\x1b8");
    let setup = setup.into_boxed_slice().into_vec();
    let teardown = teardown.into_boxed_slice().into_vec();
    let interval_ns = if id == "paced-updates" { 2_000_000 } else { 0 };
    let mut byte_count = setup.len() + teardown.len() + FIXED_OVERHEAD;
    // Budget both members of the frame pair as the larger synchronized form,
    // so an equal budget admits the same number of complete frames.
    if id == "frames-unsynchronized" {
        byte_count += b"\x1b[?2026l".len();
    }
    let mut chunk_count = 0;
    let mut logical_operations = 0;
    // Reserve room for String growth, formatting temporaries, and conversion
    // to exact-sized byte buffers. Planning allocates no retained chunks.
    let scratch = operation_bound(id, geometry) * 4;
    if byte_count + scratch > max_bytes {
        return Err(format!("budget too small for {id} generation scratch"));
    }
    let operation_limit = if id == "paced-updates" {
        PACED_OPERATIONS
    } else {
        MAX_OPERATIONS
    };
    for operation in 0..operation_limit {
        let text = operation_text(id, geometry, seed, operation);
        let count = match id {
            "split-sequences" => text.len(),
            "writes-small" | "writes-large" => 16,
            _ => 1,
        };
        let pair_overhead = if id == "frames-unsynchronized" { 16 } else { 0 };
        let next_bytes = byte_count + text.len() + pair_overhead;
        let next_chunks = chunk_count + count;
        if next_bytes + next_chunks * CHUNK_OVERHEAD + scratch > max_bytes
            || next_chunks > MAX_CHUNKS
        {
            break;
        }
        byte_count = next_bytes;
        chunk_count = next_chunks;
        logical_operations += 1;
    }
    if logical_operations == 0 {
        return Err(format!("budget too small for one complete {id} operation"));
    }
    // Plan first, allocate exactly once: geometric Vec growth must not escape
    // the memory budget, particularly for intentionally one-byte writes.
    let mut chunks = Vec::with_capacity(chunk_count);
    for operation in 0..logical_operations {
        let text = operation_text(id, geometry, seed, operation);
        if id == "split-sequences" {
            chunks.extend(text.bytes().map(|byte| vec![byte]));
        } else if id == "writes-small" {
            chunks.extend(
                text.split_inclusive('\n')
                    .map(|line| line.as_bytes().to_vec()),
            );
        } else {
            chunks.push(text.into_boxed_str().into_string().into_bytes());
        }
    }
    let logical_operations = u64::try_from(logical_operations).expect("bounded operations");
    let mut workload = Workload {
        spec,
        setup,
        chunks,
        teardown,
        interval_ns,
        logical_operations,
        payload_hash: String::new(),
    };
    workload.payload_hash = payload_hash(&workload, geometry, seed);
    Ok(workload)
}

// Conservative upper bounds include formatting overhead, but never depend on
// an untrusted string or an unbounded operation count.
fn operation_bound(id: &str, geometry: Geometry) -> usize {
    match id {
        "tui-dense" | "frames-unsynchronized" | "frames-synchronized" => {
            usize::from(geometry.rows) * (usize::from(geometry.columns) + 24) + 32
        }
        "unicode-cjk" | "unicode-emoji" => 24 * (usize::from(geometry.columns) / 2 + 1) + 32,
        "writes-small" | "writes-large" => 1024,
        _ => 256,
    }
}

pub(crate) fn minimum_budget(id: &str, geometry: Geometry) -> usize {
    let operation = operation_bound(id, geometry);
    let chunks = match id {
        "split-sequences" => operation,
        "writes-small" | "writes-large" => 16,
        _ => 1,
    };
    (2048 + 128 + operation * 5 + chunks * CHUNK_OVERHEAD).max(MIN_BYTES)
}

fn operation_text(id: &str, geometry: Geometry, seed: u64, operation: usize) -> String {
    let value = seed.wrapping_add(
        u64::try_from(operation)
            .expect("bounded operation")
            .wrapping_mul(0x9e37_79b9_7f4a_7c15),
    );
    let row = operation % usize::from(geometry.rows) + 1;
    let column = operation % usize::from(geometry.columns) + 1;
    match id {
        "plain-logs" => format!(
            "[{:06}] INFO bmux job={:08x} completed local task\r\n",
            operation,
            value & 0xffff_ffff
        ),
        "ansi-build" => format!(
            "\x1b[1;32m Compiling\x1b[0m bmux_unit_{:04x}\r\n\x1b[33mwarning:\x1b[0m authored diagnostic {operation}\r\n",
            value & 0xffff
        ),
        "color-flood" => format!(
            "\x1b[38;2;{};{};{}m\x1b[48;2;0;16;32mBMUX {:016x}\x1b[0m\r\n",
            value & 255,
            (value >> 8) & 255,
            (value >> 16) & 255,
            value
        ),
        "progress" | "paced-updates" => format!(
            "\r\x1b[2Kbmux task {:04x}: {:3}%",
            value & 0xffff,
            operation % 101
        ),
        "tui-sparse" => format!("\x1b[{row};{column}H\x1b[32m{:x}\x1b[0m", value & 15),
        "tui-dense" | "frames-unsynchronized" | "frames-synchronized" => {
            let mut frame = String::new();
            if id == "frames-synchronized" {
                frame.push_str("\x1b[?2026h");
            }
            for y in 1..=geometry.rows {
                frame.push_str(&format!(
                    "\x1b[{y};1H\x1b[{}m",
                    31 + (operation + usize::from(y)) % 6
                ));
                let symbol = char::from(b'a' + u8::try_from(value % 26).expect("ASCII alphabet"));
                frame.extend(std::iter::repeat_n(symbol, usize::from(geometry.columns)));
            }
            frame.push_str("\x1b[0m");
            if id == "frames-synchronized" {
                frame.push_str("\x1b[?2026l");
            }
            frame
        }
        "scroll-region" => {
            let bottom = (geometry.rows - 1).max(2);
            format!(
                "\x1b[1;{bottom}r\x1b[{bottom};1Hrow {:08x}\r\n\x1b[r",
                value & 0xffff_ffff
            )
        }
        "unicode-combining" => {
            format!("{operation:04} cafe\u{301} nai\u{308}ve A\u{30a} o\u{302}\u{301}\r\n")
        }
        "unicode-cjk" | "unicode-emoji" => {
            let token = if id == "unicode-cjk" {
                "界語文"
            } else {
                "👩‍💻☀️🚀"
            };
            format!(
                "{operation:04} {}\r\n",
                token.repeat(usize::from(geometry.columns) / 2 + 1)
            )
        }
        "writes-small" | "writes-large" => (0..16)
            .map(|offset| {
                small_record(
                    seed.wrapping_add(
                        u64::try_from(operation * 16 + offset)
                            .expect("bounded operation")
                            .wrapping_mul(0x9e37_79b9_7f4a_7c15),
                    ),
                    operation * 16 + offset,
                )
            })
            .collect(),
        "split-sequences" => format!("\x1b[32m界e\u{301}🚀{operation:04}\x1b[0m\r\n"),
        _ => unreachable!("catalog is checked before generation"),
    }
}

fn small_record(value: u64, operation: usize) -> String {
    format!("log {operation:06} {:016x}\r\n", value)
}

fn hash_bytes(hash: &mut Sha256, bytes: &[u8]) {
    hash.update(
        u64::try_from(bytes.len())
            .expect("bounded buffer")
            .to_le_bytes(),
    );
    hash.update(bytes);
}

fn payload_hash(workload: &Workload, geometry: Geometry, seed: u64) -> String {
    let mut hash = Sha256::new();
    hash_bytes(&mut hash, b"bmux-workload-v1");
    hash_bytes(&mut hash, workload.spec.id.as_bytes());
    hash.update(workload.spec.revision.to_le_bytes());
    hash.update(geometry.columns.to_le_bytes());
    hash.update(geometry.rows.to_le_bytes());
    hash.update(seed.to_le_bytes());
    hash.update(workload.interval_ns.to_le_bytes());
    hash.update(workload.logical_operations.to_le_bytes());
    hash_bytes(&mut hash, &workload.setup);
    hash.update(
        u64::try_from(workload.chunks.len())
            .expect("bounded chunks")
            .to_le_bytes(),
    );
    for chunk in &workload.chunks {
        hash_bytes(&mut hash, chunk);
    }
    hash_bytes(&mut hash, &workload.teardown);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = hash.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn catalog_is_unique_and_explanatory() {
        let cases = catalog();
        assert_eq!(cases.len(), 16);
        assert_eq!(
            cases
                .iter()
                .map(|case| &case.id)
                .collect::<BTreeSet<_>>()
                .len(),
            cases.len()
        );
        for case in cases {
            assert!(!case.description.is_empty() && !case.diagnostic.is_empty());
            assert!(
                case.suites
                    .iter()
                    .all(|suite| SUITES.contains(&suite.as_str()))
            );
        }
    }

    #[test]
    fn every_case_is_deterministic_bounded_and_restorable() {
        for case in catalog() {
            let a = generate(&case.id, Geometry::default(), 42, 32_768).unwrap();
            let b = generate(&case.id, Geometry::default(), 42, 32_768).unwrap();
            assert_eq!(a.payload_hash, b.payload_hash);
            assert_eq!(a.chunks, b.chunks);
            assert_eq!(a.payload_hash.len(), 64);
            let retained = a.setup.capacity()
                + a.teardown.capacity()
                + a.chunks.capacity() * size_of::<Vec<u8>>()
                + a.chunks.iter().map(Vec::capacity).sum::<usize>();
            assert!(retained <= 32_768, "{} uses {retained}", case.id);
            assert!(a.teardown.ends_with(b"\x1b8"));
            assert!(a.logical_operations > 0);
        }
    }

    #[test]
    fn revision_two_payload_has_a_golden_identity() {
        let workload = generate("plain-logs", Geometry::default(), 0, 4096).unwrap();
        assert_eq!(workload.spec.revision, 2);
        assert_eq!(workload.logical_operations, 13);
        assert_eq!(
            workload.payload_hash,
            "3847e6d919383ecc1b291fd0f20fa0c7c6b3c6d4f094304fe28f98a0bd9ee81a"
        );
    }

    #[test]
    fn screen_and_cursor_modes_have_explicit_ownership() {
        for case in catalog() {
            let workload = generate(&case.id, Geometry::default(), 0, 32_768).unwrap();
            let bytes = [
                workload.setup.clone(),
                workload.chunks.concat(),
                workload.teardown.clone(),
            ]
            .concat();
            for forbidden in [b"\x1b[?25l".as_slice(), b"\x1b[?25h".as_slice()] {
                assert!(
                    !bytes
                        .windows(forbidden.len())
                        .any(|window| window == forbidden),
                    "{}",
                    case.id
                );
            }
            assert!(workload.setup.starts_with(b"\x1b7"));
            assert!(workload.teardown.ends_with(b"\x1b8"));
            if uses_alternate_screen(&case.id) {
                assert!(
                    workload
                        .setup
                        .windows(8)
                        .any(|window| window == b"\x1b[?1049h")
                );
                assert!(
                    workload
                        .teardown
                        .windows(8)
                        .any(|window| window == b"\x1b[?1049l")
                );
                assert!(
                    case.description
                        .contains("never inside an existing alternate-screen application")
                );
            } else {
                assert!(!bytes.windows(6).any(|window| window == b"\x1b[?104"));
                assert_eq!(workload.setup, b"\x1b7\x1b[0m");
                assert_eq!(workload.teardown, b"\x1b8");
            }
            if case.id != "scroll-region" {
                assert!(!bytes.windows(3).any(|window| window == b"\x1b[r"));
            }
        }
    }

    #[test]
    fn paced_frames_are_fixed_across_byte_budgets() {
        let baseline = generate("paced-updates", Geometry::default(), 7, MIN_BYTES).unwrap();
        assert_eq!(baseline.spec.revision, 3);
        assert_eq!(baseline.chunks.len(), PACED_OPERATIONS);
        assert_eq!(baseline.logical_operations, 16);
        assert_eq!(baseline.interval_ns, 2_000_000);
        for budget in [65_536, 262_144, 1_048_576, MAX_BYTES] {
            let workload = generate("paced-updates", Geometry::default(), 7, budget).unwrap();
            assert_eq!(workload.chunks, baseline.chunks);
            assert_eq!(workload.payload_hash, baseline.payload_hash);
        }
    }

    #[test]
    fn generation_rejects_budgets_below_documented_minimum() {
        for budget in [0, 256, 2048, MIN_BYTES - 1] {
            assert!(generate("plain-logs", Geometry::default(), 0, budget).is_err());
        }
        assert!(generate("plain-logs", Geometry::default(), 0, MIN_BYTES).is_ok());
    }

    #[test]
    fn malformed_inputs_reject() {
        assert!(generate("missing", Geometry::default(), 0, 4096).is_err());
        assert!(
            generate(
                "plain-logs",
                Geometry {
                    columns: 0,
                    rows: 24
                },
                0,
                4096
            )
            .is_err()
        );
        assert!(generate("plain-logs", Geometry::default(), 0, MAX_BYTES + 1).is_err());
        assert!(generate("tui-dense", Geometry::default(), 0, 256).is_err());
    }

    #[test]
    fn framing_pacing_and_seed_participate_in_identity() {
        let original = generate("plain-logs", Geometry::default(), 0, 4096).unwrap();
        let mut changed = original.clone();
        changed.interval_ns = 1;
        assert_ne!(
            payload_hash(&changed, Geometry::default(), 0),
            original.payload_hash
        );
        changed = original.clone();
        let first = changed.chunks.remove(0);
        changed.chunks.insert(0, first[1..].to_vec());
        changed.chunks.insert(0, first[..1].to_vec());
        assert_ne!(
            payload_hash(&changed, Geometry::default(), 0),
            original.payload_hash
        );
        assert_ne!(
            generate("plain-logs", Geometry::default(), 1, 4096)
                .unwrap()
                .payload_hash,
            original.payload_hash
        );
    }

    #[test]
    fn intentional_split_chunks_reassemble_valid_utf8_and_escapes() {
        let workload = generate("split-sequences", Geometry::default(), 0, 4096).unwrap();
        assert!(workload.chunks.iter().all(|chunk| chunk.len() == 1));
        let bytes = workload.chunks.concat();
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .contains("界e\u{301}🚀")
        );
        assert!(bytes.ends_with(b"\x1b[0m\r\n"));
    }

    #[test]
    fn sync_pair_has_identical_frame_count_and_content() {
        for budget in [16_384, 32_768, 65_536] {
            let plain = generate("frames-unsynchronized", Geometry::default(), 7, budget).unwrap();
            let sync = generate("frames-synchronized", Geometry::default(), 7, budget).unwrap();
            assert_eq!(plain.logical_operations, sync.logical_operations);
            for (left, right) in plain.chunks.iter().zip(&sync.chunks) {
                assert_eq!(left, &right[8..right.len() - 8]);
            }
        }
    }

    #[test]
    fn operation_bounds_cover_supported_geometries() {
        for geometry in [
            Geometry {
                columns: 2,
                rows: 2,
            },
            Geometry::default(),
            Geometry {
                columns: 512,
                rows: 128,
            },
            Geometry {
                columns: 256,
                rows: 256,
            },
        ] {
            for case in catalog() {
                for operation in [0, 4095] {
                    let text = operation_text(&case.id, geometry, u64::MAX, operation);
                    assert!(
                        text.len() <= operation_bound(&case.id, geometry),
                        "{}",
                        case.id
                    );
                }
            }
        }
        let region = generate(
            "scroll-region",
            Geometry {
                columns: 2,
                rows: 2,
            },
            0,
            4096,
        )
        .unwrap();
        assert!(region.chunks[0].starts_with(b"\x1b[1;2r"));
    }

    #[test]
    fn write_batch_cases_share_exact_stream() {
        let small = generate("writes-small", Geometry::default(), 7, 32_768)
            .unwrap()
            .chunks
            .concat();
        let large = generate("writes-large", Geometry::default(), 7, 32_768)
            .unwrap()
            .chunks
            .concat();
        assert_eq!(small, large);
    }
}
