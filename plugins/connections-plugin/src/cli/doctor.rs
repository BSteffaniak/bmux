//! `bmux remote doctor <host> [--json] [--timeout SECS]`: diagnose the path to
//! a saved host step by step (resolve, endpoint, relay, dial, handshake,
//! hello, a typed round trip), timing each, and name the first failure:
//! unknown host, host offline, device revoked or not paired, host key
//! changed, relay unreachable, or incompatible version. Runs without a
//! local server and never changes the saved host.
use super::{CLIENT_PROFILE, flag, option, positional};
use crate::config::RelayPolicy;
use crate::identity::DeviceKey;
use crate::transport::manager::{
    ClientTransport, ConnectionManager, SESSION_IDLE_TIMEOUT, refused,
};
use crate::transport::{SESSION_ALPN, close, endpoint_addr, erased, path_of};
use anyhow::Result;
use bmux_client::{BmuxClient, ClientError};
use bmux_connections_plugin_api::hosting_state;
use bmux_plugin_sdk::NativeCommandContext;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const RELAY_WAIT: Duration = Duration::from_secs(5);

/// Why the diagnosis stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    UnknownHost,
    HostOffline,
    DeviceRevoked,
    DeviceNotPaired,
    HostKeyChanged,
    RelayUnreachable,
    IncompatibleVersion,
    Failed,
}

impl Failure {
    const fn code(self) -> &'static str {
        match self {
            Self::UnknownHost => "unknown_host",
            Self::HostOffline => "host_offline",
            Self::DeviceRevoked => "device_revoked",
            Self::DeviceNotPaired => "device_not_paired",
            Self::HostKeyChanged => "host_key_changed",
            Self::RelayUnreachable => "relay_unreachable",
            Self::IncompatibleVersion => "incompatible_version",
            Self::Failed => "failed",
        }
    }
}

struct Step {
    name: &'static str,
    ok: bool,
    elapsed: Duration,
    detail: String,
}

struct Report {
    host: String,
    steps: Vec<Step>,
    path: Option<&'static str>,
    rtt: Option<Duration>,
    failure: Option<(Failure, String)>,
}

impl Report {
    fn pass(&mut self, name: &'static str, started: Instant, detail: impl Into<String>) {
        self.steps.push(Step {
            name,
            ok: true,
            elapsed: started.elapsed(),
            detail: detail.into(),
        });
    }

    /// Record a failed step that ends the diagnosis with `failure`.
    fn fail(
        &mut self,
        name: &'static str,
        started: Instant,
        failure: Failure,
        message: String,
    ) -> Stop {
        self.steps.push(Step {
            name,
            ok: false,
            elapsed: started.elapsed(),
            detail: message.clone(),
        });
        self.failure = Some((failure, message));
        Stop
    }
}

/// The diagnosis stopped at a failure recorded in the report.
struct Stop;

