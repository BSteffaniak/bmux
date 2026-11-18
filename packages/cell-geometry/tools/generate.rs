//! Offline migration tool. Compile beside the pinned 0.2.2 reference tables
//! (see README). No reference implementation is linked into the product.
include!("reference.rs");

fn main() {
    use std::collections::HashMap;
    // Refine scalar equivalence classes: two scalars may share a class only
    // if they have identical output and successor in EVERY reachable state.
    let mut classes = vec![0u16; 0x110000];
    let mut states = vec![WidthInfo::DEFAULT];
    let mut class_count = 0;
    let mut i = 0;
    while i < states.len() {
        let mut seen = HashMap::new();

        for (cp, class) in classes.iter_mut().enumerate() {
            let Some(c) = char::from_u32(cp as u32) else {
                continue;
            };
            let (delta, next) = width_in_str(c, states[i]);
            let next = states.iter().position(|s| *s == next).unwrap_or_else(|| {
                states.push(next);
                states.len() - 1
            });
            let id = u16::try_from(seen.len()).unwrap();
            let id = *seen.entry((*class, delta, next)).or_insert(id);
            *class = id;
        }
        class_count = seen.len();
        i += 1;
    }
    let reps: Vec<_> = (0..class_count)
        .map(|id| classes.iter().position(|&k| usize::from(k) == id).unwrap())
        .collect();
    let mut page_ids = Vec::new();
    let mut pages = Vec::new();
    for page in classes.chunks_exact(256) {
        let id = pages
            .iter()
            .position(|p: &&[u16]| *p == page)
            .unwrap_or_else(|| {
                pages.push(page);
                pages.len() - 1
            });
        page_ids.push(u8::try_from(id).unwrap());
    }
    println!("// Generated Unicode 17 narrow-width transition data. See tools/README.md.");
    println!(
        "#[rustfmt::skip]\npub(super) static PAGE: [u8; {}] = {:?};",
        page_ids.len(),
        page_ids
    );
    println!(
        "#[rustfmt::skip]\npub(super) static CLASS: [[u8;256]; {}] = {:?};",
        pages.len(),
        pages
    );
    println!(
        "#[rustfmt::skip]\npub(super) static STEP: [[(i8,u8);{}];{}] = [",
        reps.len(),
        states.len()
    );
    for s in &states {
        let row: Vec<_> = reps
            .iter()
            .map(|&cp| {
                let (d, n) = width_in_str(char::from_u32(cp as u32).unwrap(), *s);
                (d, states.iter().position(|v| *v == n).unwrap())
            })
            .collect();
        // Check the actual compressed two-level lookup and emitted transition,
        // not just the representative scalar for each class.
        for ch in '\0'..=char::MAX {
            let cp = ch as usize;
            let key = pages[usize::from(page_ids[cp >> 8])][cp & 255];
            let (delta, next) = row[usize::from(key)];
            let (expected_delta, expected_next) = width_in_str(ch, *s);
            assert_eq!((delta, states[next]), (expected_delta, expected_next));
        }
        println!("{:?},", row);
    }
    println!("];");
    // Independent exhaustive verification of scalar widths versus start state.
    for c in '\0'..=char::MAX {
        assert_eq!(
            single_char_width(c).unwrap_or(1),
            width_in_str(c, WidthInfo::DEFAULT).0 as usize
        );
    }
}
