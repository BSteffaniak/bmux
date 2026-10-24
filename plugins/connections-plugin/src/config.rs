//! Private connections-plugin settings; never rewrites host configuration.
use anyhow::{Result, bail};
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    remote: RemoteSettings,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteSettings {
    pub relay: RelayPolicy,
    pub startup_wait_ms: u64,
    pub allow_full_runtime_ingress: bool,
    pub bind_addr: Option<std::net::SocketAddr>,
    pub auto_start: bool,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            relay: RelayPolicy::Default,
            startup_wait_ms: 5_000,
            allow_full_runtime_ingress: false,
            bind_addr: None,
            auto_start: false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RelayPolicy {
    #[default]
    Default,
    Disabled,
    Custom {
        urls: Vec<String>,
    },
}

impl RelayPolicy {
    pub(crate) fn apply(
        &self,
        builder: iroh::endpoint::Builder,
    ) -> Result<iroh::endpoint::Builder> {
        Ok(match self {
            Self::Default => builder,
            Self::Disabled => builder.clear_address_lookup().clear_relay_transports(),
            Self::Custom { urls } => {
                let urls = urls
                    .iter()
                    .map(|url| url.parse())
                    .collect::<Result<Vec<iroh::RelayUrl>, _>>()?;
                builder
                    .clear_address_lookup()
                    .relay_mode(iroh::RelayMode::Custom(iroh::RelayMap::from_iter(urls)))
            }
        })
    }
}

impl RemoteSettings {
    pub(crate) fn load(value: Option<&toml::Value>) -> Result<Self> {
        if let Some(relay) = value
            .and_then(|value| value.get("remote"))
            .and_then(|remote| remote.get("relay"))
            && relay.get("mode").and_then(toml::Value::as_str) != Some("custom")
            && relay.get("urls").is_some()
        {
            bail!("relay URLs require custom mode");
        }
        let settings: Settings = value.cloned().map_or_else(
            || Ok(Settings::default()),
            |value| {
                value
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("invalid connections plugin settings"))
            },
        )?;
        let remote = settings.remote;
        if remote.auto_start
            && (!remote.allow_full_runtime_ingress
                || remote.bind_addr.is_none_or(|addr| addr.port() == 0))
        {
            bail!("auto_start requires explicit ingress permission and a fixed bind_addr");
        }
        if !(1..=60_000).contains(&remote.startup_wait_ms) {
            bail!("remote.startup_wait_ms must be in 1..=60000");
        }
        if let RelayPolicy::Custom { urls } = &remote.relay {
            if urls.is_empty() || urls.len() > 16 {
                bail!("custom relay policy requires 1..=16 URLs");
            }
            let mut unique = std::collections::BTreeSet::new();
            for text in urls {
                let url: url::Url = text
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid custom relay URL"))?;
                if text.len() > 2048
                    || url.scheme() != "https"
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                    || url.path() != "/"
                    || !unique.insert(url.to_string())
                {
                    bail!("custom relay URLs must be unique HTTPS origins without credentials");
                }
                text.parse::<iroh::RelayUrl>()
                    .map_err(|_| anyhow::anyhow!("invalid Iroh relay URL"))?;
            }
        }
        Ok(remote)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> Result<RemoteSettings> {
        RemoteSettings::load(Some(&toml::from_str::<toml::Value>(text).unwrap()))
    }

    #[test]
    fn defaults_and_supported_policies() {
        let defaults = RemoteSettings::load(None).unwrap();
        assert_eq!(defaults.startup_wait_ms, 5_000);
        assert!(matches!(defaults.relay, RelayPolicy::Default));
        for mode in ["default", "disabled"] {
            assert!(load(&format!("[remote.relay]\nmode = '{mode}'")).is_ok());
        }
        assert!(load("[remote.relay]\nmode = 'custom'\nurls = ['https://relay.example']").is_ok());
    }

    #[test]
    fn invalid_settings_fail_closed() {
        for text in [
            "[remote]\nauto_start = true",
            "[remote]\nauto_start = true\nallow_full_runtime_ingress = true\nbind_addr = '127.0.0.1:0'",
            "[remote]\nstartup_wait_ms = 0",
            "[remote]\nstartup_wait_ms = 60001",
            "[remote]\nstartup_wait_ms = -1",
            "[remote]\nunknown = true",
            "[remote.relay]\nmode = 'bogus'",
            "[remote.relay]\nmode = 'custom'\nurls = []",
            "[remote.relay]\nmode = 'disabled'\nurls = ['https://relay.example']",
        ] {
            assert!(load(text).is_err(), "accepted {text}");
        }
        for url in [
            "http://relay.example",
            "https://user:secret@relay.example",
            "https://relay.example/?token=secret",
            "https://relay.example/#secret",
            "https://relay.example/path",
        ] {
            assert!(
                load(&format!(
                    "[remote.relay]\nmode = 'custom'\nurls = ['{url}']"
                ))
                .is_err()
            );
        }
    }
}
