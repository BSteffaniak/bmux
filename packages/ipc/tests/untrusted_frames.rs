//! Fuzz-style coverage of IPC frame decoding. A remote stream's first bytes (its `Hello`)
//! and every later request reach these decoders from a paired device, so arbitrary and
//! mutated input must never panic, and no length or size read from the input may size an
//! allocation beyond the protocol's limits.
//!
//! A counting global allocator records the largest single allocation made by the test's
//! own thread while a decode runs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use bmux_ipc::frame::{
    MAX_COMPRESSED_FRAME_SIZE, MAX_FRAME_PAYLOAD_SIZE, decode_frame_compressed, decode_frame_exact,
    encode_frame, try_decode_frame, try_decode_frame_compressed,
};
use bmux_ipc::transport::ErasedIpcStream;
use bmux_ipc::{
    Envelope, EnvelopeKind, InvokeServiceKind, ProtocolContract, Request, decode,
    default_supported_capabilities, encode,
};
use proptest::prelude::*;
use uuid::Uuid;

struct Tracking;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

fn note(size: usize) {
    // `try_with`: the allocator also runs while thread-locals are torn down.
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let _ = PEAK.try_with(|peak| peak.set(peak.get().max(size)));
        }
    });
}

// SAFETY: forwards to the system allocator; the bookkeeping allocates nothing.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: same layout contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: same layout contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        // SAFETY: same pointer and layout contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same pointer and layout contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Tracking = Tracking;

/// Runs `f` and returns its result with the largest single allocation it made.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    PEAK.with(|peak| peak.set(0));
    ARMED.with(|armed| armed.set(true));
    let result = f();
    ARMED.with(|armed| armed.set(false));
    (result, PEAK.with(Cell::get))
}

/// Room for the decoders' own bookkeeping on top of the protocol limit.
const SLACK: usize = 64 * 1024;

fn assert_bounded(peak: usize, limit: usize, what: &str) {
    assert!(
        peak <= limit + SLACK,
        "{what}: allocated {peak} bytes at once (limit {limit})"
    );
}

/// Every decoder over `bytes`; none may panic or allocate past its limit.
fn decode_everything(bytes: &[u8]) {
    let ((), peak) = peak_of(|| {
        let _ = decode_frame_exact(bytes);
        let _ = try_decode_frame(&mut bytes.to_vec());
        let _ = decode::<Request>(bytes);
        let _ = decode::<Envelope>(bytes);
    });
    assert_bounded(peak, MAX_FRAME_PAYLOAD_SIZE.max(bytes.len()), "plain frame");
    let ((), peak) = peak_of(|| {
        let _ = decode_frame_compressed(bytes);
        let _ = try_decode_frame_compressed(&mut bytes.to_vec());
    });
    assert_bounded(
        peak,
        MAX_FRAME_PAYLOAD_SIZE.max(bytes.len()),
        "compressed frame",
    );
}

fn hello() -> Request {
    Request::Hello {
        contract: ProtocolContract::current(default_supported_capabilities()),
        client_name: "fuzz".into(),
        principal_id: Uuid::from_u128(7),
    }
}

fn valid_frames() -> Vec<Vec<u8>> {
    [
        hello(),
        Request::Ping,
        Request::EnableEventPush,
        Request::InvokeService {
            capability: "bmux.example".into(),
            kind: InvokeServiceKind::Command,
            interface_id: "example/v1".into(),
            operation: "run".into(),
            payload: vec![0x5a; 300],
        },
    ]
    .iter()
    .map(|request| {
        let envelope = Envelope::new(1, EnvelopeKind::Request, encode(request).unwrap());
        encode_frame(&envelope).unwrap()
    })
    .collect()
}

#[derive(Debug, Clone)]
enum Mutation {
    Flip { at: usize, bits: u8 },
    Truncate { len: usize },
    Length { value: u32 },
    Insert { at: usize, bytes: Vec<u8> },
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        (any::<usize>(), 1..=255_u8).prop_map(|(at, bits)| Mutation::Flip { at, bits }),
        any::<usize>().prop_map(|len| Mutation::Truncate { len }),
        any::<u32>().prop_map(|value| Mutation::Length { value }),
        (
            any::<usize>(),
            proptest::collection::vec(any::<u8>(), 1..16)
        )
            .prop_map(|(at, bytes)| Mutation::Insert { at, bytes }),
    ]
}

