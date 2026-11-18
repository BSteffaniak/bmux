//! The counting allocator counts this thread's allocations, and live bytes follow frees.

use bmux_bench::allocations::{self, Allocations, CountingAllocator};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn measures_what_work_on_this_thread_allocates() {
    assert!(allocations::installed());
    let ((), none) = allocations::measure(|| {
        std::hint::black_box(1 + 1);
    });
    assert_eq!(none, Allocations::default());

    let (vec, counted) = allocations::measure(|| {
        let mut vec = Vec::<u8>::with_capacity(16);
        vec.extend_from_slice(&[0; 64]); // grows once
        std::hint::black_box(vec)
    });
    assert_eq!(counted.count, 2, "an allocation and one reallocation");
    assert_eq!(counted.bytes, 16 + 64);

    let ((), elsewhere) = allocations::measure(|| {
        std::thread::spawn(|| drop(std::hint::black_box(vec![0_u8; 1024])))
            .join()
            .unwrap();
    });
    assert!(
        elsewhere.bytes < 1024,
        "another thread's allocations are not this thread's"
    );
    drop(vec);
}

#[test]
fn live_and_peak_bytes_follow_allocations_and_frees() {
    allocations::reset_peak();
    let before = allocations::live_bytes();
    let block = std::hint::black_box(vec![0_u8; 1 << 20]);
    assert!(allocations::peak_bytes() >= before + (1 << 20));
    drop(block);
    // Other tests may allocate concurrently; a megabyte freed shows anyway.
    assert!(allocations::live_bytes() < before + (1 << 19));
}

#[test]
fn instruction_counts_grow_with_work_where_there_is_a_counter() {
    let Some(counter) = bmux_bench::instructions::InstructionCounter::new() else {
        return;
    };
    let (_, little) = counter.measure(|| std::hint::black_box((0..10_u64).sum::<u64>()));
    let (_, more) = counter
        .measure(|| (0..1_000_000_u64).fold(0_u64, |sum, value| std::hint::black_box(sum ^ value)));
    assert!(more > little, "{more} > {little}");
    assert!(more > 1_000_000);
}
