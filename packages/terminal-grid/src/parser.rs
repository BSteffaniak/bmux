use crate::model::{GridMode, TerminalGrid};
use vte::{Params, Perform};

pub(crate) fn process(grid: &mut TerminalGrid, bytes: &[u8]) {
    let mut parser = vte::Parser::new();
    let mut performer = GridPerformer { grid };
    parser.advance(&mut performer, bytes);
}

struct GridPerformer<'a> {
    grid: &'a mut TerminalGrid,
}

impl Perform for GridPerformer<'_> {
    fn print(&mut self, c: char) {
        self.grid.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.grid.linefeed(),
            b'\r' => self.grid.carriage_return(),
            0x08 => self.grid.backspace(),
            b'\t' => self.grid.tab(),
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let values = flatten_params(params);
        match action {
            'A' => self
                .grid
                .move_cursor_relative(-one_based(values.first()).cast_signed(), 0),
            'B' => self
                .grid
                .move_cursor_relative(one_based(values.first()).cast_signed(), 0),
            'C' => self
                .grid
                .move_cursor_relative(0, one_based(values.first()).cast_signed()),
            'D' => self
                .grid
                .move_cursor_relative(0, -one_based(values.first()).cast_signed()),
            'G' => self.grid.move_cursor_to(
                self.grid.cursor().row,
                one_based(values.first()).saturating_sub(1),
            ),
            'H' | 'f' => {
                let row = one_based(values.first()).saturating_sub(1);
                let col = one_based(values.get(1)).saturating_sub(1);
                self.grid.move_cursor_to(row, col);
            }
            'J' => self.grid.erase_display(default_zero(values.first())),
            'K' => self.grid.erase_line(default_zero(values.first())),
            'm' => self.grid.set_graphic_rendition(&values),
            'h' | 'l' if intermediates == [b'?'] => {
                let enabled = action == 'h';
                for value in values {
                    match value {
                        7 => self.grid.set_autowrap(enabled),
                        25 => self.grid.set_cursor_visible(enabled),
                        47 | 1047 | 1049 => self.grid.set_mode(if enabled {
                            GridMode::Alternate
                        } else {
                            GridMode::Main
                        }),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.grid.save_cursor(),
            b'8' => self.grid.restore_cursor(),
            b'D' => self.grid.linefeed(),
            b'E' => {
                self.grid.linefeed();
                self.grid.carriage_return();
            }
            b'M' => self.grid.move_cursor_relative(-1, 0),
            b'c' => {
                let width = u16::try_from(self.grid.width()).unwrap_or(u16::MAX);
                let height = u16::try_from(self.grid.height()).unwrap_or(u16::MAX);
                if let Ok(reset) =
                    TerminalGrid::new(width, height, crate::model::GridLimits::default())
                {
                    *self.grid = reset;
                }
            }
            _ => {}
        }
    }
}

fn flatten_params(params: &Params) -> Vec<i64> {
    let mut values = Vec::new();
    for param in params {
        let mut pushed = false;
        for subparam in param {
            values.push(i64::from(*subparam));
            pushed = true;
        }
        if !pushed {
            values.push(0);
        }
    }
    values
}

fn one_based(value: Option<&i64>) -> usize {
    let value = value.copied().unwrap_or(1);
    if value <= 0 {
        1
    } else {
        usize::try_from(value).unwrap_or(usize::MAX)
    }
}

fn default_zero(value: Option<&i64>) -> usize {
    value
        .copied()
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use crate::model::{GridLimits, TerminalGrid};

    #[test]
    fn csi_cursor_position_moves_print_location() {
        let mut grid = TerminalGrid::new(10, 3, GridLimits::default()).unwrap();
        grid.process(b"\x1b[2;3HX");
        let rows = grid.viewport_rows();
        assert_eq!(rows[1].cells()[2].text(), "X");
    }

    #[test]
    fn erase_line_clears_content() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        grid.process(b"abcdef\r\x1b[K");
        let rows = grid.viewport_rows();
        assert!(rows[0].cells().is_empty());
    }
}
