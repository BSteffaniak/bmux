//! Trusted Rust embedding: two named components, owner input, and staged reload.
//! Run with `cargo run -p bmux_presentation_plugin --example linked_host`.
use bmux_presentation::{DeckBuilder, SlideBuilder};
use bmux_presentation_plugin::{LinkedOwner, linked_demo_registry};
use bmux_presentation_runtime::{
    Action, Credential, Host, HostConfig, InteractionMode, PROTOCOL_VERSION, Scopes, Submission,
    SurfaceInput,
};
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let deck = DeckBuilder::new("linked-host")
        .slide(
            SlideBuilder::new("controls")
                .custom("count", "counter", serde_json::json!({}))
                .custom("switch", "toggle", serde_json::json!({}))
                .build(),
        )
        .build()
        .map_err(anyhow::Error::msg)?;
    let assets = Vec::new();
    // An embedding application can instead register its own trusted Rust
    // LinkedComponentFactory implementations in a LinkedRegistry.
    let registry = linked_demo_registry();
    let owner = LinkedOwner::new(deck.clone(), &assets, registry)?;
    let mut host = Host::new(deck.clone(), assets, HostConfig::default())?;
    host.set_owner(Box::new(owner))?;
    let credential = Credential::generate(
        Scopes {
            notes: false,
            navigate: true,
            demo_input: true,
            effects: false,
        },
        Duration::from_secs(60),
    );
    let secret = credential.secret.clone();
    host.add_credential(credential)?;
    let participant =
        host.authenticate(PROTOCOL_VERSION, Some(&secret), InteractionMode::Shared)?;
    let snapshot = host.snapshot(&participant)?;
    // Select the stable block ID, not the first surface or its component type.
    let toggle = snapshot
        .surfaces
        .iter()
        .find(|surface| surface.surface_id == "switch")
        .ok_or_else(|| anyhow::anyhow!("toggle surface missing"))?;
    host.submit(
        &participant,
        Submission {
            action_id: host
                .next_action_id(&participant)?
                .ok_or_else(|| anyhow::anyhow!("input unavailable"))?,
            expected: snapshot.cursor,
            action: Action::DemoInput(SurfaceInput {
                surface_id: toggle.surface_id.clone(),
                surface_revision: toggle.revision,
                x: 0,
                y: 0,
                input: " ".into(),
            }),
        },
    )?;
    // Retains compatible values and prepares fresh geometry/assets atomically.
    host.reload(deck, vec![])?;
    let reloaded = host.snapshot(&participant)?;
    assert_eq!(reloaded.surfaces.len(), 2);
    println!("Linked counter and toggle hosted; toggle input and compatible reload accepted.");
    Ok(())
}
