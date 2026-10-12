//! Audience projection strips diagnostic locations and transport-unsafe destinations.
//! Authored labels/code are public content; authors must not put secrets in them.
use crate::{Asset, HostError};
use bmux_presentation::{Block, BlockKind, Deck, TextFormat};
use pulldown_cmark::{Event, Parser};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub(crate) fn project(
    deck: &Deck,
    assets: &BTreeMap<String, Asset>,
) -> Result<(Deck, BTreeMap<String, Asset>), HostError> {
    let mut deck = deck.public();
    let mut opaque = BTreeMap::new();
    let mut output = BTreeMap::new();
    fn project_blocks(
        nodes: &mut [Block],
        assets: &BTreeMap<String, Asset>,
        opaque: &mut BTreeMap<String, String>,
        output: &mut BTreeMap<String, Asset>,
    ) -> Result<(), HostError> {
        for block in nodes {
            match &mut block.kind {
                BlockKind::Image { destination, .. } => {
                    let asset = assets.get(destination).ok_or(HostError::NotFound)?;
                    let id = opaque
                        .entry(destination.clone())
                        .or_insert_with(|| Uuid::new_v4().to_string())
                        .clone();
                    output.entry(id.clone()).or_insert_with(|| Asset {
                        id: id.clone(),
                        media_type: asset.media_type.clone(),
                        bytes: asset.bytes.clone(),
                    });
                    *destination = id;
                }
                BlockKind::Text { content, format } if *format == TextFormat::Markdown => {
                    // Portable plain projection intentionally discards href/src/title/raw HTML,
                    // including reference links, autolinks, and inline image destinations.
                    let mut plain = String::new();
                    let mut hidden_autolink = false;
                    for event in Parser::new(content) {
                        match event {
                            Event::Start(pulldown_cmark::Tag::Link {
                                link_type:
                                    pulldown_cmark::LinkType::Autolink | pulldown_cmark::LinkType::Email,
                                ..
                            }) => {
                                hidden_autolink = true;
                            }
                            Event::End(pulldown_cmark::TagEnd::Link) => {
                                hidden_autolink = false;
                            }
                            Event::Text(text) | Event::Code(text) if !hidden_autolink => {
                                plain.push_str(&text)
                            }
                            Event::SoftBreak | Event::HardBreak => plain.push('\n'),
                            _ => {}
                        }
                    }
                    *content = plain;
                    *format = TextFormat::Plain;
                }
                BlockKind::Columns { columns } => {
                    for column in columns {
                        project_blocks(&mut column.blocks, assets, opaque, output)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    for slide in &mut deck.slides {
        project_blocks(&mut slide.blocks, assets, &mut opaque, &mut output)?;
    }
    deck.metadata.extra.clear();
    deck.metadata.theme = None;
    Ok((deck, output))
}

pub(crate) fn referenced_assets(deck: &Deck) -> BTreeSet<String> {
    fn visit(blocks: &[Block], result: &mut BTreeSet<String>) {
        for block in blocks {
            match &block.kind {
                BlockKind::Image { destination, .. } => {
                    result.insert(destination.clone());
                }
                BlockKind::Columns { columns } => {
                    for column in columns {
                        visit(&column.blocks, result);
                    }
                }
                _ => {}
            }
        }
    }
    let mut result = BTreeSet::new();
    for slide in &deck.slides {
        visit(&slide.blocks, &mut result);
    }
    result
}
