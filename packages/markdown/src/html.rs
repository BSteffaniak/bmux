//! Raw HTML presentation for Markdown documents.
//!
//! Markdown allows raw HTML blocks and inline tags. This module turns the raw
//! HTML fragments in the parsed container tree into the same container shapes
//! the terminal renderer already draws for Markdown, following the element and
//! attribute subset GitHub's sanitizer keeps.
//!
//! Fragments are tokenized individually and stitched together with one open
//! element stack per container level, so an element opened in one HTML block
//! can enclose the Markdown blocks that follow it (for example a `<details>`
//! body written in Markdown after a blank line). Unclosed elements close at the
//! end of their level and unmatched end tags are ignored.

use std::{borrow::Cow, collections::BTreeMap, ops::Range};

use hyperchad_transformer::{Container, Element};
use pulldown_cmark::{Event, Parser};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    DisplayInsertion, HTML_ALIGN, HTML_CONTRIBUTION, HTML_DETAILS, HTML_DETAILS_STATE,
    HTML_ELEMENT, HTML_HEADING, HTML_IMAGE, HTML_IMAGE_LABEL, HTML_INDENT, HTML_KBD, HTML_LINK,
    HTML_MARK, HTML_PLAIN, HTML_PRE, HTML_STRIKE, HTML_TABLE_ALIGN, HTML_UNDERLINE,
    HtmlRenderOutput, MarkdownContributionKind, MarkdownDocument, MarkdownDocumentContext,
    MarkdownImage, MarkdownLink, MarkdownLinkKind, MarkdownSemanticEventKind, contribution,
    readable_destination_fallback, resolve_markdown_destination, unicode_script,
};

/// Marks text that came from HTML source.
const HTML_TEXT: &str = "bmux-html-text";
/// Marks a line break from `<br>`.
const HTML_BREAK: &str = "bmux-html-break";
/// Marks the summary of a `<details>` element.
const HTML_SUMMARY: &str = "bmux-html-summary";

/// How raw HTML embedded in Markdown is presented.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum MarkdownHtmlMode {
    /// Render the element subset GitHub keeps after sanitizing: inline
    /// formatting, links, images, headings, lists, tables, block quotes,
    /// preformatted text, alignment, and interactive `<details>` disclosure.
    /// Comments and `<script>`/`<style>`-like elements are removed with their
    /// contents; other tags are removed and their text is kept.
    #[default]
    Render,
    /// Remove every tag and keep the text. Block elements still start new
    /// rows, `<br>` still breaks, and comments and `<script>`/`<style>`-like
    /// elements are removed with their contents.
    Strip,
    /// Show HTML source as literal text.
    Raw,
}

/// Elements removed together with their contents.
const DROPPED_WITH_CONTENT: &[&str] = &[
    "applet",
    "frame",
    "frameset",
    "head",
    "iframe",
    "noembed",
    "noframes",
    "noscript",
    "object",
    "plaintext",
    "script",
    "select",
    "style",
    "svg",
    "template",
    "textarea",
    "title",
    "xmp",
];

/// Void elements outside the allowlist, removed without affecting content.
const DROPPED_VOID: &[&str] = &[
    "area", "base", "basefont", "col", "embed", "input", "keygen", "link", "meta", "param", "track",
];

/// Elements whose contents the tokenizer treats as raw text.
const RAW_TEXT: &[&str] = &[
    "iframe",
    "noembed",
    "noframes",
    "noscript",
    "plaintext",
    "script",
    "style",
    "textarea",
    "title",
    "xmp",
];

/// Allowed elements that hold children.
const CONTAINERS: &[&str] = &[
    "a",
    "abbr",
    "b",
    "blockquote",
    "caption",
    "cite",
    "code",
    "dd",
    "del",
    "details",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "i",
    "ins",
    "kbd",
    "li",
    "mark",
    "ol",
    "p",
    "picture",
    "pre",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
];

/// Elements that start a block and therefore close an open paragraph.
const BLOCKS: &[&str] = &[
    "blockquote",
    "details",
    "div",
    "dl",
    "figure",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "ol",
    "p",
    "pre",
    "table",
    "ul",
];

/// One lexical HTML token with byte ranges local to its fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Text(Range<usize>),
    Start {
        name: String,
        attributes: Vec<(String, String)>,
        self_closing: bool,
        range: Range<usize>,
    },
    End {
        name: String,
        range: Range<usize>,
    },
    /// Comments, doctypes, processing instructions, and raw-text contents.
    Ignored,
}

/// Tokens of one HTML fragment.
#[derive(Debug, Default)]
struct Tokens {
    tokens: Vec<Token>,
    /// Offset of markup that the fragment ends before completing, such as a
    /// start tag whose attributes continue on the next source line.
    incomplete: Option<usize>,
}

/// Outcome of reading markup at a `<`.
enum Markup {
    Token(Token, usize),
    /// The `<` does not begin markup and is text.
    Text,
    /// The markup is cut off by the end of the input.
    Incomplete,
}

/// Split one HTML fragment into tokens.
///
/// A `<` that does not begin markup stays text. Raw-text element contents
/// (such as `<script>` bodies) are never tokenized as markup. Tokenizing stops
/// at markup the fragment does not complete.
fn tokenize(input: &str) -> Tokens {
    let mut output = Tokens::default();
    let mut text_start = 0;
    let mut index = 0;
    while let Some(offset) = input.get(index..).and_then(|rest| rest.find('<')) {
        let position = index + offset;
        let (token, end) = match markup_at(input, position) {
            Markup::Token(token, end) => (token, end),
            Markup::Text => {
                index = position + 1;
                continue;
            }
            Markup::Incomplete => {
                output.incomplete = Some(position);
                break;
            }
        };
        if text_start < position {
            output.tokens.push(Token::Text(text_start..position));
        }
        let raw_text = match &token {
            Token::Start {
                name,
                self_closing: false,
                ..
            } if RAW_TEXT.contains(&name.as_str()) => Some(name.clone()),
            _ => None,
        };
        output.tokens.push(token);
        index = end;
        if let Some(name) = raw_text {
            output.tokens.push(Token::Ignored);
            if let Some((close_start, close_end)) = find_end_tag(input, index, &name) {
                output.tokens.push(Token::End {
                    name,
                    range: close_start..close_end,
                });
                index = close_end;
            } else {
                index = input.len();
            }
        }
        text_start = index;
    }
    let text_end = output.incomplete.unwrap_or(input.len());
    if text_start < text_end {
        output.tokens.push(Token::Text(text_start..text_end));
    }
    output
}

