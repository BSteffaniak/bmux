//! The watcher's contract test, run in both packaging modes by `TestHost`.
//!
//! Bundled: the counter and watcher load in process; the test's clients call
//! the counter's registered providers directly. Dylib: both crates are
//! packaged as real dynamic libraries and every call crosses as checked
//! frames. The watcher's activation calls the counter and follows its change
//! events through the generated `subscribe_changed()` in both.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bmux_example_counter_plugin_api::{counter_commands, counter_events, counter_state};
use bmux_example_watcher_plugin::declaration::commands::WatchArgs;
use bmux_example_watcher_plugin::declaration::provides::watcher_state;
use bmux_plugin_sdk::author::{self, Call, CommandArgs};
use bmux_plugin_test::{Mode, TestHost, both_modes};

/// Wait (bounded) until the watcher's subscription delivered `value`.
async fn observed(host: &TestHost, value: u64) -> author::Result<()> {
    let watcher = host.client::<watcher_state::v1::Client>();
    for _ in 0..200 {
        if watcher.observed().await? == value {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(author::Error::failed(format!(
        "the watcher never observed {value} (last: {})",
        watcher.observed().await?
    )))
}

async fn contract(mode: Mode) -> author::Result<()> {
    let host = TestHost::new(mode)
        .load(bmux_example_counter_plugin::plugin_crate())
        .await?;
    // The counter declared `emits::<Changed>()`: its state channel exists
    // before anyone publishes, so the test can follow it too.
    let mut changes = host.subscribe::<counter_events::v1::Changed>()?;
    let host = host
        .load(bmux_example_watcher_plugin::plugin_crate())
        .await?;

    // The watcher's activation bumped by 2 and 3 through the host.
    let state = host.client::<counter_state::v1::Client>();
    assert_eq!(state.get().await?, 5);
    observed(&host, 5).await?;
    let latest = loop {
        let change = changes.next().await.expect("counter channel stays open")?;
        if change.value == 5 {
            break change.value;
        }
    };
    assert_eq!(latest, 5);

    // A command body reaches the counter through the host as well.
    assert_eq!(
        host.command("example.watcher", "watch", &["--by", "4"])
            .await?,
        0
    );
    observed(&host, 9).await?;

    // The fast path (bundled direct handle) and the codec agree.
    let direct = host.client::<counter_commands::v1::Client>();
    let remote = host.remote::<counter_commands::v1::Client>();
    assert_eq!(direct.incr(1).await?, 10);
    assert_eq!(remote.incr(1).await?, 11);
    assert_eq!(host.remote::<counter_state::v1::Client>().get().await?, 11);
    observed(&host, 11).await?;

    // Provider failures reach consumers on either path.
    direct.incr(u64::MAX - 11).await?;
    for client in [&direct, &remote] {
        let error = client.incr(1).await.unwrap_err();
        assert!(error.message.contains("counter overflow"), "{error:?}");
    }
    host.shutdown().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_contract_holds_bundled_and_as_dylibs() {
    both_modes(contract).await.unwrap();
}

/// A fake counter the test serves in place of the counter plugin.
#[derive(Default, Clone)]
struct FakeCounter(Arc<AtomicU64>);

impl counter_commands::v1::CounterCommands for FakeCounter {
    async fn incr(&self, _cx: &Call, by: u64) -> author::Result<u64> {
        Ok(self.0.fetch_add(by, Ordering::SeqCst) + by)
    }
}

impl counter_state::v1::CounterState for FakeCounter {
    async fn get(&self, _cx: &Call) -> author::Result<u64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

/// The watcher against fake providers: its activation's calls reach the
/// fakes through the host in both modes; with no counter loaded there is no
/// change channel, which the watcher tolerates.
async fn against_fakes(mode: Mode) -> author::Result<()> {
    let fake = FakeCounter::default();
    let host = TestHost::new(mode)
        .fake::<dyn counter_commands::v1::CounterCommands, _>(fake.clone())?
        .fake::<dyn counter_state::v1::CounterState, _>(fake.clone())?
        .load(bmux_example_watcher_plugin::plugin_crate())
        .await?;
    assert_eq!(fake.0.load(Ordering::SeqCst), 5);
    assert_eq!(
        host.client::<watcher_state::v1::Client>()
            .observed()
            .await?,
        0
    );
    host.shutdown().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_runs_against_fake_providers_in_both_modes() {
    both_modes(against_fakes).await.unwrap();
}

#[test]
fn watcher_declares_only_what_it_requires() {
    let definition = bmux_example_watcher_plugin::plugin_definition();
    definition
        .validate()
        .expect("embedded manifest matches metadata");
    let manifest = definition.metadata.manifest_toml.as_deref().unwrap();
    assert!(manifest.contains("example.counter.write"));
    assert!(manifest.contains("example.counter.read"));
    assert!(manifest.contains("name = \"watch\""));
    let args = WatchArgs::parse(&["--by".to_owned(), "4".to_owned()]).unwrap();
    assert_eq!(args.by, Some(4));
}
