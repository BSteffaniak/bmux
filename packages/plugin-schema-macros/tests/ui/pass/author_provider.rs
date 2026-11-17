bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin acme.counter version 1;
capability WRITE = acme.counter.write;
@capability(WRITE)
@interface-version(1)
interface counter {
    command incr(by: u64) -> u64;
}
command bump(by: u64?) title "Bump";
"#
);

use bmux_plugin_sdk::author::{Call, CommandArgs, Provides, Result};

#[derive(Default)]
struct Counter(std::sync::atomic::AtomicU64);

// Providers implement the generated trait with plain `async fn`.
impl counter::v1::Counter for Counter {
    async fn incr(&self, _cx: &Call, by: u64) -> Result<u64> {
        Ok(self.0.fetch_add(by, std::sync::atomic::Ordering::SeqCst) + by)
    }
}

// What `Declarations::provide::<dyn counter::v1::Counter>()` requires.
fn provide<I: ?Sized + Provides<P>, P>() {}

fn main() {
    provide::<dyn counter::v1::Counter, Counter>();
    let _ = commands::BumpArgs::parse(&[]);
    let _ = commands::BUMP.name();
}
