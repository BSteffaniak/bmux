//! Optional cross-repository test. Run with BCODE_RESURRECTION_TEST_BINARY set to
//! a freshly built Bcode binary; no provider requests or user state are used.
#[allow(dead_code)]
mod support;

use bmux_pane_runtime_plugin_api::pane_runtime_commands::client;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct BcodeFixture {
    binary: PathBuf,
    state: PathBuf,
    config: PathBuf,
}
impl BcodeFixture {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .arg("--state-root")
            .arg(&self.state)
            .arg("--no-credential-discovery")
            .env("BCODE_CONFIG", &self.config)
            .env_remove("BCODE_CONFIG_TOML")
            .env_remove("BCODE_IPC_ENDPOINT")
            .env_remove("BCODE_SOCKET");
        command
    }
}
impl Drop for BcodeFixture {
    fn drop(&mut self) {
        let _ = self.command().args(["server", "stop"]).output();
    }
}

async fn committed(path: &Path, session: &str, generation: u64) {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let value = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        if let Some(value) = &value {
            let descriptor = value["descriptor"]
                .as_str()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
            if value["generation"]
                .as_u64()
                .is_some_and(|current| current >= generation)
                && value["revision"]
                    .as_u64()
                    .is_some_and(|revision| revision > 0)
                && descriptor.is_some_and(|descriptor| descriptor["session_id"] == session)
            {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "Bcode did not publish opened session: {value:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "requires a freshly built Bcode binary via BCODE_RESURRECTION_TEST_BINARY"]
async fn real_bcode_tui_reopens_session_after_bmux_restart() {
    let binary = PathBuf::from(
        std::env::var_os("BCODE_RESURRECTION_TEST_BINARY").expect("set Bcode binary path"),
    )
    .canonicalize()
    .unwrap();
    let mut server = support::ServerEnv::new("real-bcode-resurrection");
    server.write_config("[behavior]\nrestore_last_layout = true\n");
    let state = server.root().join("bcode-state");
    std::fs::create_dir_all(&state).unwrap();
    let config = server.root().join("bcode.toml");
    std::fs::write(&config, "[onboarding]\nauto_discover_credentials = false\n[tui.session_resurrection]\nmode = 'auto'\n").unwrap();
    let fixture = BcodeFixture {
        binary,
        state,
        config,
    };
    let created = fixture
        .command()
        .args([
            "session",
            "create",
            "resurrection-verification",
            "--json",
            "--cwd",
        ])
        .arg(server.root())
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let session = created["id"].as_str().expect("session ID");
    // The first launch explicitly opens the test session. Every subsequent launch
    // omits its ID, so only the host handoff can select it. exec preserves peer PID.
    let script = server.root().join("launch-bcode.py");
    std::fs::write(
        &script,
        r#"import os, sys
binary, state, config, session, marker = sys.argv[1:]
os.environ['BCODE_CONFIG'] = config
for key in ['BCODE_CONFIG_TOML', 'BCODE_IPC_ENDPOINT', 'BCODE_SOCKET']:
    os.environ.pop(key, None)
args = [binary, '--state-root', state, '--no-credential-discovery', 'tui']
if not os.path.exists(marker):
    open(marker, 'w').close()
    args.append(session)
os.execv(binary, args)
"#,
    )
    .unwrap();
    let python = Command::new("which").arg("python3").output().unwrap();
    assert!(python.status.success());
    server.start();
    let endpoint = bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock"));
    let mut connection =
        bmux_client::BmuxClient::connect(&endpoint, Duration::from_secs(10), "bcode-resurrection")
            .await
            .unwrap();
    let runtime = client::new_session_with_runtime(&mut connection, Some("bcode".into()))
        .await
        .unwrap()
        .unwrap();
    let pane = client::launch_pane(
        &mut connection,
        runtime.session_id,
        None,
        "horizontal".into(),
        50,
        None,
        String::from_utf8(python.stdout).unwrap().trim().to_owned(),
        vec![
            script.to_string_lossy().into_owned(),
            fixture.binary.to_string_lossy().into_owned(),
            fixture.state.to_string_lossy().into_owned(),
            fixture.config.to_string_lossy().into_owned(),
            session.into(),
            server.root().join("started").to_string_lossy().into_owned(),
        ],
        Some(server.root().to_string_lossy().into_owned()),
    )
    .await
    .unwrap()
    .unwrap();
    let checkpoint = server
        .state_dir
        .join("application-resurrection")
        .join(format!("{}.json", pane.pane_id));
    committed(&checkpoint, session, 1).await;
    drop(connection);
    server.stop();
    server.start();
    committed(&checkpoint, session, 2).await;
    server.stop();
}
