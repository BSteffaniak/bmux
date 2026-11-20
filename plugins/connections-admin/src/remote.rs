use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, ConfigPaths, ConnectionTargetConfig, ConnectionTransport};
use std::process::Command as ProcessCommand;

pub enum ProbeTarget<'a> {
    Local,
    Ssh(&'a str),
    Tls {
        label: &'a str,
        host: &'a str,
        port: u16,
    },
    Iroh(&'a str),
}
/// Scoped operations on a target already resolved by the caller. Implementations retain their authentication policy.
pub trait ProbeTransport {
    fn target(&self) -> ProbeTarget<'_>;
    fn ping(&self, name: &'static str) -> Result<()>;
    fn ssh(&self, argv: &[&str]) -> Result<()>;
    fn install(&self) -> Result<()>;
}
pub fn run_remote_test(target: &str, probe: &dyn ProbeTransport) -> Result<u8> {
    match probe.target() {
        ProbeTarget::Local => {
            probe.ping("bmux-cli-remote-test")?;
            println!("target '{target}' OK (local)");
            Ok(0)
        }
        ProbeTarget::Ssh(label) => {
            probe.ssh(&["server", "status", "--json"])?;
            println!("target '{}' OK (ssh)", label);
            Ok(0)
        }
        ProbeTarget::Tls {
            label,
            host: _,
            port: _,
        } => {
            probe.ping("bmux-cli-remote-test-tls")?;
            println!("target '{}' OK (tls)", label);
            Ok(0)
        }
        ProbeTarget::Iroh(label) => {
            probe.ping("bmux-cli-remote-test-iroh")?;
            println!("target '{}' OK (iroh)", label);
            Ok(0)
        }
    }
}
pub fn run_remote_doctor(target: &str, probe: &dyn ProbeTransport, fix: bool) -> Result<u8> {
    println!("remote doctor: target='{target}' fix={fix}");
    match probe.target() {
        ProbeTarget::Local => {
            print_doctor_step_ok("transport", "local");
            probe.ping("bmux-cli-remote-doctor")?;
            print_doctor_step_ok("server", "local server reachable");
            Ok(0)
        }
        ProbeTarget::Ssh(label) => {
            let version = ProcessCommand::new("ssh")
                .arg("-V")
                .output()
                .context("failed executing ssh -V")?;
            if !version.status.success() {
                anyhow::bail!("ssh binary unavailable");
            }
            let stderr = String::from_utf8_lossy(&version.stderr);
            if !stderr.trim().is_empty() {
                print_doctor_step_ok("ssh", stderr.trim());
            }
            if let Err(error) = probe.ssh(&["--version"]) {
                if fix {
                    print_doctor_step_warn(
                        "bmux",
                        "remote bmux missing/unhealthy; attempting install-server fix",
                    );
                    probe.install()?;
                    print_doctor_step_ok("bmux", "install-server fix succeeded");
                } else {
                    return Err(error);
                }
            } else {
                print_doctor_step_ok("bmux", "remote bmux binary is available");
            }
            probe.ssh(&["server", "status", "--json"])?;
            print_doctor_step_ok("server", "remote server status check succeeded");
            println!("doctor result: OK ({})", label);
            Ok(0)
        }
        ProbeTarget::Tls { label, host, port } => {
            probe.ping("bmux-cli-remote-doctor-tls")?;
            print_doctor_step_ok(
                "tls",
                &format!("handshake and ping succeeded ({}:{})", host, port),
            );
            println!("doctor result: OK ({})", label);
            Ok(0)
        }
        ProbeTarget::Iroh(label) => {
            probe.ping("bmux-cli-remote-doctor-iroh")?;
            print_doctor_step_ok("iroh", "connectivity and ping succeeded");
            println!("doctor result: OK ({})", label);
            Ok(0)
        }
    }
}
fn print_doctor_step_ok(step: &str, message: &str) {
    println!("[OK] {step}: {message}");
}
fn print_doctor_step_warn(step: &str, message: &str) {
    println!("[WARN] {step}: {message}");
}
pub fn run_remote_list(as_json: bool) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let mut entries = config
        .connections
        .targets
        .iter()
        .map(|(name, value)| {
            let transport = match value.transport {
                ConnectionTransport::Local => "local",
                ConnectionTransport::Ssh => "ssh",
                ConnectionTransport::Tls => "tls",
                ConnectionTransport::Iroh => "iroh",
            };
            serde_json::json!({
                "name": name,
                "transport": transport,
                "host": value.host,
                "user": value.user,
                "port": value.port,
                "default_session": value.default_session,
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left["name"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["name"].as_str().unwrap_or_default())
    });

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).context("failed encoding target list")?
        );
        return Ok(0);
    }

    if entries.is_empty() {
        println!("no configured targets");
        return Ok(0);
    }

    for entry in &entries {
        let name = entry["name"].as_str().unwrap_or("-");
        let transport = entry["transport"].as_str().unwrap_or("-");
        let host = entry["host"].as_str().unwrap_or("-");
        let recent = config
            .connections
            .recent_targets
            .iter()
            .position(|value| value == name)
            .map_or("", |_| "* ");
        println!("{recent}{name}\t{transport}\t{host}");
    }
    Ok(0)
}
fn save_remote_target_state_overlay(
    name: &str,
    target: &ConnectionTargetConfig,
    set_default: bool,
) -> Result<()> {
    let paths = ConfigPaths::default();
    let path = paths.state_config_file();
    let mut overlay = if path.exists() {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed reading state config overlay {}", path.display()))?;
        toml::from_str::<toml::Value>(&contents)
            .with_context(|| format!("failed parsing state config overlay {}", path.display()))?
    } else {
        toml::Value::Table(toml::Table::new())
    };

    let table = overlay
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("state config overlay root must be a TOML table"))?;
    let connections = table
        .entry("connections")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("state config overlay connections must be a TOML table"))?;
    let targets = connections
        .entry("targets")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            anyhow::anyhow!("state config overlay connections.targets must be a TOML table")
        })?;
    targets.insert(
        name.to_string(),
        toml::Value::try_from(target).context("failed serializing remote target")?,
    );
    if set_default {
        connections.insert(
            "default_target".to_string(),
            toml::Value::String(name.to_string()),
        );
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating state config overlay directory {}",
                parent.display()
            )
        })?;
    }
    let contents =
        toml::to_string_pretty(&overlay).context("failed serializing state config overlay")?;
    std::fs::write(&path, contents)
        .with_context(|| format!("failed writing state config overlay {}", path.display()))?;
    Ok(())
}
fn parse_ssh_target_parts(target: &str) -> Result<(Option<String>, String, Option<u16>)> {
    let mut raw = target.trim();
    if let Some(without_scheme) = raw.strip_prefix("ssh://") {
        raw = without_scheme;
    }
    let (user, host_port) = if let Some((user, rest)) = raw.split_once('@') {
        (Some(user.to_string()), rest)
    } else {
        (None, raw)
    };
    let (host, port) = if let Some((host, port_raw)) = host_port.rsplit_once(':') {
        if port_raw.is_empty() {
            (host_port.to_string(), None)
        } else {
            let parsed = port_raw
                .parse::<u16>()
                .with_context(|| format!("invalid SSH port in target '{target}'"))?;
            (host.to_string(), Some(parsed))
        }
    } else {
        (host_port.to_string(), None)
    };
    if host.trim().is_empty() {
        anyhow::bail!("target must include a host");
    }
    Ok((user, host, port))
}
pub fn parse_host_port_with_default(value: &str, default_port: u16) -> Result<(String, u16)> {
    let raw = value.trim();
    if let Some((host, port_raw)) = raw.rsplit_once(':') {
        if port_raw.is_empty() {
            return Ok((raw.to_string(), default_port));
        }
        let port = port_raw
            .parse::<u16>()
            .with_context(|| format!("invalid port in '{value}'"))?;
        if host.trim().is_empty() {
            anyhow::bail!("host is required");
        }
        return Ok((host.to_string(), port));
    }
    if raw.is_empty() {
        anyhow::bail!("host is required");
    }
    Ok((raw.to_string(), default_port))
}
pub struct RemoteInitOptions<'a> {
    pub name: &'a str,
    pub ssh: Option<&'a str>,
    pub tls: Option<&'a str>,
    pub iroh: Option<&'a str>,
    pub user: Option<&'a str>,
    pub port: Option<u16>,
    pub set_default: bool,
}
pub fn run_remote_init(
    options: RemoteInitOptions<'_>,
    parse_iroh: impl FnOnce(&str) -> Result<(String, Option<String>, bool)>,
    trust: impl FnOnce(&str, &ConnectionTargetConfig) -> Result<()>,
    test: impl FnOnce(&str) -> Result<u8>,
) -> Result<u8> {
    let RemoteInitOptions {
        name,
        ssh,
        tls,
        iroh,
        user,
        port,
        set_default,
    } = options;
    let selected =
        usize::from(ssh.is_some()) + usize::from(tls.is_some()) + usize::from(iroh.is_some());
    if selected == 0 {
        anyhow::bail!("remote init requires one of --ssh, --tls, or --iroh");
    }
    if selected > 1 {
        anyhow::bail!("remote init accepts only one transport selector (--ssh, --tls, or --iroh)");
    }

    let mut config = BmuxConfig::load()?;
    let mut target = ConnectionTargetConfig::default();
    if let Some(ssh_value) = ssh {
        let (parsed_user, host, parsed_port) = parse_ssh_target_parts(ssh_value)?;
        target.transport = ConnectionTransport::Ssh;
        target.host = Some(host);
        target.user = user
            .map(ToString::to_string)
            .or(parsed_user)
            .or(target.user);
        target.port = port.or(parsed_port).or(Some(22));
    }
    if let Some(tls_value) = tls {
        let (host, parsed_port) = parse_host_port_with_default(tls_value, 443)?;
        target.transport = ConnectionTransport::Tls;
        target.host = Some(host.clone());
        target.server_name = Some(host);
        target.port = Some(port.unwrap_or(parsed_port));
    }
    if let Some(iroh_value) = iroh {
        target.transport = ConnectionTransport::Iroh;
        let (endpoint_id, relay_url, require_ssh_auth) = parse_iroh(iroh_value)?;
        target.endpoint_id = Some(endpoint_id.clone());
        target.host = Some(endpoint_id);
        target.relay_url = relay_url;
        target.iroh_ssh_auth = require_ssh_auth;
        target.port = None;
        target.user = None;
    }

    trust(name, &target)?;

    config
        .connections
        .targets
        .insert(name.to_string(), target.clone());
    if set_default {
        config.connections.default_target = Some(name.to_string());
    }
    save_remote_target_state_overlay(name, &target, set_default)?;

    println!("saved remote target '{name}'");
    let test_status = test(name)?;
    if test_status == 0 {
        println!("remote init validation succeeded for '{name}'");
    }
    Ok(0)
}

