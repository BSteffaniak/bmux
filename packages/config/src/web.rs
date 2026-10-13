//! Shared browser configuration precedence and asset location resolution.
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::{WebAuth, WebConfig};

/// Explicit command-line overrides; absent values inherit environment/configuration.
#[derive(Clone, Copy, Default)]
pub struct WebOptions<'a> {
    pub port: Option<u16>,
    pub auth: Option<&'a str>,
    pub host: Option<std::net::IpAddr>,
    pub public_host: Option<std::net::IpAddr>,
    pub allowed_hosts: Option<&'a [String]>,
}

fn select_ip(
    cli: Option<std::net::IpAddr>,
    env: std::result::Result<String, std::env::VarError>,
    configured: Option<std::net::IpAddr>,
    name: &str,
) -> Result<Option<std::net::IpAddr>> {
    match (cli, env) {
        (Some(ip), _) => Ok(Some(ip)),
        (None, Ok(value)) => {
            Ok(Some(value.parse().with_context(|| {
                format!("{name} must be an IPv4 or IPv6 address")
            })?))
        }
        (None, Err(std::env::VarError::NotPresent)) => Ok(configured),
        (None, Err(error)) => Err(error.into()),
    }
}

/// Resolves every setting using CLI > environment > configured defaults.
/// # Errors
/// Rejects invalid environment settings unless the corresponding CLI override is present.
pub fn resolve(config: WebConfig, options: WebOptions<'_>) -> Result<WebConfig> {
    let mut settings = select_web_settings(
        config.clone(),
        (options.port, options.auth),
        std::env::var("BMUX_WEB_PORT"),
        std::env::var("BMUX_WEB_AUTH"),
    )?;
    settings.host = select_ip(
        options.host,
        std::env::var("BMUX_WEB_HOST"),
        Some(config.host),
        "BMUX_WEB_HOST",
    )?
    .context("missing browser bind host")?;
    settings.public_host = select_ip(
        options.public_host,
        std::env::var("BMUX_WEB_PUBLIC_HOST"),
        config.public_host,
        "BMUX_WEB_PUBLIC_HOST",
    )?;
    settings.allowed_hosts = select_allowed_hosts(
        options.allowed_hosts,
        std::env::var("BMUX_WEB_ALLOWED_HOSTS"),
        config.allowed_hosts,
    )?;
    Ok(settings)
}

fn select_allowed_hosts(
    cli: Option<&[String]>,
    env: std::result::Result<String, std::env::VarError>,
    configured: Vec<String>,
) -> Result<Vec<String>> {
    match (cli, env) {
        (Some(hosts), _) => Ok(hosts.to_vec()),
        (None, Ok(value)) => Ok(if value.is_empty() {
            Vec::new()
        } else {
            value
                .split(',')
                .map(|host| host.trim().to_owned())
                .collect()
        }),
        (None, Err(std::env::VarError::NotPresent)) => Ok(configured),
        (None, Err(error)) => Err(error.into()),
    }
}

fn select_web_settings(
    config: WebConfig,
    options: (Option<u16>, Option<&str>),
    port_env: std::result::Result<String, std::env::VarError>,
    auth_env: std::result::Result<String, std::env::VarError>,
) -> Result<WebConfig> {
    let port = match options.0 {
        Some(port) => port,
        None => match port_env {
            Ok(value) => value
                .parse()
                .context("BMUX_WEB_PORT must be a port from 0 to 65535")?,
            Err(std::env::VarError::NotPresent) => config.port,
            Err(error) => return Err(error.into()),
        },
    };
    let auth_value = match options.1 {
        Some(value) => Some(value.to_owned()),
        None => match auth_env {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(error.into()),
        },
    };
    let auth = match auth_value.as_deref() {
        None => config.auth,
        Some("none") => WebAuth::None,
        Some("token") => WebAuth::Token,
        Some(_) => anyhow::bail!("web authentication must be none or token"),
    };
    Ok(WebConfig {
        port,
        auth,
        ..config
    })
}

/// Resolves explicit caller assets: CLI > environment > configured location.
/// Absence remains absence; assets do not implicitly enable browser output.
#[must_use]
pub fn assets(cli: Option<PathBuf>, configured: Option<PathBuf>) -> Option<PathBuf> {
    select_assets(cli, std::env::var_os("BMUX_WEB_ASSETS"), configured)
}

