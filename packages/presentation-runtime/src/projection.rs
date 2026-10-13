//! Audience projection preserves inert Markdown structure, never destinations or HTML.
//! Visible authored labels/code are public content; authors must not put secrets in them.
use crate::{Asset, HostError};
use bmux_presentation::{Block, BlockKind, Deck, TextFormat};
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use uuid::Uuid;

// Keep the same syntax options as bmux_markdown::parse_markdown_document. Source
// edits retain renderer semantics without inventing a second Markdown renderer.
fn parser(source: &str) -> Parser<'_> {
    Parser::new_ext(
        source,
        Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_SMART_PUNCTUATION
            | Options::ENABLE_HEADING_ATTRIBUTES
            | Options::ENABLE_GFM
            | Options::ENABLE_MATH,
    )
}

/// All destinations are parsed, including reference-style inline images.
pub(crate) fn markdown_images(source: &str) -> Vec<String> {
    parser(source)
        .filter_map(|event| match event {
            Event::Start(Tag::Image { dest_url, .. }) => Some(dest_url.to_string()),
            _ => None,
        })
        .collect()
}

// Parser-confirmed labels may contain nested brackets and escaped brackets.
fn label(source: &str, range: &Range<usize>) -> Result<Range<usize>, HostError> {
    let bytes = source.as_bytes();
    let opening = (range.start..range.end)
        .find(|&i| bytes[i] == b'[')
        .ok_or(HostError::Invalid)?;
    let mut depth = 1usize;
    let mut i = opening + 1;
    while i < range.end {
        match bytes[i] {
            b'\\' => i += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(opening + 1..i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    Err(HostError::Invalid)
}

/// Remove transport-unsafe syntax using source ranges, retaining inline styles,
/// lists, tables, code fences, math and footnotes. Image IDs come only from the
/// caller's authorized asset map. Autolinks have no independent public label.
pub(crate) fn project_markdown(
    source: &str,
    mut image: impl FnMut(&str) -> Result<String, HostError>,
) -> Result<String, HostError> {
    let parsed = parser(source);
    let mut authorized_images = BTreeSet::new();
    let mut edits: Vec<(Range<usize>, String)> = parsed
        .reference_definitions()
        .iter()
        .map(|(_, definition)| (definition.span.clone(), String::new()))
        .collect();
    for (event, range) in parsed.into_offset_iter() {
        match event {
            Event::Start(Tag::Link { link_type, .. }) => {
                if matches!(link_type, LinkType::Autolink | LinkType::Email) {
                    edits.push((range, String::new()));
                } else {
                    let inner = label(source, &range)?;
                    edits.push((range.start..inner.start, String::new()));
                    edits.push((inner.end..range.end, String::new()));
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let inner = label(source, &range)?;
                let id = image(&dest_url)?;
                // Runtime IDs are opaque UUIDs, not arbitrary Markdown destinations.
                if Uuid::parse_str(&id).is_err() {
                    return Err(HostError::Invalid);
                }
                authorized_images.insert(id.clone());
                edits.push((range.start..inner.start, "![".into()));
                edits.push((inner.end..range.end, format!("]({id})")));
            }
            Event::Html(_) | Event::InlineHtml(_) => edits.push((range, String::new())),
            Event::Start(Tag::Heading {
                id, classes, attrs, ..
            }) if id.is_some() || !classes.is_empty() || !attrs.is_empty() => {
                // Heading attributes are metadata, not visible authored content.
                let text = &source[range.clone()];
                if let Some(start) = text.rfind('{')
                    && let Some(end) = text[start..].find('}')
                {
                    edits.push((
                        range.start + start..range.start + start + end + 1,
                        String::new(),
                    ));
                }
            }
            _ => {}
        }
    }
    // RefDefs exposes only the winning definition for each label. Duplicate
    // definitions are also invisible syntax and must not leak their destinations.
    // Protect visible leaf ranges (including fenced code) before dropping unseen
    // definition lines and their invisible continuation/title lines in one pass.
    let protected: Vec<Range<usize>> = parser(source)
        .into_offset_iter()
        .filter_map(|(event, range)| match event {
            Event::Start(
                Tag::Paragraph | Tag::Heading { .. } | Tag::CodeBlock(_) | Tag::Table(_),
            )
            | Event::Text(_)
            | Event::Code(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => Some(range),
            _ => None,
        })
        .collect();
    let mut protected_index = 0;
    let mut offset = 0;
    let mut definition = false;
    for line in source.split_inclusive('\n') {
        let end = offset + line.len();
        while protected_index < protected.len() && protected[protected_index].end <= offset {
            protected_index += 1;
        }
        let visible = protected
            .get(protected_index)
            .is_some_and(|range| range.start < end);
        let prefix = line.trim_start_matches(|ch: char| {
            ch.is_ascii_whitespace()
                || matches!(ch, '>' | '-' | '+' | '*' | '.' | ')')
                || ch.is_ascii_digit()
        });
        if visible || line.trim().is_empty() {
            definition = false;
        } else if (prefix.starts_with('[') && prefix.contains("]:")) || definition {
            edits.push((offset..end, String::new()));
            definition = true;
        }
        offset = end;
    }
    edits.sort_by_key(|(range, _)| (range.start, std::cmp::Reverse(range.end)));
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    for (range, replacement) in edits {
        // Enclosing removed syntax (e.g. HTML) wins over contained edits.
        if range.start < cursor {
            continue;
        }
        output.push_str(&source[cursor..range.start]);
        output.push_str(&replacement);
        cursor = range.end;
    }
    output.push_str(&source[cursor..]);
    // Removing syntax must not expose a previously inert destination or HTML.
    // Fail closed instead of sending reparsed unsafe syntax to the renderer.
    if parser(&output).any(|event| match event {
        Event::Start(Tag::Link { .. }) | Event::Html(_) | Event::InlineHtml(_) => true,
        Event::Start(Tag::Image { dest_url, .. }) => !authorized_images.contains(dest_url.as_ref()),
        _ => false,
    }) {
        return Err(HostError::Invalid);
    }
    Ok(output)
}

/// Prepare image destinations without changing owner-visible Markdown syntax.
pub(crate) fn remap_markdown_images(
    source: &str,
    mut image: impl FnMut(&str) -> Result<String, HostError>,
) -> Result<String, HostError> {
    let mut edits = Vec::new();
    for (event, range) in parser(source).into_offset_iter() {
        if let Event::Start(Tag::Image { dest_url, .. }) = event {
            let inner = label(source, &range)?;
            let id = image(&dest_url)?;
            if Uuid::parse_str(&id).is_err() {
                return Err(HostError::Invalid);
            }
            edits.push((inner.end..range.end, format!("]({id})")));
        }
    }
    let mut result = source.to_owned();
    for (range, replacement) in edits.into_iter().rev() {
        result.replace_range(range, &replacement);
    }
    Ok(result)
}

pub(crate) fn validate_theme(deck: &Deck) -> Result<(), HostError> {
    match deck.metadata.theme.as_deref() {
        None | Some("light" | "dark") => Ok(()),
        _ => Err(HostError::Invalid),
    }
}

pub(crate) fn project(
    deck: &Deck,
    assets: &BTreeMap<String, Asset>,
) -> Result<(Deck, BTreeMap<String, Asset>), HostError> {
    validate_theme(deck)?;
    let mut deck = deck.public();
    let mut opaque = BTreeMap::new();
    let mut output = BTreeMap::new();
    fn asset_id(
        destination: &str,
        assets: &BTreeMap<String, Asset>,
        opaque: &mut BTreeMap<String, String>,
        output: &mut BTreeMap<String, Asset>,
    ) -> Result<String, HostError> {
        let asset = assets.get(destination).ok_or(HostError::NotFound)?;
        let id = opaque
            .entry(destination.into())
            .or_insert_with(|| Uuid::new_v4().to_string())
            .clone();
        output.entry(id.clone()).or_insert_with(|| Asset {
            id: id.clone(),
            media_type: asset.media_type.clone(),
            bytes: asset.bytes.clone(),
        });
        Ok(id)
    }
    fn project_blocks(
        nodes: &mut [Block],
        assets: &BTreeMap<String, Asset>,
        opaque: &mut BTreeMap<String, String>,
        output: &mut BTreeMap<String, Asset>,
    ) -> Result<(), HostError> {
        for block in nodes {
            match &mut block.kind {
                BlockKind::Image { destination, .. } => {
                    *destination = asset_id(destination, assets, opaque, output)?;
                }
                BlockKind::Text {
                    content,
                    format: TextFormat::Markdown,
                } => {
                    *content = project_markdown(content, |destination| {
                        asset_id(destination, assets, opaque, output)
                    })?;
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
    Ok((deck, output))
}

pub(crate) fn referenced_assets(deck: &Deck) -> BTreeSet<String> {
    fn visit(blocks: &[Block], result: &mut BTreeSet<String>) {
        for block in blocks {
            match &block.kind {
                BlockKind::Image { destination, .. } => {
                    result.insert(destination.clone());
                }
                BlockKind::Text {
                    content,
                    format: TextFormat::Markdown,
                } => {
                    result.extend(markdown_images(content));
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
