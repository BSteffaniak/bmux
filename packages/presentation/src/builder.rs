//! Explicit Rust identities survive edits, reorder, and live reload.
use crate::model::{
    Block, BlockId, BlockKind, Column, Deck, DeckId, Metadata, Slide, SlideId, SpeakerNotes,
    TextFormat,
};

pub struct DeckBuilder {
    deck: Deck,
}
impl DeckBuilder {
    pub fn new(id: impl Into<DeckId>) -> Self {
        Self {
            deck: Deck {
                id: id.into(),
                metadata: Metadata::default(),
                slides: Vec::new(),
            },
        }
    }
    pub fn metadata(mut self, metadata: Metadata) -> Self {
        self.deck.metadata = metadata;
        self
    }
    pub fn slide(mut self, slide: Slide) -> Self {
        self.deck.slides.push(slide);
        self
    }
    pub fn build(self) -> Result<Deck, String> {
        self.deck.validate()?;
        Ok(self.deck)
    }
}

pub struct SlideBuilder {
    slide: Slide,
    step: u32,
}
impl SlideBuilder {
    pub fn new(id: impl Into<SlideId>) -> Self {
        Self {
            slide: Slide {
                id: id.into(),
                blocks: Vec::new(),
                reveal_steps: 0,
                notes: SpeakerNotes::default(),
            },
            step: 0,
        }
    }
    pub fn block(mut self, id: impl Into<BlockId>, kind: BlockKind) -> Self {
        self.slide.blocks.push(Block {
            id: id.into(),
            kind,
            reveal_step: self.step,
            source: None,
        });
        self
    }
    pub fn text(self, id: impl Into<BlockId>, content: impl Into<String>) -> Self {
        self.block(
            id,
            BlockKind::Text {
                content: content.into(),
                format: TextFormat::Plain,
            },
        )
    }
    pub fn markdown(self, id: impl Into<BlockId>, content: impl Into<String>) -> Self {
        self.block(
            id,
            BlockKind::Text {
                content: content.into(),
                format: TextFormat::Markdown,
            },
        )
    }
    pub fn code(
        self,
        id: impl Into<BlockId>,
        language: Option<String>,
        source: impl Into<String>,
    ) -> Self {
        self.block(
            id,
            BlockKind::Code {
                language,
                source: source.into(),
            },
        )
    }
    pub fn columns(self, id: impl Into<BlockId>, columns: Vec<Column>) -> Self {
        self.block(id, BlockKind::Columns { columns })
    }
    pub fn custom(
        self,
        id: impl Into<BlockId>,
        name: impl Into<String>,
        config: serde_json::Value,
    ) -> Self {
        self.block(
            id,
            BlockKind::Custom {
                name: name.into(),
                config,
            },
        )
    }
    pub fn pause(mut self) -> Self {
        self.step = self.step.saturating_add(1);
        self.slide.reveal_steps = self.step;
        self
    }
    pub fn notes(mut self, notes: impl Into<String>) -> Self {
        self.slide.notes.text = notes.into();
        self
    }
    pub fn build(self) -> Slide {
        self.slide
    }
}
