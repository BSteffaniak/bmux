bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin p version 1;

interface i {
    enum e { @default a, @default b }
}
"#
);

fn main() {}
