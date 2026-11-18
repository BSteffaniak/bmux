//! Check the compiled lookup against a flat reference parsed directly from UCD text.
extern crate std;

use super::lookup;

fn overlay(reference: &mut [u8], source: &str, property: impl Fn(&str) -> Option<u8>) {
    for line in source.lines() {
        let content = line.split('#').next().unwrap().trim();
        if content.is_empty() {
            continue;
        }
        let (span, fields) = content.split_once(';').unwrap();
        let Some(bits) = property(fields.trim()) else {
            continue;
        };
        let span = span.trim();
        let (lo, hi) = span.split_once("..").unwrap_or((span, span));
        let hex = |s| usize::from_str_radix(s, 16).unwrap();
        for byte in &mut reference[hex(lo)..=hex(hi)] {
            *byte |= bits;
        }
    }
}

#[test]
fn all_scalars_match_unicode_17() {
    let mut reference = std::vec![0; 0x110000];
    overlay(
        &mut reference,
        include_str!("../data/17.0.0/GraphemeBreakProperty.txt"),
        |gcb| {
            Some(match gcb {
                "Other" => 0,
                "CR" => 1,
                "LF" => 2,
                "Control" => 3,
                "Extend" => 4,
                "ZWJ" => 5,
                "Regional_Indicator" => 6,
                "Prepend" => 7,
                "SpacingMark" => 8,
                "L" => 9,
                "V" => 10,
                "T" => 11,
                "LV" => 12,
                "LVT" => 13,
                unknown => panic!("unknown GCB {unknown}"),
            })
        },
    );
    overlay(
        &mut reference,
        include_str!("../data/17.0.0/emoji-data.txt"),
        |property| (property == "Extended_Pictographic").then_some(16),
    );
    overlay(
        &mut reference,
        include_str!("../data/17.0.0/DerivedCoreProperties.txt"),
        |fields| {
            let (name, value) = fields.split_once(';')?;
            if name.trim() != "InCB" {
                return None;
            }
            Some(match value.trim() {
                "None" => 0,
                "Consonant" => 32,
                "Extend" => 64,
                "Linker" => 96,
                unknown => panic!("unknown InCB {unknown}"),
            })
        },
    );
    for ch in char::MIN..=char::MAX {
        assert_eq!(lookup(ch), reference[ch as usize], "U+{:04X}", ch as u32);
    }
}
