bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin p version 1;

interface i {
    enum color {
        red,
        @default green,
        blue,
    }

    record r {
        tags: map<string, u32>,
        choice: color,
    }

    query get() -> r;
}
"#
);

fn main() {
    // Verify @default produced a Default impl and map<_, _> lowered to BTreeMap.
    let c = i::Color::default();
    assert!(matches!(c, i::Color::Green));
    let _r = i::R {
        tags: ::std::collections::BTreeMap::new(),
        choice: i::Color::default(),
    };
}
