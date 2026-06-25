//! Low-level wire helpers shared by codec adapters.

/// A simple byte sink for wire encoders.
#[derive(Debug, Default)]
pub struct WireWriter {
    output: Vec<u8>,
}

impl WireWriter {
    /// Create an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one byte.
    pub fn push(&mut self, byte: u8) {
        self.output.push(byte);
    }

    /// Append bytes.
    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.output.extend_from_slice(bytes);
    }

    /// Consume the writer and return encoded bytes.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.output
    }
}
