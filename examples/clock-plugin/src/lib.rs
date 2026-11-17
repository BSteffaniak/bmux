//! Reference contributor to a host-declared extension point.
//!
//! BMUX declares `status-item` in `bmux_extpoints`. The clock extends it in
//! [`Plugin::declare`] with declarative data, which the host reads from the
//! instance's registrations, and implements the point's generated
//! `Behavior`. The host calls the behavior through the instance's
//! extension port in both packaging modes.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

/// Manifest generated from `bpdl/clock.bpdl` and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::schema! {
        source: "bpdl/clock.bpdl",
        manifest: "plugin.toml",
    }
}

use std::time::{SystemTime, UNIX_EPOCH};

use bmux_extpoints::extension_points::status_item_v1 as status_item;
use bmux_plugin_sdk::author::{self, Call, Declarations, Plugin};

/// The clock: a status-line segment showing UTC time.
#[derive(Default)]
pub struct Clock;

/// `HH:MM:SS` (UTC) of `seconds` since the Unix epoch.
#[must_use]
pub fn time_of_day(seconds: u64) -> String {
    let day = seconds % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, day % 3600 / 60, day % 60)
}

impl status_item::Behavior for Clock {
    async fn render(&self, _cx: &Call, width: u16) -> author::Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| author::Error::failed(error.to_string()))?;
        Ok(time_of_day(now.as_secs())
            .chars()
            .take(usize::from(width))
            .collect())
    }
}

impl Plugin for Clock {
    fn declare(d: &mut Declarations<Self>) {
        d.extend::<status_item::Point>(status_item::Data {
            id: "clock".into(),
            order: 100,
            label: Some("UTC".into()),
        });
    }
}

bmux_plugin_sdk::plugin!(Clock);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_time_of_day() {
        assert_eq!(time_of_day(0), "00:00:00");
        assert_eq!(time_of_day(86_400 + 3_723), "01:02:03");
    }
}
