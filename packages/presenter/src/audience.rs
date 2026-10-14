//! Public, bounded presentation document. This is content, never executable component code.
use crate::model::{Block, BlockKind, Deck};
use crate::navigation::PresentationState;
use bmux_web_renderer::{BrowserFrame, Limits};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const CAPABILITY: &str = "bmux.presenter.audience.v1";
pub const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_NODES: usize = 65_536;
const MAX_DEPTH: usize = 32;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudienceSurface {
    pub block_id: String,
    pub frame: BrowserFrame,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudienceDocument {
    pub deck: Deck,
    pub state: PresentationState,
    pub assets: BTreeMap<String, String>,
    pub surfaces: Vec<AudienceSurface>,
}

pub(crate) fn resource_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl AudienceDocument {
    /// Bound JSON before typed decoding and reject private fields serde would otherwise ignore.
    pub fn decode(content: &str) -> Result<Self, String> {
        if content.len() > MAX_DOCUMENT_BYTES {
            return Err("audience document byte limit".into());
        }
        let value: serde_json::Value =
            serde_json::from_str(content).map_err(|error| error.to_string())?;
        fn bounded(
            value: &serde_json::Value,
            depth: usize,
            nodes: &mut usize,
        ) -> Result<(), String> {
            *nodes += 1;
            if depth > MAX_DEPTH || *nodes > MAX_NODES {
                return Err("audience document structure limit".into());
            }
            match value {
                serde_json::Value::Array(values) => {
                    for child in values {
                        bounded(child, depth + 1, nodes)?;
                    }
                }
                serde_json::Value::Object(values) => {
                    for child in values.values() {
                        bounded(child, depth + 1, nodes)?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        bounded(&value, 0, &mut 0)?;
        if let Some(slides) = value
            .pointer("/deck/slides")
            .and_then(serde_json::Value::as_array)
            && slides.iter().any(|slide| slide.get("notes").is_some())
        {
            return Err("private speaker notes in audience document".into());
        }
        let document: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        document.validate()?;
        Ok(document)
    }

    pub fn validate(&self) -> Result<(), String> {
        // Also bound directly constructed documents before recursive deck validation.
        let encoded = serde_json::to_string(self).map_err(|error| error.to_string())?;
        if encoded.len() > MAX_DOCUMENT_BYTES
            || self.deck.slides.len() > 1024
            || self.assets.len() > 128
            || self.surfaces.len() > 128
        {
            return Err("audience document limit".into());
        }
        let mut blocks = BTreeMap::new();
        let mut count = 0;
        fn collect<'a>(
            items: &'a [Block],
            depth: usize,
            count: &mut usize,
            blocks: &mut BTreeMap<String, &'a Block>,
            assets: &BTreeMap<String, String>,
        ) -> Result<(), String> {
            if depth > MAX_DEPTH {
                return Err("audience block depth limit".into());
            }
            for block in items {
                *count += 1;
                if *count > 8192 {
                    return Err("audience block count limit".into());
                }
                if block.source.is_some() {
                    return Err("private block source".into());
                }
                if let BlockKind::Custom { config, .. } = &block.kind
                    && !config.is_null()
                {
                    return Err("private custom configuration".into());
                }
                if blocks.insert(block.id.0.clone(), block).is_some() {
                    return Err("audience block IDs must be globally unique".into());
                }
                let mut image_depth = 0_usize;
                let mut destination = None;
                if let BlockKind::Text {
                    content,
                    format: crate::model::TextFormat::Markdown,
                } = &block.kind
                {
                    for event in
                        pulldown_cmark::Parser::new_ext(content, pulldown_cmark::Options::all())
                    {
                        match event {
                            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Image {
                                dest_url,
                                ..
                            }) => {
                                image_depth += 1;
                                destination = Some(dest_url.into_string());
                            }
                            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Image) => {
                                image_depth = image_depth.saturating_sub(1);
                                if image_depth == 0
                                    && destination
                                        .as_ref()
                                        .is_some_and(|id| !assets.contains_key(id))
                                {
                                    return Err("unmapped audience Markdown image".into());
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if let BlockKind::Columns { columns } = &block.kind {
                    for column in columns {
                        collect(&column.blocks, depth + 1, count, blocks, assets)?;
                    }
                }
            }
            Ok(())
        }
        for slide in &self.deck.slides {
            if slide.notes != crate::model::SpeakerNotes::default() {
                return Err("private speaker notes".into());
            }
            collect(&slide.blocks, 0, &mut count, &mut blocks, &self.assets)?;
        }
        self.deck.validate()?;
        match self.state.current_slide(&self.deck) {
            Some(slide) if self.state.reveal_step <= slide.max_reveal_step() => {}
            None if self.deck.slides.is_empty()
                && self.state.slide_id.is_none()
                && self.state.reveal_step == 0 => {}
            _ => return Err("invalid audience navigation state".into()),
        }
        for (destination, id) in &self.assets {
            if destination.is_empty()
                || destination.len() > 256
                || !destination
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_:".contains(&byte))
                || !resource_id(id)
            {
                return Err("invalid opaque audience asset reference".into());
            }
        }
        for block in blocks.values() {
            if let BlockKind::Image { destination, .. } = &block.kind
                && !self.assets.contains_key(destination)
            {
                return Err("unmapped audience image".into());
            }
        }
        let mut seen = BTreeSet::new();
        let mut surface_cells = 0_usize;
        for surface in &self.surfaces {
            if !blocks.contains_key(&surface.block_id) || !seen.insert(&surface.block_id) {
                return Err("unknown or duplicate audience surface block".into());
            }
            surface
                .frame
                .validate(Limits::default())
                .map_err(|error| error.to_string())?;
            surface_cells = surface_cells
                .checked_add(surface.frame.buffer().cells().len())
                .ok_or("audience surface cell overflow")?;
            if surface_cells > Limits::default().cells {
                return Err("audience surface cell limit".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn document() -> AudienceDocument {
        let deck = crate::DeckBuilder::new("test").build().unwrap();
        AudienceDocument {
            state: PresentationState::new(&deck),
            deck,
            assets: BTreeMap::new(),
            surfaces: vec![],
        }
    }
    #[test]
    fn public_document_roundtrip() {
        let document = document();
        assert!(AudienceDocument::decode(&serde_json::to_string(&document).unwrap()).is_ok());
    }
    #[test]
    fn rejects_private_projection_fields() {
        let mut deck = crate::parse_markdown("public.md", "# Slide\n\npublic")
            .unwrap()
            .public();
        let state = PresentationState::new(&deck);
        deck.slides[0].blocks[0].source = Some(crate::SourceLocation {
            source: "private.md".into(),
            start: 0,
            end: 1,
            line: 1,
            column: 1,
        });
        let document = AudienceDocument {
            deck,
            state,
            assets: BTreeMap::new(),
            surfaces: vec![],
        };
        assert!(document.validate().is_err());
        let mut value = serde_json::to_value(document).unwrap();
        value["deck"]["slides"][0]["notes"] = serde_json::json!({"text":"secret"});
        assert!(AudienceDocument::decode(&value.to_string()).is_err());
    }
    #[test]
    fn rejects_invalid_state_and_resource_identity() {
        let mut document = document();
        document.state.reveal_step = 1;
        assert!(document.validate().is_err());
        document.state.reveal_step = 0;
        document.assets.insert("asset:one".into(), "not-sha".into());
        assert!(document.validate().is_err());
    }
}
