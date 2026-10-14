//! CLI syntax and policy. No terminal or host side effects during parsing.
use anyhow::{Context, Result, bail, ensure};
use bmux_presenter_runtime::protocol::InteractionMode;
use std::{net::IpAddr, path::PathBuf};

/// CLI values only: shared configuration resolution owns environment/defaults.
#[derive(Default, Debug)]
pub(crate) struct WebOptions {
    pub enabled: bool,
    pub broadcast: Option<uuid::Uuid>,
    pub assets: Option<PathBuf>,
    pub only: bool,
    pub port: Option<u16>,
    pub auth: Option<String>,
    pub host: Option<IpAddr>,
    pub public_host: Option<IpAddr>,
    pub allowed_hosts: Option<Vec<String>>,
}

impl WebOptions {
    fn parse(args: &mut Arguments<'_>) -> Result<Self> {
        let broadcast = args
            .value("--broadcast")?
            .map(|value| value.parse().context("--broadcast must be a source UUID"))
            .transpose()?;
        let enabled = args.flag("--web")?;
        let assets = args.value("--web-assets")?.map(PathBuf::from);
        let only = args.flag("--web-only")?;
        let port = args
            .value("--web-port")?
            .map(|value| value.parse().context("--web-port must be 0..65535"))
            .transpose()?;
        let auth = args.value("--web-auth")?;
        ensure!(
            auth.as_deref()
                .is_none_or(|auth| matches!(auth, "none" | "token")),
            "--web-auth must be none or token"
        );
        let host = args
            .value("--web-host")?
            .map(|value| value.parse().context("--web-host must be an IP address"))
            .transpose()?;
        let public_host = args
            .value("--web-public-host")?
            .map(|value| {
                value
                    .parse()
                    .context("--web-public-host must be an IP address")
            })
            .transpose()?;
        let allowed_hosts = args.value("--web-allowed-hosts")?.map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|host| !host.is_empty())
                .map(str::to_owned)
                .collect()
        });
        Ok(Self {
            broadcast,
            enabled,
            assets,
            only,
            port,
            auth,
            host,
            public_host,
            allowed_hosts,
        })
    }
}

