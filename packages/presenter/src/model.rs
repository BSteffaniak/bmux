//! Portable content. Executable factories and private notes never enter the wire model.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

macro_rules! identity {
    ($name:ident) => {
        #[derive(
            Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }
        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}
identity!(DeckId);
identity!(SlideId);
identity!(BlockId);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub title: String,
    pub author: Option<String>,
    pub theme: Option<String>,
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

/// UTF-8 byte offsets in the original source, not renderer cell positions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLocation {
    pub source: String,
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerNotes {
    pub text: String,
    pub source: Option<SourceLocation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deck {
    pub id: DeckId,
    pub metadata: Metadata,
    pub slides: Vec<Slide>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slide {
    pub id: SlideId,
    pub blocks: Vec<Block>,
    /// Explicit pauses at the end of a slide remain navigable.
    #[serde(default)]
    pub reveal_steps: u32,
    /// Private by default even when callers serialize a local deck directly.
    #[serde(skip)]
    pub notes: SpeakerNotes,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub id: BlockId,
    pub reveal_step: u32,
    pub source: Option<SourceLocation>,
    pub kind: BlockKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextFormat {
    #[default]
    Plain,
    Markdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub weight: u16,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockKind {
    Text {
        content: String,
        format: TextFormat,
    },
    Code {
        language: Option<String>,
        source: String,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Image {
        destination: String,
        alt: String,
        rows: u16,
    },
    Mermaid {
        source: String,
        rows: u16,
    },
    Columns {
        columns: Vec<Column>,
    },
    /// A name and inert configuration, never executable code or permission.
    Custom {
        name: String,
        config: serde_json::Value,
    },
}

impl Slide {
    pub fn max_reveal_step(&self) -> u32 {
        fn maximum(blocks: &[Block]) -> u32 {
            blocks
                .iter()
                .map(|block| match &block.kind {
                    BlockKind::Columns { columns } => block.reveal_step.max(
                        columns
                            .iter()
                            .map(|column| maximum(&column.blocks))
                            .max()
                            .unwrap_or(0),
                    ),
                    _ => block.reveal_step,
                })
                .max()
                .unwrap_or(0)
        }
        self.reveal_steps.max(maximum(&self.blocks))
    }
}

impl Deck {
    pub fn slide(&self, id: &SlideId) -> Option<&Slide> {
        self.slides.iter().find(|slide| &slide.id == id)
    }
    pub fn slide_index(&self, id: &SlideId) -> Option<usize> {
        self.slides.iter().position(|slide| &slide.id == id)
    }
    /// Strip local notes, diagnostic source locations, and custom configuration.
    /// This is NOT an audience sanitization/asset authorization boundary. Authored
    /// Markdown destinations, image destinations, metadata, IDs, text/code/Mermaid,
    /// and custom names remain public content requiring runtime review/redaction.
    /// Runtime must rewrite authorized local asset destinations to opaque IDs and
    /// must not activate custom components received from an audience peer.
    pub fn public(&self) -> Self {
        fn strip_sources(blocks: &mut [Block]) {
            for block in blocks {
                block.source = None;
                if let BlockKind::Custom { config, .. } = &mut block.kind {
                    *config = serde_json::Value::Null;
                }
                if let BlockKind::Columns { columns } = &mut block.kind {
                    for column in columns {
                        strip_sources(&mut column.blocks);
                    }
                }
            }
        }
        let mut deck = self.clone();
        for slide in &mut deck.slides {
            slide.notes = SpeakerNotes::default();
            strip_sources(&mut slide.blocks);
        }
        deck
    }
    /// IDs are nonempty and unique in their persistent scope; zero-width columns are invalid.
    pub fn validate(&self) -> Result<(), String> {
        fn blocks_valid(blocks: &[Block], ids: &mut BTreeSet<BlockId>) -> Result<(), String> {
            for block in blocks {
                if block.id.0.is_empty() || !ids.insert(block.id.clone()) {
                    return Err(format!("empty or duplicate block ID: {}", block.id.0));
                }
                if let BlockKind::Columns { columns } = &block.kind {
                    if columns.is_empty() || columns.iter().any(|column| column.weight == 0) {
                        return Err("columns require positive weights".into());
                    }
                    for column in columns {
                        blocks_valid(&column.blocks, ids)?;
                    }
                }
            }
            Ok(())
        }
        if self.id.0.is_empty() {
            return Err("empty deck ID".into());
        }
        let mut slides = BTreeSet::new();
        for slide in &self.slides {
            if slide.id.0.is_empty() || !slides.insert(slide.id.clone()) {
                return Err(format!("empty or duplicate slide ID: {}", slide.id.0));
            }
            blocks_valid(&slide.blocks, &mut BTreeSet::new())?;
        }
        Ok(())
    }
}

/// Deterministic FNV-1a identity, independent of Rust's randomized hash state.
pub(crate) fn stable_key(value: &str) -> String {
    let hash = value
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_projection_recursively_removes_sources_and_custom_config() {
        let source = SourceLocation {
            source: "/private/deck.md".into(),
            start: 0,
            end: 1,
            line: 1,
            column: 1,
        };
        let deck = Deck {
            id: "d".into(),
            metadata: Metadata::default(),
            slides: vec![Slide {
                id: "s".into(),
                reveal_steps: 0,
                notes: SpeakerNotes::default(),
                blocks: vec![Block {
                    id: "columns".into(),
                    reveal_step: 0,
                    source: Some(source.clone()),
                    kind: BlockKind::Columns {
                        columns: vec![Column {
                            weight: 1,
                            blocks: vec![Block {
                                id: "custom".into(),
                                reveal_step: 0,
                                source: Some(source),
                                kind: BlockKind::Custom {
                                    name: "linked".into(),
                                    config: serde_json::json!({"secret": "/private/app"}),
                                },
                            }],
                        }],
                    },
                }],
            }],
        };
        let projected = deck.public();
        let wire = serde_json::to_string(&projected).unwrap();
        assert!(!wire.contains("/private"));
        assert!(!wire.contains("secret"));
        assert!(wire.contains("linked"));
        assert!(deck.slides[0].blocks[0].source.is_some());
    }
    #[test]
    fn notes_never_serialize_and_public_projection_removes_them() {
        let deck = Deck {
            id: "deck".into(),
            metadata: Metadata::default(),
            slides: vec![Slide {
                id: "slide".into(),
                blocks: vec![],
                reveal_steps: 0,
                notes: SpeakerNotes {
                    text: "SECRET".into(),
                    source: None,
                },
            }],
        };
        assert!(!serde_json::to_string(&deck).unwrap().contains("SECRET"));
        assert!(deck.public().slides[0].notes.text.is_empty());
        assert_eq!(deck.slides[0].notes.text, "SECRET");
    }
}
