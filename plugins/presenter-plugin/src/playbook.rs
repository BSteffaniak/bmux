//! Explicit executable-input approval. Deck parsing and component build never call this.
use anyhow::{Context, Result, ensure};
use bmux_presenter::{Block, BlockKind, Deck};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

const MAX_PLAYBOOKS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemoConfig {
    pub playbook: PathBuf,
    pub title: String,
}
impl DemoConfig {
    pub fn parse(value: &serde_json::Value) -> Result<Self> {
        let object = value
            .as_object()
            .context("playbook-demo requires an object")?;
        ensure!(
            object
                .keys()
                .all(|key| matches!(key.as_str(), "playbook" | "title" | "on_leave")),
            "unknown playbook-demo property"
        );
        let playbook = PathBuf::from(
            object
                .get("playbook")
                .and_then(serde_json::Value::as_str)
                .context("playbook-demo requires a relative playbook path")?,
        );
        ensure!(
            !playbook.as_os_str().is_empty()
                && playbook
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "playbook path must be relative without traversal"
        );
        let title = match object.get("title") {
            None => "Live playbook".to_owned(),
            Some(value) => value.as_str().context("title must be text")?.to_owned(),
        };
        ensure!(title.len() <= 256, "playbook title exceeds limit");
        if let Some(policy) = object.get("on_leave") {
            ensure!(policy.as_str() == Some("stop"), "on_leave must be stop");
        }
        Ok(Self { playbook, title })
    }
}

