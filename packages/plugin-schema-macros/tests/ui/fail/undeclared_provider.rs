// Declaring that a plugin provides an interface it does not implement must
// not compile.
bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin acme.counter version 1;
capability WRITE = acme.counter.write;
@capability(WRITE)
@interface-version(1)
interface counter {
    command incr(by: u64) -> u64;
}
"#
);

struct NotACounter;

fn provide<I: ?Sized + bmux_plugin_sdk::author::Provides<P>, P>() {}

fn main() {
    provide::<dyn counter::v1::Counter, NotACounter>();
}
