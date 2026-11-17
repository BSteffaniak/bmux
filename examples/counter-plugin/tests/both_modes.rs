//! The counter's contract, bundled and as a real dylib, under `TestHost`.

use bmux_example_counter_plugin_api::{counter_commands, counter_events, counter_state};
use bmux_plugin_sdk::author::{self, Subscription};
use bmux_plugin_test::{Mode, TestHost, both_modes};

/// Follow the retained channel until it reports `value` (state channels
/// deliver the latest value, so intermediate ones may be skipped).
async fn until(
    changes: &mut Subscription<counter_events::v1::Changed>,
    value: u64,
) -> author::Result<()> {
    while let Some(change) = changes.next().await {
        if change?.value == value {
            return Ok(());
        }
    }
    Err(author::Error::failed(format!(
        "the channel ended before {value}"
    )))
}

async fn contract(mode: Mode) -> author::Result<()> {
    let host = TestHost::new(mode)
        .load(bmux_example_counter_plugin::plugin_crate())
        .await?;
    let mut changes = host.subscribe::<counter_events::v1::Changed>()?;
    let commands = host.client::<counter_commands::v1::Client>();
    let state = host.client::<counter_state::v1::Client>();

    assert_eq!(commands.incr(4).await?, 4);
    until(&mut changes, 4).await?;

    // `bump` reaches the counter through the host, like any consumer.
    assert_eq!(
        host.command("example.counter", "bump", &["--by", "3"])
            .await?,
        0
    );
    assert_eq!(state.get().await?, 7);
    until(&mut changes, 7).await?;
    assert!(
        host.command("example.counter", "bump", &["--nope", "1"])
            .await
            .is_err()
    );

    // A failed increment leaves the counter (and its channel) unchanged.
    let error = commands.incr(u64::MAX).await.unwrap_err();
    assert!(error.message.contains("counter overflow"), "{error:?}");
    assert_eq!(state.get().await?, 7);
    host.shutdown().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn counter_contract_holds_bundled_and_as_a_dylib() {
    both_modes(contract).await.unwrap();
}
