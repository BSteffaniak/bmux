//! Neutral host pointer capability and exclusive OSC 22 output ownership.
//!
//! Transactions must be completed after the actual transport flush, not after
//! encoding into a memory buffer. Failed stack mutations are never replayed.

use std::io::{self, Write};

use crate::pointer_shape::{
    PointerShape, encode_pointer_shape_pop, encode_pointer_shape_push, encode_pointer_shape_set,
    encode_pointer_shape_tmux,
};

/// Proven OSC 22 support; setter support does not imply stack support.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PointerShapeCapability {
    #[default]
    Unsupported,
    Set,
    Stack,
}

impl PointerShapeCapability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::Set => "set",
            Self::Stack => "stack",
        }
    }

    /// Interpret explicit host evidence without I/O or environment mutation.
    #[must_use]
    pub fn from_environment(term: &str, program: &str, override_value: Option<&str>) -> Self {
        if let Some(value) = override_value {
            return match value {
                "stack" => Self::Stack,
                "set" => Self::Set,
                _ => Self::Unsupported,
            };
        }
        let term = term.to_ascii_lowercase();
        let program = program.to_ascii_lowercase();
        if term == "dumb" {
            return Self::Unsupported;
        }
        if matches!(program.as_str(), "kitty" | "ghostty" | "foot")
            || term.contains("kitty")
            || term.contains("ghostty")
            || term.starts_with("foot")
        {
            return Self::Stack;
        }
        if program == "xterm" {
            Self::Set
        } else {
            Self::Unsupported
        }
    }

    /// Conservative detection. Multiplexer passthrough is never enabled implicitly.
    #[must_use]
    pub fn detect() -> Self {
        if tmux_present() && !tmux_passthrough_enabled() {
            return Self::Unsupported;
        }
        Self::from_environment(
            &std::env::var("TERM").unwrap_or_default(),
            &std::env::var("TERM_PROGRAM").unwrap_or_default(),
            std::env::var("BMUX_PLAYBOOK_HEADLESS_POINTER_SHAPES")
                .ok()
                .as_deref(),
        )
    }
}

fn tmux_present() -> bool {
    std::env::var_os("TMUX").is_some_and(|value| !value.is_empty())
}

