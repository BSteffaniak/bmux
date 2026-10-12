//! Canonical fixed-canvas owner for the linked counter demo.
use bmux_presentation::{BlockKind, Deck};
use bmux_presentation_runtime::mirror::MirrorFrame;
use bmux_presentation_runtime::{HostError, Owner, OwnerContext, OwnerUpdate, SurfaceInput};
use bmux_tui::prelude::{Component, Constraints, LayoutCx, PaintCx, Size};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub(crate) struct DemoOwner {
    pub deck: Deck,
    pub rich: crate::rich::RichAssets,
    instances: BTreeMap<String, Arc<Mutex<i64>>>,
    layouts: BTreeMap<String, bmux_tui::component::LayoutNode>,
    revisions: BTreeMap<String, u64>,
}
impl DemoOwner {
    pub fn new(deck: Deck, rich: crate::rich::RichAssets) -> Self {
        Self {
            deck,
            rich,
            instances: BTreeMap::new(),
            layouts: BTreeMap::new(),
            revisions: BTreeMap::new(),
        }
    }
    pub fn scenes(&mut self, context: OwnerContext<'_>) -> Result<Vec<MirrorFrame>, HostError> {
        let Some(slide) = context.state.current_slide(&self.deck) else {
            return Ok(vec![]);
        };
        if !has_counter(&slide.blocks) {
            return Ok(vec![]);
        }
        let counter = self
            .instances
            .entry(context.instance.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(0)));
        let registry = crate::demo::registry(counter);
        let theme = bmux_presentation::PresentationTheme::default();
        let mut component = bmux_presentation::SlideComponent::new(
            slide,
            context.state.reveal_step,
            &theme,
            &registry,
        )
        .rich_content(&self.rich);
        if let Some(block) = slide.blocks.iter().find(|block| {
            matches!(&block.kind, BlockKind::Custom { name, .. } if name == "counter")
                && block.reveal_step <= context.state.reveal_step
        }) {
            component = component.focused_block(&block.id);
        }
        let mut buffer =
            bmux_tui::buffer::Buffer::empty(bmux_tui::geometry::Rect::new(0, 0, 80, 24));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        let layout = component.layout(
            Constraints::tight(Size::new(80, 24)),
            &mut LayoutCx::default(),
        );
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        self.layouts.insert(context.instance.to_owned(), layout);
        let mut images = bmux_tui::image_scene::ImageScene::default();
        images.reconcile(frame.images());
        let revision = self
            .revisions
            .entry(context.instance.to_owned())
            .or_default();
        *revision = revision.checked_add(1).ok_or(HostError::Limit)?;
        Ok(vec![MirrorFrame::capture(
            "counter".into(),
            slide.id.as_str().into(),
            *revision,
            &frame,
            &images,
            16_384,
            512 * 1024,
        )?])
    }
}
fn has_counter(blocks: &[bmux_presentation::Block]) -> bool {
    blocks.iter().any(|block| match &block.kind {
        BlockKind::Custom { name, .. } => name == "counter",
        BlockKind::Columns { columns } => columns.iter().any(|column| has_counter(&column.blocks)),
        _ => false,
    })
}
impl Owner for DemoOwner {
    fn scene(&mut self, context: OwnerContext<'_>) -> Result<Vec<MirrorFrame>, String> {
        self.scenes(context).map_err(|error| error.to_string())
    }
    fn demo_input(
        &mut self,
        context: OwnerContext<'_>,
        input: &SurfaceInput,
    ) -> Result<OwnerUpdate, HostError> {
        if input.surface_id != "counter" {
            return Err(HostError::NotFound);
        }
        let value = self
            .instances
            .get(context.instance)
            .ok_or(HostError::Stale)?;
        let layout = self.layouts.get(context.instance).ok_or(HostError::Stale)?;
        let slide = context
            .state
            .current_slide(&self.deck)
            .ok_or(HostError::Stale)?;
        let registry = crate::demo::registry(value);
        let theme = bmux_presentation::PresentationTheme::default();
        let mut component = bmux_presentation::SlideComponent::new(
            slide,
            context.state.reveal_step,
            &theme,
            &registry,
        )
        .rich_content(&self.rich);
        if let Some(block) = slide.blocks.iter().find(|block| {
            matches!(&block.kind, BlockKind::Custom { name, .. } if name == "counter")
                && block.reveal_step <= context.state.reveal_step
        }) {
            component = component.focused_block(&block.id);
        }
        let event = match input.input.as_str() {
            "+" => bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke::simple(
                bmux_keyboard::KeyCode::Char('+'),
            )),
            "-" => bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke::simple(
                bmux_keyboard::KeyCode::Char('-'),
            )),
            "click" => bmux_tui::event::Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(input.x, input.y),
            )),
            _ => return Err(HostError::Invalid),
        };
        let outcome = component.event(
            &event,
            layout,
            &mut bmux_tui::component::EventCx::new(layout),
        );
        if outcome == bmux_tui::event::EventOutcome::Ignored {
            return Ok(OwnerUpdate {
                outcome: "input outside active control".into(),
                surfaces: self.scenes(context)?,
            });
        }
        Ok(OwnerUpdate {
            outcome: "counter updated".into(),
            surfaces: self.scenes(context)?,
        })
    }
    fn effect(
        &mut self,
        _context: OwnerContext<'_>,
        _name: &str,
        _input: &str,
    ) -> Result<OwnerUpdate, HostError> {
        Err(HostError::Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prepared_owner_reload_is_atomic_and_reconciles_scene_identity() {
        let original = bmux_presentation::DeckBuilder::new("reload")
            .slide(
                bmux_presentation::SlideBuilder::new("old")
                    .custom("demo", "counter", serde_json::json!({}))
                    .build(),
            )
            .build()
            .unwrap();
        let mut host = bmux_presentation_runtime::Host::new(
            original.clone(),
            vec![],
            bmux_presentation_runtime::HostConfig {
                public_audience: true,
                ..Default::default()
            },
        )
        .unwrap();
        host.set_owner(Box::new(DemoOwner::new(
            original,
            crate::rich::RichAssets::default(),
        )))
        .unwrap();
        let viewer = host
            .authenticate(
                bmux_presentation_runtime::PROTOCOL_VERSION,
                None,
                bmux_presentation_runtime::InteractionMode::ReceiveOnly,
            )
            .unwrap();
        let before = host.snapshot(&viewer).unwrap();
        let replacement = bmux_presentation::DeckBuilder::new("reload")
            .slide(
                bmux_presentation::SlideBuilder::new("new")
                    .custom("demo", "counter", serde_json::json!({}))
                    .build(),
            )
            .build()
            .unwrap();
        host.reload_with_owner(
            replacement.clone(),
            vec![],
            Box::new(DemoOwner::new(
                replacement,
                crate::rich::RichAssets::default(),
            )),
        )
        .unwrap();
        let after = host.snapshot(&viewer).unwrap();
        assert_eq!(after.cursor.run_id, before.cursor.run_id);
        assert!(after.cursor.deck_revision > before.cursor.deck_revision);
        assert_eq!(after.surfaces[0].slide_id, "new");
        assert_eq!(after.state.slide_id, Some("new".into()));
    }
    #[test]
    fn hosted_modes_publish_scenes_and_preserve_private_notes() {
        use bmux_presentation_runtime::{
            Action, Credential, Host, HostConfig, InteractionMode, PROTOCOL_VERSION, Scopes,
            Submission,
        };
        let mut deck = bmux_presentation::DeckBuilder::new("hosted")
            .slide(
                bmux_presentation::SlideBuilder::new("s")
                    .custom("demo", "counter", serde_json::json!({}))
                    .build(),
            )
            .build()
            .unwrap();
        deck.slides[0].notes.text = "SECRET-NOTES".into();
        let mut host = Host::new(deck.clone(), vec![], HostConfig::default()).unwrap();
        host.set_owner(Box::new(DemoOwner::new(
            deck,
            crate::rich::RichAssets::default(),
        )))
        .unwrap();
        let presenter = Credential::generate(
            Scopes {
                notes: true,
                navigate: true,
                demo_input: true,
                effects: false,
            },
            std::time::Duration::from_secs(60),
        );
        let secret = presenter.secret.clone();
        host.add_credential(presenter).unwrap();
        let audience = Credential::generate(
            Scopes {
                notes: false,
                navigate: true,
                demo_input: true,
                effects: false,
            },
            std::time::Duration::from_secs(60),
        );
        let audience_secret = audience.secret.clone();
        host.add_credential(audience).unwrap();
        let readonly = host
            .authenticate(
                PROTOCOL_VERSION,
                Some(&audience_secret),
                InteractionMode::ReceiveOnly,
            )
            .unwrap();
        let snapshot = host.snapshot(&readonly).unwrap();
        assert_eq!(snapshot.surfaces.len(), 1);
        assert!(snapshot.notes.is_none());
        assert!(
            !serde_json::to_string(&snapshot.deck)
                .unwrap()
                .contains("SECRET-NOTES")
        );
        let submit = host
            .authenticate(PROTOCOL_VERSION, Some(&secret), InteractionMode::SubmitOnly)
            .unwrap();
        assert!(host.poll(&submit, None).is_err());
        let initial = host.snapshot(&submit).unwrap();
        assert_eq!(initial.notes.as_deref(), Some("SECRET-NOTES"));
        let frame = &initial.surfaces[0];
        let ack = host
            .submit(
                &submit,
                Submission {
                    action_id: host.next_action_id(&submit).unwrap().unwrap(),
                    expected: initial.cursor,
                    action: Action::DemoInput(SurfaceInput {
                        surface_id: frame.surface_id.clone(),
                        surface_revision: frame.revision,
                        x: 0,
                        y: 0,
                        input: "+".into(),
                    }),
                },
            )
            .unwrap();
        assert_eq!(ack.action_id, 1);
        let shared = host
            .authenticate(PROTOCOL_VERSION, Some(&secret), InteractionMode::Shared)
            .unwrap();
        let shared_frame = host.snapshot(&shared).unwrap().surfaces.remove(0);
        let text = shared_frame
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert!(text.contains("Counter: 1"));
        let independent = host
            .authenticate(
                PROTOCOL_VERSION,
                Some(&audience_secret),
                InteractionMode::Independent,
            )
            .unwrap();
        let independent_frame = host.snapshot(&independent).unwrap().surfaces.remove(0);
        let text = independent_frame
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert!(text.contains("Counter: 0"));
    }
    #[test]
    fn linked_owner_publishes_fixed_mirror_and_isolates_instances() {
        let deck = bmux_presentation::DeckBuilder::new("d")
            .slide(
                bmux_presentation::SlideBuilder::new("s")
                    .custom("demo", "counter", serde_json::json!({}))
                    .build(),
            )
            .build()
            .unwrap();
        let state = bmux_presentation::PresentationState::new(&deck);
        let cursor = bmux_presentation_runtime::Cursor {
            run_id: "r".into(),
            deck_revision: 1,
            revision: 1,
        };
        let mut owner = DemoOwner::new(deck, crate::rich::RichAssets::default());
        let first = owner
            .scenes(OwnerContext {
                participant: "p",
                instance: "shared",
                state: &state,
                cursor: &cursor,
            })
            .unwrap();
        assert_eq!((first[0].width, first[0].height), (80, 24));
        let changed = owner
            .demo_input(
                OwnerContext {
                    participant: "p",
                    instance: "shared",
                    state: &state,
                    cursor: &cursor,
                },
                &SurfaceInput {
                    surface_id: "counter".into(),
                    surface_revision: first[0].revision,
                    x: 0,
                    y: 0,
                    input: "+".into(),
                },
            )
            .unwrap();
        assert!(changed.surfaces[0].revision > first[0].revision);
        assert_eq!(*owner.instances["shared"].lock().unwrap(), 1);
        owner
            .scenes(OwnerContext {
                participant: "p2",
                instance: "private",
                state: &state,
                cursor: &cursor,
            })
            .unwrap();
        assert_eq!(*owner.instances["private"].lock().unwrap(), 0);
    }
}
