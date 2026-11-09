//! Scaling microbenchmark for incremental `MeasuredListIndex` mutation.
//!
//! Run with `cargo run --release -p bmux_tui --example measured_list_benchmark`.
//!
//! For each collection size the benchmark compares a full rebuild (the cost a
//! non-incremental index pays for any membership change) with the incremental
//! keyed operations used by paged, virtualized collections: prepending and
//! appending a page, replacing a range in the middle, trimming one end, and
//! key-to-index lookups that must stay correct across those splices. Each
//! incremental result is checked against a full rebuild of the same items.

use std::hint::black_box;
use std::time::{Duration, Instant};

use bmux_tui::measured_list::{MeasuredListIndex, MeasuredListItem};

const PAGE: usize = 16;
const LOOKUPS: usize = 10_000;

fn main() {
    println!(
        "items,rebuild_us,sync_prepend_us,prepend_page_us,append_page_us,splice_middle_us,trim_front_us,update_height_ns,index_of_ns,item_at_offset_ns"
    );
    for count in [1_000usize, 10_000, 100_000, 1_000_000] {
        benchmark(count);
    }
}

fn measured(key: usize) -> MeasuredListItem<usize> {
    MeasuredListItem::new(key, 0, 80, 0, u64::try_from(key % 7 + 1).unwrap_or(1))
}

fn page(start: usize) -> Vec<MeasuredListItem<usize>> {
    (start..start + PAGE).map(measured).collect()
}

fn benchmark(count: usize) {
    let base = (PAGE * 4..PAGE * 4 + count)
        .map(measured)
        .collect::<Vec<_>>();
    let mut index = MeasuredListIndex::from_items(1, base.clone()).expect("unique keys");

    // Full rebuild of the collection after prepending one page.
    let mut rebuilt_items = page(0);
    rebuilt_items.extend(base.iter().cloned());
    let started = Instant::now();
    let rebuilt = MeasuredListIndex::from_items(1, rebuilt_items).expect("unique keys");
    let rebuild = started.elapsed();
    black_box(&rebuilt);

    // `sync` after a prepend: compares all entries but splices only the page.
    let mut synced = index.clone();
    let entries = (0..PAGE)
        .chain(PAGE * 4..PAGE * 4 + count)
        .map(|key| (key, 0))
        .collect::<Vec<_>>();
    let started = Instant::now();
    synced.sync(entries, 80, 0, |key| measured(*key).height);
    let sync_prepend = started.elapsed();
    assert_eq!(synced, rebuilt);

    let started = Instant::now();
    index.prepend(page(0)).expect("unique keys");
    let prepend = started.elapsed();
    assert_eq!(index, rebuilt);

    let tail = PAGE * 4 + count;
    let started = Instant::now();
    index.append(page(tail)).expect("unique keys");
    let append = started.elapsed();

    let middle = index.len() / 2;
    let started = Instant::now();
    index
        .splice(middle..middle + PAGE, page(tail + PAGE))
        .expect("unique keys");
    let splice = started.elapsed();

    let started = Instant::now();
    index.remove_range(0..PAGE).expect("in bounds");
    let trim = started.elapsed();

    let mut expected = base;
    expected.extend(page(tail));
    expected.splice(middle - PAGE..middle, page(tail + PAGE));
    assert_eq!(
        index,
        MeasuredListIndex::from_items(1, expected).expect("unique keys")
    );

    let started = Instant::now();
    for key in (0..LOOKUPS).map(|step| PAGE * 4 + step * 7 % count) {
        black_box(index.update_height(&key, 3));
    }
    let update = started.elapsed();

    let started = Instant::now();
    for key in (0..LOOKUPS).map(|step| PAGE * 4 + step * 13 % count) {
        black_box(index.index_of(&key));
    }
    let lookup = started.elapsed();

    let total = index.total_height().max(1);
    let started = Instant::now();
    for step in 0..LOOKUPS {
        black_box(index.item_at_offset(u64::try_from(step).unwrap_or(0) * 7_919 % total));
    }
    let offset_lookup = started.elapsed();

    println!(
        "{count},{},{},{},{},{},{},{},{},{}",
        micros(rebuild),
        micros(sync_prepend),
        micros(prepend),
        micros(append),
        micros(splice),
        micros(trim),
        per_op_nanos(update),
        per_op_nanos(lookup),
        per_op_nanos(offset_lookup),
    );
}

fn micros(duration: Duration) -> u128 {
    duration.as_micros()
}

fn per_op_nanos(duration: Duration) -> u128 {
    duration.as_nanos() / u128::try_from(LOOKUPS).unwrap_or(1)
}