fn apply(frame: &mut Vec<u8>, mutation: &Mutation) {
    match mutation {
        Mutation::Flip { at, bits } => {
            let index = at % frame.len();
            frame[index] ^= bits;
        }
        Mutation::Truncate { len } => frame.truncate(len % (frame.len() + 1)),
        Mutation::Length { value } => frame[..4].copy_from_slice(&value.to_le_bytes()),
        Mutation::Insert { at, bytes } => {
            let index = at % (frame.len() + 1);
            frame.splice(index..index, bytes.iter().copied());
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_bytes_never_panic_or_over_allocate(
        bytes in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        decode_everything(&bytes);
    }

    #[test]
    fn mutated_valid_frames_never_panic_or_over_allocate(
        which in 0..4_usize,
        mutations in proptest::collection::vec(mutation(), 1..6),
    ) {
        let mut frame = valid_frames().swap_remove(which);
        for mutation in &mutations {
            if frame.len() < 4 {
                break;
            }
            apply(&mut frame, mutation);
        }
        decode_everything(&frame);
    }

    #[test]
    fn arbitrary_compression_payloads_are_bounded(
        id in 0..=3_u8,
        declared in any::<u32>(),
        body in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        // `[len][compression id][4-byte declared size][body]`: the lz4 layout, and noise
        // for the others.
        let mut frame = Vec::new();
        let total = u32::try_from(1 + 4 + body.len()).unwrap();
        frame.extend_from_slice(&total.to_le_bytes());
        frame.push(id);
        frame.extend_from_slice(&declared.to_le_bytes());
        frame.extend_from_slice(&body);
        decode_everything(&frame);
    }
}

#[test]
fn valid_frames_still_decode() {
    for frame in valid_frames() {
        decode_frame_exact(&frame).expect("a valid frame decodes");
    }
}

fn read_one(bytes: Vec<u8>, compressed: bool) -> Result<Envelope, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let (mut peer, ours) = tokio::io::duplex(64 * 1024);
        tokio::io::AsyncWriteExt::write_all(&mut peer, &bytes)
            .await
            .unwrap();
        drop(peer);
        let mut stream = ErasedIpcStream::new(Box::new(ours));
        if compressed {
            stream.enable_frame_decompression();
        }
        stream
            .recv_envelope()
            .await
            .map_err(|error| error.to_string())
    })
}

#[test]
fn stream_reads_refuse_oversized_lengths_before_allocating() {
    for (compressed, limit) in [
        (false, MAX_FRAME_PAYLOAD_SIZE),
        (true, MAX_COMPRESSED_FRAME_SIZE),
    ] {
        for claimed in [
            u32::MAX,
            u32::try_from(limit + 1).unwrap(),
            u32::try_from(limit * 4).unwrap(),
        ] {
            // A header claiming `claimed` bytes, then a few bytes and EOF.
            let mut bytes = claimed.to_le_bytes().to_vec();
            bytes.extend_from_slice(&[0; 16]);
            let (result, peak) = peak_of(|| read_one(bytes, compressed));
            let error = result.expect_err("an oversized frame is refused");
            assert!(
                error.contains("exceeds max size"),
                "refused for the size, not by EOF: {error}"
            );
            assert_bounded(peak, 64 * 1024, "stream read of an oversized header");
        }
    }
}

#[test]
fn stream_reads_accept_valid_frames() {
    for frame in valid_frames() {
        read_one(frame, false).expect("a valid frame reads");
    }
}

#[cfg(feature = "compression-lz4")]
#[test]
fn lz4_declared_sizes_are_checked_before_allocating() {
    use bmux_attach_image_protocol::CompressionId;
    use bmux_ipc::compression::CompressionCodec as _;
    use bmux_ipc::compression::{Lz4Codec, decompress_by_id_bounded};
    let mut bomb = u32::MAX.to_le_bytes().to_vec();
    bomb.extend_from_slice(&[0x10, b'a']);
    let (result, peak) = peak_of(|| Lz4Codec::new().decompress(&bomb));
    assert!(result.is_err());
    assert_bounded(peak, 0, "lz4 bomb");
    let (result, peak) =
        peak_of(|| decompress_by_id_bounded(&bomb, CompressionId::Lz4, MAX_FRAME_PAYLOAD_SIZE));
    assert!(result.is_err());
    assert_bounded(peak, 0, "bounded lz4 bomb");
    let data = vec![7_u8; 4096];
    let packed = Lz4Codec::new()
        .compress(&data, bmux_ipc::compression::CompressionHint::Frame)
        .unwrap();
    assert_eq!(
        decompress_by_id_bounded(&packed, CompressionId::Lz4, 4096).unwrap(),
        data
    );
    assert!(decompress_by_id_bounded(&packed, CompressionId::Lz4, 4095).is_err());
}

#[cfg(feature = "compression-zstd")]
#[test]
fn zstd_output_is_bounded() {
    use bmux_attach_image_protocol::CompressionId;
    use bmux_ipc::compression::decompress_by_id_bounded;
    // 32 MiB of zeros compresses to a few KiB: a classic bomb against an 8 MiB limit.
    let bomb = zstd::bulk::compress(&vec![0_u8; 32 * 1024 * 1024], 3).unwrap();
    let (result, peak) =
        peak_of(|| decompress_by_id_bounded(&bomb, CompressionId::Zstd, MAX_FRAME_PAYLOAD_SIZE));
    assert!(result.is_err());
    assert_bounded(peak, 2 * MAX_FRAME_PAYLOAD_SIZE, "zstd bomb");
    let data = vec![3_u8; 10_000];
    let packed = zstd::bulk::compress(&data, 3).unwrap();
    assert_eq!(
        decompress_by_id_bounded(&packed, CompressionId::Zstd, 10_000).unwrap(),
        data
    );
    assert!(decompress_by_id_bounded(&packed, CompressionId::Zstd, 9_999).is_err());
}
