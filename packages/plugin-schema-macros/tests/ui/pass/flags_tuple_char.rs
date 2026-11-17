bmux_plugin_schema_macros::schema_inline!(
    r#"
/// Documented package.
plugin p version 2;

capability I_READ = p.i.read;

/// Documented interface.
@capability(I_READ)
@since(1)
interface i {
    /// Permission bits.
    flags mode {
        read,
        write,
    }

    record entry {
        /// Pair of a count and a marker.
        pair: tuple<u32, char>,
        single: tuple<string>,
        mode: mode,
    }

    @deprecated(2)
    @since(1)
    query get() -> entry;
}
"#
);

fn main() {
    let mode = i::Mode::READ.union(i::Mode::WRITE);
    assert!(mode.contains(i::Mode::READ));
    assert_eq!(i::Mode::all(), mode);
    assert_eq!(i::Mode::from_bits_truncate(u64::MAX).bits(), 0b11);
    let entry = i::Entry {
        pair: (1, 'x'),
        single: ("one".to_string(),),
        mode: i::Mode::empty(),
    };
    assert_eq!(entry.pair.1, 'x');
}