pub async fn run(context: &NativeCommandContext) -> Result<i32> {
    let args = &context.arguments;
    let [host] = positional(args, &["--timeout"]).try_into().map_err(|_| {
        anyhow::anyhow!("usage: bmux remote doctor <host> [--json] [--timeout SECS]")
    })?;
    let timeout = option(args, "--timeout").map_or(Ok(DEFAULT_TIMEOUT), |text| {
        text.parse::<u64>()
            .ok()
            .filter(|secs| (1..=120).contains(secs))
            .map(Duration::from_secs)
            .ok_or_else(|| anyhow::anyhow!("--timeout takes 1..=120 seconds"))
    })?;
    let paths = super::paths(context);
    let mut report = Report {
        host,
        steps: Vec::new(),
        path: None,
        rtt: None,
        failure: None,
    };
    let _ = diagnose(&paths, timeout, &mut report).await;
    if flag(args, "--json") {
        println!("{}", serde_json::to_string_pretty(&report_json(&report))?);
    } else {
        print_report(&report);
    }
    Ok(i32::from(report.failure.is_some()))
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn report_json(report: &Report) -> Value {
    json!({
        "host": report.host,
        "ok": report.failure.is_none(),
        "failure": report.failure.as_ref().map(|(failure, message)| json!({
            "kind": failure.code(),
            "message": message,
        })),
        "path": report.path,
        "rtt_ms": report.rtt.map(millis),
        "steps": report.steps.iter().map(|step| json!({
            "step": step.name,
            "ok": step.ok,
            "ms": millis(step.elapsed),
            "detail": step.detail,
        })).collect::<Vec<_>>(),
    })
}

fn print_report(report: &Report) {
    println!("Diagnosing {}:", report.host);
    for step in &report.steps {
        println!(
            "  {:<4} {:<10} {:>6} ms  {}",
            if step.ok { "ok" } else { "FAIL" },
            step.name,
            millis(step.elapsed),
            step.detail
        );
    }
    match &report.failure {
        None => println!(
            "{} is reachable{}.",
            report.host,
            match (report.path, report.rtt) {
                (Some(path), Some(rtt)) => format!(" via {path}, rtt {}ms", millis(rtt)),
                (Some(path), None) => format!(" via {path}"),
                _ => String::new(),
            }
        ),
        Some((failure, message)) => println!("Problem ({}): {message}", failure.code()),
    }
}

/// Classify a dial or handshake error.
fn classify_connect(alias: &str, error: &str, relay_ok: Option<bool>) -> (Failure, String) {
    let lower = error.to_ascii_lowercase();
    if lower.contains("invalid peer certificate") {
        return (
            Failure::HostKeyChanged,
            format!("{:#}", crate::known_hosts::host_key_changed(alias)),
        );
    }
    if lower.contains("application protocol")
        || lower.contains("applicationprotocol")
        || lower.contains("known protocol")
    {
        return (
            Failure::IncompatibleVersion,
            format!(
                "{alias} does not speak this device's remote protocol ({}); update bmux on both machines",
                String::from_utf8_lossy(SESSION_ALPN)
            ),
        );
    }
    if relay_ok == Some(false) {
        return (
            Failure::RelayUnreachable,
            format!(
                "the relay is unreachable from this device and {alias} did not answer on its direct addresses; check the network or the relay settings (`remote.relay`)"
            ),
        );
    }
    (
        Failure::HostOffline,
        format!(
            "{alias} did not answer: it is offline, unreachable from this network, or has remote access disabled (`bmux remote status` on the host)"
        ),
    )
}

/// What the resolve step found.
struct Resolved {
    known: crate::known_hosts::KnownHost,
    host_key: iroh::PublicKey,
    device: DeviceKey,
    transport: ClientTransport,
}

/// Run the steps in order, recording each; stops at the first failure.
async fn diagnose(
    paths: &bmux_config::ConfigPaths,
    timeout: Duration,
    report: &mut Report,
) -> std::result::Result<(), Stop> {
    let resolved = resolve(paths, report)?;
    let endpoint = bind(&resolved, report).await?;
    let relay_ok = relay(&resolved.transport, &endpoint, timeout, report).await;
    let connection = handshake(&resolved, &endpoint, relay_ok, timeout, report).await?;
    let result = admit_and_call(&resolved, &connection, timeout, report).await;
    connection.close(close::DONE.into(), b"doctor done");
    result
}

/// Step 1, resolve: the saved host, this device's key and endpoint settings.
fn resolve(
    paths: &bmux_config::ConfigPaths,
    report: &mut Report,
) -> std::result::Result<Resolved, Stop> {
    let started = Instant::now();
    let alias = report.host.clone();
    let connector = crate::connector::SavedHostConnector::new(paths.clone());
    let known = match connector.saved(&alias) {
        Ok(Some(known)) => known,
        Ok(None) => {
            return Err(report.fail(
                "resolve",
                started,
                Failure::UnknownHost,
                format!("no saved host named '{alias}'; see `bmux remote hosts`"),
            ));
        }
        Err(error) => {
            return Err(report.fail("resolve", started, Failure::Failed, format!("{error:#}")));
        }
    };
    let Ok(host_key) = known.host_key.parse::<iroh::PublicKey>() else {
        return Err(report.fail(
            "resolve",
            started,
            Failure::Failed,
            format!("saved host '{alias}' has an invalid host key"),
        ));
    };
    let setup = DeviceKey::load_or_create(&paths.state_dir, CLIENT_PROFILE)
        .and_then(|device| Ok((device, crate::connector::client_transport(paths)?)));
    let (device, transport) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            return Err(report.fail("resolve", started, Failure::Failed, format!("{error:#}")));
        }
    };
    report.pass(
        "resolve",
        started,
        format!(
            "host key {}, {} address hint(s), relay hint {}",
            super::host::short(&known.host_key),
            known.addrs.len(),
            known.relay.as_deref().unwrap_or("none")
        ),
    );
    Ok(Resolved {
        known,
        host_key,
        device,
        transport,
    })
}

