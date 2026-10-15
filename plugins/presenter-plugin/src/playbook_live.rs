//! Shared presenter product ownership for local and managed playbook demos.
use crate::{
    playbook::{Approval, DemoConfig},
    playbook_controller::{Control, Controller, LifecycleSnapshot},
};
use bmux_presenter::{Block, BlockKind, Deck};
use bmux_tui::prelude::{Element, TextBlock};
use std::{collections::BTreeMap, path::PathBuf};

#[cfg(test)]
#[path = "playbook_reload_tests.rs"]
pub(crate) mod reload_tests;

pub(crate) struct DemoEntry {
    pub slide: String,
    pub config: DemoConfig,
    execution_key: (String, String),
    pub controller: Controller,
    pub snapshot: LifecycleSnapshot,
    pub open: bool,
    pub viewport: (u16, u16),
    pub preview_size: bmux_tui::geometry::Size,
}
pub(crate) struct LiveDemos {
    pub entries: BTreeMap<String, DemoEntry>,
    // References may disappear; execution ownership survives until explicit shutdown.
    retained: Vec<(String, DemoEntry)>,
    connection: crate::owned_runtime::ExecutionConnection,
    environment: DemoEnvironment,
}
#[derive(Clone)]
pub(crate) struct DemoEnvironment {
    pub paths: bmux_config::ConfigPaths,
    pub approval: Approval,
    pub executable: PathBuf,
    pub attempts_root: PathBuf,
    pub runtime: tokio::runtime::Handle,
}
impl LiveDemos {
    pub fn prepare(deck: &Deck, environment: &DemoEnvironment) -> Result<Self, String> {
        let mut demos = Self::detached(deck, environment)?;
        for entry in demos.entries.values_mut() {
            entry.controller = Self::spawn(environment, &demos.connection, &entry.config);
            entry.snapshot = entry.controller.latest();
        }
        Ok(demos)
    }
    pub fn prepare_local(
        deck: &Deck,
        environment: &DemoEnvironment,
        search_roots: Vec<PathBuf>,
    ) -> Result<Self, String> {
        let mut demos = Self::detached(deck, environment)?;
        let selected = bmux_config::BmuxConfig::load_with_paths_and_overrides(
            &environment.paths,
            &bmux_config::ConfigLoadOverrides::for_process(),
        )
        .map_err(|error| error.to_string())?;
        demos.connection = crate::owned_runtime::ExecutionConnection::Local(
            crate::owned_runtime::OwnedRuntime::with_behavior(
                environment.executable.clone(),
                environment.attempts_root.clone(),
                search_roots,
                selected.general,
                selected.behavior,
            ),
        );
        for entry in demos.entries.values_mut() {
            entry.controller = Self::spawn(environment, &demos.connection, &entry.config);
            entry.snapshot = entry.controller.latest();
        }
        Ok(demos)
    }
    fn spawn(
        environment: &DemoEnvironment,
        connection: &crate::owned_runtime::ExecutionConnection,
        config: &DemoConfig,
    ) -> Controller {
        Controller::spawn_connected(
            &environment.runtime,
            connection.clone(),
            environment.approval.clone(),
            config.clone(),
            environment.executable.clone(),
            environment.attempts_root.clone(),
        )
    }
    fn detached(deck: &Deck, environment: &DemoEnvironment) -> Result<Self, String> {
        let mut demos = Self {
            entries: BTreeMap::new(),
            retained: Vec::new(),
            connection: crate::owned_runtime::ExecutionConnection::Host(environment.paths.clone()),
            environment: environment.clone(),
        };
        for slide in &deck.slides {
            demos.collect(slide.id.as_str(), &slide.blocks, environment)?;
        }
        Ok(demos)
    }
    /// Validate against the original approval before copying retained snapshots.
    /// Title and slide placement are presentation metadata, not execution identity.
    pub fn prepare_reload(&self, deck: &Deck) -> Result<Self, String> {
        let mut candidate = Self::detached(deck, &self.environment)?;
        candidate.connection = self.connection.clone();
        for (id, entry) in &mut candidate.entries {
            if let Some(old) = self.entries.get(id)
                && old.compatible(entry)
            {
                entry.snapshot = old.controller.latest();
                entry.controller = Controller::detached(entry.snapshot.clone());
                entry.open = old.open;
                entry.viewport = old.viewport;
            }
        }
        Ok(candidate)
    }
    /// Reload changes presentation references, never execution lifetime.
    pub fn admit_reload(&mut self, candidate: &Self) -> Result<(), String> {
        let retiring = self
            .entries
            .iter()
            .filter(|(id, entry)| {
                !candidate
                    .entries
                    .get(*id)
                    .is_some_and(|next| entry.compatible(next))
            })
            .count();
        if self.retained.len() + retiring > 16 {
            return Err("retained playbook owner limit reached; explicitly shut down the owner before replacing more demos".into());
        }
        Ok(())
    }
    /// Infallible handoff after every instance has passed retirement admission.
    pub fn commit_reload(&mut self, candidate: &mut Self) {
        for (id, next) in &mut candidate.entries {
            if self.entries.get(id).is_some_and(|old| old.compatible(next)) {
                let mut old = self.entries.remove(id).expect("compatible entry exists");
                old.slide.clone_from(&next.slide);
                old.config.clone_from(&next.config);
                old.snapshot = next.snapshot.clone();
                *next = old;
            } else {
                next.controller =
                    Self::spawn(&candidate.environment, &candidate.connection, &next.config);
                next.snapshot = next.controller.latest();
            }
        }
        candidate.retained.append(&mut self.retained);
        candidate
            .retained
            .retain(|(_, entry)| !entry.controller.settled());
        for (id, next) in &mut candidate.entries {
            if next.snapshot.attempt.is_none()
                && let Some(index) = candidate
                    .retained
                    .iter()
                    .position(|(old_id, old)| old_id == id && old.compatible(next))
            {
                let (_, mut old) = candidate.retained.remove(index);
                old.slide.clone_from(&next.slide);
                old.config.clone_from(&next.config);
                old.open = false;
                *next = old;
            }
        }
        candidate
            .retained
            .extend(
                std::mem::take(&mut self.entries)
                    .into_iter()
                    .map(|(id, mut entry)| {
                        entry.open = false;
                        (id, entry)
                    }),
            );
    }
    fn collect(
        &mut self,
        slide: &str,
        blocks: &[Block],
        environment: &DemoEnvironment,
    ) -> Result<(), String> {
        for block in blocks {
            match &block.kind {
                BlockKind::Custom { name, config } if name == "playbook-demo" => {
                    if self.entries.len() >= 16 {
                        return Err("playbook demo limit exceeded".into());
                    }
                    let config = DemoConfig::parse(config).map_err(|error| error.to_string())?;
                    // Validate original content approval without starting execution.
                    let (source, digest) = environment
                        .approval
                        .approved_identity(&config)
                        .map_err(|error| error.to_string())?;
                    let parsed = environment
                        .approval
                        .preview(&config)
                        .map_err(|e| e.to_string())?;
                    let preview_size = bmux_tui::geometry::Size::new(
                        parsed.config.viewport.cols,
                        parsed.config.viewport.rows,
                    );
                    let snapshot = LifecycleSnapshot {
                        owner_epoch: uuid::Uuid::new_v4(),
                        revision: 0,
                        attempt: None,
                        acknowledgment: None,
                        surface: None,
                        cancellation_revision: 0,
                        unresolved: false,
                        error: None,
                    };
                    let controller = Controller::detached(snapshot.clone());
                    self.entries.insert(
                        block.id.0.clone(),
                        DemoEntry {
                            slide: slide.into(),
                            execution_key: (source.to_string_lossy().into_owned(), digest),
                            config,
                            controller,
                            snapshot,
                            open: false,
                            viewport: (0, 0),
                            preview_size,
                        },
                    );
                }
                BlockKind::Columns { columns } => {
                    for column in columns {
                        self.collect(slide, &column.blocks, environment)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    pub async fn shutdown(&mut self) -> Result<(), String> {
        if let crate::owned_runtime::ExecutionConnection::Local(runtime) = &self.connection {
            runtime.request_shutdown();
        }
        for entry in self
            .entries
            .values()
            .chain(self.retained.iter().map(|(_, entry)| entry))
        {
            entry.controller.request_shutdown();
        }
        for entry in self
            .entries
            .values_mut()
            .chain(self.retained.iter_mut().map(|(_, entry)| entry))
        {
            entry.controller.shutdown_settled().await?;
        }
        if let crate::owned_runtime::ExecutionConnection::Local(runtime) = &self.connection {
            runtime.shutdown().await?;
        }
        Ok(())
    }
    pub fn update(&mut self) -> bool {
        let mut dirty = false;
        for entry in self
            .entries
            .values_mut()
            .chain(self.retained.iter_mut().map(|(_, entry)| entry))
        {
            if let Some(snapshot) = entry.controller.take_changed()
                && snapshot.owner_epoch == entry.snapshot.owner_epoch
            {
                entry.snapshot = snapshot;
                dirty = true;
            }
        }
        dirty
    }
    pub fn leave(&mut self, slide: &str) -> Result<(), String> {
        for entry in self
            .entries
            .values_mut()
            .filter(|entry| entry.slide == slide)
        {
            entry.open = false;
        }
        Ok(())
    }
    pub fn input_committed(
        &mut self,
        id: &str,
        epoch: uuid::Uuid,
        generation: Option<u64>,
        input: &str,
    ) -> Result<String, String> {
        let entry = self.entries.get(id).ok_or("playbook demo not found")?;
        if entry.controller.owner_epoch() != epoch
            || entry
                .controller
                .latest()
                .attempt
                .as_ref()
                .map(|attempt| attempt.generation)
                != generation
        {
            return Err("displayed playbook identity is stale; wait for committed redraw".into());
        }
        self.input_expected(id, input, true, generation)
    }
    pub fn input(&mut self, id: &str, input: &str, execute: bool) -> Result<String, String> {
        let generation = self
            .entries
            .get(id)
            .ok_or("playbook demo not found")?
            .snapshot
            .attempt
            .as_ref()
            .map(|attempt| attempt.generation);
        self.input_expected(id, input, execute, generation)
    }
    fn input_expected(
        &mut self,
        id: &str,
        input: &str,
        execute: bool,
        expected_generation: Option<u64>,
    ) -> Result<String, String> {
        let entry = self.entries.get_mut(id).ok_or("playbook demo not found")?;
        match input {
            "pan:left" | "pan:right" | "pan:up" | "pan:down" if entry.open => {
                let (x, y) = &mut entry.viewport;
                if let Some(capture) = &entry.snapshot.surface {
                    let area = capture.surface.buffer().area();
                    *x = (*x).min(area.width.saturating_sub(1));
                    *y = (*y).min(area.height.saturating_sub(1));
                }
                match input {
                    "pan:left" => *x = x.saturating_sub(8),
                    "pan:right" => *x = x.saturating_add(8),
                    "pan:up" => *y = y.saturating_sub(4),
                    _ => *y = y.saturating_add(4),
                }
                return Ok("viewport moved; execution unchanged".into());
            }
            "open" | "click" | "o" => {
                entry.open = true;
                return Ok("overlay opened; execution unchanged".into());
            }
            "close" | "escape" => {
                entry.open = false;
                return Ok("overlay hidden; execution unchanged".into());
            }
            _ => {}
        }
        if !execute {
            return Err("playbook execution requires effects authority".into());
        }
        if !entry.open {
            return Err("open the playbook overlay before controlling execution".into());
        }
        let control = match input {
            "play-toggle" => crate::playbook_component::play_control(&entry.controller.latest()),
            "speed:half" => Control::Speed(crate::playbook_controller::PlaybackSpeed::Half),
            "speed:normal" => Control::Speed(crate::playbook_controller::PlaybackSpeed::Normal),
            "speed:double" => Control::Speed(crate::playbook_controller::PlaybackSpeed::Double),
            "speed:instant" => Control::Speed(crate::playbook_controller::PlaybackSpeed::Instant),
            "run" | "r" => Control::Run,
            "stop" | "s" => Control::Stop,
            "reattach" => Control::Reattach,
            "restart" | "R" => Control::Restart,
            "pause" | "p" => Control::Pause,
            "step" | "n" => Control::Step,
            "resume" | "c" => Control::Resume,
            _ => return Err("unknown playbook control".into()),
        };
        entry.controller.admit(expected_generation, control)?;
        Ok("playbook command admitted; completion pending".into())
    }
    pub fn visible(&self, slide: &str) -> Option<(&str, &DemoEntry)> {
        self.entries
            .iter()
            .find(|(_, entry)| entry.slide == slide && entry.open)
            .map(|(id, entry)| (id.as_str(), entry))
    }
}

impl DemoEntry {
    fn compatible(&self, next: &Self) -> bool {
        self.config.playbook == next.config.playbook && self.execution_key == next.execution_key
    }
    /// Public projection of the full authored terminal, never the native panned viewport.
    pub(crate) fn audience_playbook(
        &self,
        generation: &str,
        revision: u64,
    ) -> Result<bmux_presenter::audience::AudiencePlaybook, String> {
        use bmux_tui::{
            damage::Damage,
            geometry::Rect,
            image::{ImageContribution, ImageKey, ImageLifecycle},
            paint::LocalRect,
            presentation::{PresentationLimits, PresentationState},
        };
        let source = self.snapshot.surface.as_ref().map(|capture| &capture.frame);
        if let Some(frame) = source {
            frame
                .validate(bmux_web_renderer::Limits::default())
                .map_err(|error| error.to_string())?;
            let area = frame.buffer().area();
            if area.x != 0 || area.y != 0 {
                return Err("audience terminal must have an origin-zero snapshot".into());
            }
        }
        let size = source.map_or(self.preview_size, |frame| {
            let area = frame.buffer().area();
            bmux_tui::geometry::Size::new(area.width, area.height)
        });
        if size.width == 0 || size.height == 0 {
            return Err("empty authored audience terminal dimensions".into());
        }
        let scene = PresentationState::new(Rect::new(0, 0, size.width, size.height));
        let prepared = scene
            .prepare_bounded(
                PresentationLimits {
                    max_cells: 262_144,
                    max_entries: 16_384,
                    max_bytes: 16 * 1024 * 1024,
                },
                Damage::Full,
                |cx| {
                    cx.declare_metadata_availability(
                        LocalRect::new(0, 0, size.width, size.height),
                        false,
                        false,
                    );
                    if let Some(frame) = source {
                        let area = frame.buffer().area();
                        for (index, cell) in frame.buffer().cells().iter().enumerate() {
                            if cell.is_wide_continuation() {
                                continue;
                            }
                            let x = u16::try_from(index % usize::from(area.width)).unwrap_or(0);
                            let y = u16::try_from(index / usize::from(area.width)).unwrap_or(0);
                            cx.set_cell(i32::from(x), i64::from(y), &cell.symbol, cell.style);
                        }
                        for (index, image) in frame.image_scene().placements().enumerate() {
                            let mut image = image.clone();
                            // Managed keys can encode owner/session identity. Preserve
                            // complete image geometry/payload, not private transport IDs.
                            image.key = ImageKey::new(format!("audience-terminal-{index}"));
                            image.lifecycle = ImageLifecycle::Frame;
                            cx.push_image(ImageContribution::Present(image));
                        }
                        if let Some(cursor) = frame.cursor() {
                            cx.set_cursor_local(
                                cursor.position.x,
                                cursor.position.y,
                                cursor.visible,
                            );
                        }
                    }
                },
            )
            .map_err(|error| error.to_string())?
            .ok_or("missing audience terminal preparation")?;
        let terminal = bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
            generation, revision, &prepared,
        )
        .map_err(|error| error.to_string())?;
        let mut status = crate::playbook_component::header_status(&self.snapshot);
        if source.is_none() {
            status.push_str(" · Terminal preview");
        }
        let playbook = bmux_presenter::audience::AudiencePlaybook {
            title: self.config.title.clone(),
            status,
            progress: crate::playbook_component::progress_basis_points(&self.snapshot),
            terminal,
        };
        playbook.validate()?;
        Ok(playbook)
    }

    pub fn view(&self, audience: bool) -> Element<'_> {
        let mut component = crate::playbook_component::PlaybookDemo::new(
            &self.config.title,
            &self.controller,
            &self.snapshot,
        )
        .audience(audience)
        .viewport(self.viewport)
        .preview_size(self.preview_size);
        if let Some(capture) = &self.snapshot.surface {
            component = component.retained_surface(&capture.surface);
        }
        Element::new(component)
    }
}
#[cfg(test)]
mod audience_tests {
    use super::*;
    use bmux_tui::{
        damage::Damage,
        embedded_surface::{RetainedSurface, SurfaceIdentity, SurfaceLimits},
        geometry::Rect,
        image::{
            ImageContribution, ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement,
        },
        presentation::PresentationState,
        style::Style,
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn full_terminal_keeps_corners_images_and_cursor_without_private_identity() {
        let fixture = reload_tests::Fixture::new();
        let mut demos =
            LiveDemos::detached(&reload_tests::deck("s", "old"), &fixture.environment).unwrap();
        let entry = demos.entries.get_mut("demo").unwrap();
        entry.open = true;
        entry.viewport = (99, 88);
        let area = Rect::new(0, 0, 300, 140);
        let prepared = PresentationState::new(area)
            .prepare(Damage::Full, |cx| {
                for (x, y, text) in [(0, 0, "A"), (299, 0, "B"), (0, 139, "C"), (299, 139, "D")] {
                    cx.set_cell(x, y, text, Style::new());
                }
                cx.set_cursor_local(299, 139, true);
                cx.push_image(ImageContribution::Present(ImagePlacement {
                    key: ImageKey::new("private-session-image"),
                    payload: ImagePayload::Pixels {
                        bytes: vec![255, 0, 0, 255],
                        width: 1,
                        height: 1,
                        format: bmux_tui::image::ImagePixelFormat::Rgba8,
                    },
                    destination: Rect::new(250, 130, 10, 5),
                    clip: area,
                    fit: ImageFit::Stretch,
                    allocation: None,
                    lifecycle: ImageLifecycle::Frame,
                }));
            })
            .unwrap();
        let frame = bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
            "private-session-identity",
            42,
            &prepared,
        )
        .unwrap();
        let retained = RetainedSurface::new(
            SurfaceIdentity {
                owner: "private-owner".into(),
                generation: 1,
                revision: 42,
            },
            Arc::new(frame.buffer().clone()),
            Arc::new(frame.image_scene().clone()),
            SurfaceLimits {
                max_cells: 262_144,
                max_bytes: 16 * 1024 * 1024,
            },
        )
        .unwrap();
        entry.snapshot.error = Some("private diagnostic".into());
        entry.snapshot.surface = Some(Arc::new(crate::playbook_capture::CaptureSnapshot {
            attempt_generation: 1,
            epoch: uuid::Uuid::new_v4(),
            session: uuid::Uuid::new_v4(),
            frame: Arc::new(frame),
            surface: Arc::new(retained),
        }));
        let playbook = entry.audience_playbook("public", 7).unwrap();
        assert_eq!(playbook.terminal.buffer().area(), area);
        let text = playbook
            .terminal
            .buffer()
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        for corner in ['A', 'B', 'C', 'D'] {
            assert!(text.contains(corner));
        }
        let cursor = playbook.terminal.cursor().unwrap();
        assert_eq!(
            (cursor.position.x, cursor.position.y, cursor.visible),
            (299, 139, true)
        );
        let image = playbook.terminal.image_scene().placements().next().unwrap();
        assert_eq!(image.destination, Rect::new(250, 130, 10, 5));
        assert_eq!(
            image.payload,
            entry
                .snapshot
                .surface
                .as_ref()
                .unwrap()
                .frame
                .image_scene()
                .placements()
                .next()
                .unwrap()
                .payload
        );
        assert_eq!(playbook.terminal.generation(), "public");
        assert_eq!(playbook.terminal.revision(), 7);
        let json = serde_json::to_string(&playbook).unwrap();
        assert!(!json.contains("private-"));
        assert!(!json.contains("private diagnostic"));
        assert_eq!(entry.viewport, (99, 88));
        assert!(entry.snapshot.attempt.is_none());
    }
}

/// Inert allowlist registration. No controller or executable resources in JSON.
pub(crate) struct DemoFactory;
impl bmux_presenter::custom::LinkedComponentFactory for DemoFactory {
    fn state_version(&self) -> u64 {
        1
    }
    fn initial_state(&self, block: &Block) -> Result<serde_json::Value, String> {
        let BlockKind::Custom { config, .. } = &block.kind else {
            return Err("custom block required".into());
        };
        // Public decks deliberately redact executable configuration. This factory
        // owns only an inert placeholder; Approval/LiveDemos still require the
        // complete private config before creating any execution resources.
        if !config.is_null() {
            DemoConfig::parse(config).map_err(|error| error.to_string())?;
        }
        Ok(serde_json::json!({}))
    }
    fn build<'a>(
        &'a self,
        block: &'a Block,
        _state: bmux_presenter::custom::LinkedState,
    ) -> Result<Element<'a>, String> {
        let BlockKind::Custom { config, .. } = &block.kind else {
            return Err("custom block required".into());
        };
        let text = if config.is_null() {
            "Live playbook\nOwner display pending".to_owned()
        } else {
            let config = DemoConfig::parse(config).map_err(|error| error.to_string())?;
            format!(
                "{}\no: open live demo (execution requires --allow-playbooks)",
                config.title
            )
        };
        Ok(Element::new(
            TextBlock::new(text).id(format!("playbook-demo/{}", block.id.as_str())),
        ))
    }
}

#[cfg(test)]
mod owned_runtime_tests {
    use super::*;
    #[tokio::test]
    async fn overlays_and_reload_leave_local_runtime_untouched() {
        let fixture = reload_tests::Fixture::new();
        let deck = reload_tests::deck("s", "old");
        let mut environment = fixture.environment.clone();
        environment.attempts_root = fixture.root.join("untouched-attempts");
        let mut demos = LiveDemos::prepare_local(&deck, &environment, vec![]).unwrap();
        demos.input("demo", "open", true).unwrap();
        demos.input("demo", "close", true).unwrap();
        let candidate = demos.prepare_reload(&deck).unwrap();
        assert!(!environment.attempts_root.exists());
        assert!(
            candidate
                .entries
                .values()
                .all(|entry| entry.controller.latest().attempt.is_none())
        );
        demos.shutdown().await.unwrap();
        assert!(!environment.attempts_root.exists());
    }
}
