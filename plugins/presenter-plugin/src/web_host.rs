//! Audience-only host observation. Host transport, not browsers, owns navigation.
use anyhow::Result;
use std::time::Duration;

fn audience_player(prepared: crate::viewer::RemoteUpdate) -> crate::player::Player {
    let mut player = crate::player::Player::local(prepared.snapshot.deck, None, false);
    player
        .observe_producer_run(&prepared.snapshot.cursor.run_id)
        .expect("fresh observer producer epoch is available");
    player.state = prepared.snapshot.state;
    player.surfaces = prepared.snapshot.surfaces;
    player.rich = prepared.rich;
    // Never copy privately scoped notes into the browser's player.
    player.remote = Some(crate::player::RemoteConnection {
        actions: tokio::sync::mpsc::channel(1).0,
        updates: tokio::sync::mpsc::channel(1).1,
        receive_only: true,
    });
    player
}

pub(crate) async fn run(
    mut connection: crate::viewer::Connection,
    mut browser: crate::web::Browser,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let result = async {
        let mut cache = crate::viewer::PreparationCache::default();
        let snapshot = connection.snapshot().await?;
        let prepared = connection.prepare(snapshot, &mut cache).await?;
        let mut cursor = prepared.snapshot.cursor.clone();
        let mut player = audience_player(prepared);
        let mut width = 80;
        let mut height = 24;
        let mut tick = tokio::time::interval(Duration::from_millis(300));
        if let Err(error) = browser.publish(&mut player, width, height) {
            eprintln!("Audience update rejected; committed scene retained: {error}");
        }
        loop {
            let changed = tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() { break; }
                    false
                }
                request = browser.viewport_request() => {
                    let Some(request) = request else { break; };
                    if browser.only && browser.viewport_is_current(&request) {
                        let changed = width != request.cols || height != request.rows;
                        width = request.cols;
                        height = request.rows;
                        changed
                    } else { false }
                }
                _ = tick.tick() => {
                    match connection.poll(cursor.clone()).await {
                        Ok(Some(snapshot)) => {
                            let candidate_cursor = snapshot.cursor.clone();
                            match connection.prepare(snapshot, &mut cache).await {
                            Ok(prepared) => {
                                player.observe_producer_run(&prepared.snapshot.cursor.run_id)?;
                                cursor = prepared.snapshot.cursor;
                                player.deck = prepared.snapshot.deck;
                                player.state = prepared.snapshot.state;
                                player.surfaces = prepared.snapshot.surfaces;
                                player.rich = prepared.rich;
                                true
                            }
                            Err(error) => {
                                eprintln!("Audience update rejected; committed scene retained: {error}");
                                cursor = candidate_cursor;
                                false
                            }
                        }
                        },
                        Ok(None) => false,
                        Err(error) => return Err(error),
                    }
                }
            };
            if changed && let Err(error) = browser.publish(&mut player, width, height) {
                eprintln!("Audience update rejected; committed scene retained: {error}");
            }
        }
        Ok(())
    }.await;
    connection.leave().await;
    browser.shutdown().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_presenter::{DeckBuilder, SlideBuilder};
    use bmux_presenter_runtime::{
        Credential, Host, HostConfig, InteractionMode, PROTOCOL_VERSION, Scopes,
    };

    #[tokio::test]
    async fn private_host_projection_never_enters_browser_payload_or_action_authority() {
        let deck = DeckBuilder::new("host")
            .slide(
                SlideBuilder::new("s")
                    .text("body", "PUBLIC")
                    .notes("HOSTPRIVATESECRET")
                    .build(),
            )
            .build()
            .unwrap();
        let mut host = Host::new(deck, vec![], HostConfig::default()).unwrap();
        let credential = Credential::generate(
            Scopes {
                notes: true,
                navigate: true,
                ..Scopes::default()
            },
            Duration::from_secs(60),
        );
        let secret = credential.secret.clone();
        host.add_credential(credential).unwrap();
        let participant = host
            .authenticate(
                PROTOCOL_VERSION,
                Some(&secret),
                InteractionMode::ReceiveOnly,
            )
            .unwrap();
        let host = std::sync::Arc::new(std::sync::Mutex::new(host));
        let mut connection = crate::viewer::Connection::Local {
            host: std::sync::Arc::clone(&host),
            participant: participant.clone(),
        };
        let snapshot = connection.snapshot().await.unwrap();
        assert_eq!(snapshot.notes.as_deref(), Some("HOSTPRIVATESECRET"));
        let mut cache = crate::viewer::PreparationCache::default();
        let cursor = snapshot.cursor.clone();
        let prepared = connection.prepare(snapshot, &mut cache).await.unwrap();
        let mut player = audience_player(prepared);
        assert!(player.notes.is_none());
        assert!(!player.presenter);
        assert!(player.remote.as_ref().unwrap().receive_only);
        let scene = bmux_tui::presentation::PresentationState::new(bmux_tui::geometry::Rect::new(
            0, 0, 40, 20,
        ));
        let prepared = scene
            .prepare(bmux_tui::damage::Damage::Full, |cx| {
                crate::player::paint_browser_audience(&mut player, cx)
            })
            .unwrap();
        let frame = bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
            "host-audience",
            1,
            &prepared,
        )
        .unwrap();
        let encoded = bmux_web_renderer::encode_json(&frame).unwrap();
        assert!(!encoded.contains("HOSTPRIVATESECRET"));
        assert!(!encoded.contains(&secret));
        {
            let mut host = host.lock().unwrap();
            assert!(
                host.submit(
                    &participant,
                    bmux_presenter_runtime::Submission {
                        action_id: 1,
                        expected: cursor,
                        action: bmux_presenter_runtime::Action::Navigate(
                            bmux_presenter::NavigationAction::Next
                        ),
                    }
                )
                .is_err()
            );
        }
        connection.leave().await;
    }
}