/// Print the session candidates in baseline recency order, leaving unranked ties in server order.
pub fn complete_sessions(target: &str, mut candidates: Vec<String>) -> Result<u8> {
    let config = BmuxConfig::load()?;
    if let Some(recents) = config.connections.recent_sessions.get(target) {
        candidates.sort_by_key(|v| recents.iter().position(|s| s == v).unwrap_or(usize::MAX));
    }
    for name in candidates {
        println!("{name}");
    }
    Ok(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct SshProbe(Mutex<Vec<Vec<String>>>);
    impl ProbeTransport for SshProbe {
        fn target(&self) -> ProbeTarget<'_> {
            ProbeTarget::Ssh("fixture")
        }
        fn ping(&self, _: &'static str) -> Result<()> {
            panic!("SSH must use bmux server status")
        }
        fn install(&self) -> Result<()> {
            panic!("test cannot install software")
        }
        fn ssh(&self, argv: &[&str]) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(argv.iter().map(ToString::to_string).collect());
            Ok(())
        }
    }
    #[test]
    fn ssh_test_probes_status_without_install() {
        let probe = SshProbe(Mutex::new(vec![]));
        assert_eq!(run_remote_test("fixture", &probe).unwrap(), 0);
        assert_eq!(
            *probe.0.lock().unwrap(),
            [vec!["server", "status", "--json"]]
        );
    }
}
