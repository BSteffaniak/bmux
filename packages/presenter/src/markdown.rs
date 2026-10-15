//! Source-located Markdown adapter. Parsing is inert: no file IO, fetching, or execution.
use crate::model::{
    Block, BlockId, BlockKind, Column, Deck, DeckId, Metadata, Slide, SlideId, SourceLocation,
    SpeakerNotes, TextFormat, stable_key,
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownError {
    pub message: String,
    pub source: SourceLocation,
}
impl fmt::Display for MarkdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: {}",
            self.source.source, self.source.line, self.source.column, self.message
        )
    }
}
impl std::error::Error for MarkdownError {}

fn location(name: &str, source: &str, range: Range<usize>) -> SourceLocation {
    let prefix = &source[..range.start];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    SourceLocation {
        source: name.into(),
        start: range.start,
        end: range.end,
        line,
        column,
    }
}

struct Unit<'a> {
    range: Range<usize>,
    events: Vec<Event<'a>>,
}
fn units(source: &str, base: usize) -> Vec<Unit<'_>> {
    let mut units = Vec::new();
    let mut depth = 0usize;
    let mut current: Option<Unit<'_>> = None;
    for (event, range) in Parser::new_ext(
        source,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
    )
    .into_offset_iter()
    {
        if current.is_none() {
            current = Some(Unit {
                range: range.start + base..range.end + base,
                events: Vec::new(),
            });
        }
        let unit = current.as_mut().expect("current unit was initialized");
        unit.range.end = range.end + base;
        match &event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
        unit.events.push(event);
        if depth == 0 {
            units.push(current.take().expect("current unit exists"));
        }
    }
    if let Some(unit) = current {
        units.push(unit);
    }
    // CommonMark groups adjacent comments, including authoring metadata and
    // BMUX directives. Split complete comment-only HTML blocks, never code or
    // nested Markdown. Source ranges remain offsets into the original input.
    let mut separated = Vec::new();
    for unit in units {
        let raw = &source[unit.range.start - base..unit.range.end - base];
        let standalone_html = unit.events.iter().all(|event| {
            matches!(
                event,
                Event::Html(_) | Event::Start(Tag::HtmlBlock) | Event::End(TagEnd::HtmlBlock)
            )
        });
        if let Some(comments) = standalone_html.then(|| comment_ranges(raw)).flatten() {
            for range in comments {
                separated.push(Unit {
                    range: unit.range.start + range.start..unit.range.start + range.end,
                    events: Vec::new(),
                });
            }
        } else {
            separated.push(unit);
        }
    }
    separated
}

/// Recognize complete comment-only blocks without interpreting comment bodies.
fn comment_ranges(raw: &str) -> Option<Vec<Range<usize>>> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    while offset < raw.len() {
        let rest = &raw[offset..];
        offset += rest.len() - rest.trim_start().len();
        if offset == raw.len() {
            break;
        }
        let body = raw[offset..].strip_prefix("<!--")?;
        let end = offset + 4 + body.find("-->")? + 3;
        ranges.push(offset..end);
        offset = end;
    }
    Some(ranges)
}

fn kind_key(kind: &BlockKind) -> String {
    fn strip_locations(kind: &mut BlockKind) {
        if let BlockKind::Columns { columns } = kind {
            for column in columns {
                for block in &mut column.blocks {
                    block.source = None;
                    strip_locations(&mut block.kind);
                }
            }
        }
    }
    let mut kind = kind.clone();
    strip_locations(&mut kind);
    stable_key(&serde_json::to_string(&kind).expect("portable block serialization cannot fail"))
}

struct ColumnsDraft {
    columns: Vec<Column>,
    active: usize,
    source: SourceLocation,
    id: BlockId,
    step: u32,
}