pub(crate) enum Command {
    Play {
        source: PathBuf,
        presenter: bool,
        reload: bool,
        allow_playbooks: bool,
        web: WebOptions,
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
    pub allow_playbooks: bool,
    pub web: WebOptions,
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
                let allow_playbooks = args.flag("--allow-playbooks")?;
                let web = WebOptions::parse(&mut args)?;
                args.finish()?;
                Ok(Self::Play {
                    source: target.into(),
                    presenter,
                    reload,
                    allow_playbooks,
                    web,
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
                let allow_playbooks = args.flag("--allow-playbooks")?;
                let web = WebOptions::parse(&mut args)?;
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
                    allow_playbooks,
                    web,
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
    fn broadcast_is_explicit_validated_and_independent_of_web() {
        let source = "12345678-1234-4234-8234-123456789abc";
        let Command::Play { web, .. } = parse("play", &["deck.md", "--broadcast", source]).unwrap()
        else {
            panic!()
        };
        assert_eq!(web.broadcast.unwrap().to_string(), source);
        assert!(!web.enabled && !web.only);
        assert!(parse("play", &["deck.md", "--broadcast", "invalid"]).is_err());
        assert!(parse("join", &["bmux:host", "--broadcast", source]).is_err());
        assert!(parse("check", &["deck.md", "--broadcast", source]).is_err());
        for flag in ["--web", "--web-only"] {
            let Command::Play { web, .. } =
                parse("play", &["deck.md", flag, "--broadcast", source]).unwrap()
            else {
                panic!()
            };
            assert!(web.broadcast.is_some());
            assert_eq!(web.only, flag == "--web-only");
        }
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
    fn executable_input_approval_is_creation_only_and_explicit() {
        assert!(matches!(
            parse("play", &["deck.md"]).unwrap(),
            Command::Play {
                allow_playbooks: false,
                ..
            }
        ));
        assert!(matches!(
            parse("play", &["deck.md", "--allow-playbooks"]).unwrap(),
            Command::Play {
                allow_playbooks: true,
                ..
            }
        ));
        let Command::Host(options) = parse(
            "host",
            &[
                "deck.md",
                "--managed",
                "--presenter-credential",
                "p",
                "--public-read",
                "--allow-playbooks",
            ],
        )
        .unwrap() else {
            panic!("host")
        };
        assert!(options.allow_playbooks);
        assert!(parse("check", &["deck.md", "--allow-playbooks"]).is_err());
        assert!(parse("join", &["bmux:host", "--allow-playbooks"]).is_err());
        assert!(
            parse(
                "play",
                &["deck.md", "--allow-playbooks", "--allow-playbooks"]
            )
            .is_err()
        );
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
    fn web_flags_are_play_and_host_only_and_preserve_explicit_overrides() {
        let flags = [
            "deck.md",
            "--web-assets",
            "browser",
            "--web-only",
            "--web-port",
            "0",
            "--web-auth",
            "token",
            "--web-host",
            "127.0.0.1",
            "--web-public-host",
            "::1",
            "--web-allowed-hosts",
            "a.example,b.example",
        ];
        let Command::Play { web, .. } = parse("play", &flags).unwrap() else {
            panic!("expected play");
        };
        assert!(web.only);
        assert_eq!(web.assets, Some(PathBuf::from("browser")));
        assert_eq!(web.port, Some(0));
        assert_eq!(web.auth.as_deref(), Some("token"));
        assert_eq!(web.host, Some("127.0.0.1".parse().unwrap()));
        assert_eq!(web.public_host, Some("::1".parse().unwrap()));
        assert_eq!(
            web.allowed_hosts,
            Some(vec!["a.example".into(), "b.example".into()])
        );
        let mut host = flags.to_vec();
        host.extend(["--managed", "--presenter-credential", "p", "--public-read"]);
        assert!(matches!(parse("host", &host).unwrap(), Command::Host(_)));
        assert!(parse("join", &["bmux:host", "--web-only"]).is_err());
        assert!(parse("check", &["deck.md", "--web-assets", "browser"]).is_err());
        for invalid in [
            vec!["--web-port", "65536"],
            vec!["--web-auth", "password"],
            vec!["--web-host", "localhost"],
            vec!["--web-public-host", "example.com"],
            vec!["--web-only", "--web-only"],
            vec!["--web-assets"],
        ] {
            let mut args = vec!["deck.md"];
            args.extend(invalid);
            assert!(parse("play", &args).is_err());
        }
    }
    #[test]
    fn simple_web_flags_enable_without_assets() {
        for verb in ["play", "host"] {
            for flag in ["--web", "--web-only"] {
                let mut flags = vec!["deck.md", flag];
                if verb == "host" {
                    flags.extend([
                        "--managed",
                        "--presenter-credential",
                        "private",
                        "--public-read",
                    ]);
                }
                let web = match parse(verb, &flags).unwrap() {
                    Command::Play { web, .. } => web,
                    Command::Host(options) => options.web,
                    _ => panic!("expected play or host"),
                };
                assert_eq!(web.enabled, flag == "--web");
                assert_eq!(web.only, flag == "--web-only");
                assert!(web.assets.is_none());
            }
        }
        assert!(parse("join", &["bmux:host", "--web"]).is_err());
        assert!(parse("check", &["deck.md", "--web"]).is_err());
        assert!(parse("play", &["deck.md", "--web", "--web"]).is_err());
    }

    #[test]
    fn web_network_flags_do_not_imply_presenter_activation() {
        let Command::Play { web, .. } = parse("play", &["deck.md", "--web-port", "8080"]).unwrap()
        else {
            panic!("expected play");
        };
        assert!(!web.enabled);
        assert!(!web.only);
        assert!(web.assets.is_none());
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
