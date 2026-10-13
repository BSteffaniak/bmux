//! CLI syntax and policy. No terminal or host side effects during parsing.
use anyhow::{Context, Result, bail, ensure};
use bmux_presenter_runtime::protocol::InteractionMode;
use std::path::PathBuf;

pub(crate) enum Command {
    Play {
        source: PathBuf,
        presenter: bool,
        reload: bool,
    },
    Check {
        source: PathBuf,
    },
    Host(HostOptions),
    Join(JoinOptions),
}

pub(crate) struct HostOptions {
    pub source: PathBuf,
    pub managed: bool,
    pub listen: String,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub presenter_credential: PathBuf,
    pub audience_credential: Option<PathBuf>,
    pub public_read: bool,
    pub reload: bool,
}

#[derive(Clone)]
pub(crate) struct JoinOptions {
    pub endpoint: String,
    pub ca: Option<PathBuf>,
    pub server_name: Option<String>,
    pub credential: Option<PathBuf>,
    pub mode: InteractionMode,
    pub presenter: bool,
}

impl Command {
    pub fn parse(name: &str, arguments: &[String]) -> Result<Self> {
        ensure!(
            name != "present",
            "present is a namespace; use explicit play, host, join or check"
        );
        let name = name.strip_prefix("present-").unwrap_or(name);
        let mut args = Arguments::new(arguments);
        let target = args
            .positional()?
            .context("a deck path or endpoint is required")?;
        match name {
            "play" => {
                let presenter = args.flag("--presenter")?;
                let reload = !args.flag("--no-reload")?;
                args.finish()?;
                Ok(Self::Play {
                    source: target.into(),
                    presenter,
                    reload,
                })
            }
            "check" => {
                args.finish()?;
                Ok(Self::Check {
                    source: target.into(),
                })
            }
            "host" => {
                let managed = args.flag("--managed")?;
                let listen = args
                    .value("--listen")?
                    .unwrap_or_else(|| "127.0.0.1:7443".into());
                let cert = args.value("--cert")?.map(PathBuf::from);
                let key = args.value("--key")?.map(PathBuf::from);
                let presenter_credential = args
                    .value("--presenter-credential")?
                    .context(
                        "host requires --presenter-credential FILE (private scoped credential)",
                    )?
                    .into();
                let audience_credential = args.value("--audience-credential")?.map(PathBuf::from);
                let public_read = args.flag("--public-read")?;
                let reload = !args.flag("--no-reload")?;
                ensure!(
                    public_read || audience_credential.is_some(),
                    "host requires --audience-credential FILE or explicit --public-read"
                );
                ensure!(
                    managed || (cert.is_some() && key.is_some()),
                    "standalone host requires --cert FILE and --key FILE"
                );
                ensure!(
                    !managed || (cert.is_none() && key.is_none() && listen == "127.0.0.1:7443"),
                    "--managed uses BMUX service transport; TLS listener options are standalone-only"
                );
                args.finish()?;
                Ok(Self::Host(HostOptions {
                    source: target.into(),
                    managed,
                    listen,
                    cert,
                    key,
                    presenter_credential,
                    audience_credential,
                    public_read,
                    reload,
                }))
            }
            "join" => {
                let ca = args.value("--ca")?.map(PathBuf::from);
                let server_name = args.value("--server-name")?;
                let credential = args.value("--credential")?.map(PathBuf::from);
                let mode = match args.value("--mode")?.as_deref().unwrap_or("receive-only") {
                    "receive-only" => InteractionMode::ReceiveOnly,
                    "submit-only" => InteractionMode::SubmitOnly,
                    "shared" => InteractionMode::Shared,
                    "independent" => InteractionMode::Independent,
                    other => bail!("invalid interaction mode: {other}"),
                };
                let presenter = args.flag("--presenter")?;
                ensure!(
                    !presenter || credential.is_some(),
                    "presenter requires --credential FILE"
                );
                if target.starts_with("bmux:") {
                    ensure!(
                        ca.is_none() && server_name.is_none(),
                        "BMUX join uses BMUX service transport"
                    );
                } else {
                    ensure!(
                        target.starts_with("tls://"),
                        "join endpoint must be tls://HOST:PORT or bmux:HOST_ID"
                    );
                    ensure!(
                        ca.is_some(),
                        "TLS join requires --ca FILE (explicit host trust)"
                    );
                }
                args.finish()?;
                Ok(Self::Join(JoinOptions {
                    endpoint: target,
                    ca,
                    server_name,
                    credential,
                    mode,
                    presenter,
                }))
            }
            other => bail!("unknown presentation command: {other}"),
        }
    }
}