struct SlideDraft {
    slide: Slide,
    step: u32,
    explicit_id: bool,
    pending_id: Option<BlockId>,
    columns: Option<ColumnsDraft>,
    notes_start: Option<usize>,
    block_counts: BTreeMap<String, usize>,
}
impl SlideDraft {
    fn new() -> Self {
        Self {
            slide: Slide {
                id: SlideId::default(),
                blocks: Vec::new(),
                reveal_steps: 0,
                notes: SpeakerNotes::default(),
            },
            step: 0,
            explicit_id: false,
            pending_id: None,
            columns: None,
            notes_start: None,
            block_counts: BTreeMap::new(),
        }
    }
    fn next_id(&mut self, kind: &BlockKind) -> BlockId {
        if let Some(id) = self.pending_id.take() {
            return id;
        }
        let key = kind_key(kind);
        let count = self.block_counts.entry(key.clone()).or_default();
        *count += 1;
        BlockId::new(format!("block-{key}-{}", *count))
    }
    fn push(&mut self, block: Block) {
        if let Some(group) = &mut self.columns {
            group.columns[group.active].blocks.push(block);
        } else {
            self.slide.blocks.push(block);
        }
    }
}

pub fn parse_markdown(source_name: &str, source: &str) -> Result<Deck, MarkdownError> {
    let error = |message: String, range: Range<usize>| MarkdownError {
        message,
        source: location(source_name, source, range),
    };
    let (metadata, explicit_deck_id, base) =
        front_matter(source).map_err(|message| error(message, 0..0))?;
    let mut deck = Deck {
        id: explicit_deck_id
            .unwrap_or_else(|| DeckId::new(format!("deck-{}", stable_key(source_name)))),
        metadata,
        slides: Vec::new(),
    };
    let mut draft = SlideDraft::new();
    let mut slide_counts = BTreeMap::<String, usize>::new();
    for unit in units(&source[base..], base) {
        let raw = &source[unit.range.clone()];
        let trimmed = raw.trim();
        let directive = trimmed
            .strip_prefix("<!--")
            .and_then(|value| value.strip_suffix("-->"))
            .map(str::trim)
            .and_then(|value| value.strip_prefix("bmux:"))
            .map(str::trim);
        if let Some(start) = draft.notes_start {
            if directive == Some("end-notes") {
                let notes = &source[start..unit.range.start];
                draft.slide.notes.text.push_str(notes);
                draft.slide.notes.source =
                    Some(location(source_name, source, start..unit.range.start));
                draft.notes_start = None;
            }
            continue;
        }
        if unit.events.is_empty() && directive.is_none() {
            // Complete ordinary HTML comments are authoring metadata, not slides.
            // Keep pending block IDs and reveal state untouched.
            continue;
        }
        if let Some(directive) = directive {
            let (command, argument) = directive
                .split_once(char::is_whitespace)
                .unwrap_or((directive, ""));
            let argument = argument.trim();
            match command {
                "slide-id" if !argument.is_empty() => {
                    if draft.explicit_id {
                        return Err(error("duplicate slide-id directive".into(), unit.range));
                    }
                    draft.slide.id = SlideId::new(argument);
                    draft.explicit_id = true;
                }
                "block-id" if !argument.is_empty() => {
                    if draft.pending_id.is_some() {
                        return Err(error("unused block-id directive".into(), unit.range));
                    }
                    draft.pending_id = Some(BlockId::new(argument));
                }
                "pause" if argument.is_empty() => {
                    draft.step = draft.step.saturating_add(1);
                    draft.slide.reveal_steps = draft.step;
                }
                "notes" if argument.is_empty() => {
                    draft.notes_start = Some(unit.range.end);
                }
                "columns" => {
                    if draft.columns.is_some() {
                        return Err(error(
                            "nested columns directives are not supported; use Rust composition"
                                .into(),
                            unit.range,
                        ));
                    }
                    let weights = argument
                        .split(',')
                        .map(|value| value.trim().parse::<u16>())
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|_| {
                            error(
                                "columns expects comma-separated positive weights".into(),
                                unit.range.clone(),
                            )
                        })?;
                    if weights.is_empty() || weights.contains(&0) {
                        return Err(error("columns require positive weights".into(), unit.range));
                    }
                    let id = draft.pending_id.take().unwrap_or_default();
                    let columns = weights
                        .into_iter()
                        .map(|weight| Column {
                            weight,
                            blocks: Vec::new(),
                        })
                        .collect();
                    draft.columns = Some(ColumnsDraft {
                        columns,
                        active: 0,
                        source: location(source_name, source, unit.range),
                        id,
                        step: draft.step,
                    });
                }
                "column" if argument.is_empty() => {
                    let Some(group) = &mut draft.columns else {
                        return Err(error("column outside columns".into(), unit.range));
                    };
                    if group.active + 1 >= group.columns.len() {
                        return Err(error("too many column directives".into(), unit.range));
                    }
                    group.active += 1;
                }
                "end-columns" if argument.is_empty() => {
                    let Some(mut group) = draft.columns.take() else {
                        return Err(error("end-columns outside columns".into(), unit.range));
                    };
                    if group.active + 1 != group.columns.len() {
                        return Err(error(
                            "each declared column needs a column directive".into(),
                            unit.range,
                        ));
                    }
                    group.source.end = unit.range.end;
                    let kind = BlockKind::Columns {
                        columns: group.columns,
                    };
                    let id = if group.id.as_str().is_empty() {
                        draft.next_id(&kind)
                    } else {
                        group.id
                    };
                    draft.push(Block {
                        id,
                        reveal_step: group.step,
                        source: Some(group.source),
                        kind,
                    });
                }
                "component" => {
                    let (name, config) = argument
                        .split_once(char::is_whitespace)
                        .unwrap_or((argument, "{}"));
                    if name.is_empty() {
                        return Err(error(
                            "component needs a registered name".into(),
                            unit.range,
                        ));
                    }
                    let config = serde_json::from_str(config).map_err(|cause| {
                        error(
                            format!("invalid component configuration: {cause}"),
                            unit.range.clone(),
                        )
                    })?;
                    let kind = BlockKind::Custom {
                        name: name.into(),
                        config,
                    };
                    let id = draft.next_id(&kind);
                    draft.push(Block {
                        id,
                        reveal_step: draft.step,
                        source: Some(location(source_name, source, unit.range)),
                        kind,
                    });
                }
                _ => {
                    return Err(error(
                        format!("unknown or malformed bmux directive: {directive}"),
                        unit.range,
                    ));
                }
            }
            continue;
        }
        if matches!(unit.events.as_slice(), [Event::Rule]) {
            if draft.columns.is_some() {
                return Err(error("slide break inside columns".into(), unit.range));
            }
            if draft.pending_id.is_some() {
                return Err(error("block-id has no following block".into(), unit.range));
            }
            finish_slide(&mut deck, &mut draft, &mut slide_counts);
            continue;
        }
        let kind = block_kind(&unit.events, raw);
        let id = draft.next_id(&kind);
        draft.push(Block {
            id,
            reveal_step: draft.step,
            source: Some(location(source_name, source, unit.range)),
            kind,
        });
    }
    if draft.notes_start.is_some() || draft.columns.is_some() || draft.pending_id.is_some() {
        return Err(error(
            "unterminated notes/columns or unused block-id directive".into(),
            source.len()..source.len(),
        ));
    }
    finish_slide(&mut deck, &mut draft, &mut slide_counts);
    deck.validate().map_err(|message| error(message, 0..0))?;
    Ok(deck)
}

