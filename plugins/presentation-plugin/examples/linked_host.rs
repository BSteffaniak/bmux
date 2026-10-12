//! A linked Rust application: DeckBuilder + authoritative Host + trusted Owner.
//! No Rust compiler or viewer-supplied executable is activated by presentation IO.
use bmux_presentation::{DeckBuilder, NavigationAction, SlideBuilder};
use bmux_presentation_runtime::{
    Action, Credential, Host, HostConfig, HostError, InteractionMode, Owner, OwnerContext,
    OwnerUpdate, PROTOCOL_VERSION, Scopes, Submission, SurfaceInput,
};
use std::time::Duration;

struct LinkedOwner;
impl Owner for LinkedOwner {
    fn demo_input(
        &mut self,
        _context: OwnerContext<'_>,
        _input: &SurfaceInput,
    ) -> Result<OwnerUpdate, HostError> {
        Err(HostError::NotFound)
    }
    fn effect(
        &mut self,
        _context: OwnerContext<'_>,
        name: &str,
        input: &str,
    ) -> Result<OwnerUpdate, HostError> {
        if name != "announce" {
            return Err(HostError::Unauthorized);
        }
        // Trusted application-owned effect, not performed by remote viewers.
        Ok(OwnerUpdate {
            outcome: format!("Announcement accepted: {input}"),
            surfaces: vec![],
        })
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let deck = DeckBuilder::new("linked-rust")
        .slide(
            SlideBuilder::new("intro")
                .text("title", "Linked Rust presentation")
                .pause()
                .text("reveal", "Pure reducer and authoritative runtime")
                .build(),
        )
        .build()
        .map_err(std::io::Error::other)?;
    let mut host = Host::new(deck, vec![], HostConfig::default())?;
    host.set_owner(Box::new(LinkedOwner))?;
    let credential = Credential::generate(
        Scopes {
            notes: true,
            navigate: true,
            demo_input: false,
            effects: true,
        },
        Duration::from_secs(60),
    );
    let secret = credential.secret.clone();
    host.add_credential(credential)?;
    let participant =
        host.authenticate(PROTOCOL_VERSION, Some(&secret), InteractionMode::Shared)?;
    let initial = host.snapshot(&participant)?;
    let ack = host.submit(
        &participant,
        Submission {
            action_id: host.next_action_id(&participant)?.unwrap(),
            expected: initial.cursor,
            action: Action::Navigate(NavigationAction::Next),
        },
    )?;
    assert_eq!(host.snapshot(&participant)?.state.reveal_step, 1);
    println!("Linked host revision {}", ack.cursor.revision);
    host.disconnect(&participant);
    Ok(())
}
