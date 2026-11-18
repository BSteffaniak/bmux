use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
    time::Instant,
};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
// Forward the original layout and pointers to the same system allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static HEAP: Counting = Counting;

#[inline(never)]
fn scan(mut text: &str, mode: &str) -> usize {
    let mut digest = 0usize;
    let mut add = |i: usize| digest = digest.wrapping_mul(31).wrapping_add(i);
    match mode {
        "fwd" => {
            for g in graphemes(text) {
                add(g.len());
            }
        }
        "rev" => {
            for g in graphemes(text).rev() {
                add(g.len());
            }
        }
        "ends" => {
            let mut it = grapheme_indices(text);
            while let Some((i, g)) = it.next() {
                add(i);
                add(g.len());
                if let Some((i, g)) = it.next_back() {
                    add(i);
                    add(g.len());
                }
            }
        }
        "erase" => {
            while !text.is_empty() {
                let (i, _) = grapheme_indices(text).next_back().unwrap();
                assert!(i < text.len());
                add(i);
                text = &text[..i];
            }
        }
        _ => unreachable!(),
    }
    digest
}
fn main() {
    println!("case,n,bytes,mode,allocs,digest,reps,s1_ns,s2_ns,s3_ns,s4_ns,s5_ns");
    for (case, unit) in [
        ("ascii", "edit a line. "),
        ("mixed", "票é a\u{301}\r\n각"),
        ("emoji", "👩🏽‍🚀🇺🇳🧑‍🔧"),
        ("ri", "🇦"),
        ("indic", "क्"),
        ("extend", "\u{301}"),
        ("zwj", "👩‍"),
    ] {
        for n in [256, 512, 1024] {
            let text = match case {
                "extend" => format!("x{}", unit.repeat(n)),
                "indic" => format!("{}क", unit.repeat(n)),
                "zwj" => format!("{}👩", unit.repeat(n)),
                _ => unit.repeat(n),
            };
            for mode in ["fwd", "rev", "ends", "erase"] {
                let before = ALLOCS.load(Relaxed);
                let digest = scan(black_box(&text), mode);
                let allocs = ALLOCS.load(Relaxed) - before;
                let batch = |r| {
                    let t = Instant::now();
                    for _ in 0..r {
                        black_box(scan(black_box(&text), mode));
                    }
                    t.elapsed().as_nanos()
                };
                let mut reps = 1;
                while batch(reps) < 2_000_000 {
                    reps *= 2;
                }
                print!("{case},{n},{},{mode},{allocs},{digest},{reps}", text.len());
                for _ in 0..5 {
                    print!(",{:.2}", batch(reps) as f64 / reps as f64);
                }
                println!();
            }
        }
    }
}
