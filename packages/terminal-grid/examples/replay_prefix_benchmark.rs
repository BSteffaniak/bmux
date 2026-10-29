//! Bounded shared-parser benchmark; no PTY, native renderer, or runtime needed.
//!
//! Run an optimized build of this example on both revisions with the same toolchain.
//! Fixture/stream construction, correctness checks, and CSV output are outside timing.
//! `bytes` and `process_calls` are exact boundary work counts, NOT allocation counts.
//! Prefix-buffer reuse/allocation avoidance is checked by unit tests in parser.rs;
//! this example deliberately makes no allocator-wide or copy-volume claims.

use std::hint::black_box;
use std::time::Instant;

use bmux_terminal_grid::{GridLimits, TerminalGridStream, TerminalProtocolTracker};

const WIDTH: u16 = 80;
const HEIGHT: u16 = 24;
const FIXTURE_BYTES: usize = 128 * 1024;
const SAMPLES: usize = 3;

fn fixture(seed: &[u8]) -> Vec<u8> {
    let mut bytes = seed.repeat(FIXTURE_BYTES / seed.len());
    // Leave a prefix for snapshot/hydration checks, not just completed sequences.
    bytes.extend_from_slice(b"\x1b[3");
    bytes
}

fn benchmark(name: &str, bytes: &[u8]) {
    let limits = GridLimits { scrollback_rows: 0 };
    let mut reference = TerminalGridStream::new(WIDTH, HEIGHT, limits).unwrap();
    reference.process(bytes);
    let expected = reference.snapshot(0, usize::from(HEIGHT));
    let mut protocol_reference = TerminalProtocolTracker::new();
    protocol_reference.process(bytes);

    for chunk_size in [1, 32, 8192] {
        // One untimed warmup followed by a fixed number of fresh-stream samples.
        for sample in 0..=SAMPLES {
            let mut stream = TerminalGridStream::new(WIDTH, HEIGHT, limits).unwrap();
            let started = Instant::now();
            for chunk in bytes.chunks(chunk_size) {
                stream.process(black_box(chunk));
            }
            let elapsed = started.elapsed().as_nanos();
            let mut actual = stream.snapshot(0, usize::from(HEIGHT));
            // Prefix-only chunks legitimately add replication revisions.
            actual.revision = expected.revision;
            assert_eq!(actual, expected);
            let mut hydrated =
                TerminalGridStream::from_snapshot(&stream.snapshot(0, usize::from(HEIGHT)), limits)
                    .unwrap();
            stream.process(b"1m!");
            hydrated.process(b"1m!");
            assert_eq!(
                stream.snapshot(0, usize::from(HEIGHT)),
                hydrated.snapshot(0, usize::from(HEIGHT))
            );
            if sample != 0 {
                println!(
                    "grid,{name},{chunk_size},{sample},{},{},{elapsed}",
                    bytes.len(),
                    bytes.len().div_ceil(chunk_size)
                );
            }

            let mut tracker = TerminalProtocolTracker::new();
            let started = Instant::now();
            for chunk in bytes.chunks(chunk_size) {
                black_box(tracker.process(black_box(chunk)));
            }
            let elapsed = started.elapsed().as_nanos();
            assert_eq!(
                tracker.protocol_state(),
                protocol_reference.protocol_state()
            );
            assert_eq!(
                tracker.alternate_screen(),
                protocol_reference.alternate_screen()
            );
            if sample != 0 {
                println!(
                    "protocol,{name},{chunk_size},{sample},{},{},{elapsed}",
                    bytes.len(),
                    bytes.len().div_ceil(chunk_size)
                );
            }
        }
    }
}

fn main() {
    println!("parser,fixture,chunk_bytes,sample,bytes,process_calls,elapsed_ns");
    benchmark(
        "plain",
        &fixture(b"ordinary terminal output 0123456789\r\n"),
    );
    benchmark(
        "mixed",
        &fixture("\x1b[31m界é\x1b[0m\r\n\x1b[?1000h\x1b[?1000l".as_bytes()),
    );
}
