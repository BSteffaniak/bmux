//! Smoke test: the BPDL-generated agents bindings compile, keep their wire
//! identifiers, and round-trip through serde.

use bmux_agents_plugin_api::{agent_states, agents_state};
use uuid::Uuid;

#[test]
fn identifiers_match_bpdl_source() {
    assert_eq!(agent_states::INTERFACE_ID, "agent-states");
    assert_eq!(agents_state::INTERFACE_ID, "agents-state");
    assert_eq!(
        agent_states::STATE_KIND.as_str(),
        "bmux.agents/agent-states"
    );
}

#[test]
fn snapshots_round_trip() {
    let snapshot = agent_states::AgentSnapshot {
        panes: vec![agent_states::PaneAgent {
            session_id: Uuid::from_u128(1),
            pane_id: Uuid::from_u128(2),
            agent: "claude".into(),
            state: agent_states::AgentState::Blocked,
            source: agent_states::AgentSource::Report,
            rule: None,
            message: Some("approve edit?".into()),
            changed_at_ms: 10,
            finished_at_ms: Some(5),
            background_shell_count: None,
            background_shell_since_ms: None,
        }],
        revision: 3,
    };
    let json = serde_json::to_string(&snapshot).unwrap();
    assert_eq!(
        serde_json::from_str::<agent_states::AgentSnapshot>(&json).unwrap(),
        snapshot
    );
}
