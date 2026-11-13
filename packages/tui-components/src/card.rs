//! A framed card laid out as rows: for lists whose items are text rows (virtual lists, logs,
//! comment threads) rather than live components.
//!
//! A card is stateless. [`Card::rows`] lays it out at a width and returns its [`CardRows`]: the
//! painted lines, and the cells that act (each carrying the caller's action value), so a caller
//! can cache the rows, know the card's height exactly, and route clicks itself.
//!
//! ```text
//! ╭─ ▾ 💬 ana · 2 comments ── waiting on you ─────────────────────── ○ Viewed ─╮
//! │ ana · 3d ago                                                                │
//! │ This couples the parser to the storage layer.                               │
//! ├─────────────────────────────────────────────────────────────────────────────┤
//! │ you · 1d ago                                                                │
//! │ Agreed.                                                                     │
//! ╰─ Reply · Resolve · Open ↗ ────────────────────────────────────────── L12 ─╯
//! ```
//!
//! * **Compact** ([`CardLayout::Compact`]): the title rides in the top border and the buttons in
//!   the bottom one, so the frame costs no rows. Title segments are kept in order while they
//!   fit; the first is truncated rather than dropped. Buttons that do not fit the bottom border
//!   wrap onto rows inside the card.
//! * **Roomy** ([`CardLayout::Roomy`]): plain top and bottom borders, the title and the buttons
//!   on rows of their own inside.
//! * **Folded**: one row, the title in a rule.
//! * Sections are separated by dividers. Narrower than [`CardPolicy::min_framed`], the sides go
//!   and only the rules stay. [`CardGlyphs::ASCII`] draws the frame in `+ - |`.
//! * [`CardStyles::background`] fills the card (frame included), under every span that sets no
//!   background of its own.

use std::ops::Range;

use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::text_width::display_width;

/// Frame glyphs of a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardGlyphs {
    /// Top corners.
    pub top: [&'static str; 2],
    /// Bottom corners.
    pub bottom: [&'static str; 2],
    /// Divider ends.
    pub tee: [&'static str; 2],
    /// Ends of a folded card's rule.
    pub ends: [&'static str; 2],
    /// Sides.
    pub side: &'static str,
    /// Horizontal rule.
    pub rule: &'static str,
}

impl CardGlyphs {
    /// Rounded box drawing.
    pub const ROUNDED: Self = Self {
        top: ["╭", "╮"],
        bottom: ["╰", "╯"],
        tee: ["├", "┤"],
        ends: ["╶", "╴"],
        side: "│",
        rule: "─",
    };

    /// Plain ASCII.
    pub const ASCII: Self = Self {
        top: ["+", "+"],
        bottom: ["+", "+"],
        tee: ["+", "+"],
        ends: ["-", "-"],
        side: "|",
        rule: "-",
    };

    /// These glyphs without sides: rules only (for narrow cards).
    #[must_use]
    pub const fn sideless(self) -> Self {
        Self {
            top: [self.rule, self.rule],
            bottom: [self.rule, self.rule],
            tee: [self.rule, self.rule],
            ends: [self.rule, self.rule],
            side: " ",
            rule: self.rule,
        }
    }
}

/// Where a card's title and buttons go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CardLayout {
    /// In the borders: the frame costs no rows.
    #[default]
    Compact,
    /// On rows of their own inside the frame.
    Roomy,
}

/// How a card is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardPolicy {
    /// Compact or roomy.
    pub layout: CardLayout,
    /// Frame glyphs.
    pub glyphs: CardGlyphs,
    /// Narrower than this, the sides go.
    pub min_framed: usize,
    /// Between buttons.
    pub separator: &'static str,
}

impl Default for CardPolicy {
    fn default() -> Self {
        Self {
            layout: CardLayout::Compact,
            glyphs: CardGlyphs::ROUNDED,
            min_framed: 40,
            separator: " · ",
        }
    }
}

/// A card's styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CardStyles {
    /// Fill of the whole card, frame included; spans that set a background keep theirs.
    pub background: Option<Style>,
    /// Frame glyphs.
    pub border: Style,
    /// Buttons.
    pub button: Style,
    /// Separators between buttons.
    pub separator: Style,
}