/// Step 2, endpoint: this process's one endpoint (bound on first use).
async fn bind(
    resolved: &Resolved,
    report: &mut Report,
) -> std::result::Result<iroh::Endpoint, Stop> {
    let started = Instant::now();
    let manager = ConnectionManager::shared(resolved.device.secret(), &resolved.transport);
    let endpoint = match manager.endpoint().await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            return Err(report.fail("endpoint", started, Failure::Failed, format!("{error:#}")));
        }
    };
    let mut bound: Vec<_> = endpoint
        .bound_sockets()
        .iter()
        .map(ToString::to_string)
        .collect();
    bound.sort();
    report.pass("endpoint", started, format!("bound {}", bound.join(", ")));
    Ok(endpoint)
}

/// Step 3, relay: whether this device reaches a relay. Not fatal by itself (a
/// direct path may work); it explains a later dial failure. `None` when
/// relays are disabled.
async fn relay(
    transport: &ClientTransport,
    endpoint: &iroh::Endpoint,
    timeout: Duration,
    report: &mut Report,
) -> Option<bool> {
    let started = Instant::now();
    if matches!(transport.relay, RelayPolicy::Disabled) {
        report.pass("relay", started, "relays disabled (direct only)");
        return None;
    }
    let wait = RELAY_WAIT.min(timeout);
    let online = tokio::time::timeout(wait, endpoint.online()).await.is_ok();
    let relay = crate::host::relay_state(endpoint);
    let url = relay.url.unwrap_or_else(|| "none".to_owned());
    if online {
        report.pass(
            "relay",
            started,
            format!(
                "{url}{}",
                relay.latency.map_or_else(String::new, |latency| format!(
                    ", latency {}ms",
                    millis(latency)
                ))
            ),
        );
    } else {
        report.steps.push(Step {
            name: "relay",
            ok: false,
            elapsed: started.elapsed(),
            detail: format!(
                "not connected to a relay ({url}) within {}s",
                wait.as_secs()
            ),
        });
    }
    Some(online)
}

/// Steps 4 and 5, dial and handshake (QUIC and TLS: proves the pinned host key).
async fn handshake(
    resolved: &Resolved,
    endpoint: &iroh::Endpoint,
    relay_ok: Option<bool>,
    timeout: Duration,
    report: &mut Report,
) -> std::result::Result<iroh::endpoint::Connection, Stop> {
    let alias = report.host.clone();
    let known = &resolved.known;
    let deadline = Instant::now() + timeout;
    let started = Instant::now();
    let addr = match endpoint_addr(resolved.host_key, &known.addrs, known.relay.as_deref()) {
        Ok(addr) => addr,
        Err(error) => {
            return Err(report.fail("dial", started, Failure::Failed, format!("{error:#}")));
        }
    };
    let options = iroh::endpoint::ConnectOptions::new().with_transport_config(
        iroh::endpoint::QuicTransportConfig::builder()
            .max_idle_timeout(SESSION_IDLE_TIMEOUT.try_into().ok())
            .build(),
    );
    let connecting = match tokio::time::timeout(
        timeout,
        endpoint.connect_with_opts(addr, SESSION_ALPN, options),
    )
    .await
    {
        Ok(Ok(connecting)) => connecting,
        Ok(Err(error)) => {
            let (failure, message) = classify_connect(&alias, &format!("{error:#}"), relay_ok);
            return Err(report.fail("dial", started, failure, format!("{message} ({error})")));
        }
        Err(_) => {
            let (failure, message) = classify_connect(&alias, "timed out", relay_ok);
            return Err(report.fail("dial", started, failure, message));
        }
    };
    report.pass(
        "dial",
        started,
        format!("{} address hint(s)", known.addrs.len()),
    );
    let started = Instant::now();
    let remaining = deadline.saturating_duration_since(Instant::now());
    let connection = match tokio::time::timeout(remaining, connecting).await {
        Ok(Ok(connection)) => connection,
        Ok(Err(error)) => {
            let text = format!("{:#}", anyhow::Error::new(error));
            let (failure, message) = classify_connect(&alias, &text, relay_ok);
            let message = if failure == Failure::HostOffline {
                format!("{message} ({text})")
            } else {
                message
            };
            return Err(report.fail("handshake", started, failure, message));
        }
        Err(_) => {
            let (failure, message) = classify_connect(&alias, "timed out", relay_ok);
            return Err(report.fail("handshake", started, failure, message));
        }
    };
    let (path, rtt) = path_of(&connection);
    report.path = Some(path);
    report.rtt = rtt;
    report.pass(
        "handshake",
        started,
        format!(
            "host key verified, path {path}{}",
            rtt.map_or_else(String::new, |rtt| format!(", rtt {}ms", millis(rtt)))
        ),
    );
    Ok(connection)
}