fn finish_slide(deck: &mut Deck, draft: &mut SlideDraft, counts: &mut BTreeMap<String, usize>) {
    let mut finished = std::mem::replace(draft, SlideDraft::new());
    if !finished.explicit_id {
        let key = finished
            .slide
            .blocks
            .first()
            .map_or_else(|| "empty".into(), |block| kind_key(&block.kind));
        let count = counts.entry(key.clone()).or_default();
        *count += 1;
        finished.slide.id = SlideId::new(format!("slide-{key}-{}", *count));
    }
    deck.slides.push(finished.slide);
}

fn block_kind(events: &[Event<'_>], raw: &str) -> BlockKind {
    if let Some(Event::Start(Tag::CodeBlock(kind))) = events.first() {
        let language = match kind {
            CodeBlockKind::Fenced(info) => info.split_whitespace().next().map(str::to_owned),
            CodeBlockKind::Indented => None,
        };
        let source: String = events
            .iter()
            .filter_map(|event| {
                if let Event::Text(text) = event {
                    Some(text.as_ref())
                } else {
                    None
                }
            })
            .collect();
        if language.as_deref() == Some("mermaid") {
            return BlockKind::Mermaid { source, rows: 12 };
        }
        return BlockKind::Code { language, source };
    }
    if matches!(events.first(), Some(Event::Start(Tag::Table(_)))) {
        let mut headers = Vec::new();
        let mut rows = Vec::new();
        let mut row = Vec::new();
        let mut cell = String::new();
        let mut in_head = false;
        for event in events {
            match event {
                Event::Start(Tag::TableHead) => in_head = true,
                Event::Start(Tag::TableCell) => cell.clear(),
                Event::Text(text) | Event::Code(text) => cell.push_str(text),
                Event::SoftBreak | Event::HardBreak => cell.push(' '),
                Event::End(TagEnd::TableCell) => row.push(std::mem::take(&mut cell)),
                Event::End(TagEnd::TableHead) => {
                    headers = std::mem::take(&mut row);
                    in_head = false;
                }
                Event::End(TagEnd::TableRow) if !in_head => rows.push(std::mem::take(&mut row)),
                _ => {}
            }
        }
        return BlockKind::Table { headers, rows };
    }
    // A paragraph containing only an image is promoted to a rich image block.
    if events.len() >= 4
        && matches!(events.first(), Some(Event::Start(Tag::Paragraph)))
        && let Some(Event::Start(Tag::Image { dest_url, .. })) = events.get(1)
        && matches!(
            events.get(events.len() - 2),
            Some(Event::End(TagEnd::Image))
        )
    {
        let alt = events[2..events.len() - 2]
            .iter()
            .filter_map(|event| match event {
                Event::Text(text) | Event::Code(text) => Some(text.as_ref()),
                _ => None,
            })
            .collect();
        return BlockKind::Image {
            destination: dest_url.to_string(),
            alt,
            rows: 12,
        };
    }
    BlockKind::Text {
        content: raw.to_owned(),
        format: TextFormat::Markdown,
    }
}

fn front_matter(source: &str) -> Result<(Metadata, Option<DeckId>, usize), String> {
    let mut lines = source.split_inclusive('\n');
    if lines.next().map(str::trim) != Some("---") {
        return Ok((Metadata::default(), None, 0));
    }
    let start = source.find('\n').map_or(source.len(), |index| index + 1);
    let mut end = start;
    for line in lines {
        if matches!(line.trim(), "---" | "...") {
            let body = &source[start..end];
            let values: BTreeMap<String, serde_yaml::Value> = if body.trim().is_empty() {
                BTreeMap::new()
            } else {
                serde_yaml::from_str(body)
                    .map_err(|cause| format!("invalid front matter: {cause}"))?
            };
            let mut metadata = Metadata::default();
            let mut id = None;
            for (key, value) in values {
                let value = match value {
                    serde_yaml::Value::String(value) => value,
                    serde_yaml::Value::Null => String::new(),
                    other => serde_yaml::to_string(&other)
                        .map_err(|cause| cause.to_string())?
                        .trim()
                        .to_owned(),
                };
                match key.as_str() {
                    "title" => metadata.title = value,
                    "author" => metadata.author = Some(value),
                    "theme" => metadata.theme = Some(value),
                    "id" => id = Some(DeckId::new(value)),
                    _ => {
                        metadata.extra.insert(key, value);
                    }
                }
            }
            return Ok((metadata, id, end + line.len()));
        }
        end += line.len();
    }
    Err("unterminated front matter".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_metadata_reveals_private_notes_and_locations() {
        let input = "---\ntitle: Demo\nid: demo\n---\n<!-- bmux:slide-id intro -->\n\n# Hello\n\n<!-- bmux:pause -->\n\nVisible later\n\n<!-- bmux:notes -->\n\nsecret\n\n<!-- bmux:end-notes -->\n\n---\n\nNext";
        let deck = parse_markdown("demo.md", input).unwrap();
        assert_eq!(deck.id.as_str(), "demo");
        assert_eq!(deck.metadata.title, "Demo");
        assert_eq!(deck.slides.len(), 2);
        assert_eq!(deck.slides[0].id.as_str(), "intro");
        assert_eq!(deck.slides[0].blocks[1].reveal_step, 1);
        let source = deck.slides[0].blocks[0].source.as_ref().unwrap();
        assert_eq!(input[source.start..source.end].trim(), "# Hello");
        assert_eq!(source.line, 7);
        assert!(deck.slides[0].notes.text.contains("secret"));
        assert!(!serde_json::to_string(&deck).unwrap().contains("secret"));
    }
    #[test]
    fn authoring_comments_do_not_hide_adjacent_directives_or_create_blocks() {
        let input = concat!(
            "---\ntitle: Tour\n---\n",
            "<!-- markdownlint-disable MD025 -->\n",
            "<!-- Slide decks intentionally have one top-level heading per slide. -->\n",
            "<!-- bmux:slide-id opening -->\n",
            "<!-- bmux:block-id title -->\n",
            "<!-- ordinary\nmultiline comment -->\n",
            "# Hello\n\n",
            "<!-- ignored --><!-- bmux:pause -->\n",
            "<!-- bmux:block-id later --><!-- ignored again -->\n",
            "Visible later\n",
        );
        let deck = parse_markdown("tour.md", input).unwrap();
        assert_eq!(deck.slides.len(), 1);
        let slide = &deck.slides[0];
        assert_eq!(slide.id.as_str(), "opening");
        assert_eq!(slide.blocks.len(), 2);
        assert_eq!(slide.blocks[0].id.as_str(), "title");
        assert_eq!(slide.blocks[1].id.as_str(), "later");
        assert_eq!(slide.blocks[1].reveal_step, 1);
        let source = slide.blocks[0].source.as_ref().unwrap();
        assert_eq!(source.line, 10);
        assert_eq!(&input[source.start..source.end], "# Hello\n");
        let public = serde_json::to_string(&deck).unwrap();
        assert!(!public.contains("markdownlint"));
        assert!(!public.contains("multiline comment"));
        assert!(!public.contains("ignored"));
    }

    #[test]
    fn comments_preserve_code_literals_and_do_not_activate_nested_directives() {
        let input = concat!(
            "<!-- hidden\n<!-- bmux:pause -->\n\n",
            "```html\n<!-- literal -->\n<!-- bmux:pause -->\n```\n\n",
            "    <!-- indented literal -->\n\n",
            "`<!-- inline code -->`\n\n",
            "> <!-- bmux:pause -->\n",
        );
        let deck = parse_markdown("x", input).unwrap();
        let slide = &deck.slides[0];
        assert_eq!(slide.max_reveal_step(), 0);
        assert!(
            matches!(&slide.blocks[0].kind, BlockKind::Code { source, .. }
            if source.contains("<!-- literal -->") && source.contains("<!-- bmux:pause -->"))
        );
        assert!(
            matches!(&slide.blocks[1].kind, BlockKind::Code { source, .. }
            if source.contains("<!-- indented literal -->"))
        );
        assert!(
            matches!(&slide.blocks[2].kind, BlockKind::Text { content, .. }
            if content.contains("`<!-- inline code -->`"))
        );
    }

    #[test]
    fn ordinary_comments_leave_notes_and_errors_intact() {
        let input = "<!-- ignored -->\n<!-- bmux:notes -->\nprivate\n<!-- ordinary -->\n<!-- bmux:end-notes -->\n# Hello\n";
        let deck = parse_markdown("x", input).unwrap();
        assert!(deck.slides[0].notes.text.contains("private"));
        assert_eq!(deck.slides[0].blocks.len(), 1);
        assert!(!serde_json::to_string(&deck).unwrap().contains("private"));
        assert!(parse_markdown("x", "<!-- ignored --><!-- bmux:run curl -->").is_err());
        assert!(parse_markdown("x", "<!-- bmux:block-id unused --><!-- ignored -->").is_err());
    }

    #[test]
    fn fences_and_nested_rules_do_not_split_slides_or_execute_directives() {
        let deck =
            parse_markdown("x", "```text\n---\n<!-- bmux:pause -->\n```\n\n> ---\n").unwrap();
        assert_eq!(deck.slides.len(), 1);
        assert_eq!(deck.slides[0].max_reveal_step(), 0);
        assert!(matches!(
            deck.slides[0].blocks[0].kind,
            BlockKind::Code { .. }
        ));
    }
    #[test]
    fn playbook_demo_authoring_is_inert_custom_data_with_a_stable_identity() {
        let source = concat!(
            "<!-- bmux:block-id live-demo -->\n",
            "<!-- bmux:component playbook-demo ",
            "{\"playbook\":\"demo.playbook.toml\",\"title\":\"Live demo\",\"on_leave\":\"stop\"} -->\n"
        );
        // No registry, runner, file, or trust input is available to the parser.
        let deck = parse_markdown("deck.md", source).unwrap();
        let block = &deck.slides[0].blocks[0];
        assert_eq!(block.id.as_str(), "live-demo");
        let BlockKind::Custom { name, config } = &block.kind else {
            panic!("custom component")
        };
        assert_eq!(name, "playbook-demo");
        assert_eq!(
            config,
            &serde_json::json!({
                "playbook": "demo.playbook.toml",
                "title": "Live demo",
                "on_leave": "stop"
            })
        );
        let registry = crate::custom::LinkedRegistry::default();
        assert!(registry.factory(name).is_none());
        assert!(registry.compatibility_key(block).is_none());
        let mut restored: Deck =
            serde_json::from_str(&serde_json::to_string(&deck).unwrap()).unwrap();
        let restored_block = restored.slides[0].blocks.remove(0);
        assert_eq!(restored_block.id, block.id);
        let BlockKind::Custom {
            name: restored_name,
            config: restored_config,
        } = restored_block.kind
        else {
            panic!("restored custom component")
        };
        assert_eq!(restored_name, *name);
        assert_eq!(restored_config, *config);
    }

    #[test]
    fn columns_preserve_source_order_and_weights() {
        let deck = parse_markdown("x", "<!-- bmux:columns 1,2 -->\n\nLeft\n\n<!-- bmux:column -->\n\nRight\n\n<!-- bmux:end-columns -->").unwrap();
        let BlockKind::Columns { columns } = &deck.slides[0].blocks[0].kind else {
            panic!("columns")
        };
        assert_eq!(columns[0].weight, 1);
        assert_eq!(columns[1].weight, 2);
        assert!(
            matches!(&columns[0].blocks[0].kind, BlockKind::Text { content, .. } if content.contains("Left"))
        );
    }
    #[test]
    fn stable_ids_survive_unrelated_insertions() {
        let before = parse_markdown("x", "# A\n\nalpha\n\n---\n\n# B\n").unwrap();
        let after = parse_markdown(
            "x",
            "# Extra\n\n---\n\n# A\n\ninserted\n\nalpha\n\n---\n\n# B\n",
        )
        .unwrap();
        assert_eq!(before.slides[0].id, after.slides[1].id);
        assert_eq!(before.slides[0].blocks[1].id, after.slides[1].blocks[2].id);
    }
    #[test]
    fn checked_in_markdown_example_parses_and_keeps_adjacent_directives_separate() {
        let deck = parse_markdown("demo.md", include_str!("../examples/deck.md")).unwrap();
        assert_eq!(deck.slides.len(), 4);
        assert_eq!(deck.slides[0].max_reveal_step(), 1);
        assert!(matches!(
            deck.slides[1].blocks[0].kind,
            BlockKind::Columns { .. }
        ));
        assert!(deck.slides[2].blocks.iter().any(|block| matches!(
            &block.kind,
            BlockKind::Image { destination, .. } if destination == "architecture.png"
        )));
        assert!(
            deck.slides[3]
                .blocks
                .iter()
                .any(|block| matches!(block.kind, BlockKind::Mermaid { .. }))
        );
    }
    #[test]
    fn malformed_directives_fail_closed() {
        for input in [
            "<!-- bmux:run curl -->",
            "<!-- bmux:notes -->",
            "<!-- bmux:columns 0,1 -->",
        ] {
            assert!(parse_markdown("x", input).is_err());
        }
    }
}
