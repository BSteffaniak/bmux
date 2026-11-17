//! The clock's contribution to a host-declared extension point, bundled and
//! as a real dylib, under `TestHost`.

use bmux_extpoints::extension_points::status_item_v1 as status_item;
use bmux_plugin_sdk::author;
use bmux_plugin_test::{Mode, TestHost, both_modes};

async fn contract(mode: Mode) -> author::Result<()> {
    let host = TestHost::new(mode)
        .extension_point::<status_item::Point>()?
        .load(bmux_example_clock_plugin::plugin_crate())
        .await?;
    let items = host.extensions::<status_item::Point>()?.snapshot()?;
    assert_eq!(items.len(), 1);
    let clock = &items[0];
    assert_eq!(clock.plugin_id, "example.clock");
    assert_eq!(
        clock.data,
        status_item::Data {
            id: "clock".into(),
            order: 100,
            label: Some("UTC".into()),
        }
    );
    let full: String = clock
        .call(&status_item::Request::Render { width: 8 })
        .await?;
    assert_eq!(full.len(), 8);
    assert_eq!(full.as_bytes()[2], b':');
    let narrow: String = clock
        .call(&status_item::Request::Render { width: 2 })
        .await?;
    assert_eq!(narrow.len(), 2);

    // The contribution leaves with its plugin.
    let mut set = host.extensions::<status_item::Point>()?;
    host.shutdown().await?;
    assert!(set.changed().await);
    assert!(set.snapshot()?.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_extends_the_status_line_bundled_and_as_a_dylib() {
    both_modes(contract).await.unwrap();
}

/// A host that does not declare the point refuses the contribution, naming
/// the plugin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undeclared_extension_points_are_refused() {
    let error = TestHost::new(Mode::Bundled)
        .load(bmux_example_clock_plugin::plugin_crate())
        .await
        .err()
        .expect("contribution to an undeclared point is refused");
    assert!(
        error.message.contains("example.clock") && error.message.contains("status-item"),
        "{error:?}"
    );
}
