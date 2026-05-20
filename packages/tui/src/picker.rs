//! Dropdown and list-picker composition widgets.

use bmux_text_edit::TextEditBuffer;

use crate::chrome::{Border, Panel};
use crate::frame::Frame;
use crate::geometry::Rect;
use crate::input::TextInput;
use crate::layout::{Direction, split_leading};
use crate::list::{List, ListItem, ListState};
use crate::widget::Widget;

/// A lightweight dropdown/list popup widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropdown<'items> {
    list: List<'items>,
    panel: Option<Panel>,
    max_height: Option<u16>,
}

impl<'items> Dropdown<'items> {
    /// Create a dropdown from list items.
    #[must_use]
    pub const fn new(items: &'items [ListItem]) -> Self {
        Self {
            list: List::new(items),
            panel: None,
            max_height: None,
        }
    }

    /// Set the list widget used by this dropdown.
    #[must_use]
    pub fn list(mut self, list: List<'items>) -> Self {
        self.list = list;
        self
    }

    /// Add panel chrome around the dropdown.
    #[must_use]
    pub fn panel(mut self, panel: Panel) -> Self {
        self.panel = Some(panel);
        self
    }

    /// Set maximum rendered height.
    #[must_use]
    pub const fn max_height(mut self, height: u16) -> Self {
        self.max_height = Some(height);
        self
    }

    /// Return the content area inside optional panel chrome and max-height limit.
    #[must_use]
    pub fn content_area(&self, area: Rect) -> Rect {
        let limited = self.max_height.map_or(area, |max_height| {
            Rect::new(area.x, area.y, area.width, min_u16(area.height, max_height))
        });
        self.panel
            .as_ref()
            .map_or(limited, |panel| panel.inner_area(limited))
    }
}

impl crate::widget::StatefulWidget for Dropdown<'_> {
    type State = ListState;

    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State) {
        if area.is_empty() {
            return;
        }
        let area = self.max_height.map_or(area, |max_height| {
            Rect::new(area.x, area.y, area.width, min_u16(area.height, max_height))
        });
        if let Some(panel) = &self.panel {
            panel.render(area, frame);
        }
        self.list.render(self.content_area(area), frame, state);
    }
}

const fn min_u16(a: u16, b: u16) -> u16 {
    if a < b { a } else { b }
}

/// A command-palette-style list picker composed from a panel, text input, and list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPicker<'a> {
    input: TextInput<'a>,
    items: &'a [ListItem],
    panel: Panel,
    input_height: u16,
    gap: u16,
    list: List<'a>,
}

impl<'a> ListPicker<'a> {
    /// Create a list picker from an input buffer and list items.
    #[must_use]
    pub const fn new(input: &'a TextEditBuffer, items: &'a [ListItem]) -> Self {
        Self {
            input: TextInput::new(input),
            items,
            panel: Panel::new().border(Border::single()),
            input_height: 1,
            gap: 1,
            list: List::new(items),
        }
    }

    /// Set the panel chrome.
    #[must_use]
    pub fn panel(mut self, panel: Panel) -> Self {
        self.panel = panel;
        self
    }

    /// Set the text input widget.
    #[must_use]
    pub fn input(mut self, input: TextInput<'a>) -> Self {
        self.input = input;
        self
    }

    /// Set the list widget.
    #[must_use]
    pub fn list(mut self, list: List<'a>) -> Self {
        self.list = list;
        self
    }

    /// Set the input area height.
    #[must_use]
    pub const fn input_height(mut self, height: u16) -> Self {
        self.input_height = height;
        self
    }

    /// Set the gap between input and list.
    #[must_use]
    pub const fn gap(mut self, rows: u16) -> Self {
        self.gap = rows;
        self
    }

    /// Return the input and list areas inside the picker.
    #[must_use]
    pub const fn content_areas(&self, area: Rect) -> ListPickerAreas {
        let inner = self.panel.inner_area(area);
        let input_split = split_leading(inner, Direction::Vertical, self.input_height);
        let list_split = split_leading(input_split.second, Direction::Vertical, self.gap);
        ListPickerAreas {
            input: input_split.first,
            list: list_split.second,
        }
    }
}

impl crate::widget::StatefulWidget for ListPicker<'_> {
    type State = ListState;

    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State) {
        if area.is_empty() {
            return;
        }
        self.panel.render(area, frame);
        let areas = self.content_areas(area);
        self.input.render(areas.input, frame);
        self.list.render(areas.list, frame, state);
    }
}

/// Content areas computed by [`ListPicker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListPickerAreas {
    /// Text input area.
    pub input: Rect,
    /// List area.
    pub list: Rect,
}

impl ListPicker<'_> {
    /// Return picker item count.
    #[must_use]
    pub const fn item_count(&self) -> usize {
        self.items.len()
    }
}
