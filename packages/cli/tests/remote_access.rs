//! Acceptance gates for remote access (docs/development/remote-access-architecture.md,
//! "Tests and acceptance gates"). Each gate drives the real `bmux` binary as a person would:
//! an isolated host runtime, an isolated daemonless client profile, real pseudo-terminals,
//! and the public CLI only. Gate `N` is `gate{N}_*`.
//!
//! A gate that today's code cannot pass is `#[ignore]`d with the lane or feature it waits
//! for; the coordinator removes the ignore when that lane lands. Fixture self-tests run
//! everywhere, so the harness itself stays honest in CI.
//!
//! Run them with `cargo nextest run -p bmux_cli --test remote_access --run-ignored all`.
#![cfg(all(unix, feature = "bundled-plugin-connections"))]

#[allow(dead_code)] // Shared process fixture; this target uses the host-runtime part.
mod support;

#[cfg(feature = "bundled-plugin-cluster")]
#[path = "remote_access/cluster.rs"]
mod cluster;
#[path = "remote_access/harness.rs"]
mod harness;
#[path = "remote_access/probe.rs"]
mod probe;
#[path = "remote_access/relay.rs"]
mod relay;
#[path = "remote_access/udp_proxy.rs"]
mod udp_proxy;

use std::net::{Ipv6Addr, SocketAddr, UdpSocket};
use std::path::Path;
use std::time::{Duration, Instant};

use harness::{
    COLS, Client, Host, PREFIX, PREFIX_KEYMAP, QUIET, RemoteSettings, Role, STEP, Term,
    lines_with, marker, pair, pair_only, reserve_udp_port,
};
use probe::{Dial, PAIR_ALPN, Peer, SESSION_ALPN, assert_refused};
use relay::LocalRelay;
use udp_proxy::UdpProxy;

/// The old paired-terminal lease. Idle survival must outlast it several times over.
const OLD_LEASE: Duration = Duration::from_secs(60);