/// Read markup beginning at the `<` at `start`.
fn markup_at(input: &str, start: usize) -> Markup {
    let Some(rest) = input.get(start..) else {
        return Markup::Text;
    };
    if rest.starts_with("<!--") {
        return rest
            .get(4..)
            .and_then(|body| body.find("-->"))
            .map_or(Markup::Incomplete, |offset| {
                Markup::Token(Token::Ignored, start + 4 + offset + 3)
            });
    }
    if rest.starts_with("<!") || rest.starts_with("<?") {
        return rest.find('>').map_or(Markup::Incomplete, |end| {
            Markup::Token(Token::Ignored, start + end + 1)
        });
    }
    if let Some(after) = rest.strip_prefix("</") {
        let Some(name) = tag_name(after) else {
            return Markup::Text;
        };
        return after.find('>').map_or(Markup::Incomplete, |end| {
            Markup::Token(
                Token::End {
                    name,
                    range: start..start + 2 + end + 1,
                },
                start + 2 + end + 1,
            )
        });
    }
    let Some(after) = rest.strip_prefix('<') else {
        return Markup::Text;
    };
    let Some(name) = tag_name(after) else {
        return Markup::Text;
    };
    let Some((attributes, self_closing, consumed)) =
        after.get(name.len()..).and_then(parse_attributes)
    else {
        return Markup::Incomplete;
    };
    let end = start + 1 + name.len() + consumed;
    Markup::Token(
        Token::Start {
            name,
            attributes,
            self_closing,
            range: start..end,
        },
        end,
    )
}

/// Read an ASCII tag name, lowercased.
fn tag_name(input: &str) -> Option<String> {
    let mut characters = input.char_indices();
    let (_, first) = characters.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let end = characters
        .find(|(_, character)| !(character.is_ascii_alphanumeric() || *character == '-'))
        .map_or(input.len(), |(index, _)| index);
    Some(input[..end].to_ascii_lowercase())
}

type ParsedAttributes = (Vec<(String, String)>, bool, usize);

/// Parse attributes up to and including the closing `>`.
///
/// Returns the attributes, whether the tag is self-closing, and the consumed
/// byte count. A missing `>` or unterminated quoted value fails.
fn parse_attributes(input: &str) -> Option<ParsedAttributes> {
    let bytes = input.as_bytes();
    let mut attributes = Vec::new();
    let mut index = 0;
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        match bytes.get(index)? {
            b'>' => return Some((attributes, false, index + 1)),
            b'/' if bytes.get(index + 1) == Some(&b'>') => {
                return Some((attributes, true, index + 2));
            }
            b'/' => {
                index += 1;
                continue;
            }
            _ => {}
        }
        let name_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && !matches!(bytes[index], b'=' | b'>' | b'/')
        {
            index += 1;
        }
        let name = input[name_start..index].to_ascii_lowercase();
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let mut value = String::new();
        if bytes.get(index) == Some(&b'=') {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            let quote = *bytes.get(index)?;
            if matches!(quote, b'"' | b'\'') {
                let close = input.get(index + 1..)?.find(char::from(quote))?;
                value = decode_entities(&input[index + 1..index + 1 + close]).into_owned();
                index += close + 2;
            } else {
                let value_start = index;
                while index < bytes.len()
                    && !bytes[index].is_ascii_whitespace()
                    && bytes[index] != b'>'
                {
                    index += 1;
                }
                value = decode_entities(&input[value_start..index]).into_owned();
            }
        }
        if !name.is_empty() {
            attributes.push((name, value));
        }
    }
}

/// Find `</name ...>` at or after `from`, case-insensitively.
fn find_end_tag(input: &str, from: usize, name: &str) -> Option<(usize, usize)> {
    let lower = input.get(from..)?.to_ascii_lowercase();
    let needle = format!("</{name}");
    let mut search = 0;
    while let Some(offset) = lower.get(search..)?.find(&needle) {
        let start = search + offset;
        let after = start + needle.len();
        let boundary = lower.as_bytes().get(after).copied();
        if boundary.is_none_or(|byte| byte == b'>' || byte == b'/' || byte.is_ascii_whitespace()) {
            let end = lower
                .get(after..)?
                .find('>')
                .map(|close| after + close + 1)?;
            return Some((from + start, from + end));
        }
        search = after;
    }
    None
}

/// Decode HTML character references.
///
/// Named and numeric references are resolved with the Markdown parser's
/// complete HTML5 entity table; unknown references stay literal.
pub fn decode_entities(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    let mut changed = false;
    while let Some(ampersand) = rest.find('&') {
        output.push_str(&rest[..ampersand]);
        let candidate = &rest[ampersand..];
        let decoded = candidate
            .get(1..)
            .and_then(|body| body.find(';'))
            .filter(|end| (1..=32).contains(end))
            .and_then(|end| {
                let entity = &candidate[..end + 2];
                decode_entity(entity).map(|value| (value, entity.len()))
            });
        if let Some((value, length)) = decoded {
            output.push_str(&value);
            rest = &candidate[length..];
            changed = true;
        } else {
            output.push('&');
            rest = &candidate[1..];
        }
    }
    output.push_str(rest);
    if changed {
        Cow::Owned(output)
    } else {
        Cow::Borrowed(text)
    }
}

fn decode_entity(entity: &str) -> Option<String> {
    let body = entity.get(1..entity.len().checked_sub(1)?)?;
    if body.is_empty()
        || !body
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '#')
    {
        return None;
    }
    let mut decoded = String::new();
    for event in Parser::new(entity) {
        match event {
            Event::Text(text) => decoded.push_str(&text),
            Event::Start(_) | Event::End(_) => {}
            _ => return None,
        }
    }
    (!decoded.is_empty() && decoded != entity).then_some(decoded)
}

/// Collapse runs of HTML whitespace into one space.
fn collapse_whitespace(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut previous_whitespace = false;
    for character in text.chars() {
        if matches!(character, ' ' | '\t' | '\n' | '\r' | '\x0c') {
            if !previous_whitespace {
                output.push(' ');
            }
            previous_whitespace = true;
        } else {
            output.push(character);
            previous_whitespace = false;
        }
    }
    output
}

