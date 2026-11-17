bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin acme.counter version 1;
capability WRITE = acme.counter.write;
@capability(WRITE)
@interface-version(1)
interface counter {
    command incr(by: u64) -> u64;
    command count_to(limit: u32) -> u32 emits u32;
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
    async fn count_to(
        &self,
        _cx: &Call,
        limit: u32,
        events: bmux_plugin_sdk::typed_dispatch::TypedServiceEventSink<u32>,
        _cancellation: bmux_plugin_sdk::ServiceCancellation,
    ) -> Result<u32> {
        for value in 0..limit {
            events
                .emit(value)
                .map_err(|error| bmux_plugin_sdk::PluginCommandError::failed(error.to_string()))?;
        }
        Ok(limit)
    }
}

// What `Declarations::provide::<dyn counter::v1::Counter>()` requires.
fn provide<I: ?Sized + Provides<P>, P>() {}

fn main() {
    provide::<dyn counter::v1::Counter, Counter>();
    let _ = commands::BumpArgs::parse(&[]);
    let _ = commands::BUMP.name();
}
