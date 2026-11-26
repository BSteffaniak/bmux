//! The mobile client over the paired-host model: `bmux_mobile_core` pairs with a real
//! isolated host through the host owner's `bmux remote pair` link, saves the host, and
//! attaches a terminal stream through the connections plugin's saved-host connector, as
//! the Android app does through the FFI.

use std::time::{Duration, Instant};

use bmux_mobile_core::{
    ConnectionManager, ConnectionRequest, ConnectionStatus, PairedHostRole, PairingRequest,
    PairingState, PairingStatus, TerminalChunkKind, TerminalOpenRequest,
};

use super::harness::{Client, Host, RemoteSettings, STEP, marker, printf_line, reserve_udp_port};

const ALIAS: &str = "desktop";

fn wait_pairing(
    manager: &ConnectionManager,
    state: &PairingState,
    until: impl Fn(&PairingState) -> bool,
) -> PairingState {
    let deadline = Instant::now() + STEP;
    loop {
        let current = manager
            .pairing_state(state.id)
            .expect("the pairing stays known");
        if until(&current) {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "pairing did not progress in {STEP:?}: {current:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn mobile_core_pairs_with_a_host_and_attaches_a_terminal() {
    let host = Host::start(
        "mobile-h",
        RemoteSettings::loopback_host(reserve_udp_port()),
    );
    host.new_session("work");
    let mut owner_view = host.attach("work");
    // The mobile storage root has the same layout as a client profile (config, state).
    let phone = Client::new("mobile-c", &RemoteSettings::default());
    let mut manager = ConnectionManager::new(phone.root());
    assert_eq!(manager.list_hosts().expect("list saved hosts"), Vec::new());

    // Pair: the owner opens a pairing session, the phone uses its link.
    let mut owner = host.terminal(
        "host-pair",
        &["remote", "pair", "--role", "full", "--name", "phone"],
    );
    let link = owner.expect("Pairing link: (bmux-pair:[^ \\t\\r\\n\\x1b]+)", STEP)[1].clone();
    let started = manager
        .start_pairing(PairingRequest {
            link,
            alias: Some(ALIAS.to_owned()),
            device_name: "phone".to_owned(),
        })
        .expect("a well-formed link starts pairing");
    let waiting = wait_pairing(&manager, &started, |state| {
        state.status != PairingStatus::Connecting
    });
    assert_eq!(waiting.status, PairingStatus::AwaitingApproval, "{waiting:?}");
    let host_phrase = owner.expect(
        "Compare this phrase on the other device: ([0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4})",
        STEP,
    )[1]
    .clone();
    assert_eq!(waiting.phrase.as_deref(), Some(host_phrase.as_str()));
    owner.expect("\\[y/N\\]", STEP);
    owner.send(b"y\r");
    let paired = wait_pairing(&manager, &started, |state| {
        state.status != PairingStatus::AwaitingApproval
    });
    assert_eq!(paired.status, PairingStatus::Paired, "{paired:?}");
    let saved = paired.host.expect("a paired state carries the saved host");
    assert_eq!(saved.alias, ALIAS);
    assert_eq!(saved.role, Some(PairedHostRole::Full));
    assert_eq!(saved.host_key, host.endpoint_id().to_string());
    assert!(
        owner.wait_exit(STEP).is_some_and(|exit| exit.success()),
        "`remote pair` exits after approval"
    );
    assert_eq!(
        manager.list_hosts().expect("list saved hosts"),
        vec![saved.clone()]
    );

    // Connect: the saved host answers and accepts this device.
    let connection = manager
        .connect(ConnectionRequest {
            host: ALIAS.to_owned(),
            session: None,
        })
        .expect("the paired host accepts the phone");
    assert_eq!(connection.status, ConnectionStatus::Connected);

    // Attach `work`, type, and see the output on the phone and on the owner's view.
    let terminal = manager
        .open_terminal(TerminalOpenRequest {
            host: ALIAS.to_owned(),
            session: Some("work".to_owned()),
            rows: 30,
            cols: 100,
        })
        .expect("the phone attaches the host's session");
    let typed = marker("mobile");
    manager
        .write_terminal_input(terminal.id, &printf_line(&typed))
        .expect("the phone types into the session");
    let deadline = Instant::now() + STEP;
    let mut output = Vec::new();
    while !String::from_utf8_lossy(&output).contains(&typed) {
        assert!(
            Instant::now() < deadline,
            "the phone never showed {typed}: {:?}",
            String::from_utf8_lossy(&output)
        );
        for chunk in manager
            .poll_terminal_output(terminal.id, 64)
            .expect("poll the phone's terminal")
        {
            if chunk.kind == TerminalChunkKind::Stdout {
                output.extend_from_slice(&chunk.bytes);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    owner_view.wait_text(&typed, STEP);
    manager
        .close_terminal(terminal.id)
        .expect("close the phone's terminal");

    // Forget: the saved host is gone on the phone.
    manager.forget_host(ALIAS).expect("forget the saved host");
    assert_eq!(manager.list_hosts().expect("list saved hosts"), Vec::new());
}