/// Append the text an HTML fragment shows to `output`, without markup,
/// comments, or raw-text element contents.
pub fn collect_plain_text(html: &str, output: &mut String) {
    let mut dropping: Option<String> = None;
    let tokens = tokenize(html);
    for token in tokens.tokens {
        match token {
            Token::Text(range) if dropping.is_none() => {
                output.push_str(&decode_entities(&html[range]));
            }
            Token::Start {
                name, self_closing, ..
            } => {
                if dropping.is_none() {
                    if DROPPED_WITH_CONTENT.contains(&name.as_str()) && !self_closing {
                        dropping = Some(name);
                    } else {
                        output.push(' ');
                    }
                }
            }
            Token::End { name, .. } => {
                if dropping.as_deref() == Some(name.as_str()) {
                    dropping = None;
                }
                output.push(' ');
            }
            Token::Text(_) | Token::Ignored => {}
        }
    }
    if let Some(rest) = tokens.incomplete.and_then(|start| html.get(start..))
        && dropping.is_none()
        && !rest.starts_with("<!--")
    {
        output.push_str(&decode_entities(rest));
    }
}

/// Inputs for one HTML presentation pass.
pub struct HtmlRenderInput<'a> {
    pub mode: MarkdownHtmlMode,
    /// Original Markdown source.
    pub source: &'a str,
    /// Raw HTML events of the original document, in source order.
    pub events: Vec<HtmlEvent>,
    pub context: Option<&'a MarkdownDocumentContext>,
    pub document_id: Option<&'a str>,
    pub details_interactive: bool,
    pub details_open: &'a BTreeMap<String, bool>,
    pub link_fallbacks: bool,
}

/// One raw HTML event and where its text lies in the original source.
#[derive(Debug)]
pub struct HtmlEvent {
    /// Event text; container prefixes such as `> ` are already removed.
    text: String,
    /// Source segments: local start in `text` and original source offset.
    segments: Vec<(usize, Option<usize>)>,
}