/// Whether the explicitly opted-in tmux output route is selected.
#[must_use]
pub fn tmux_passthrough_enabled() -> bool {
    tmux_present() && std::env::var("BMUX_POINTER_SHAPE_TMUX_PASSTHROUGH").as_deref() == Ok("1")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Lease {
    #[default]
    Idle,
    Set,
    Stack,
    Uncertain,
}

/// One exclusive output owner's pointer lease. Cloning is only for staged
/// encoding: acknowledge successful host delivery or invalidate on failure.
#[derive(Clone, Debug, Default)]
pub struct PointerOutput {
    lease: Lease,
    shape: Option<PointerShape>,
    tmux_passthrough: bool,
}

/// One encoded output attempt; complete it exactly once after transport flush.
#[derive(Debug)]
pub struct PointerOutputTransaction {
    bytes: Vec<u8>,
    lease: Lease,
    shape: Option<PointerShape>,
}

impl PointerOutputTransaction {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl PointerOutput {
    #[must_use]
    pub const fn new(tmux_passthrough: bool) -> Self {
        Self {
            lease: Lease::Idle,
            shape: None,
            tmux_passthrough,
        }
    }

    #[must_use]
    pub fn is_uncertain(&self) -> bool {
        self.lease == Lease::Uncertain
    }

    /// Stage a setter or the first stack push. No output is performed here.
    /// # Errors
    /// Rejects uncertain stack ownership and invalid passthrough encoding.
    pub fn begin_present(
        &mut self,
        capability: PointerShapeCapability,
        shape: PointerShape,
    ) -> io::Result<Option<PointerOutputTransaction>> {
        self.check_certain()?;
        if capability == PointerShapeCapability::Unsupported || self.shape == Some(shape) {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        let lease = if self.lease == Lease::Idle && capability == PointerShapeCapability::Stack {
            encode_pointer_shape_push(shape, &mut bytes);
            Lease::Stack
        } else {
            encode_pointer_shape_set(Some(shape), &mut bytes);
            if self.lease == Lease::Idle {
                Lease::Set
            } else {
                self.lease
            }
        };
        let bytes = self.wrap(bytes)?;
        self.shape = None;
        self.lease = if lease == Lease::Stack && self.lease == Lease::Idle {
            Lease::Uncertain
        } else {
            lease
        };
        Ok(Some(PointerOutputTransaction {
            bytes,
            lease,
            shape: Some(shape),
        }))
    }

    /// Stage release of only this owner's lease.
    /// # Errors
    /// Rejects uncertain stack ownership; never guesses whether a pop is safe.
    pub fn begin_restore(&mut self) -> io::Result<Option<PointerOutputTransaction>> {
        self.check_certain()?;
        let mut bytes = Vec::new();
        match self.lease {
            Lease::Idle => return Ok(None),
            Lease::Stack => encode_pointer_shape_pop(&mut bytes),
            Lease::Set => encode_pointer_shape_set(None, &mut bytes),
            Lease::Uncertain => unreachable!(),
        }
        let bytes = self.wrap(bytes)?;
        if self.lease == Lease::Stack {
            self.lease = Lease::Uncertain;
        }
        self.shape = None;
        Ok(Some(PointerOutputTransaction {
            bytes,
            lease: Lease::Idle,
            shape: None,
        }))
    }

    /// Complete an attempt only after actual output and flush. Failure leaves
    /// non-idempotent operations uncertain and idempotent setters retryable.
    pub fn complete(&mut self, transaction: PointerOutputTransaction, succeeded: bool) {
        if succeeded {
            self.lease = transaction.lease;
            self.shape = transaction.shape;
        }
    }

    /// Reconcile failed host delivery of an already encoded candidate. An
    /// initial push or release may have reached the host; neither is replayable.
    pub fn invalidate_after_failed_delivery(&mut self, candidate: &Self) {
        if candidate.is_uncertain()
            || (self.lease != candidate.lease
                && (self.lease == Lease::Stack || candidate.lease == Lease::Stack))
        {
            self.lease = Lease::Uncertain;
        } else if self.lease == Lease::Idle && candidate.lease == Lease::Set {
            self.lease = Lease::Set;
        }
        self.shape = None;
    }

    /// Write and flush using this lease's retained route.
    /// # Errors
    /// Returns ownership, write, or flush failures.
    pub fn present<W: Write>(
        &mut self,
        writer: &mut W,
        capability: PointerShapeCapability,
        shape: PointerShape,
    ) -> io::Result<()> {
        if let Some(transaction) = self.begin_present(capability, shape)? {
            let result = writer
                .write_all(transaction.bytes())
                .and_then(|()| writer.flush());
            self.complete(transaction, result.is_ok());
            result?;
        }
        Ok(())
    }

    /// Restore using this lease's retained route.
    /// # Errors
    /// Returns uncertain ownership, write, or flush failures.
    pub fn restore<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if let Some(transaction) = self.begin_restore()? {
            let result = writer
                .write_all(transaction.bytes())
                .and_then(|()| writer.flush());
            self.complete(transaction, result.is_ok());
            result?;
        }
        Ok(())
    }

    fn check_certain(&self) -> io::Result<()> {
        if self.is_uncertain() {
            Err(io::Error::other("pointer stack ownership is uncertain"))
        } else {
            Ok(())
        }
    }

    fn wrap(&self, bytes: Vec<u8>) -> io::Result<Vec<u8>> {
        if !self.tmux_passthrough {
            return Ok(bytes);
        }
        let mut wrapped = Vec::new();
        encode_pointer_shape_tmux(&bytes, &mut wrapped)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        Ok(wrapped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_push_dedup_set_and_restore() {
        let mut owner = PointerOutput::default();
        let mut bytes = Vec::new();
        owner
            .present(
                &mut bytes,
                PointerShapeCapability::Stack,
                PointerShape::Text,
            )
            .unwrap();
        let pushed = bytes.clone();
        owner
            .present(
                &mut bytes,
                PointerShapeCapability::Stack,
                PointerShape::Text,
            )
            .unwrap();
        assert_eq!(bytes, pushed);
        owner
            .present(
                &mut bytes,
                PointerShapeCapability::Stack,
                PointerShape::Pointer,
            )
            .unwrap();
        owner.restore(&mut bytes).unwrap();
        let count = bytes.len();
        owner.restore(&mut bytes).unwrap();
        assert_eq!(count, bytes.len());
    }

    #[test]
    fn ambiguous_push_and_pop_fail_closed() {
        let mut owner = PointerOutput::default();
        let transaction = owner
            .begin_present(PointerShapeCapability::Stack, PointerShape::Text)
            .unwrap()
            .unwrap();
        owner.complete(transaction, false);
        assert!(owner.is_uncertain());
        assert!(owner.restore(&mut Vec::new()).is_err());
        let mut owner = PointerOutput::default();
        owner
            .present(
                &mut Vec::new(),
                PointerShapeCapability::Stack,
                PointerShape::Text,
            )
            .unwrap();
        let transaction = owner.begin_restore().unwrap().unwrap();
        owner.complete(transaction, false);
        assert!(owner.begin_restore().is_err());
    }

    #[test]
    fn failed_staged_push_is_not_replayed_but_setter_is() {
        let mut owner = PointerOutput::default();
        let mut candidate = owner.clone();
        candidate
            .present(
                &mut Vec::new(),
                PointerShapeCapability::Stack,
                PointerShape::Text,
            )
            .unwrap();
        owner.invalidate_after_failed_delivery(&candidate);
        assert!(owner.is_uncertain());
        let mut owner = PointerOutput::default();
        let transaction = owner
            .begin_present(PointerShapeCapability::Set, PointerShape::Text)
            .unwrap()
            .unwrap();
        owner.complete(transaction, false);
        assert!(
            owner
                .begin_present(PointerShapeCapability::Set, PointerShape::Text)
                .unwrap()
                .is_some()
        );
    }
}