/// A run of styled text that may act when clicked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSegment<A> {
    /// Text.
    pub spans: Vec<Span>,
    /// What a click on it does.
    pub action: Option<A>,
}

impl<A> CardSegment<A> {
    /// A segment that does nothing when clicked.
    #[must_use]
    pub const fn text(spans: Vec<Span>) -> Self {
        Self {
            spans,
            action: None,
        }
    }

    /// A segment that runs `action` when clicked.
    #[must_use]
    pub const fn action(spans: Vec<Span>, action: A) -> Self {
        Self {
            spans,
            action: Some(action),
        }
    }

    fn width(&self) -> usize {
        width_of(&self.spans)
    }
}

/// Cells that act: `cells` of row `row`, counted from the item's first row and cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardHit<A> {
    /// Row.
    pub row: u16,
    /// Cells.
    pub cells: Range<u16>,
    /// Action.
    pub action: A,
}

/// A run of the body: lines at most the card's inner width wide ([`Card::inner_width`]), and
/// their hits, with rows from the section's first line and cells from the inner start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSection<A> {
    /// Lines.
    pub lines: Vec<Line>,
    /// Hits.
    pub hits: Vec<CardHit<A>>,
}

impl<A> CardSection<A> {
    /// A section of `lines` that do nothing when clicked.
    #[must_use]
    pub const fn lines(lines: Vec<Line>) -> Self {
        Self {
            lines,
            hits: Vec::new(),
        }
    }
}

/// A button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardButton<A> {
    /// Label.
    pub label: String,
    /// What a click on it does.
    pub action: A,
}

/// A laid-out card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardRows<A> {
    /// One line per row.
    pub lines: Vec<Line>,
    /// Cells that act.
    pub hits: Vec<CardHit<A>>,
}

/// A framed card (see the [module documentation](self)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card<A> {
    title: Vec<CardSegment<A>>,
    right: Option<CardSegment<A>>,
    header_action: Option<A>,
    sections: Vec<CardSection<A>>,
    buttons: Vec<CardButton<A>>,
    footer: Vec<Span>,
    folded: bool,
    styles: CardStyles,
    policy: CardPolicy,
}

impl<A: Clone> Default for Card<A> {
    fn default() -> Self {
        Self::new()
    }
}

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(|span| display_width(&span.content)).sum()
}

fn cells(x: usize) -> u16 {
    u16::try_from(x).unwrap_or(u16::MAX)
}

