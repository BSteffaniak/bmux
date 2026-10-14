//! Shared presenter product ownership for local and managed playbook demos.
use crate::{
    playbook::{Approval, DemoConfig},
    playbook_controller::{Control, Controller, LifecycleSnapshot},
};
use bmux_presenter::{Block, BlockKind, Deck};
use bmux_tui::prelude::{Element, TextBlock};
use std::{collections::BTreeMap, path::PathBuf};

pub(crate) struct DemoEntry {
    pub slide: String,
    pub config: DemoConfig,
    pub controller: Controller,
    pub snapshot: LifecycleSnapshot,
    pub open: bool,
}
pub(crate) struct LiveDemos {
    pub entries: BTreeMap<String, DemoEntry>,
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
        let mut demos = Self {
            entries: BTreeMap::new(),
        };
        for slide in &deck.slides {
            demos.collect(slide.id.as_str(), &slide.blocks, environment)?;
        }
        Ok(demos)
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
                    environment
                        .approval
                        .preparation(&config, &environment.executable, &environment.attempts_root)
                        .map_err(|error| error.to_string())?;
                    let controller = Controller::spawn(
                        &environment.runtime,
                        environment.paths.clone(),
                        environment.approval.clone(),
                        config.clone(),
                        environment.executable.clone(),
                        environment.attempts_root.clone(),
                    );
                    let snapshot = controller.latest();
                    self.entries.insert(
                        block.id.0.clone(),
                        DemoEntry {
                            slide: slide.into(),
                            config,
                            controller,
                            snapshot,
                            open: false,
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
    pub fn settled(&self) -> bool {
        self.entries
            .values()
            .all(|entry| entry.controller.settled())
    }
    pub async fn shutdown(&mut self) -> Result<(), String> {
        for entry in self.entries.values() {
            entry.controller.request_shutdown();
        }
        for entry in self.entries.values_mut() {
            entry.controller.shutdown_settled().await?;
        }
        Ok(())
    }
    pub fn update(&mut self) -> bool {
        let mut dirty = false;
        for entry in self.entries.values_mut() {
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
            entry.controller.cancel_navigation();
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
            "run" | "r" => Control::Run,
            "stop" | "s" => Control::Stop,
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
    pub fn view(&self, audience: bool) -> Element<'_> {
        let mut component = crate::playbook_component::PlaybookDemo::new(
            &self.config.title,
            &self.controller,
            &self.snapshot,
        )
        .audience(audience);
        if let Some(capture) = &self.snapshot.surface {
            component = component.retained_surface(&capture.surface);
        }
        Element::new(component)
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