fn select_assets(
    cli: Option<PathBuf>,
    env: Option<std::ffi::OsString>,
    configured: Option<PathBuf>,
) -> Option<PathBuf> {
    cli.or_else(|| env.map(PathBuf::from)).or(configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_precedence_does_not_imply_enablement() {
        assert_eq!(select_assets(None, None, None), None);
        assert_eq!(
            select_assets(None, None, Some("config".into())),
            Some("config".into())
        );
        assert_eq!(
            select_assets(None, Some("env".into()), Some("config".into())),
            Some("env".into())
        );
        assert_eq!(
            select_assets(
                Some("cli".into()),
                Some("env".into()),
                Some("config".into())
            ),
            Some("cli".into())
        );
    }

    #[test]
    fn presenter_defaults_preserve_existing_serialization_and_modes_round_trip() {
        let defaults: crate::BmuxConfig = toml::from_str("").unwrap();
        assert_eq!(defaults.presenter.web.mode, crate::PresenterWebMode::Off);
        assert!(defaults.presenter.web.assets.is_none());
        assert!(!toml::to_string(&defaults).unwrap().contains("[presenter"));
        for (mode, expected) in [
            ("off", crate::PresenterWebMode::Off),
            ("mirror", crate::PresenterWebMode::Mirror),
            ("only", crate::PresenterWebMode::Only),
        ] {
            let config: crate::BmuxConfig = toml::from_str(&format!(
                "[presenter.web]\nmode = '{mode}'\nassets = '/browser/assets'\n"
            ))
            .unwrap();
            assert_eq!(config.presenter.web.mode, expected);
            assert_eq!(
                config.presenter.web.assets,
                Some(PathBuf::from("/browser/assets"))
            );
            let round_trip: crate::BmuxConfig =
                toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(round_trip.presenter, config.presenter);
        }
        assert!(
            toml::from_str::<crate::BmuxConfig>("[presenter.web]\nmode = 'invalid'\n").is_err()
        );
    }

    #[test]
    fn allowed_host_precedence_and_explicit_empty_override() {
        let configured = vec!["configured.example:8080".into()];
        let cli = vec!["cli.example:8080".into()];
        assert_eq!(
            select_allowed_hosts(
                Some(&cli),
                Ok("env.example:8080".into()),
                configured.clone()
            )
            .unwrap(),
            cli
        );
        assert!(
            select_allowed_hosts(Some(&[]), Ok("env.example:8080".into()), configured.clone())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            select_allowed_hosts(
                None,
                Ok(" a.example:8080, b.example:8080 ".into()),
                configured.clone()
            )
            .unwrap(),
            vec!["a.example:8080", "b.example:8080"]
        );
        assert!(
            select_allowed_hosts(None, Ok(String::new()), configured.clone())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            select_allowed_hosts(
                None,
                Err(std::env::VarError::NotPresent),
                configured.clone()
            )
            .unwrap(),
            configured
        );
    }

    #[test]
    fn web_ip_settings_precedence_and_validation() {
        let absent = || Err(std::env::VarError::NotPresent);
        let local = "127.0.0.1".parse().unwrap();
        let lan = "192.0.2.10".parse().unwrap();
        let wildcard = "0.0.0.0".parse().unwrap();
        assert_eq!(
            select_ip(None, absent(), Some(local), "host").unwrap(),
            Some(local)
        );
        assert_eq!(
            select_ip(None, Ok("192.0.2.10".into()), Some(local), "host").unwrap(),
            Some(lan)
        );
        assert_eq!(
            select_ip(Some(wildcard), Ok("invalid".into()), Some(local), "host").unwrap(),
            Some(wildcard)
        );
        assert_eq!(
            select_ip(None, absent(), None, "public_host").unwrap(),
            None
        );
        assert!(select_ip(None, Ok("hostname.example".into()), None, "host").is_err());
        assert!(select_ip(None, Ok(String::new()), None, "public_host").is_err());
        assert_eq!(
            select_ip(None, Ok("::1".into()), None, "host").unwrap(),
            Some("::1".parse().unwrap())
        );
        let config: WebConfig =
            toml::from_str("host = '0.0.0.0'\npublic_host = '192.0.2.10'\nauth = 'token'").unwrap();
        assert_eq!(config.host, wildcard);
        assert_eq!(config.public_host, Some(lan));
        assert_eq!(config.port, 8080);
        assert_eq!(config.auth, WebAuth::Token);
        assert!(toml::from_str::<WebConfig>("host = 'bad'").is_err());
    }

    #[test]
    fn web_settings_precedence_and_invalid_environment() {
        let absent = || Err(std::env::VarError::NotPresent);
        let defaults =
            select_web_settings(WebConfig::default(), (None, None), absent(), absent()).unwrap();
        assert_eq!(defaults.port, 8080);
        assert_eq!(defaults.auth, WebAuth::None);
        let configured = WebConfig {
            port: 9090,
            auth: WebAuth::Token,
            ..WebConfig::default()
        };
        let env = select_web_settings(
            configured.clone(),
            (None, None),
            Ok("8081".into()),
            Ok("none".into()),
        )
        .unwrap();
        assert_eq!(env.port, 8081);
        assert_eq!(env.auth, WebAuth::None);
        let cli = select_web_settings(
            configured.clone(),
            (Some(0), Some("token")),
            Ok("bad".into()),
            Ok("bad".into()),
        )
        .unwrap();
        assert_eq!(cli.port, 0);
        assert_eq!(cli.auth, WebAuth::Token);
        assert!(
            select_web_settings(
                configured.clone(),
                (None, None),
                Ok("65536".into()),
                absent()
            )
            .is_err()
        );
        assert!(
            select_web_settings(configured.clone(), (None, None), absent(), Ok("bad".into()))
                .is_err()
        );
        let inherited = select_web_settings(configured, (None, None), absent(), absent()).unwrap();
        assert_eq!(inherited.port, 9090);
        assert_eq!(inherited.auth, WebAuth::Token);
    }
}
