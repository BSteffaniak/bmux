//! Widget traits and helpers.

use crate::frame::Frame;
use crate::geometry::Rect;

/// A renderable terminal UI component.
pub trait Widget {
    /// Render this widget into `area`.
    fn render(&self, area: Rect, frame: &mut Frame<'_>);
}

/// A renderable terminal UI component with caller-owned state.
pub trait StatefulWidget {
    /// Caller-owned widget state.
    type State;

    /// Render this widget into `area` using `state`.
    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State);
}

#[cfg(test)]
mod tests {
    use super::{StatefulWidget, Widget};
    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::Rect;
    use crate::text::Line;

    struct Label<'a>(&'a str);

    impl Widget for Label<'_> {
        fn render(&self, area: Rect, frame: &mut Frame<'_>) {
            frame.write_line(area, &Line::from(self.0));
        }
    }

    struct Counter;

    impl StatefulWidget for Counter {
        type State = u8;

        fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State) {
            frame.write_line(area, &Line::from(format!("count={state}")));
            *state = state.saturating_add(1);
        }
    }

    #[test]
    fn widget_trait_supports_stateless_rendering_by_reference() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        Label("hello").render(Rect::new(0, 0, 8, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("hello   "));
    }

    #[test]
    fn stateful_widget_trait_keeps_state_caller_owned() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);
        let mut state = 7;

        Counter.render(Rect::new(0, 0, 8, 1), &mut frame, &mut state);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("count=7 "));
        assert_eq!(state, 8);
    }
}