struct Arguments<'a> {
    values: &'a [String],
    used: Vec<bool>,
}
impl<'a> Arguments<'a> {
    fn new(values: &'a [String]) -> Self {
        Self {
            values,
            used: vec![false; values.len()],
        }
    }
    fn positional(&mut self) -> Result<Option<String>> {
        // CLI registry normalizes arguments to positionals first. Do not interpret
        // option values as decks; direct invocation must use the same order.
        let Some(value) = self.values.first() else {
            return Ok(None);
        };
        ensure!(
            !value.starts_with('-'),
            "deck path or endpoint must precede options (use ./ for hyphen-prefixed paths)"
        );
        self.used[0] = true;
        Ok(Some(value.clone()))
    }
    fn index(&self, flag: &str) -> Result<Option<usize>> {
        let found: Vec<_> = self
            .values
            .iter()
            .enumerate()
            .filter(|(index, value)| !self.used[*index] && value.as_str() == flag)
            .map(|(index, _)| index)
            .collect();
        ensure!(found.len() <= 1, "duplicate {flag}");
        Ok(found.first().copied())
    }
    fn flag(&mut self, flag: &str) -> Result<bool> {
        if let Some(index) = self.index(flag)? {
            self.used[index] = true;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn value(&mut self, flag: &str) -> Result<Option<String>> {
        let Some(index) = self.index(flag)? else {
            return Ok(None);
        };
        let value = self
            .values
            .get(index + 1)
            .filter(|value| !value.starts_with('-'))
            .with_context(|| format!("{flag} requires a value"))?;
        ensure!(!self.used[index + 1], "{flag} value already consumed");
        self.used[index] = true;
        self.used[index + 1] = true;
        Ok(Some(value.clone()))
    }
    fn finish(self) -> Result<()> {
        if let Some((_, value)) = self.values.iter().enumerate().find(|(i, _)| !self.used[*i]) {
            bail!("unexpected presentation argument: {value}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(name: &str, args: &[&str]) -> Result<Command> {
        Command::parse(
            name,
            &args.iter().map(ToString::to_string).collect::<Vec<_>>(),
        )
    }
    #[test]
    fn explicit_play_only_and_namespace_is_not_a_leaf() {
        assert!(parse("present", &["deck.md"]).is_err());
        assert!(parse("present", &["play", "deck.md"]).is_err());
        for (name, args) in [("play", vec!["deck.md"]), ("present-play", vec!["deck.md"])] {
            assert!(matches!(
                parse(name, &args).unwrap(),
                Command::Play {
                    reload: true,
                    presenter: false,
                    ..
                }
            ));
        }
    }
    #[test]
    fn hosting_is_private_unless_explicitly_public() {
        assert!(
            parse(
                "host",
                &["deck.md", "--managed", "--presenter-credential", "p"]
            )
            .is_err()
        );
        assert!(
            parse(
                "host",
                &[
                    "deck.md",
                    "--managed",
                    "--presenter-credential",
                    "p",
                    "--public-read"
                ]
            )
            .is_ok()
        );
        assert!(
            parse(
                "host",
                &["deck.md", "--presenter-credential", "p", "--public-read"]
            )
            .is_err()
        );
    }
    #[test]
    fn tls_requires_explicit_trust_and_rejects_unknown_flags() {
        assert!(parse("join", &["tls://localhost:7443"]).is_err());
        assert!(parse("join", &["tls://localhost:7443", "--ca", "host.pem"]).is_ok());
        assert!(
            parse(
                "join",
                &["tls://localhost:7443", "--ca", "host.pem", "--insecure"]
            )
            .is_err()
        );
        assert!(parse("play", &["deck.md", "--presenter", "--presenter"]).is_err());
    }
    #[test]
    fn supports_all_four_modes() {
        for mode in ["receive-only", "submit-only", "shared", "independent"] {
            assert!(parse("join", &["bmux:host", "--mode", mode]).is_ok());
        }
        assert!(parse("join", &["bmux:host", "--mode", "unknown"]).is_err());
        assert!(parse("join", &["bmux:host", "--presenter"]).is_err());
    }
}
