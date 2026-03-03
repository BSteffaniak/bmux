pub(crate) const PREFIX_KEY: u8 = 0x01;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeAction {
    Quit,
    FocusNext,
    IncreaseSplit,
    DecreaseSplit,
    RestartFocusedPane,
    CloseFocusedPane,
    ShowHelp,
    ForwardToPane(Vec<u8>),
    Eof,
}

#[derive(Debug, Clone, Copy)]
enum InputMode {
    Normal,
    Prefix,
}

pub(crate) struct InputProcessor {
    mode: InputMode,
}

impl InputProcessor {
    pub(crate) fn new() -> Self {
        Self {
            mode: InputMode::Normal,
        }
    }

    pub(crate) fn process_chunk(&mut self, bytes: &[u8]) -> Vec<RuntimeAction> {
        let mut actions = Vec::new();
        let mut forwarded = Vec::with_capacity(bytes.len() + 1);

        for byte in bytes {
            match self.mode {
                InputMode::Normal => {
                    if *byte == PREFIX_KEY {
                        self.mode = InputMode::Prefix;
                    } else {
                        forwarded.push(*byte);
                    }
                }
                InputMode::Prefix => {
                    self.mode = InputMode::Normal;
                    match *byte {
                        b'q' | b'Q' => actions.push(RuntimeAction::Quit),
                        b'o' | b'O' => actions.push(RuntimeAction::FocusNext),
                        b'+' => actions.push(RuntimeAction::IncreaseSplit),
                        b'-' => actions.push(RuntimeAction::DecreaseSplit),
                        b'r' | b'R' => actions.push(RuntimeAction::RestartFocusedPane),
                        b'x' | b'X' => actions.push(RuntimeAction::CloseFocusedPane),
                        b'?' => actions.push(RuntimeAction::ShowHelp),
                        PREFIX_KEY => forwarded.push(PREFIX_KEY),
                        other => {
                            forwarded.push(PREFIX_KEY);
                            forwarded.push(other);
                        }
                    }
                }
            }
        }

        if !forwarded.is_empty() {
            actions.insert(0, RuntimeAction::ForwardToPane(forwarded));
        }

        actions
    }

    pub(crate) fn finish(&mut self) -> Option<RuntimeAction> {
        match self.mode {
            InputMode::Normal => None,
            InputMode::Prefix => {
                self.mode = InputMode::Normal;
                Some(RuntimeAction::ForwardToPane(vec![PREFIX_KEY]))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InputProcessor, PREFIX_KEY, RuntimeAction};

    #[test]
    fn maps_prefix_commands() {
        let mut processor = InputProcessor::new();
        let actions = processor.process_chunk(&[PREFIX_KEY, b'r']);
        assert_eq!(actions, vec![RuntimeAction::RestartFocusedPane]);
    }

    #[test]
    fn forwards_unknown_prefix_combo() {
        let mut processor = InputProcessor::new();
        let actions = processor.process_chunk(&[PREFIX_KEY, b'z']);
        assert_eq!(
            actions,
            vec![RuntimeAction::ForwardToPane(vec![PREFIX_KEY, b'z'])]
        );
    }

    #[test]
    fn flushes_pending_prefix_on_finish() {
        let mut processor = InputProcessor::new();
        let _ = processor.process_chunk(&[PREFIX_KEY]);
        assert_eq!(
            processor.finish(),
            Some(RuntimeAction::ForwardToPane(vec![PREFIX_KEY]))
        );
    }
}