/// The host's refusal of this device, read from the connection's close.
fn refusal(alias: &str, connection: &iroh::endpoint::Connection) -> Option<(Failure, String)> {
    refused(connection).map(|refused| {
        let failure = if refused.is_revoked() {
            Failure::DeviceRevoked
        } else if refused.is_permanent() {
            Failure::DeviceNotPaired
        } else {
            Failure::HostOffline
        };
        (failure, format!("{alias}: {refused}"))
    })
}

/// Step 6, hello: the host admits this device (or closes the connection), and
/// step 7, a typed round trip (an observe route every role may call).
async fn admit_and_call(
    resolved: &Resolved,
    connection: &iroh::endpoint::Connection,
    timeout: Duration,
    report: &mut Report,
) -> std::result::Result<(), Stop> {
    let alias = report.host.clone();
    let started = Instant::now();
    let hello = async {
        let (send, recv) = connection.open_bi().await?;
        BmuxClient::connect_with_bridge_stream(
            erased(send, recv),
            timeout,
            "bmux-remote-doctor".to_owned(),
            resolved.known.principal,
        )
        .await
        .map_err(anyhow::Error::new)
    };
    let mut client = match hello.await {
        Ok(client) => client,
        Err(error) => {
            // The host closes a refused device's connection right after the
            // handshake; give the close a moment to arrive.
            let _ = tokio::time::timeout(Duration::from_secs(2), connection.closed()).await;
            let (failure, message) = refusal(&alias, connection).unwrap_or_else(|| {
                if matches!(
                    error.downcast_ref::<ClientError>(),
                    Some(ClientError::ProtocolIncompatible { .. })
                ) {
                    (
                        Failure::IncompatibleVersion,
                        format!(
                            "{alias} runs an incompatible bmux protocol ({error:#}); update bmux on both machines"
                        ),
                    )
                } else {
                    (Failure::Failed, format!("{alias}: hello failed: {error:#}"))
                }
            });
            return Err(report.fail("hello", started, failure, message));
        }
    };
    report.pass("hello", started, "device admitted");

    let started = Instant::now();
    match hosting_state::client::status(&mut client).await {
        Ok(status) => report.pass(
            "round-trip",
            started,
            format!(
                "hosting-state.status: remote access {} ({})",
                if status.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                status.source
            ),
        ),
        Err(error) => {
            let _ = tokio::time::timeout(Duration::from_millis(500), connection.closed()).await;
            let (failure, message) = refusal(&alias, connection).unwrap_or_else(|| {
                (
                    Failure::Failed,
                    format!("{alias}: typed call failed: {error:#}"),
                )
            });
            return Err(report.fail("round-trip", started, failure, message));
        }
    }
    let (path, rtt) = path_of(connection);
    report.path = Some(path);
    report.rtt = rtt.or(report.rtt);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_errors_are_classified() {
        assert_eq!(
            classify_connect("desk", "invalid peer certificate: UnknownIssuer", None).0,
            Failure::HostKeyChanged
        );
        assert_eq!(
            classify_connect("desk", "peer doesn't support any known protocol", None).0,
            Failure::IncompatibleVersion
        );
        assert_eq!(
            classify_connect("desk", "received fatal alert: NoApplicationProtocol", None).0,
            Failure::IncompatibleVersion
        );
        assert_eq!(
            classify_connect("desk", "timed out", Some(false)).0,
            Failure::RelayUnreachable
        );
        assert_eq!(
            classify_connect("desk", "timed out", Some(true)).0,
            Failure::HostOffline
        );
        assert_eq!(
            classify_connect("desk", "timed out", None).0,
            Failure::HostOffline
        );
    }
}