/// How long the idle gate stays idle: `BMUX_REMOTE_IDLE_SECS` (default 300, the gate's
/// five minutes), never less than three old leases.
fn idle_duration() -> Duration {
    let requested = std::env::var("BMUX_REMOTE_IDLE_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok())
        .map_or(Duration::from_secs(300), Duration::from_secs);
    requested.max(OLD_LEASE * 3)
}

/// How long the reconnect gate keeps the path down: longer than QUIC's 30 s idle timeout,
/// so the connection really drops rather than riding out the gap on retransmits.
const BLACKHOLE: Duration = Duration::from_secs(45);

/// A host with one session, `work`, its owner's local view, and a paired client.
struct Paired {
    host: Host,
    client: Client,
    owner_view: Term,
}

const ALIAS: &str = "desktop";
const DEVICE: &str = "laptop";

fn paired(label: &str, role: Role) -> Paired {
    let host = Host::start(
        &format!("{label}-h"),
        RemoteSettings::loopback_host(reserve_udp_port()),
    );
    host.new_session("work");
    let owner_view = host.attach("work");
    let client = Client::new(&format!("{label}-c"), &RemoteSettings::default());
    pair_only(&host, &client, ALIAS, DEVICE, role);
    Paired {
        host,
        client,
        owner_view,
    }
}

// ---------------------------------------------------------------------------------------
// Gate 1: idle survival.
// ---------------------------------------------------------------------------------------

#[test]
#[ignore = "remote-access: needs lane A (pairing CLI, session listener) + lane D (connect over the connector); slow: idles 5 minutes"]
fn gate1_idle_attach_survives_and_input_arrives() {
    let mut fixture = paired("idle", Role::Full);
    let mut viewer = fixture.client.connect(ALIAS, "work");
    let before = marker("idle-before");
    viewer.type_marker(&before);
    viewer.wait_text(&before, STEP);

    let idle = idle_duration();
    let until = Instant::now() + idle;
    while Instant::now() < until {
        assert!(
            !viewer.has_exited(),
            "the remote viewer exited while idle for {idle:?}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    let after = marker("idle-after");
    viewer.type_marker(&after);
    viewer.wait_text(&after, Duration::from_secs(10));
    fixture
        .owner_view
        .wait_text(&after, Duration::from_secs(10));
}

// ---------------------------------------------------------------------------------------
// Gate 2: parity with a local attach.
// ---------------------------------------------------------------------------------------

/// Presses the default keymap's prefix, then `key`.
fn prefixed(term: &mut Term, key: &[u8]) {
    term.send(PREFIX);
    term.send(key);
}

/// The pane frame alone puts `│` at both edges; a vertical split adds a divider between them.
fn two_pane_border(screen: &str) -> bool {
    screen
        .lines()
        .filter(|line| line.matches('│').count() >= 3)
        .count()
        >= 10
}

/// SGR mouse report for a left-button event at 1-based `col`, `row`.
fn sgr_mouse(code: u8, col: u16, row: u16, press: bool) -> Vec<u8> {
    format!("\x1b[<{code};{col};{row}{}", if press { 'M' } else { 'm' }).into_bytes()
}

#[test]
fn gate2_remote_attach_matches_local_attach() {
    let host = Host::start(
        "parity-h",
        RemoteSettings::loopback_host(reserve_udp_port()),
    );
    // Tabs, not bare sessions: tab navigation cycles the workspace's tabs.
    host.new_tab("work");
    host.new_tab("side");
    let mut owner = host.attach("work");
    let existing = marker("existing");
    owner.type_marker(&existing);
    owner.wait_text(&existing, STEP);
    {
        let mut side = host.attach("side");
        let side_marker = marker("side");
        side.type_marker(&side_marker);
        side.wait_text(&side_marker, STEP);
    }

    let client = Client::new("parity-c", &RemoteSettings::default());
    client.configure(PREFIX_KEYMAP);
    // Releasing a mouse selection copies it and leaves the selection (scroll) mode.
    client.configure("\n[behavior.mouse]\nselection_release = \"copy_and_exit\"\n");
    pair_only(&host, &client, ALIAS, DEVICE, Role::Full);

    // List sessions through the saved host.
    let sessions = client.ok(
        &["--target", ALIAS, "list-sessions"],
        "list remote sessions",
    );
    assert!(
        sessions.contains("work") && sessions.contains("side"),
        "remote session list misses the host's sessions: {sessions}"
    );

    // Attach to the EXISTING session: it shows what was already there.
    let mut remote = client.connect(ALIAS, "work");
    remote.wait_text(&existing, STEP);

    // Tab switch: next tab shows `side`, previous returns to `work`.
    remote.send(b"\x13"); // ctrl+s: next tab
    remote.wait_screen("the `side` tab", STEP, |screen| screen.contains("SIDE-"));
    remote.send(b"\x08"); // ctrl+h: previous tab
    remote.wait_text(&existing, STEP);

    // Split vertically from the remote side; both viewers show two panes.
    prefixed(&mut remote, b"%");
    remote.wait_screen("a vertical split", STEP, two_pane_border);
    owner.wait_screen("the remote's split", STEP, two_pane_border);

    // Mouse: click the left pane to focus it, then type there.
    assert!(
        remote.raw_contains(b"\x1b[?1006h"),
        "the remote attach did not enable SGR mouse reporting"
    );
    remote.send(&sgr_mouse(0, 5, 5, true));
    remote.send(&sgr_mouse(0, 5, 5, false));
    let clicked = marker("clicked");
    remote.type_marker(&clicked);
    remote.wait_text(&clicked, STEP);
    let left_half = COLS as usize / 2;
    let row = remote
        .lines()
        .into_iter()
        .find(|line| line.contains(&clicked))
        .expect("clicked marker row");
    // Screen columns are characters; the frame's `│` is several bytes.
    let column = row[..row.find(&clicked).expect("marker column")]
        .chars()
        .count();
    assert!(
        column < left_half,
        "the click did not focus the left pane (marker at column {column}): {row}"
    );

    // Mouse selection: dragging over the marker copies it on the VIEWING machine. The copy
    // runs in the viewer's own process through its clipboard provider (here the harness's
    // `wl-copy` capture), never on the host and never as OSC 52 to the outer terminal.
    let screen_row = remote
        .lines()
        .iter()
        .position(|line| line.contains(&clicked))
        .expect("marker row index");
    let (row, start) = (
        u16::try_from(screen_row + 1).expect("row"),
        u16::try_from(column + 1).expect("col"),
    );
    let end = start + u16::try_from(clicked.len() - 1).expect("marker length");
    remote.send(&sgr_mouse(0, start, row, true));
    remote.send(&sgr_mouse(32, end, row, true));
    remote.send(&sgr_mouse(0, end, row, false));
    let deadline = Instant::now() + STEP;
    let copied = loop {
        if let Some(copied) = client.clipboard() {
            break copied;
        }
        assert!(
            Instant::now() < deadline,
            "the mouse selection copied nothing on the viewing machine:\n{}",
            remote.text()
        );
        assert!(!remote.has_exited(), "the remote viewer exited");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        copied.trim_end(),
        clicked,
        "the selection copied the wrong text"
    );
    assert!(
        host.clipboard().is_none(),
        "the remote viewer's selection was copied on the host"
    );
    assert!(
        !remote.raw_contains(b"\x1b]52;"),
        "the selection was written to the viewing terminal as OSC 52"
    );
    // `copy_and_exit` leaves the selection's scroll mode.
    remote.wait_screen("insert mode after the copy", STEP, |screen| {
        !screen.contains("SCROLL")
    });

    // Scrollback: 200 numbered lines, then scroll mode to the top shows the first.
    remote.send(b"i=1; while [ $i -le 200 ]; do printf 'SB%03d\\n' $i; i=$((i+1)); done\r");
    remote.wait_text("SB200", STEP);
    prefixed(&mut remote, b"[");
    remote.send(b"g");
    remote.wait_text("SB001", STEP);
    remote.send(b"\x1b");
    remote.wait_screen("scroll mode left", STEP, |screen| {
        !screen.contains("SCROLL")
    });
    remote.wait_text("SB200", STEP);

    // Screens match: what the owner sees locally of the same session, the remote sees too.
    // The bottom row is each viewer's own status bar (its mode, its transient messages, the
    // host's hosting state), so only the panes above it are compared.
    let needles = [existing.as_str(), clicked.as_str(), "SB200", "│"];
    let panes = |term: &Term| {
        let mut lines = term.lines();
        lines.pop();
        lines_with(&lines, &needles)
    };
    let settle = Instant::now() + STEP;
    loop {
        // Read what both terminals drew since the last look.
        assert!(!owner.has_exited(), "the owner's attach exited");
        assert!(!remote.has_exited(), "the remote viewer exited");
        let local = panes(&owner);
        let seen = panes(&remote);
        if local == seen && local.iter().any(|line| line.contains("SB200")) {
            break;
        }
        assert!(
            Instant::now() < settle,
            "remote and local screens differ.\nlocal:\n{}\nremote:\n{}",
            local.join("\n"),
            seen.join("\n")
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------------------
// Gate 3: revocation and role change while attached.
// ---------------------------------------------------------------------------------------

#[test]
fn gate3_revoke_closes_viewer_within_one_second_and_refuses_reconnect() {
    let fixture = paired("revoke", Role::Full);
    let mut viewer = fixture.client.connect(ALIAS, "work");
    let proof = marker("before-revoke");
    viewer.type_marker(&proof);
    viewer.wait_text(&proof, STEP);

    fixture
        .host
        .ok(&["remote", "revoke", DEVICE], "revoke the device");
    let revoked_at = Instant::now();
    let exit = viewer.wait_exit(Duration::from_secs(1));
    assert!(
        exit.is_some(),
        "the viewer was still open {:?} after revocation",
        revoked_at.elapsed()
    );

    let mut again = fixture
        .client
        .terminal("reconnect", &["connect", ALIAS, "work"]);
    let exit = again
        .wait_exit(STEP)
        .expect("a revoked device's reconnect ends");
    assert!(!exit.success(), "a revoked device reconnected: {exit:?}");
    assert!(
        !again.text().contains(&proof),
        "a revoked device saw the session"
    );
    let listed = fixture.client.run(&["--target", ALIAS, "list-sessions"]);
    assert!(
        !listed.status.success(),
        "a revoked device still ran a command on the host"
    );
}

#[test]
fn gate3_role_change_to_view_only_refuses_input_and_keeps_output() {
    let mut fixture = paired("role", Role::Full);
    let mut viewer = fixture.client.connect(ALIAS, "work");
    let typed = marker("full-input");
    viewer.type_marker(&typed);
    fixture.owner_view.wait_text(&typed, STEP);

    fixture.host.ok(
        &["remote", "role", DEVICE, "view-only"],
        "change the device to view-only",
    );
    // Let the role change reach the live connection before typing.
    std::thread::sleep(Duration::from_millis(500));

    let refused = marker("refused-input");
    viewer.type_marker(&refused);
    fixture.owner_view.assert_never_shows(&refused, QUIET);
    viewer.assert_never_shows(&refused, Duration::from_millis(200));

    let output = marker("owner-output");
    fixture.owner_view.type_marker(&output);
    viewer.wait_text(&output, STEP);
    assert!(
        !viewer.has_exited(),
        "a view-only viewer must stay attached"
    );
    let listed = fixture.client.ok(
        &["--target", ALIAS, "list-sessions"],
        "view-only may observe",
    );
    assert!(listed.contains("work"), "{listed}");
    assert!(fixture.host.is_running());
}

#[test]
fn gate3_view_only_device_observes_but_cannot_type() {
    let mut fixture = paired("viewonly", Role::ViewOnly);
    let mut viewer = fixture.client.connect(ALIAS, "work");
    let refused = marker("view-only-input");
    viewer.type_marker(&refused);
    fixture.owner_view.assert_never_shows(&refused, QUIET);
    let output = marker("owner-output");
    fixture.owner_view.type_marker(&output);
    viewer.wait_text(&output, STEP);
    assert!(
        !viewer.has_exited(),
        "a view-only viewer must stay attached"
    );
    assert!(fixture.host.is_running());
}

// ---------------------------------------------------------------------------------------
// Gate 4: reconnect after the path drops.
// ---------------------------------------------------------------------------------------

#[test]
fn gate4_reconnect_after_blackholed_path_resumes_without_replaying_input() {
    // The host binds IPv4 only and the client IPv6 only, so the proxy (IPv6 in, IPv4 out) is
    // the ONLY path: NAT traversal cannot find a direct route around it.
    let host_port = reserve_udp_port();
    let proxy = UdpProxy::start(
        SocketAddr::from((Ipv6Addr::LOCALHOST, 0)),
        SocketAddr::from(([127, 0, 0, 1], host_port)),
    );
    let mut settings = RemoteSettings::loopback_host(host_port);
    settings.external_addrs = vec![proxy.addr()];
    let host = Host::start("reconn-h", settings);
    // Two tabs: the viewer starts on `work` and switches to `side` before the drop, so the
    // resume must return to the session it was last on, not the one it was started with.
    host.new_tab("work");
    host.new_tab("side");
    let mut owner = host.attach("side");
    let side_ready = marker("side-ready");
    owner.type_marker(&side_ready);
    owner.wait_text(&side_ready, STEP);
    let client = Client::new(
        "reconn-c",
        &RemoteSettings {
            bind_addr: Some(SocketAddr::from((Ipv6Addr::LOCALHOST, 0))),
            ..RemoteSettings::default()
        },
    );
    pair_only(&host, &client, ALIAS, DEVICE, Role::Full);
    let mut viewer = client.connect(ALIAS, "work");
    viewer.send(b"\x13"); // ctrl+s: next tab, `side`
    viewer.wait_text(&side_ready, STEP);
    let before = marker("before-drop");
    viewer.type_marker(&before);
    owner.wait_text(&before, STEP);
    assert!(
        proxy.forwarded() > 0,
        "the session did not go through the proxy"
    );

    proxy.blackhole(true);
    let during = marker("during-drop");
    owner.type_marker(&during);
    // Typed while the path is down: must never be delivered, now or after the resume.
    let lost = marker("lost-input");
    viewer.type_marker(&lost);
    viewer.assert_never_shows(&during, Duration::from_secs(5));
    std::thread::sleep(BLACKHOLE.saturating_sub(Duration::from_secs(5)));
    assert!(proxy.dropped() > 0, "nothing tried the dropped path");
    assert!(
        !viewer.has_exited(),
        "the client gave up instead of reconnecting; output tail: {:?}",
        String::from_utf8_lossy(&viewer.raw()[viewer.raw().len().saturating_sub(1600)..])
    );

    proxy.blackhole(false);
    // Resumed from a snapshot of `side`, the session it was on: output produced there
    // during the drop shows up.
    viewer.wait_text(&during, Duration::from_secs(60));
    let after = marker("after-drop");
    viewer.type_marker(&after);
    owner.wait_text(&after, STEP);
    owner.assert_never_shows(&lost, QUIET);
    viewer.assert_never_shows(&lost, Duration::from_millis(200));
}

// ---------------------------------------------------------------------------------------
// Gate 5: relay only.
// ---------------------------------------------------------------------------------------

#[test]
fn gate5_relay_only_pairs_attaches_and_types() {
    let relay = LocalRelay::start();
    let relay_urls = Some(vec![relay.url().to_owned()]);
    // IPv4-only host and IPv6-only client have no direct path; only the relay joins them.
    let host = Host::start(
        "relay-h",
        RemoteSettings {
            relay_urls: relay_urls.clone(),
            ..RemoteSettings::loopback_host(reserve_udp_port())
        },
    );
    host.new_session("work");
    let mut owner = host.attach("work");
    let client = Client::new(
        "relay-c",
        &RemoteSettings {
            bind_addr: Some(SocketAddr::from((Ipv6Addr::LOCALHOST, 0))),
            relay_urls,
            ..RemoteSettings::default()
        },
    );
    pair_only(&host, &client, ALIAS, DEVICE, Role::Full);
    let mut viewer = client.connect(ALIAS, "work");
    let typed = marker("over-relay");
    viewer.type_marker(&typed);
    owner.wait_text(&typed, STEP);
    viewer.wait_text(&typed, STEP);
    let host_id = host.endpoint_id().to_string();
    let admitted = relay.admitted();
    assert!(
        admitted.contains(&host_id) && admitted.len() >= 2,
        "the relay did not carry both endpoints: {admitted:?}"
    );
}

// ---------------------------------------------------------------------------------------
// Gate 6: pre-auth surface.
// ---------------------------------------------------------------------------------------

fn hosting(label: &str) -> (Host, SocketAddr) {
    let port = reserve_udp_port();
    let host = Host::start(label, RemoteSettings::loopback_host(port));
    (host, SocketAddr::from(([127, 0, 0, 1], port)))
}

/// How quickly a refusal must close the connection.
const REFUSAL: Duration = Duration::from_secs(2);

#[test]
fn gate6_unknown_device_is_refused_during_the_session_handshake() {
    let (mut host, addr) = hosting("preauth-s");
    let id = host.endpoint_id();
    let peer = Peer::new();
    // A Hello-shaped first frame: whatever it is, an unknown key must never get it read.
    let dial = peer.dial(
        id,
        addr,
        SESSION_ALPN,
        b"\x00\x00\x00\x08hello!!!",
        REFUSAL * 2,
    );
    assert_refused(&dial, "unknown device on bmux/session/1", REFUSAL);
    assert!(host.is_running());
}

#[test]
fn gate6_pair_alpn_is_refused_without_a_pairing_session() {
    let (mut host, addr) = hosting("preauth-p");
    let id = host.endpoint_id();
    let peer = Peer::new();
    let dial = peer.dial(id, addr, PAIR_ALPN, b"bmux-pair-request", REFUSAL * 2);
    assert_refused(&dial, "bmux/pair/1 without a pairing session", REFUSAL);
    assert!(host.is_running());
}

#[test]
fn gate6_malformed_pairing_frames_are_bounded_and_refused() {
    let (mut host, addr) = hosting("preauth-m");
    let id = host.endpoint_id();
    let mut owner = host.terminal(
        "host-pair",
        &["remote", "pair", "--role", "full", "--name", "intruder"],
    );
    owner.expect("Pairing link: bmux-pair:", STEP);

    let peer = Peer::new();
    // Per-frame deadline bound: the host must give up on a silent or slow peer by itself.
    let bound = Duration::from_secs(15);
    let oversized_prefix = [0xff_u8; 8];
    let garbage = vec![0xa5_u8; 256 * 1024];
    let truncated = b"\x00\x00\x10\x00{\"v\":1,".to_vec();
    for (what, payload) in [
        ("an oversized length prefix", &oversized_prefix[..]),
        ("256 KiB of garbage", &garbage[..]),
        ("a truncated frame", &truncated[..]),
        ("nothing at all", &[][..]),
    ] {
        match peer.dial(id, addr, PAIR_ALPN, payload, bound + REFUSAL) {
            Dial::Refused(_) => {}
            Dial::Connected(after) => {
                assert!(
                    after.reply_bytes <= 1024,
                    "{what}: the host sent {} bytes back",
                    after.reply_bytes
                );
                assert!(
                    after.closed && after.closed_after.is_some_and(|t| t <= bound + REFUSAL),
                    "{what}: the host kept the pairing connection open: {after:?}"
                );
            }
        }
        assert!(host.is_running(), "{what}: the host server died");
        assert!(
            !owner.text().contains("Compare this phrase"),
            "{what}: a malformed request reached approval"
        );
    }
    drop(owner);

    // The host still pairs a real device afterwards.
    host.new_session("work");
    let client = Client::new("preauth-mc", &RemoteSettings::default());
    pair_only(&host, &client, ALIAS, DEVICE, Role::Full);
    let listed = client.ok(&["--target", ALIAS, "list-sessions"], "list after pairing");
    assert!(listed.contains("work"), "{listed}");
}

// ---------------------------------------------------------------------------------------
// Gate 7: the connecting side runs no daemon.
// ---------------------------------------------------------------------------------------

#[test]
fn gate7_connecting_side_creates_no_socket_server_or_runtime_dir() {
    let host = Host::start(
        "daemonless-h",
        RemoteSettings::loopback_host(reserve_udp_port()),
    );
    host.new_session("work");
    let client = Client::new("daemonless-c", &RemoteSettings::default());
    client.assert_daemonless();

    // Pairing, then the attach that follows it, run without a local server.
    let mut pairing = pair(&host, &client, ALIAS, DEVICE, Role::Full);
    pairing.wait_screen("the post-pairing attach", STEP, |screen| {
        screen.contains(harness::PROMPT)
    });
    client.assert_daemonless();
    drop(pairing);

    let hosts = client.ok(&["remote", "hosts"], "list saved hosts");
    assert!(hosts.contains(ALIAS), "{hosts}");
    let mut viewer = client.connect(ALIAS, "work");
    let typed = marker("daemonless");
    viewer.type_marker(&typed);
    viewer.wait_text(&typed, STEP);
    client.ok(&["--target", ALIAS, "list-sessions"], "remote command");
    client.assert_daemonless();
    drop(viewer);

    client.ok(&["remote", "forget", ALIAS], "forget the saved host");
    let hosts = client.ok(&["remote", "hosts"], "list saved hosts after forget");
    assert!(!hosts.contains(ALIAS), "{hosts}");
    client.assert_daemonless();
    // Forgetting is local: the host still lists the device.
    let devices = host.ok(&["remote", "list"], "list devices");
    assert!(devices.contains(DEVICE), "{devices}");
}

// ---------------------------------------------------------------------------------------
// Gate 8: owner-local routes.
// ---------------------------------------------------------------------------------------

#[test]
fn gate8_owner_local_routes_are_refused_for_a_full_remote_principal() {
    let mut fixture = paired("ownerlocal", Role::Full);
    let owner_local: [&[&str]; 7] = [
        &["server", "stop"],
        &["server", "save"],
        &["remote", "disable"],
        &["remote", "list"],
        &["remote", "revoke", DEVICE],
        &["remote", "role", DEVICE, "view-only"],
        &["remote", "pair", "--role", "full", "--name", "intruder"],
    ];
    for command in owner_local {
        let mut args = vec!["--target", ALIAS];
        args.extend_from_slice(command);
        let output = fixture.client.run(&args);
        let text = support::combined_output(&output);
        assert!(
            !output.status.success(),
            "owner-local `{}` ran for a remote principal: {text}",
            command.join(" ")
        );
        assert!(
            !text.contains("bmux-pair:"),
            "a remote principal opened a pairing session"
        );
        assert!(
            fixture.host.is_running(),
            "`{}` stopped the host",
            command.join(" ")
        );
    }
    // Nothing above took effect: hosting is on, the device is still full and still paired.
    let listed = fixture.client.ok(
        &["--target", ALIAS, "list-sessions"],
        "full principal observes",
    );
    assert!(listed.contains("work"), "{listed}");
    let mut viewer = fixture.client.connect(ALIAS, "work");
    let typed = marker("still-full");
    viewer.type_marker(&typed);
    fixture.owner_view.wait_text(&typed, STEP);
}

// ---------------------------------------------------------------------------------------
// Gate 9: cluster over the paired transport.
// ---------------------------------------------------------------------------------------

/// Pairs every node's own profile with every node, itself included, saving each under that
/// node's name: a member reaches every member, itself too, by its advertised endpoint.
/// Pairing never grants cluster membership; enrollment does.
#[cfg(feature = "bundled-plugin-cluster")]
fn pair_mesh(nodes: &[(&Host, &str)]) {
    for (host, host_name) in nodes {
        for (connecting, connecting_name) in nodes {
            let device = if host_name == connecting_name {
                format!("{connecting_name}-self")
            } else {
                (*connecting_name).to_owned()
            };
            pair_only(host, *connecting, host_name, &device, Role::Full);
        }
    }
}

#[test]
#[cfg(feature = "bundled-plugin-cluster")]
fn gate9_cluster_nodes_over_paired_transport_attach_across_hosts() {
    let node_a = Host::start(
        "cluster-a",
        RemoteSettings::loopback_host(reserve_udp_port()),
    );
    let node_b = Host::start(
        "cluster-b",
        RemoteSettings::loopback_host(reserve_udp_port()),
    );
    pair_mesh(&[(&node_a, "node-a"), (&node_b, "node-b")]);

    // Each node advertises the saved-host name the other paired with it under.
    node_a.ok(&["cluster", "init", "--endpoint", "node-a"], "init cluster");
    let token_output = node_a.ok(
        &[
            "cluster",
            "enrollment-token",
            "create",
            "--endpoint",
            "node-a",
            // A worker verifies each lease against linearizable membership, which takes
            // a consensus voter.
            "--role",
            "voter",
            "--worker",
        ],
        "create enrollment token",
    );
    let token = token_output
        .lines()
        .find(|line| line.starts_with("bmux-enroll-v1:"))
        .expect("enrollment token")
        .to_owned();
    let joined = node_b.ok(
        &[
            "cluster",
            "join",
            &token,
            "--issuer",
            "node-a",
            "--endpoint",
            "node-b",
        ],
        "join over the paired transport",
    );
    assert!(joined.contains("joined cluster:"), "{joined}");
    for node in [&node_a, &node_b] {
        let members = node.ok(&["cluster", "members"], "read members");
        assert_eq!(
            members.matches(" state=Active ").count(),
            2,
            "unexpected membership: {members}"
        );
    }

    // A pane placed on the worker (node B) through the ingress (node A).
    let word = marker("worker");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    let cluster_id = runtime.block_on(async {
        let mut worker = cluster::owner(&node_b).await;
        let (worker_node, _) = cluster::identity(&mut worker).await;
        let mut ingress = cluster::owner(&node_a).await;
        let (_, cluster_id) = cluster::identity(&mut ingress).await;
        let mut nodes = cluster::Nodes(vec![ingress, worker]);
        cluster::place_pane(
            &mut nodes,
            "gate9",
            &worker_node,
            &word.to_ascii_lowercase(),
        )
        .await;
        cluster_id.expect("initialized node has a cluster id")
    });

    // Node A's owner attaches to the cluster workspace: frames come from node B over the
    // paired transport, and typed input reaches node B's pane.
    let target = format!("cluster://{cluster_id}/gate9");
    let mut attach = node_a.terminal("cluster-attach", &["attach", &target]);
    attach.wait_text(&format!("ready:{}", word.to_ascii_lowercase()), STEP);
    let typed = marker("typed");
    attach.send(format!("{}\r", typed.to_ascii_lowercase()).as_bytes());
    attach.wait_text(&format!("SEEN:{typed}"), STEP);
}

// ---------------------------------------------------------------------------------------
// Operations: `remote status`, `remote doctor`, `remote log` (architecture section 8).
// ---------------------------------------------------------------------------------------

/// Stdout of a successful command, parsed as JSON.
fn json_of(output: &std::process::Output, what: &str) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{what}: stdout is not JSON ({error}): {}",
            support::combined_output(output)
        )
    })
}

/// `bmux remote doctor <alias> --json` on `client`: its exit success and report.
fn doctor(client: &Client, alias: &str) -> (bool, serde_json::Value) {
    let output = client.run(&["remote", "doctor", alias, "--json", "--timeout", "3"]);
    let report = json_of(&output, "remote doctor");
    assert_eq!(
        output.status.success(),
        report["ok"] == true,
        "the exit status must match the report: {report}"
    );
    (output.status.success(), report)
}

fn failure_kind(report: &serde_json::Value) -> &str {
    report["failure"]["kind"].as_str().unwrap_or("none")
}

#[test]
fn operations_status_shows_hosting_devices_live_connections_and_saved_hosts() {
    let fixture = paired("opstatus", Role::Full);
    let mut viewer = fixture.client.connect(ALIAS, "work");
    let typed = marker("status");
    viewer.type_marker(&typed);
    viewer.wait_text(&typed, STEP);

    let output = fixture.host.run(&["remote", "status", "--json"]);
    support::assert_success(&output, "host status --json");
    let status = json_of(&output, "host status");
    let host = &status["host"];
    assert_eq!(host["server_running"], true, "{status}");
    assert_eq!(host["enabled"], true, "{status}");
    assert_eq!(host["source"], "remote enable", "{status}");
    assert!(
        host["reason"]
            .as_str()
            .is_some_and(|r| r.contains("remote enable")),
        "{status}"
    );
    assert!(
        host["endpoint_id"]
            .as_str()
            .is_some_and(|id| id.len() >= 52),
        "{status}"
    );
    assert_eq!(host["relay_mode"], "disabled", "{status}");
    let devices = host["devices"].as_array().expect("devices");
    assert!(
        devices.iter().any(|device| device["name"] == DEVICE
            && device["role"] == "full"
            && device["state"] == "active"
            && device["last_seen"].is_u64()),
        "{status}"
    );
    let live = host["live_connections"]
        .as_array()
        .expect("live connections");
    assert!(
        live.iter().any(|connection| connection["name"] == DEVICE
            && connection["path"] == "direct"
            && connection["rtt_ms"].is_u64()),
        "{status}"
    );
    let text = fixture.host.ok(&["remote", "status"], "host status");
    for needle in [
        "Remote access: enabled (remote enable)",
        "Host key: ",
        DEVICE,
        "via direct",
    ] {
        assert!(text.contains(needle), "`{needle}` missing: {text}");
    }
    drop(viewer);

    // A bare `bmux connect` offers the saved hosts with their online state.
    let mut picker = fixture.client.terminal("picker", &["connect"]);
    picker.expect(&format!("1\\) {ALIAS} +online \\(direct"), STEP);
    picker.expect("Host \\[1-1\\]: ", STEP);
    picker.send(b"1\r");
    picker.wait_attached();
    drop(picker);

    // The connecting side has no server: status shows its saved hosts, creates nothing.
    let output = fixture.client.run(&["remote", "status", "--json"]);
    support::assert_success(&output, "client status --json");
    let status = json_of(&output, "client status");
    assert_eq!(status["host"]["server_running"], false, "{status}");
    assert_eq!(status["host"]["enabled"], false, "{status}");
    assert!(status["client"]["device_key"].is_string(), "{status}");
    let saved = status["client"]["saved_hosts"]
        .as_array()
        .expect("saved hosts");
    assert!(
        saved
            .iter()
            .any(|host| host["alias"] == ALIAS && host["last_path"] == "direct"),
        "{status}"
    );
    let text = fixture.client.ok(&["remote", "status"], "client status");
    assert!(text.contains("not running"), "{text}");
    assert!(text.contains(ALIAS), "{text}");
    let log = fixture.client.ok(&["remote", "log"], "client log");
    assert!(log.contains("No remote access events"), "{log}");
    fixture.client.assert_daemonless();
}

#[test]
fn operations_doctor_times_each_step_and_names_offline_revoked_and_changed_hosts() {
    let fixture = paired("opdoctor", Role::ViewOnly);

    // Healthy: every step passes, in order, over a direct path.
    let (ok, report) = doctor(&fixture.client, ALIAS);
    assert!(ok, "{report}");
    let steps: Vec<_> = report["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|step| step["step"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        steps,
        [
            "resolve",
            "endpoint",
            "relay",
            "dial",
            "handshake",
            "hello",
            "round-trip"
        ],
        "{report}"
    );
    assert!(
        report["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .all(|step| step["ok"] == true && step["ms"].is_u64()),
        "{report}"
    );
    assert_eq!(report["path"], "direct", "{report}");
    let text = fixture
        .client
        .ok(&["remote", "doctor", ALIAS], "doctor text");
    assert!(text.contains("is reachable via direct"), "{text}");

    // Unknown saved host.
    let (ok, report) = doctor(&fixture.client, "nowhere");
    assert!(!ok);
    assert_eq!(failure_kind(&report), "unknown_host", "{report}");

    // Host offline: hosting off, nothing answers.
    fixture.host.ok(&["remote", "disable"], "disable hosting");
    let (ok, report) = doctor(&fixture.client, ALIAS);
    assert!(!ok);
    assert_eq!(failure_kind(&report), "host_offline", "{report}");
    let text = support::combined_output(&fixture.client.run(&[
        "remote",
        "doctor",
        ALIAS,
        "--timeout",
        "2",
    ]));
    assert!(text.contains("host_offline"), "{text}");
    fixture
        .host
        .ok(&["remote", "enable", "--yes"], "enable hosting again");

    // Host key changed: the saved host's pinned key no longer matches.
    let known = fixture
        .client
        .root()
        .join("state/connections/known-hosts/default.json");
    let original = std::fs::read_to_string(&known).expect("read known hosts");
    let record: serde_json::Value = serde_json::from_str(&original).expect("known hosts JSON");
    let pinned = record["hosts"][ALIAS]["host_key"]
        .as_str()
        .expect("pinned host key")
        .to_owned();
    let other = iroh::SecretKey::generate().public().to_string();
    std::fs::write(&known, original.replace(&pinned, &other)).expect("change pinned key");
    let (ok, report) = doctor(&fixture.client, ALIAS);
    assert!(!ok);
    assert_eq!(failure_kind(&report), "host_key_changed", "{report}");
    std::fs::write(&known, &original).expect("restore pinned key");
    let (ok, report) = doctor(&fixture.client, ALIAS);
    assert!(ok, "{report}");

    // Revoked: the host tells the device, and the doctor names it.
    fixture.host.ok(&["remote", "revoke", DEVICE], "revoke");
    let (ok, report) = doctor(&fixture.client, ALIAS);
    assert!(!ok);
    assert_eq!(failure_kind(&report), "device_revoked", "{report}");
    fixture.client.assert_daemonless();

    // The audit log recorded it all, with no secrets.
    let output = fixture.host.run(&["remote", "log", "--json"]);
    support::assert_success(&output, "remote log --json");
    let events = json_of(&output, "remote log");
    let kinds: Vec<_> = events
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["kind"].as_str().unwrap_or_default().to_owned())
        .collect();
    for kind in ["paired", "connected", "disconnected", "revoked", "refused"] {
        assert!(
            kinds.iter().any(|k| k == kind),
            "`{kind}` missing: {events}"
        );
    }
    assert!(
        events
            .as_array()
            .expect("events")
            .iter()
            .all(|event| { event["device_name"] == DEVICE && event["at"].is_u64() }),
        "{events}"
    );
    let text = fixture
        .host
        .ok(&["remote", "log", "--limit", "5"], "remote log");
    assert!(text.contains("refused") && text.contains(DEVICE), "{text}");
    assert!(!text.contains("bmux-pair:"), "{text}");
}

// ---------------------------------------------------------------------------------------
// Fixture self-tests: always run, so the harness is known good before the gates are.
// ---------------------------------------------------------------------------------------

#[test]
fn fixture_terminal_renders_screen_and_answers_typed_commands() {
    let root = tempfile::tempdir().expect("terminal root");
    let env: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> = vec![
        ("ENV".into(), None),
        ("PS1".into(), Some("$ ".into())),
        ("HOME".into(), Some(root.path().as_os_str().to_owned())),
    ];
    let mut term = Term::start("sh", Path::new("/bin/sh"), &["-i"], root.path(), &env);
    let typed = marker("fixture");
    term.type_marker(&typed);
    term.wait_text(&typed, STEP);
    // The echo of the typed command never contains the marker itself.
    let echoed = term
        .lines()
        .into_iter()
        .filter(|line| line.contains("printf"))
        .any(|line| line.contains(&typed));
    assert!(!echoed, "the marker leaked into the typed command");
    term.send(b"exit\r");
    let exit = term.wait_exit(STEP).expect("shell exits");
    assert!(exit.success(), "{exit:?}");
}

#[test]
fn fixture_udp_proxy_forwards_across_families_and_blackholes() {
    let server = UdpSocket::bind("127.0.0.1:0").expect("echo server");
    let server_addr = server.local_addr().expect("echo address");
    std::thread::spawn(move || {
        let mut buffer = [0u8; 1500];
        while let Ok((len, from)) = server.recv_from(&mut buffer) {
            if &buffer[..len] == b"stop" {
                return;
            }
            let _ = server.send_to(&buffer[..len], from);
        }
    });
    let proxy = UdpProxy::start(SocketAddr::from((Ipv6Addr::LOCALHOST, 0)), server_addr);
    let client = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).expect("IPv6 client");
    client
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("client timeout");
    let mut buffer = [0u8; 1500];
    let mut echo = |payload: &[u8]| -> Option<Vec<u8>> {
        client
            .send_to(payload, proxy.addr())
            .expect("send through proxy");
        client
            .recv_from(&mut buffer)
            .ok()
            .map(|(len, _)| buffer[..len].to_vec())
    };
    assert_eq!(echo(b"one").as_deref(), Some(&b"one"[..]));
    proxy.blackhole(true);
    assert_eq!(echo(b"two"), None, "a blackholed proxy forwarded");
    assert!(proxy.dropped() > 0);
    proxy.blackhole(false);
    assert_eq!(echo(b"three").as_deref(), Some(&b"three"[..]));
    assert!(proxy.forwarded() >= 4);
    let _ = UdpSocket::bind("127.0.0.1:0").and_then(|socket| socket.send_to(b"stop", server_addr));
}

#[test]
fn fixture_local_relay_serves_plain_http_on_loopback() {
    let relay = LocalRelay::start();
    assert!(
        relay.url().starts_with("http://127.0.0.1:"),
        "{}",
        relay.url()
    );
    assert!(relay.is_serving(), "the local relay does not answer HTTP");
    assert!(relay.admitted().is_empty());
}

#[test]
fn fixture_idle_duration_outlasts_the_old_lease() {
    assert!(idle_duration() >= OLD_LEASE * 3);
    assert!(BLACKHOLE > Duration::from_secs(30));
}

// ---------------------------------------------------------------------------------------
// Two local attaches to one tab: what gate 2's owner view relies on.
// ---------------------------------------------------------------------------------------

/// A split made by one attachment reaches every other attachment of the same tab, not only
/// the one that ran the command (which refreshes from its own command's reply).
#[test]
fn local_split_reaches_every_attach_of_the_tab() {
    let host = Host::start_with(
        "split-h",
        RemoteSettings::loopback_host(reserve_udp_port()),
        PREFIX_KEYMAP,
    );
    host.new_tab("work");
    host.new_tab("side");
    let mut first = host.attach("work");
    let existing = marker("existing");
    first.type_marker(&existing);
    first.wait_text(&existing, STEP);
    let mut second = host.attach("work");
    second.wait_text(&existing, STEP);
    prefixed(&mut second, b"%");
    second.wait_screen("a vertical split", STEP, two_pane_border);
    first.wait_screen("the other attach's split", STEP, two_pane_border);
}