/// Private approval data; never serialize into an audience deck or linked state.
#[derive(Clone)]
pub struct Approval {
    root: PathBuf,
    sources: std::sync::Arc<BTreeMap<PathBuf, String>>,
}
impl Approval {
    /// Called only after explicit owner authorization, outside rendering.
    pub fn prepare(deck_path: &Path, deck: &Deck) -> Result<Self> {
        let deck_path = std::fs::canonicalize(deck_path)?;
        let root = deck_path
            .parent()
            .context("deck has no asset root")?
            .to_path_buf();
        let mut approval = Self {
            root,
            sources: std::sync::Arc::new(BTreeMap::new()),
        };
        for slide in &deck.slides {
            approval.collect(&slide.blocks)?;
        }
        Ok(approval)
    }
    fn collect(&mut self, blocks: &[Block]) -> Result<()> {
        for block in blocks {
            match &block.kind {
                BlockKind::Custom { name, config } if name == "playbook-demo" => {
                    let config = DemoConfig::parse(config)?;
                    let source = self.resolve(&config.playbook)?;
                    if !self.sources.contains_key(&source) {
                        ensure!(
                            self.sources.len() < MAX_PLAYBOOKS,
                            "too many approved playbooks"
                        );
                        let digest =
                            bmux_playbook::parser::source_closure_sha256(&source, &self.root)?;
                        std::sync::Arc::make_mut(&mut self.sources).insert(source, digest);
                    }
                }
                BlockKind::Columns { columns } => {
                    for column in columns {
                        self.collect(&column.blocks)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn resolve(&self, relative: &Path) -> Result<PathBuf> {
        ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "playbook path must be relative without traversal"
        );
        let source = self.root.join(relative);
        ensure!(
            source.starts_with(&self.root) && source.is_file(),
            "playbook escapes approved deck root or is not a file"
        );
        Ok(source)
    }
    /// Recheck the complete no-follow include closure before service preparation.
    /// The service freezes the same digest-bound closure before execution.
    pub fn preparation(
        &self,
        config: &DemoConfig,
        executable: &Path,
        attempts_root: &Path,
    ) -> Result<bmux_playbook_plugin_api::playbook_types::Preparation> {
        let source = self.resolve(&config.playbook)?;
        let approved = self
            .sources
            .get(&source)
            .context("playbook was not explicitly approved")?;
        ensure!(
            &bmux_playbook::parser::source_closure_sha256(&source, &self.root)? == approved,
            "playbook include closure changed since approval; explicit reapproval required"
        );
        let executable = std::fs::canonicalize(executable)?;
        ensure!(
            executable.is_file(),
            "selected worker executable is not a file"
        );
        let attempts_root = std::fs::canonicalize(attempts_root)?;
        ensure!(attempts_root.is_dir(), "attempts root is not a directory");
        Ok(bmux_playbook_plugin_api::playbook_types::Preparation {
            source: source.to_string_lossy().into_owned(),
            allowed_root: self.root.to_string_lossy().into_owned(),
            executable: executable.to_string_lossy().into_owned(),
            attempts_root: attempts_root.to_string_lossy().into_owned(),
            request_id: String::new(),
            expected_closure_sha256: approved.clone(),
            trusted_executable_input: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn capture_rejects_foreign_target_without_replacing_retained_view() {
        use tokio::io::AsyncWriteExt;
        let expected = uuid::Uuid::new_v4();
        let foreign = uuid::Uuid::new_v4();
        let (mut writer, reader) = tokio::io::duplex(64);
        let mut header = Vec::from(&b"BMAO\x01"[..]);
        header.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        header.extend_from_slice(foreign.as_bytes());
        writer.write_all(&header).await.unwrap();
        drop(writer);
        let (sender, receiver) = tokio::sync::watch::channel(None);
        assert!(
            crate::playbook_capture::read_capture(reader, "test-owner".into(), 1, expected, sender)
                .await
                .is_err()
        );
        assert!(receiver.borrow().is_none());
    }
    #[test]
    fn shipped_live_demo_is_inert_and_uses_relative_executable_input() {
        let deck = bmux_presenter::parse_markdown(
            "live-demo.md",
            include_str!("../examples/live-demo.md"),
        )
        .unwrap();
        let block = deck
            .slides
            .iter()
            .flat_map(|slide| &slide.blocks)
            .find(|block| block.id.as_str() == "live-demo")
            .unwrap();
        let BlockKind::Custom { name, config } = &block.kind else {
            panic!("custom demo")
        };
        assert_eq!(name, "playbook-demo");
        assert_eq!(
            DemoConfig::parse(config).unwrap().playbook,
            PathBuf::from("live-demo.dsl")
        );
        assert!(
            include_str!("../examples/live-demo.dsl")
                .contains("wait-for pattern=\"Retained final view\"")
        );
    }
    #[test]
    fn approval_rejects_changed_source_without_executing_it() {
        let root =
            std::env::temp_dir().join(format!("presenter-approval-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let deck_path = root.join("deck.md");
        let playbook = root.join("demo.dsl");
        std::fs::write(&deck_path, "deck").unwrap();
        std::fs::write(&playbook, "@include steps.dsl\n").unwrap();
        let included = root.join("steps.dsl");
        std::fs::write(&included, "new-session\n").unwrap();
        let deck = bmux_presenter::DeckBuilder::new("approval")
            .slide(
                bmux_presenter::SlideBuilder::new("s")
                    .custom(
                        "demo",
                        "playbook-demo",
                        serde_json::json!({"playbook":"demo.dsl"}),
                    )
                    .build(),
            )
            .build()
            .unwrap();
        let approval = Approval::prepare(&deck_path, &deck).unwrap();
        let config = DemoConfig::parse(&serde_json::json!({"playbook":"demo.dsl"})).unwrap();
        let executable = std::env::current_exe().unwrap();
        let prepared = approval.preparation(&config, &executable, &root).unwrap();
        assert_eq!(prepared.expected_closure_sha256.len(), 64);
        std::fs::write(&included, "new-session\nsend-keys keys=changed\n").unwrap();
        assert!(approval.preparation(&config, &executable, &root).is_err());
        std::fs::remove_file(included).unwrap();
        std::fs::remove_file(playbook).unwrap();
        std::fs::remove_file(deck_path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[test]
    fn config_never_accepts_executable_json_or_root_override() {
        for value in [
            serde_json::json!({"playbook":"../escape"}),
            serde_json::json!({"playbook":"/absolute"}),
            serde_json::json!({"playbook":"demo", "code":"execute"}),
            serde_json::json!({"playbook":"demo", "allowed_root":"/"}),
            serde_json::json!({"playbook":"demo", "on_leave":"keep"}),
            serde_json::json!({"playbook":"demo", "title":false}),
        ] {
            assert!(DemoConfig::parse(&value).is_err());
        }
        assert!(
            DemoConfig::parse(&serde_json::json!({"playbook":"demos/demo.toml", "title":"Demo"}))
                .is_ok()
        );
    }
}
