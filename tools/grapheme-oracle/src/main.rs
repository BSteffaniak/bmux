//! Emit frozen expectations for the replacement's integration tests, never using its code.
use std::fmt::Write;
use unicode_segmentation::UnicodeSegmentation;

fn main() {
    // Exercise interactions among break classes as well as arbitrary Unicode scalars.
    let classes: Vec<_> = "a\r\n\0\u{600}\u{301}\u{903}\u{1100}\u{1161}\u{11a8}가각🇦🇧😀\u{200d}क्\u{200c}🏿\u{fe0f}\u{e0067}\u{10ffff}".chars().collect();
    let mut state = 0x4731_3733_u32;
    let mut draw = || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    println!(
        "# unicode-segmentation 1.13.3 extended graphemes: hexadecimal scalars ; decimal byte starts"
    );
    for i in 0..4096 {
        let text: String = (0..i % 32)
            .map(|_| {
                let n = draw();
                if n % 8 == 0 {
                    char::from_u32(draw() % 0x110000).unwrap_or('\u{fffd}')
                } else {
                    classes[draw() as usize % classes.len()]
                }
            })
            .collect();
        let mut row = String::new();
        for ch in text.chars() {
            write!(row, "{:X} ", u32::from(ch)).unwrap();
        }
        row.push(';');
        for (start, _) in text.grapheme_indices(true) {
            write!(row, " {start}").unwrap();
        }
        println!("{row}");
    }
}
