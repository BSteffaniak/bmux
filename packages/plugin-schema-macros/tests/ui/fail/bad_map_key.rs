bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin p version 1;

interface i {
    record r { m: map<bool, u32> }
}
"#
);

fn main() {}