impl<A: Clone> Card<A> {
    /// An empty card.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            title: Vec::new(),
            right: None,
            header_action: None,
            sections: Vec::new(),
            buttons: Vec::new(),
            footer: Vec::new(),
            folded: false,
            styles: CardStyles {
                background: None,
                border: Style::new(),
                button: Style::new(),
                separator: Style::new(),
            },
            policy: CardPolicy {
                layout: CardLayout::Compact,
                glyphs: CardGlyphs::ROUNDED,
                min_framed: 40,
                separator: " · ",
            },
        }
    }

    /// Title segments, most important first.
    #[must_use]
    pub fn title(mut self, segments: Vec<CardSegment<A>>) -> Self {
        self.title = segments;
        self
    }

    /// The right end of the title row (a checkbox, a count).
    #[must_use]
    pub fn right(mut self, segment: CardSegment<A>) -> Self {
        self.right = Some(segment);
        self
    }

    /// What a click on the title row does where no segment acts (folding the card, say).
    #[must_use]
    pub fn header_action(mut self, action: A) -> Self {
        self.header_action = Some(action);
        self
    }

    /// Body sections, separated by dividers.
    #[must_use]
    pub fn sections(mut self, sections: Vec<CardSection<A>>) -> Self {
        self.sections = sections;
        self
    }

    /// Buttons.
    #[must_use]
    pub fn buttons(mut self, buttons: Vec<CardButton<A>>) -> Self {
        self.buttons = buttons;
        self
    }

    /// The right end of the button row (a reference, a status).
    #[must_use]
    pub fn footer(mut self, footer: Vec<Span>) -> Self {
        self.footer = footer;
        self
    }

    /// Show only the title row.
    #[must_use]
    pub const fn folded(mut self, folded: bool) -> Self {
        self.folded = folded;
        self
    }

    /// Styles.
    #[must_use]
    pub const fn styles(mut self, styles: CardStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Policy.
    #[must_use]
    pub const fn policy(mut self, policy: CardPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Cells inside a card `width` cells wide (its sides and their padding taken): what its
    /// sections' lines may use.
    #[must_use]
    pub const fn inner_width(width: usize) -> usize {
        let inner = width.saturating_sub(4);
        if inner == 0 { 1 } else { inner }
    }

    /// The card `width` cells wide, `indent` cells in.
    #[must_use]
    pub fn rows(&self, indent: usize, width: usize) -> CardRows<A> {
        let glyphs = if width < self.policy.min_framed {
            self.policy.glyphs.sideless()
        } else {
            self.policy.glyphs
        };
        let mut out = Out {
            indent,
            width,
            glyphs,
            styles: self.styles,
            separator: self.policy.separator,
            header_action: self.header_action.clone(),
            lines: Vec::new(),
            hits: Vec::new(),
        };
        if self.folded {
            out.header_rule(self, glyphs.ends);
            return out.finish();
        }
        match self.policy.layout {
            CardLayout::Compact => out.header_rule(self, glyphs.top),
            CardLayout::Roomy => {
                out.plain_rule(glyphs.top);
                out.header_row(self);
                out.plain_rule(glyphs.tee);
            }
        }
        for (index, section) in self.sections.iter().enumerate() {
            if index > 0 {
                out.plain_rule(glyphs.tee);
            }
            out.section(section);
        }
        match self.policy.layout {
            CardLayout::Compact => out.footer_rule(&self.buttons, &self.footer),
            CardLayout::Roomy => {
                if !self.buttons.is_empty() || !self.footer.is_empty() {
                    out.plain_rule(glyphs.tee);
                    out.button_rows(&self.buttons, &self.footer);
                }
                out.plain_rule(glyphs.bottom);
            }
        }
        out.finish()
    }
}

/// Rows being laid out.
struct Out<A> {
    indent: usize,
    width: usize,
    glyphs: CardGlyphs,
    styles: CardStyles,
    separator: &'static str,
    header_action: Option<A>,
    lines: Vec<Line>,
    hits: Vec<CardHit<A>>,
}

impl<A: Clone> Out<A> {
    fn finish(self) -> CardRows<A> {
        CardRows {
            lines: self.lines,
            hits: self.hits,
        }
    }

    fn row(&self) -> u16 {
        cells(self.lines.len())
    }

    const fn inner(&self) -> usize {
        Card::<A>::inner_width(self.width)
    }

    /// `style` over the card's background.
    fn on_card(&self, style: Style) -> Style {
        self.styles
            .background
            .map_or(style, |background| background.patch(style))
    }

    /// Push a row: the indent, then `spans` over the card's background, clipped to the card.
    fn push(&mut self, spans: Vec<Span>) {
        let mut line = vec![Span::raw(" ".repeat(self.indent))];
        line.extend(
            spans
                .into_iter()
                .map(|span| Span::styled(span.content, self.on_card(span.style))),
        );
        self.lines
            .push(Line::from_spans(line).truncate(self.indent + self.width));
    }

    fn border(&self, text: impl Into<String>) -> Span {
        Span::styled(text.into(), self.styles.border)
    }

    /// `ends[0] ─── ends[1]`.
    fn plain_rule(&mut self, ends: [&'static str; 2]) {
        let spans = vec![
            self.border(ends[0]),
            self.border(self.glyphs.rule.repeat(self.width.saturating_sub(2))),
            self.border(ends[1]),
        ];
        self.push(spans);
    }

    /// Title segments that fit `room` cells: the first always (truncated), then in order while
    /// they fit. Returns their spans and their acting cells, from the title's start.
    fn fit(title: &[CardSegment<A>], room: usize) -> (Vec<Span>, Vec<(Range<usize>, A)>) {
        let mut spans: Vec<Span> = Vec::new();
        let mut acts = Vec::new();
        let mut used = 0;
        for (index, segment) in title.iter().enumerate() {
            let gap = usize::from(index > 0);
            if index > 0 && used + gap + segment.width() > room {
                break;
            }
            if gap > 0 {
                spans.push(Span::raw(" "));
            }
            let start = used + gap;
            spans.extend(segment.spans.iter().cloned());
            used = start + segment.width();
            if let Some(action) = &segment.action {
                acts.push((start..used.min(room), action.clone()));
            }
        }
        let line = Line::from_spans(spans).truncate(room);
        (line.spans, acts)
    }

    /// The title row's hits: segment actions at `start` plus their cells, the right segment's
    /// action, and the header action over the rest of `span`.
    fn header_hits(
        &mut self,
        span: Range<usize>,
        segment_hits: Vec<(Range<usize>, A)>,
        right: Option<(Range<usize>, Option<A>)>,
    ) {
        let row = self.row();
        let mut claimed: Vec<(Range<usize>, A)> = segment_hits;
        if let Some((cells_at, Some(action))) = right {
            claimed.push((cells_at, action));
        }
        claimed.sort_by_key(|(cells_at, _)| cells_at.start);
        let mut x = span.start;
        for (cells_at, action) in claimed {
            if let Some(header) = &self.header_action
                && cells_at.start > x
            {
                self.hits.push(CardHit {
                    row,
                    cells: cells(x)..cells(cells_at.start),
                    action: header.clone(),
                });
            }
            self.hits.push(CardHit {
                row,
                cells: cells(cells_at.start)..cells(cells_at.end),
                action,
            });
            x = cells_at.end;
        }
        if let Some(header) = &self.header_action
            && span.end > x
        {
            self.hits.push(CardHit {
                row,
                cells: cells(x)..cells(span.end),
                action: header.clone(),
            });
        }
    }

    /// `ends[0]─ title ─── right ─ends[1]`: the compact top border, or a folded card.
    fn header_rule(&mut self, card: &Card<A>, ends: [&'static str; 2]) {
        let rule = self.glyphs.rule;
        let right_width = card.right.as_ref().map_or(0, |right| right.width() + 1);
        // `end─ ` title ` ─fill` [` right`] ` ─end`: 7 cells besides the title, fill, right.
        let fixed = 7 + right_width;
        let room = self.width.saturating_sub(fixed + 1);
        let (title, acts) = Self::fit(&card.title, room);
        let title_width = width_of(&title);
        let fill = self.width.saturating_sub(fixed + title_width).max(1);
        let title_start = self.indent + 3;
        let mut spans = vec![self.border(ends[0]), self.border(format!("{rule} "))];
        spans.extend(title);
        spans.push(self.border(format!(" {}", rule.repeat(fill))));
        let mut right_hit = None;
        if let Some(right) = &card.right {
            let x = title_start + title_width + 1 + fill + 1;
            right_hit = Some((x..x + right.width(), right.action.clone()));
            spans.push(Span::raw(" "));
            spans.extend(right.spans.iter().cloned());
        }
        spans.push(self.border(format!(" {rule}")));
        spans.push(self.border(ends[1]));
        let segment_hits = acts
            .into_iter()
            .map(|(at, action)| (title_start + at.start..title_start + at.end, action))
            .collect();
        self.header_hits(
            self.indent..self.indent + self.width,
            segment_hits,
            right_hit,
        );
        self.push(spans);
    }

    /// `│ title ... right │`: the roomy title row.
    fn header_row(&mut self, card: &Card<A>) {
        let right_width = card.right.as_ref().map_or(0, |right| right.width() + 1);
        let room = self.inner().saturating_sub(right_width);
        let (title, acts) = Self::fit(&card.title, room);
        let gap = self.inner().saturating_sub(width_of(&title) + right_width);
        let start = self.indent + 2;
        let mut content = title;
        content.push(Span::raw(" ".repeat(gap)));
        let mut right_hit = None;
        if let Some(right) = &card.right {
            let x = start + self.inner() - right.width();
            right_hit = Some((x..x + right.width(), right.action.clone()));
            content.push(Span::raw(" "));
            content.extend(right.spans.iter().cloned());
        }
        let segment_hits = acts
            .into_iter()
            .map(|(at, action)| (start + at.start..start + at.end, action))
            .collect();
        self.header_hits(
            self.indent..self.indent + self.width,
            segment_hits,
            right_hit,
        );
        self.content_row(content);
    }

    /// `│ content │`, the content padded to the inner width.
    fn content_row(&mut self, content: Vec<Span>) {
        let inner = self.inner();
        let line = Line::from_spans(content).truncate(inner);
        let gap = inner.saturating_sub(line.width());
        let mut spans = vec![self.border(self.glyphs.side), Span::raw(" ")];
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(gap + 1)));
        spans.push(self.border(self.glyphs.side));
        self.push(spans);
    }

    fn section(&mut self, section: &CardSection<A>) {
        let first = self.row();
        let left = cells(self.indent + 2);
        for hit in &section.hits {
            self.hits.push(CardHit {
                row: first.saturating_add(hit.row),
                cells: hit.cells.start.saturating_add(left)..hit.cells.end.saturating_add(left),
                action: hit.action.clone(),
            });
        }
        for line in &section.lines {
            self.content_row(line.spans.clone());
        }
    }

    /// `buttons` from `x`, as spans and their hits on row `row`, and the cells they take.
    fn buttons(
        &self,
        buttons: &[CardButton<A>],
        x: usize,
        row: u16,
    ) -> (Vec<Span>, Vec<CardHit<A>>, usize) {
        let mut spans = Vec::new();
        let mut hits = Vec::new();
        let mut at = x;
        for (index, button) in buttons.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(self.separator, self.styles.separator));
                at += display_width(self.separator);
            }
            let width = display_width(&button.label);
            spans.push(Span::styled(button.label.clone(), self.styles.button));
            hits.push(CardHit {
                row,
                cells: cells(at)..cells(at + width),
                action: button.action.clone(),
            });
            at += width;
        }
        (spans, hits, at - x)
    }

    /// `buttons` on rows inside the card, wrapped to its inner width.
    fn wrapped_buttons(&mut self, buttons: &[CardButton<A>]) {
        let inner = self.inner();
        let separator = display_width(self.separator);
        let mut row: Vec<CardButton<A>> = Vec::new();
        let mut used = 0;
        for button in buttons {
            let width = display_width(&button.label);
            if !row.is_empty() && used + separator + width > inner {
                self.button_row(&row);
                row.clear();
                used = 0;
            }
            used += if row.is_empty() {
                width
            } else {
                separator + width
            };
            row.push(button.clone());
        }
        if !row.is_empty() {
            self.button_row(&row);
        }
    }

    fn button_row(&mut self, buttons: &[CardButton<A>]) {
        let (spans, hits, _) = self.buttons(buttons, self.indent + 2, self.row());
        self.hits.extend(hits);
        self.content_row(spans);
    }

    /// `╰─ buttons ─── footer ─╯`: the compact bottom border; buttons that do not fit go on
    /// rows above it.
    fn footer_rule(&mut self, buttons: &[CardButton<A>], footer: &[Span]) {
        let rule = self.glyphs.rule;
        let ends = self.glyphs.bottom;
        // `end─ ` buttons ` ─fill` [` footer`] ` ─end`: 7 cells besides them and the fill.
        let fixed = 7;
        let (spans, hits, used) = self.buttons(buttons, self.indent + 3, self.row());
        let footer_width = width_of(footer);
        let (spans, hits, used) =
            if buttons.is_empty() || used + footer_width + 1 + fixed < self.width {
                (spans, hits, used)
            } else {
                self.wrapped_buttons(buttons);
                (Vec::new(), Vec::new(), 0)
            };
        self.hits.extend(hits);
        let footer: Vec<Span> = if used + footer_width + 1 + fixed < self.width {
            footer.to_vec()
        } else {
            Vec::new()
        };
        let footer_cells = if footer.is_empty() {
            0
        } else {
            width_of(&footer) + 1
        };
        let fill = self
            .width
            .saturating_sub(fixed + used + footer_cells)
            .max(1);
        let mut line = vec![self.border(ends[0]), self.border(format!("{rule} "))];
        line.extend(spans);
        line.push(self.border(format!(" {}", rule.repeat(fill))));
        if !footer.is_empty() {
            line.push(Span::raw(" "));
            line.extend(footer);
        }
        line.push(self.border(format!(" {rule}")));
        line.push(self.border(ends[1]));
        self.push(line);
    }

    /// `│ buttons ... footer │`: the roomy button row (wrapped when needed).
    fn button_rows(&mut self, buttons: &[CardButton<A>], footer: &[Span]) {
        let (spans, hits, used) = self.buttons(buttons, self.indent + 2, self.row());
        if used + width_of(footer) < self.inner() {
            self.hits.extend(hits);
            let mut content = spans;
            let gap = self.inner().saturating_sub(used + width_of(footer));
            content.push(Span::raw(" ".repeat(gap)));
            content.extend(footer.iter().cloned());
            self.content_row(content);
        } else {
            self.wrapped_buttons(buttons);
            if !footer.is_empty() {
                self.content_row(footer.to_vec());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::style::Color;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Act {
        Fold,
        Check,
        Reply,
        Resolve,
        Inner,
    }

    fn card() -> Card<Act> {
        Card::new()
            .title(vec![
                CardSegment::action(vec![Span::raw("▾")], Act::Fold),
                CardSegment::text(vec![Span::raw("ana · 2 comments")]),
                CardSegment::text(vec![Span::raw("waiting on you")]),
            ])
            .right(CardSegment::action(vec![Span::raw("○ Viewed")], Act::Check))
            .header_action(Act::Fold)
            .sections(vec![
                CardSection::lines(vec![Line::raw("first")]),
                CardSection {
                    lines: vec![Line::raw("second"), Line::raw("Inner")],
                    hits: vec![CardHit {
                        row: 1,
                        cells: 0..5,
                        action: Act::Inner,
                    }],
                },
            ])
            .buttons(vec![
                CardButton {
                    label: "Reply".to_owned(),
                    action: Act::Reply,
                },
                CardButton {
                    label: "Resolve".to_owned(),
                    action: Act::Resolve,
                },
            ])
            .footer(vec![Span::raw("L12")])
    }

    fn text(rows: &CardRows<Act>) -> Vec<String> {
        rows.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect()
            })
            .collect()
    }

    /// The action at `(x, row)`.
    fn at(rows: &CardRows<Act>, x: u16, row: u16) -> Option<Act> {
        rows.hits
            .iter()
            .find(|hit| hit.row == row && hit.cells.contains(&x))
            .map(|hit| hit.action)
    }

    fn column(rows: &CardRows<Act>, row: usize, needle: &str) -> u16 {
        let line = &text(rows)[row];
        let byte = line.find(needle).expect("needle");
        cells(display_width(&line[..byte]))
    }

    #[test]
    fn a_compact_card_carries_its_title_and_buttons_in_its_borders() {
        let rows = card().rows(2, 60);
        let lines = text(&rows);
        assert_eq!(lines.len(), 6, "{lines:#?}");
        for line in &lines {
            assert_eq!(display_width(line), 62, "{line:?}");
        }
        assert!(
            lines[0].starts_with("  ╭─ ▾ ana · 2 comments waiting on you"),
            "{lines:#?}"
        );
        assert!(lines[0].ends_with("○ Viewed ─╮"));
        assert!(lines[1].starts_with("  │ first"));
        assert!(lines[2].starts_with("  ├──"));
        assert!(lines[5].starts_with("  ╰─ Reply · Resolve"));
        assert!(lines[5].ends_with("L12 ─╯"));
        // Every part acts where it is painted; the title row folds elsewhere.
        assert_eq!(at(&rows, column(&rows, 0, "▾"), 0), Some(Act::Fold));
        assert_eq!(at(&rows, column(&rows, 0, "Viewed"), 0), Some(Act::Check));
        assert_eq!(at(&rows, column(&rows, 0, "waiting"), 0), Some(Act::Fold));
        assert_eq!(at(&rows, column(&rows, 4, "Inner"), 4), Some(Act::Inner));
        assert_eq!(at(&rows, column(&rows, 5, "Reply"), 5), Some(Act::Reply));
        assert_eq!(
            at(&rows, column(&rows, 5, "Resolve"), 5),
            Some(Act::Resolve)
        );
    }

    #[test]
    fn a_folded_card_is_one_row_and_a_roomy_one_has_rows_for_its_title_and_buttons() {
        let folded = card().folded(true).rows(0, 60);
        let lines = text(&folded);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("╶─ ▾"), "{lines:#?}");
        assert!(lines[0].ends_with("─╴"));
        let roomy = card()
            .policy(CardPolicy {
                layout: CardLayout::Roomy,
                ..CardPolicy::default()
            })
            .rows(0, 60);
        let lines = text(&roomy);
        assert!(lines[0].starts_with("╭──"), "{lines:#?}");
        assert!(lines[1].starts_with("│ ▾ ana"));
        assert!(lines.last().is_some_and(|line| line.starts_with("╰──")));
        let buttons = lines.len() - 2;
        assert!(
            lines[buttons].starts_with("│ Reply · Resolve"),
            "{lines:#?}"
        );
        assert_eq!(
            at(&roomy, column(&roomy, buttons, "Resolve"), cells(buttons)),
            Some(Act::Resolve)
        );
    }

    #[test]
    fn titles_drop_from_the_end_and_buttons_wrap_inside_when_narrow() {
        let rows = card().rows(0, 34);
        let lines = text(&rows);
        // Sideless under the framed width, the first title segment kept.
        assert!(lines[0].starts_with("── ▾"), "{lines:#?}");
        assert!(!lines[0].contains("waiting"));
        let many = card()
            .buttons(
                (0..8)
                    .map(|index| CardButton {
                        label: format!("Button {index}"),
                        action: Act::Reply,
                    })
                    .collect(),
            )
            .rows(0, 50);
        let lines = text(&many);
        let wrapped = lines.iter().filter(|line| line.contains("Button")).count();
        assert!(wrapped > 1, "{lines:#?}");
        assert!(lines.iter().any(|line| line.contains("Button 7")));
        assert!(lines.iter().all(|line| display_width(line) == 50));
    }

    #[test]
    fn the_background_fills_the_card_under_spans_without_their_own() {
        let background = Style::new().bg(Color::Rgb(1, 2, 3));
        let rows = card()
            .styles(CardStyles {
                background: Some(background),
                border: Style::new().fg(Color::Rgb(9, 9, 9)),
                ..CardStyles::default()
            })
            .sections(vec![CardSection::lines(vec![Line::from_spans(vec![
                Span::raw("plain "),
                Span::styled("pill", Style::new().bg(Color::Rgb(7, 7, 7))),
            ])])])
            .rows(2, 50);
        let body = &rows.lines[1];
        // The indent is not the card's; everything after it is on the card, the pill on its own.
        assert_eq!(body.spans[0].style.bg, None);
        for span in &body.spans[1..] {
            let expected = if span.content == "pill" {
                Some(Color::Rgb(7, 7, 7))
            } else {
                Some(Color::Rgb(1, 2, 3))
            };
            assert_eq!(span.style.bg, expected, "{:?}", span.content);
        }
        // The border keeps its color over the card.
        assert_eq!(rows.lines[0].spans[1].style.fg, Some(Color::Rgb(9, 9, 9)));
        assert_eq!(rows.lines[0].spans[1].style.bg, Some(Color::Rgb(1, 2, 3)));
    }
}
