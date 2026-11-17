//! The hello command, bundled and as a real dylib, under `TestHost`.

use bmux_plugin_sdk::author;
use bmux_plugin_test::{Mode, TestHost, both_modes};

async fn contract(mode: Mode) -> author::Result<()> {
    let host = TestHost::new(mode)
        .load(bmux_hello_plugin::plugin_crate())
        .await?;
    assert_eq!(host.command("example.hello", "hello", &[]).await?, 0);
    assert_eq!(
        host.command("example.hello", "hello", &["--name", "bmux"])
            .await?,
        0
    );
    let error = host
        .command("example.hello", "hello", &["positional"])
        .await
        .unwrap_err();
    assert!(error.message.contains("named"), "{error:?}");
    host.shutdown().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_runs_bundled_and_as_a_dylib() {
    both_modes(contract).await.unwrap();
}