impl HtmlRenderInput<'_> {
    /// Collect the raw HTML events of `document` parsed from `source`.
    pub fn events(source: &str, document: &MarkdownDocument) -> Vec<HtmlEvent> {
        document
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                MarkdownSemanticEventKind::Html(text) => Some(HtmlEvent {
                    segments: event_segments(source, text, event.source_range.clone()),
                    text: text.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// Align an event's text with its source range line by line. The parser
/// removes block-quote and list prefixes from continuation lines, so a
/// multi-line event is not always one contiguous source slice.
fn event_segments(source: &str, text: &str, range: Range<usize>) -> Vec<(usize, Option<usize>)> {
    let Some(slice) = source.get(range.clone()) else {
        return vec![(0, None)];
    };
    if slice == text {
        return vec![(0, Some(range.start))];
    }
    let mut segments = Vec::new();
    let mut local = 0;
    let mut cursor = 0;
    for line in text.split_inclusive('\n') {
        let found = slice.get(cursor..).and_then(|rest| rest.find(line));
        segments.push((local, found.map(|offset| range.start + cursor + offset)));
        if let Some(offset) = found {
            cursor += offset + line.len();
        }
        local += line.len();
    }
    if segments.is_empty() {
        segments.push((0, None));
    }
    segments
}

/// Replace raw HTML in `container` with renderable containers.
pub fn render_html(
    container: Container,
    input: &HtmlRenderInput<'_>,
) -> (Container, HtmlRenderOutput) {
    let mut transformer = Transformer {
        input,
        cursor: 0,
        output: HtmlRenderOutput::default(),
    };
    let container = transformer.transform_container(container);
    let mut output = transformer.output;
    output
        .provenance
        .visible_text
        .sort_by_key(|range| range.start);
    output.provenance.unmapped.sort_by_key(|range| range.start);
    output
        .provenance
        .insertions
        .sort_by_key(|insertion| insertion.after);
    output
        .contributions
        .sort_by_key(|item| (item.source_range.start, item.source_range.end));
    (container, output)
}

/// An open element and the content collected for it so far.
#[derive(Debug, Default)]
struct Frame {
    name: String,
    attributes: Vec<(String, String)>,
    /// Original source offset of the start tag's `<`.
    start: Option<usize>,
    /// Original source offset just past the start tag.
    open_end: Option<usize>,
    children: Vec<Container>,
    visible: Vec<Range<usize>>,
    insertions: Vec<DisplayInsertion>,
    /// `<source>` candidates of a `<picture>`: `srcset` and `media`.
    sources: Vec<(String, Option<String>)>,
    /// Original source offset just past a `<details>` element's `</summary>`.
    summary_end: Option<usize>,
}

impl Frame {
    fn attribute(&self, name: &str) -> Option<&str> {
        attribute(&self.attributes, name)
    }
}

fn attribute<'a>(attributes: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Open elements of one container level.
#[derive(Debug, Default)]
struct Level {
    root: Frame,
    stack: Vec<Frame>,
    /// Name of a removed element whose contents are being skipped.
    dropping: Option<String>,
    /// Markup cut off at the end of the previous fragment, completed by the
    /// next fragment (HTML block events arrive one source line at a time).
    carry: String,
    /// Source segments of `carry`: local start and original source offset.
    carry_segments: Vec<(usize, Option<usize>)>,
}

impl Level {
    fn top(&mut self) -> &mut Frame {
        self.stack.last_mut().unwrap_or(&mut self.root)
    }

    fn top_name(&self) -> Option<&str> {
        self.stack.last().map(|frame| frame.name.as_str())
    }

    fn in_element(&self, name: &str) -> bool {
        self.stack.iter().any(|frame| frame.name == name)
    }
}

struct Transformer<'a, 'input> {
    input: &'a HtmlRenderInput<'input>,
    cursor: usize,
    output: HtmlRenderOutput,
}

/// Look-ahead bound when matching a raw fragment to its source event.
const SOURCE_LOOKAHEAD: usize = 64;

impl Transformer<'_, '_> {
    fn transform_container(&mut self, mut container: Container) -> Container {
        let block = is_block(&container);
        let children = std::mem::take(&mut container.children);
        container.children = self.transform_children(children, block);
        container
    }

    fn transform_children(&mut self, children: Vec<Container>, block: bool) -> Vec<Container> {
        if !children.iter().any(contains_raw) {
            return children;
        }
        let mut level = Level::default();
        for child in children {
            if let Element::Raw { value } = &child.element {
                let (html, segments) = self.locate(value);
                self.process_fragment(&mut level, &html, segments);
                continue;
            }
            Self::flush_carry(&mut level);
            if level.dropping.is_none() {
                let child = self.transform_container(child);
                level.top().children.push(child);
            }
        }
        Self::flush_carry(&mut level);
        while !level.stack.is_empty() {
            self.close_top(&mut level, None);
        }
        let mut root = level.root;
        if block {
            normalize_block_children(&mut root.children);
        }
        self.output
            .provenance
            .visible_text
            .append(&mut root.visible);
        self.output
            .provenance
            .insertions
            .append(&mut root.insertions);
        root.children
    }

    /// Match a raw container value to the next original HTML event.
    ///
    /// The parser escapes fragments it considers dangerous; the original source
    /// is recovered so those fragments are sanitized here instead.
    fn locate(&mut self, value: &str) -> (String, Vec<(usize, Option<usize>)>) {
        let events = &self.input.events;
        let end = events
            .len()
            .min(self.cursor.saturating_add(SOURCE_LOOKAHEAD));
        let found = events[self.cursor..end]
            .iter()
            .position(|event| event.text == value || parser_escape(&event.text) == value);
        let Some(offset) = found else {
            return (value.to_owned(), vec![(0, None)]);
        };
        let event = &events[self.cursor + offset];
        self.cursor += offset + 1;
        (event.text.clone(), event.segments.clone())
    }

    /// Show cut-off markup that no later fragment completed as text.
    fn flush_carry(level: &mut Level) {
        if level.carry.is_empty() {
            return;
        }
        let carry = std::mem::take(&mut level.carry);
        level.carry_segments.clear();
        if level.dropping.is_none() && !carry.starts_with("<!--") {
            Self::push_text(level, &carry, None);
        }
    }

    /// Bound on cut-off markup carried into following fragments.
    const MAX_CARRY: usize = 64 * 1024;

    fn process_fragment(
        &mut self,
        level: &mut Level,
        fragment: &str,
        fragment_segments: Vec<(usize, Option<usize>)>,
    ) {
        let mut html = std::mem::take(&mut level.carry);
        let mut segments = std::mem::take(&mut level.carry_segments);
        let offset = html.len();
        segments.extend(
            fragment_segments
                .into_iter()
                .map(|(local, base)| (offset + local, base)),
        );
        html.push_str(fragment);
        // Original source offset of a local offset, attributed to the segment
        // holding the byte at `local` (or, for an end offset, the byte before).
        let original = |local: usize, end: bool| {
            let probe = if end { local.saturating_sub(1) } else { local };
            let index = segments.partition_point(|(start, _)| *start <= probe);
            let (segment_start, segment_base) = segments[index.saturating_sub(1)];
            segment_base.map(|base| base + local - segment_start)
        };
        // Markup spans may cross fragments whose source is not contiguous; each
        // end is mapped independently.
        let span =
            |range: &Range<usize>| Some(original(range.start, false)?..original(range.end, true)?);
        // Text claims verbatim source bytes, so it must lie within one fragment.
        let contiguous =
            |range: &Range<usize>| span(range).filter(|mapped| mapped.len() == range.len());
        let tokens = tokenize(&html);
        let mut overflow = None;
        if let Some(start) = tokens.incomplete
            && html.len() - start <= Self::MAX_CARRY
        {
            let index = segments.partition_point(|(segment, _)| *segment <= start);
            let (segment_start, segment_base) = segments[index.saturating_sub(1)];
            html[start..].clone_into(&mut level.carry);
            level.carry_segments =
                std::iter::once((0, segment_base.map(|base| base + start - segment_start)))
                    .chain(
                        segments[index..]
                            .iter()
                            .map(|(segment, base)| (segment - start, *base)),
                    )
                    .collect();
        } else if let Some(start) = tokens.incomplete
            && !html[start..].starts_with("<!--")
        {
            overflow = Some(start..html.len());
        }
        let html = html.as_str();
        for token in tokens.tokens {
            if let Some(dropping) = &level.dropping {
                if matches!(&token, Token::End { name, .. } if name == dropping) {
                    level.dropping = None;
                }
                continue;
            }
            match token {
                Token::Text(range) => {
                    Self::push_text(level, &html[range.clone()], contiguous(&range));
                }
                Token::Ignored => {}
                Token::Start {
                    name,
                    attributes,
                    self_closing,
                    range,
                } => self.start_tag(level, name, attributes, self_closing, span(&range)),
                Token::End { name, range } => self.end_tag(level, &name, span(&range)),
            }
        }
        if let Some(range) = overflow
            && level.dropping.is_none()
        {
            Self::push_text(level, &html[range.clone()], contiguous(&range));
        }
    }

    fn push_text(level: &mut Level, raw: &str, source: Option<Range<usize>>) {
        let decoded = decode_entities(raw);
        let verbatim = matches!(decoded, Cow::Borrowed(_));
        let preformatted = level.in_element("pre");
        let mut text = if preformatted {
            decoded.into_owned()
        } else {
            collapse_whitespace(&decoded)
        };
        let top = level.top();
        if preformatted && top.name == "pre" && top.children.is_empty() {
            // A newline directly after `<pre>` is not content.
            if text.starts_with('\n') {
                text.remove(0);
            }
        }
        if text.is_empty() {
            return;
        }
        if verbatim && let Some(source) = source {
            top.visible.push(source);
        }
        top.children.push(html_text(text));
    }

    fn start_tag(
        &mut self,
        level: &mut Level,
        name: String,
        attributes: Vec<(String, String)>,
        self_closing: bool,
        range: Option<Range<usize>>,
    ) {
        if DROPPED_WITH_CONTENT.contains(&name.as_str()) {
            if !self_closing {
                level.dropping = Some(name);
            }
            return;
        }
        if DROPPED_VOID.contains(&name.as_str()) {
            return;
        }
        match name.as_str() {
            "br" => {
                level.top().children.push(line_break());
                return;
            }
            "wbr" => return,
            "hr" => {
                self.close_paragraph(level);
                let rule = if self.input.mode == MarkdownHtmlMode::Render {
                    Container {
                        element: Element::Div,
                        classes: vec!["markdown-hr".to_owned()],
                        ..Container::default()
                    }
                } else {
                    block_container(Vec::new())
                };
                level.top().children.push(rule);
                return;
            }
            "img" => {
                let image = self.image(level, &attributes, range);
                level.top().children.extend(image);
                return;
            }
            "source" => {
                let top = level.top();
                if top.name == "picture"
                    && let Some(srcset) = attribute(&attributes, "srcset")
                {
                    let media = attribute(&attributes, "media").map(str::to_owned);
                    top.sources.push((srcset.to_owned(), media));
                }
                return;
            }
            _ => {}
        }
        if !CONTAINERS.contains(&name.as_str()) {
            return;
        }
        self.implicitly_close(level, &name);
        level.stack.push(Frame {
            open_end: range.as_ref().map(|range| range.end),
            start: range.map(|range| range.start),
            name,
            attributes,
            ..Frame::default()
        });
        if self_closing {
            let end = level.stack.last().and_then(|frame| frame.open_end);
            self.close_top(level, end);
        }
    }

    fn close_paragraph(&mut self, level: &mut Level) {
        if level.top_name() == Some("p") {
            self.close_top(level, None);
        }
    }

    /// Close elements that an incoming start tag implicitly ends.
    fn implicitly_close(&mut self, level: &mut Level, name: &str) {
        loop {
            let Some(top) = level.top_name() else {
                return;
            };
            let closes = match name {
                "li" => top == "li",
                "dt" | "dd" => matches!(top, "dt" | "dd"),
                "tr" => matches!(top, "tr" | "td" | "th"),
                "td" | "th" => matches!(top, "td" | "th"),
                "thead" | "tbody" | "tfoot" => {
                    matches!(top, "thead" | "tbody" | "tfoot" | "tr" | "td" | "th")
                }
                _ => BLOCKS.contains(&name) && top == "p",
            };
            if !closes {
                return;
            }
            self.close_top(level, None);
        }
    }

    fn end_tag(&mut self, level: &mut Level, name: &str, range: Option<Range<usize>>) {
        if name == "br" {
            level.top().children.push(line_break());
            return;
        }
        let Some(index) = level.stack.iter().rposition(|frame| frame.name == name) else {
            return;
        };
        while level.stack.len() > index + 1 {
            self.close_top(level, None);
        }
        self.close_top(level, range.map(|range| range.end));
    }

    /// Close the innermost open element; `end` is the original source offset
    /// just past its end tag, absent when it closes implicitly.
    fn close_top(&mut self, level: &mut Level, end: Option<usize>) {
        let Some(mut frame) = level.stack.pop() else {
            return;
        };
        if is_block_name(&frame.name) && frame.name != "pre" {
            normalize_block_children(&mut frame.children);
        }
        let built = if self.input.mode == MarkdownHtmlMode::Render {
            self.build_rendered(level, &mut frame, end)
        } else {
            build_stripped(&mut frame)
        };
        let parent = level.top();
        if frame.name == "summary" && parent.name == "details" {
            parent.summary_end = end.or(frame.open_end);
        }
        parent.visible.append(&mut frame.visible);
        parent.insertions.append(&mut frame.insertions);
        parent.children.extend(built);
    }

    /// Record a contribution and return its owner-qualified identity.
    fn qualified_id(
        &mut self,
        prefix: &str,
        range: Range<usize>,
        kind: MarkdownContributionKind,
    ) -> String {
        let mut item = contribution(prefix, range, kind);
        if let Some(document_id) = self.input.document_id {
            item.id = format!("{document_id}:{}", item.id);
        }
        let id = item.id.clone();
        self.output.contributions.push(item);
        id
    }

    fn build_rendered(
        &mut self,
        level: &Level,
        frame: &mut Frame,
        end: Option<usize>,
    ) -> Vec<Container> {
        let children = std::mem::take(&mut frame.children);
        match frame.name.as_str() {
            "sub" | "sup" => self.script(frame, children, end),
            "a" => self.anchor(frame, children, end),
            "details" => self.details(frame, children, end),
            "picture" => self.picture(level, frame, children, end),
            _ => build_element(frame, children),
        }
    }

    fn script(
        &mut self,
        frame: &mut Frame,
        children: Vec<Container>,
        end: Option<usize>,
    ) -> Vec<Container> {
        let superscript = frame.name == "sup";
        let text = plain_text_if_flat(&children);
        if let Some(text) = text.as_deref().filter(|text| !text.is_empty())
            && let Some(converted) = unicode_script(text, superscript)
        {
            // The displayed characters no longer match their source bytes.
            frame.visible.clear();
            if let (Some(start), Some(end)) = (frame.start, end.or(frame.open_end)) {
                self.output.provenance.unmapped.push(start..end);
            }
            return vec![html_text(converted)];
        }
        let marker = if superscript { "^" } else { "_" };
        let single = text
            .as_deref()
            .is_some_and(|text| text.graphemes(true).count() == 1);
        let mut output = Vec::with_capacity(children.len() + 2);
        output.push(html_text(if single {
            marker.to_owned()
        } else {
            format!("{marker}(")
        }));
        output.extend(children);
        if !single {
            output.push(html_text(")".to_owned()));
        }
        vec![span(&[], output)]
    }

    fn anchor(
        &mut self,
        frame: &mut Frame,
        children: Vec<Container>,
        end: Option<usize>,
    ) -> Vec<Container> {
        let Some(href) = frame
            .attribute("href")
            .map(str::trim)
            .filter(|href| !href.is_empty())
        else {
            return vec![span(&[], children)];
        };
        let href = href.to_owned();
        let destination = resolve_markdown_destination(&href, self.input.context);
        let label = collapse_whitespace(&plain_text(&children))
            .trim()
            .to_owned();
        let link = MarkdownLink {
            destination: href,
            title: frame.attribute("title").map(str::to_owned),
            reference_id: None,
            kind: MarkdownLinkKind::Html,
        };
        let fallback = self
            .input
            .link_fallbacks
            .then(|| readable_destination_fallback(&link, &label, &destination))
            .flatten();
        let range = frame
            .start
            .zip(end.or(frame.open_end))
            .map(|(start, end)| start..end);
        let mut data = BTreeMap::new();
        if let Some(range) = range.clone() {
            let id = self.qualified_id(
                "link",
                range,
                MarkdownContributionKind::Link {
                    link,
                    label,
                    destination,
                },
            );
            data.insert(HTML_CONTRIBUTION.to_owned(), id);
        }
        let mut output = vec![Container {
            element: Element::Anchor {
                target: None,
                href: None,
            },
            classes: vec!["markdown-link".to_owned(), HTML_LINK.to_owned()],
            data,
            children,
            ..Container::default()
        }];
        if let Some(fallback) = fallback {
            output.push(html_text(format!(" ({fallback})")));
            if let Some(range) = range {
                frame.insertions.push(DisplayInsertion {
                    after: range.end,
                    text: format!("({fallback})"),
                });
            }
        }
        output
    }

    fn image(
        &mut self,
        level: &Level,
        attributes: &[(String, String)],
        range: Option<Range<usize>>,
    ) -> Vec<Container> {
        let alt =
            attribute(attributes, "alt").map(|alt| collapse_whitespace(alt).trim().to_owned());
        let source = attribute(attributes, "src")
            .map(str::trim)
            .filter(|source| !source.is_empty());
        if self.input.mode != MarkdownHtmlMode::Render {
            return alt
                .filter(|alt| !alt.is_empty())
                .map(html_text)
                .into_iter()
                .collect();
        }
        let label = alt
            .clone()
            .filter(|alt| !alt.is_empty())
            .or_else(|| source.map(str::to_owned))
            .unwrap_or_else(|| "image".to_owned());
        let mut data = BTreeMap::from([(HTML_IMAGE_LABEL.to_owned(), label)]);
        if let (Some(source), Some(range)) = (source, range) {
            let linked_destination = level.stack.iter().rev().find_map(|frame| {
                (frame.name == "a")
                    .then(|| frame.attribute("href"))
                    .flatten()
                    .map(|href| resolve_markdown_destination(href.trim(), self.input.context))
            });
            let pixels = |name: &str| {
                attribute(attributes, name).and_then(|value| value.trim().parse().ok())
            };
            let id = self.qualified_id(
                "image",
                range,
                MarkdownContributionKind::Image {
                    source: resolve_markdown_destination(source, self.input.context),
                    image: MarkdownImage {
                        source: source.to_owned(),
                        title: attribute(attributes, "title").map(str::to_owned),
                        reference_id: None,
                        kind: MarkdownLinkKind::Html,
                        width: pixels("width"),
                        height: pixels("height"),
                    },
                    alt: alt.unwrap_or_default(),
                    linked_destination,
                },
            );
            data.insert(HTML_CONTRIBUTION.to_owned(), id);
        }
        vec![Container {
            element: Element::Span,
            classes: vec![HTML_IMAGE.to_owned()],
            data,
            ..Container::default()
        }]
    }

    fn picture(
        &mut self,
        level: &Level,
        frame: &Frame,
        children: Vec<Container>,
        end: Option<usize>,
    ) -> Vec<Container> {
        let images = children
            .into_iter()
            .filter(|child| !is_whitespace_text(child))
            .collect::<Vec<_>>();
        if images.iter().any(contains_image) {
            return images;
        }
        // Without a fallback `<img>`, use a source without a media condition,
        // then the first source.
        let Some((srcset, _)) = frame
            .sources
            .iter()
            .find(|(_, media)| media.is_none())
            .or_else(|| frame.sources.first())
        else {
            return images;
        };
        let Some(source) = srcset
            .split(',')
            .next()
            .and_then(|candidate| candidate.split_whitespace().next())
        else {
            return images;
        };
        let attributes = [("src".to_owned(), source.to_owned())];
        let range = frame
            .start
            .zip(end.or(frame.open_end))
            .map(|(start, end)| start..end);
        let mut output = images;
        output.extend(self.image(level, &attributes, range));
        output
    }

    fn details(
        &mut self,
        frame: &Frame,
        children: Vec<Container>,
        end: Option<usize>,
    ) -> Vec<Container> {
        let mut summary = None;
        let mut body = Vec::new();
        for child in children {
            if summary.is_none() && child.classes.iter().any(|class| class == HTML_SUMMARY) {
                summary = Some(child);
            } else {
                body.push(child);
            }
        }
        normalize_block_children(&mut body);
        let summary = summary.unwrap_or_else(|| Container {
            element: Element::Div,
            classes: vec![HTML_SUMMARY.to_owned()],
            children: vec![html_text("Details".to_owned())],
            ..Container::default()
        });
        let mut data = BTreeMap::new();
        let mut state = "static";
        if let (Some(start), Some(end)) = (frame.start, end) {
            let source_range = start..end;
            let body_start = frame.summary_end.or(frame.open_end).unwrap_or(start);
            let body_end = end.saturating_sub("</details>".len()).max(body_start);
            let default_open = frame.attribute("open").is_some();
            let id = self.qualified_id(
                "details",
                source_range,
                MarkdownContributionKind::Details {
                    summary: collapse_whitespace(&plain_text(&summary.children))
                        .trim()
                        .to_owned(),
                    body: self
                        .input
                        .source
                        .get(body_start..body_end)
                        .unwrap_or_default()
                        .to_owned(),
                    default_open,
                },
            );
            if self.input.details_interactive {
                let open = self
                    .input
                    .details_open
                    .get(&id)
                    .copied()
                    .unwrap_or(default_open);
                state = if open { "open" } else { "closed" };
                if !open {
                    self.output.provenance.unmapped.push(body_start..body_end);
                    body.clear();
                }
            }
            data.insert(HTML_CONTRIBUTION.to_owned(), id);
        }
        data.insert(HTML_DETAILS_STATE.to_owned(), state.to_owned());
        let mut children = vec![summary];
        children.extend(body);
        vec![Container {
            element: Element::Div,
            classes: vec![HTML_DETAILS.to_owned()],
            data,
            children,
            ..Container::default()
        }]
    }
}

/// Render-mode presentation of an element without contributions.
fn build_element(frame: &Frame, children: Vec<Container>) -> Vec<Container> {
    match frame.name.as_str() {
        "b" | "strong" => vec![span(&["markdown-strong"], children)],
        "i" | "em" | "cite" | "dfn" | "var" => vec![span(&["markdown-em"], children)],
        "code" | "tt" | "samp" => vec![span(&["inline-code"], children)],
        "kbd" => vec![span(&[HTML_KBD], children)],
        "mark" => vec![span(&[HTML_MARK], children)],
        "s" | "strike" | "del" => vec![span(&[HTML_STRIKE], children)],
        "ins" | "u" => vec![span(&[HTML_UNDERLINE], children)],
        "q" => {
            let mut quoted = vec![html_text("\u{201c}".to_owned())];
            quoted.extend(children);
            quoted.push(html_text("\u{201d}".to_owned()));
            vec![span(&[], quoted)]
        }
        "p" => vec![aligned(
            frame,
            Container {
                element: Element::Div,
                classes: vec!["markdown-p".to_owned()],
                children,
                ..Container::default()
            },
        )],
        "div" | "figure" | "figcaption" | "dl" => {
            vec![aligned(frame, block_container(children))]
        }
        "dt" => vec![Container {
            element: Element::Div,
            classes: vec!["markdown-strong".to_owned()],
            children,
            ..Container::default()
        }],
        "dd" => vec![Container {
            element: Element::Div,
            classes: vec![HTML_INDENT.to_owned()],
            children,
            ..Container::default()
        }],
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => vec![aligned(
            frame,
            Container {
                element: Element::Div,
                classes: vec![HTML_HEADING.to_owned()],
                children,
                ..Container::default()
            },
        )],
        "ul" => vec![list(Element::UnorderedList, children)],
        "ol" => vec![list(Element::OrderedList, children)],
        "li" => vec![Container {
            element: Element::ListItem,
            children,
            ..Container::default()
        }],
        "blockquote" => vec![Container {
            element: Element::Div,
            classes: vec!["markdown-blockquote".to_owned()],
            data: BTreeMap::from([(HTML_ELEMENT.to_owned(), "blockquote".to_owned())]),
            children,
            ..Container::default()
        }],
        "pre" => vec![Container {
            element: Element::Div,
            classes: vec![HTML_PRE.to_owned()],
            children,
            ..Container::default()
        }],
        "table" => table(children),
        "thead" => vec![Container {
            element: Element::THead,
            children,
            ..Container::default()
        }],
        "tbody" | "tfoot" => vec![Container {
            element: Element::TBody,
            children,
            ..Container::default()
        }],
        "tr" => vec![Container {
            element: Element::TR,
            children,
            ..Container::default()
        }],
        "td" | "th" => table_cell(frame, children),
        "caption" => vec![Container {
            element: Element::Div,
            classes: vec!["markdown-p".to_owned(), HTML_CAPTION.to_owned()],
            children,
            ..Container::default()
        }],
        "summary" => vec![Container {
            element: Element::Div,
            classes: vec![HTML_SUMMARY.to_owned()],
            children,
            ..Container::default()
        }],
        _ => vec![span(&[], children)],
    }
}

/// Escape a fragment the way the container parser escapes dangerous HTML.
fn parser_escape(html: &str) -> String {
    html.replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
        .replace('&', "&amp;")
}

/// Strip-mode element presentation: tags vanish, blocks keep their rows.
fn build_stripped(frame: &mut Frame) -> Vec<Container> {
    let children = std::mem::take(&mut frame.children);
    match frame.name.as_str() {
        "pre" => vec![Container {
            element: Element::Div,
            classes: vec![HTML_PRE.to_owned()],
            data: BTreeMap::from([(HTML_PLAIN.to_owned(), "true".to_owned())]),
            children,
            ..Container::default()
        }],
        "td" | "th" => {
            let mut cell = children;
            cell.push(html_text(" ".to_owned()));
            vec![span(&[], cell)]
        }
        name if is_block_name(name) => vec![block_container(children)],
        _ => vec![span(&[], children)],
    }
}

fn html_text(value: String) -> Container {
    Container {
        element: Element::Text { value },
        classes: vec![HTML_TEXT.to_owned()],
        ..Container::default()
    }
}

fn line_break() -> Container {
    Container {
        element: Element::Text {
            value: "\n".to_owned(),
        },
        classes: vec![HTML_BREAK.to_owned()],
        ..Container::default()
    }
}

fn span(classes: &[&str], children: Vec<Container>) -> Container {
    Container {
        element: Element::Span,
        classes: classes.iter().map(|class| (*class).to_owned()).collect(),
        children,
        ..Container::default()
    }
}

fn block_container(children: Vec<Container>) -> Container {
    Container {
        element: Element::Div,
        children,
        ..Container::default()
    }
}

fn list(element: Element, children: Vec<Container>) -> Container {
    Container {
        element,
        children: children
            .into_iter()
            .filter(|child| matches!(child.element, Element::ListItem))
            .collect(),
        ..Container::default()
    }
}

/// Attach a block alignment from the element's `align` attribute.
fn aligned(frame: &Frame, mut container: Container) -> Container {
    if let Some(align) = frame
        .attribute("align")
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .filter(|align| matches!(align.as_str(), "center" | "right"))
    {
        container.data.insert(HTML_ALIGN.to_owned(), align);
    }
    container
}

/// Marks a caption paragraph that a table moves before itself.
const HTML_CAPTION: &str = "bmux-html-caption";

fn table_cell(frame: &Frame, children: Vec<Container>) -> Vec<Container> {
    let mut data = BTreeMap::new();
    if let Some(align) = frame.attribute("align") {
        data.insert(HTML_ALIGN.to_owned(), align.trim().to_ascii_lowercase());
    }
    let element = if frame.name == "th" {
        Element::TH {
            rows: None,
            columns: None,
        }
    } else {
        Element::TD {
            rows: None,
            columns: None,
        }
    };
    let span_count = frame
        .attribute("colspan")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 64);
    let mut output = vec![Container {
        element: element.clone(),
        data,
        children,
        ..Container::default()
    }];
    output.extend((1..span_count).map(|_| Container {
        element: element.clone(),
        ..Container::default()
    }));
    output
}

/// Build a table, its header section, and per-column alignments.
fn table(children: Vec<Container>) -> Vec<Container> {
    let mut captions = Vec::new();
    let mut sections = Vec::new();
    let mut loose_rows = Vec::new();
    for child in children {
        if child.classes.iter().any(|class| class == HTML_CAPTION) {
            captions.push(child);
            continue;
        }
        match child.element {
            Element::TR => loose_rows.push(child),
            Element::THead | Element::TBody => {
                if !loose_rows.is_empty() {
                    sections.push(Container {
                        element: Element::TBody,
                        children: std::mem::take(&mut loose_rows),
                        ..Container::default()
                    });
                }
                sections.push(child);
            }
            _ => {}
        }
    }
    if !loose_rows.is_empty() {
        sections.push(Container {
            element: Element::TBody,
            children: loose_rows,
            ..Container::default()
        });
    }
    let has_head = sections
        .iter()
        .any(|section| matches!(section.element, Element::THead));
    if !has_head
        && let Some(first) = sections.first_mut()
        && let Some(row) = first.children.first()
        && !row.children.is_empty()
        && row
            .children
            .iter()
            .all(|cell| matches!(cell.element, Element::TH { .. }))
    {
        let row = first.children.remove(0);
        sections.insert(
            0,
            Container {
                element: Element::THead,
                children: vec![row],
                ..Container::default()
            },
        );
    }
    let alignments = sections
        .iter()
        .find_map(|section| section.children.first())
        .map(|row| {
            row.children
                .iter()
                .map(|cell| match cell.data.get(HTML_ALIGN).map(String::as_str) {
                    Some("center") => "c",
                    Some("right") => "r",
                    Some("left") => "l",
                    _ => "n",
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let mut output = captions;
    output.push(Container {
        element: Element::Table,
        data: BTreeMap::from([(HTML_TABLE_ALIGN.to_owned(), alignments)]),
        children: sections,
        ..Container::default()
    });
    output
}

fn is_block_name(name: &str) -> bool {
    BLOCKS.contains(&name)
        || matches!(
            name,
            "caption"
                | "dd"
                | "dt"
                | "figcaption"
                | "li"
                | "summary"
                | "tbody"
                | "td"
                | "tfoot"
                | "th"
                | "thead"
                | "tr"
        )
}

/// Whether a container occupies its own rows.
const fn is_block(container: &Container) -> bool {
    !matches!(
        container.element,
        Element::Text { .. }
            | Element::Raw { .. }
            | Element::Span
            | Element::Anchor { .. }
            | Element::Image { .. }
            | Element::Input { .. }
    )
}

fn contains_raw(container: &Container) -> bool {
    matches!(container.element, Element::Raw { .. }) || container.children.iter().any(contains_raw)
}

fn contains_image(container: &Container) -> bool {
    container.classes.iter().any(|class| class == HTML_IMAGE)
        || container.children.iter().any(contains_image)
}

fn is_html_text(container: &Container) -> bool {
    container.classes.iter().any(|class| class == HTML_TEXT)
}

fn is_line_break(container: &Container) -> bool {
    container.classes.iter().any(|class| class == HTML_BREAK)
}

fn is_whitespace_text(container: &Container) -> bool {
    matches!(&container.element, Element::Text { value } if value.trim().is_empty())
        && !is_line_break(container)
}

/// Remove HTML source whitespace that would show at the start or end of a row.
///
/// Text is trimmed at the start of a row (after a block boundary or line
/// break, where removed markup can leave source spacing behind) and HTML text
/// is trimmed at the end of a row. Inline code keeps its spacing.
fn normalize_block_children(children: &mut Vec<Container>) {
    let mut at_row_start = true;
    let mut index = 0;
    while index < children.len() {
        let child = &mut children[index];
        if is_line_break(child) {
            trim_trailing_text(children, index);
            at_row_start = true;
            index += 1;
            continue;
        }
        if is_block(child) {
            trim_trailing_text(children, index);
            at_row_start = true;
            index += 1;
            continue;
        }
        let code = child.classes.iter().any(|class| class == "inline-code");
        if at_row_start
            && !code
            && let Element::Text { value } = &mut child.element
        {
            let trimmed = value.trim_start();
            if trimmed.is_empty() {
                children.remove(index);
                continue;
            }
            if trimmed.len() != value.len() {
                *value = trimmed.to_owned();
            }
        }
        at_row_start = false;
        index += 1;
    }
    trim_trailing_text(children, children.len());
}

/// Trim trailing whitespace from HTML text directly before `end`.
fn trim_trailing_text(children: &mut Vec<Container>, end: usize) {
    let mut index = end;
    while index > 0 {
        let candidate = &mut children[index - 1];
        if !is_html_text(candidate) {
            return;
        }
        let Element::Text { value } = &mut candidate.element else {
            return;
        };
        let trimmed = value.trim_end().len();
        value.truncate(trimmed);
        if !value.is_empty() {
            return;
        }
        children.remove(index - 1);
        index -= 1;
    }
}

fn plain_text(children: &[Container]) -> String {
    let mut output = String::new();
    for child in children {
        match &child.element {
            Element::Text { value } | Element::Raw { value } => output.push_str(value),
            _ => {
                if let Some(label) = child.data.get(HTML_IMAGE_LABEL) {
                    output.push_str(label);
                }
                output.push_str(&plain_text(&child.children));
            }
        }
    }
    output
}

/// Text of `children` when they are all text nodes.
fn plain_text_if_flat(children: &[Container]) -> Option<String> {
    children
        .iter()
        .map(|child| match &child.element {
            Element::Text { value } if !is_line_break(child) => Some(value.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Token, decode_entities, tokenize};

    #[test]
    fn tokenizer_keeps_stray_angle_brackets_as_text() {
        let tokens = tokenize("a < b <3 and <b>x</b>").tokens;
        assert_eq!(tokens.first(), Some(&Token::Text(0..13)));
        assert!(
            tokens
                .iter()
                .any(|token| matches!(token, Token::Start { name, .. } if name == "b"))
        );
    }

    #[test]
    fn tokenizer_reads_quoted_unquoted_and_bare_attributes() {
        let tokens = tokenize(r#"<img src="a b.png" width=200 alt='x > y' hidden/>"#).tokens;
        let Some(Token::Start {
            attributes,
            self_closing,
            ..
        }) = tokens.first()
        else {
            panic!("expected a start tag: {tokens:?}");
        };
        assert!(*self_closing);
        assert_eq!(
            attributes,
            &[
                ("src".to_owned(), "a b.png".to_owned()),
                ("width".to_owned(), "200".to_owned()),
                ("alt".to_owned(), "x > y".to_owned()),
                ("hidden".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn raw_text_elements_hide_markup_inside() {
        let tokens = tokenize("<script>if (a<b) { x('</p>') }</script>after").tokens;
        assert!(matches!(tokens.last(), Some(Token::Text(_))));
        assert!(
            !tokens
                .iter()
                .any(|token| matches!(token, Token::End { name, .. } if name == "p"))
        );
    }

    #[test]
    fn entities_use_the_complete_html_table() {
        assert_eq!(
            decode_entities("&nbsp;&amp;&lt;&copy;&#65;&#x1F600;&bogus;&"),
            "\u{a0}&<©A😀&bogus;&"
        );
        assert!(matches!(
            decode_entities("plain"),
            std::borrow::Cow::Borrowed(_)
        ));
    }
}
