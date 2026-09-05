use serde::{Deserialize, Serialize};

/// Character translation and repeat state retained across terminal updates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CharacterState {
    pub(crate) graphics: [bool; 2],
    pub(crate) active: usize,
    pub(crate) last: Option<char>,
}

impl CharacterState {
    pub(crate) fn translate(self, ch: char) -> char {
        if !self.graphics.get(self.active).copied().unwrap_or(false) {
            return ch;
        }
        match ch {
            '_' => ' ',
            '`' => '◆',
            'a' => '▒',
            'b' => '␉',
            'c' => '␌',
            'd' => '␍',
            'e' => '␊',
            'f' => '°',
            'g' => '±',
            'h' => '␤',
            'i' => '␋',
            'j' => '┘',
            'k' => '┐',
            'l' => '┌',
            'm' => '└',
            'n' => '┼',
            'o' => '⎺',
            'p' => '⎻',
            'q' => '─',
            'r' => '⎼',
            's' => '⎽',
            't' => '├',
            'u' => '┤',
            'v' => '┴',
            'w' => '┬',
            'x' => '│',
            'y' => '≤',
            'z' => '≥',
            '{' => 'π',
            '|' => '≠',
            '}' => '£',
            '~' => '·',
            _ => ch,
        }
    }
}
