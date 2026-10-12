use crate::{BrowserFrame, DecodeError, Limits};

/// Replaceable output backend. Implementations must accept the *complete* frame
/// before returning success. Failure must not mutate interaction authority.
/// Backend-owned resources are released by reset and by Drop.
pub trait BrowserBackend {
    type Error;
    fn render(
        &mut self,
        frame: &BrowserFrame,
        previous: Option<&BrowserFrame>,
    ) -> Result<(), Self::Error>;
    fn reset(&mut self) -> Result<(), Self::Error>;
}

#[derive(Debug)]
pub enum PresentError<E> {
    Invalid(DecodeError),
    StaleRevision,
    GenerationChanged,
    Backend(E),
}

/// Controller over output data only. It cannot mutate the source compositor.
pub struct Renderer<B> {
    backend: B,
    committed: Option<BrowserFrame>,
    limits: Limits,
}
impl<B: BrowserBackend> Renderer<B> {
    pub const fn new(backend: B, limits: Limits) -> Self {
        Self {
            backend,
            committed: None,
            limits,
        }
    }
    pub const fn committed(&self) -> Option<&BrowserFrame> {
        self.committed.as_ref()
    }
    pub const fn backend(&self) -> &B {
        &self.backend
    }
    /// Resource preparation only; changing this backend does not commit metadata.
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }
    pub fn present(&mut self, frame: BrowserFrame) -> Result<(), PresentError<B::Error>> {
        frame.validate(self.limits).map_err(PresentError::Invalid)?;
        if self
            .committed
            .as_ref()
            .is_some_and(|previous| frame.generation() != previous.generation())
        {
            return Err(PresentError::GenerationChanged);
        }
        if self
            .committed
            .as_ref()
            .is_some_and(|previous| frame.revision() <= previous.revision())
        {
            return Err(PresentError::StaleRevision);
        }
        self.backend
            .render(&frame, self.committed.as_ref())
            .map_err(PresentError::Backend)?;
        self.committed = Some(frame);
        Ok(())
    }
    /// A successful reset clears output, resources, and revision together.
    /// Failed reset retains the last committed interaction snapshot.
    pub fn reset(&mut self) -> Result<(), B::Error> {
        self.backend.reset()?;
        self.committed = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::{
        damage::Damage,
        geometry::{Point, Rect},
        paint::LocalRect,
        presentation::PresentationState,
        style::Style,
    };
    #[derive(Default)]
    struct Recording {
        fail: bool,
        revisions: Vec<u64>,
        resets: usize,
    }
    impl BrowserBackend for Recording {
        type Error = ();
        fn render(
            &mut self,
            frame: &BrowserFrame,
            previous: Option<&BrowserFrame>,
        ) -> Result<(), ()> {
            if self.fail {
                return Err(());
            }
            if let Some(previous) = previous {
                assert!(previous.revision() < frame.revision());
            }
            self.revisions.push(frame.revision());
            Ok(())
        }
        fn reset(&mut self) -> Result<(), ()> {
            if self.fail {
                return Err(());
            }
            self.resets += 1;
            Ok(())
        }
    }
    fn frame(revision: u64) -> BrowserFrame {
        let state = PresentationState::new(Rect::new(0, 0, 4, 2));
        let prepared = state
            .prepare(Damage::Full, |cx| {
                cx.fill(LocalRect::new(0, 0, 4, 2), "x", Style::new());
                cx.set_cursor(Point::new(1, 1), true);
            })
            .expect("prepared");
        BrowserFrame::from_prepared(revision, &prepared).expect("valid")
    }
    #[test]
    fn failed_output_and_reset_preserve_committed_metadata() {
        let mut renderer = Renderer::new(Recording::default(), Limits::default());
        renderer.present(frame(1)).expect("output");
        renderer.backend_mut().fail = true;
        assert!(renderer.present(frame(2)).is_err());
        assert_eq!(renderer.committed(), Some(&frame(1)));
        assert!(renderer.reset().is_err());
        assert_eq!(renderer.committed().expect("committed").revision(), 1);
        renderer.backend_mut().fail = false;
        renderer.present(frame(2)).expect("retry");
        assert!(matches!(
            renderer.present(frame(2)),
            Err(PresentError::StaleRevision)
        ));
        renderer.reset().expect("reset");
        assert!(renderer.committed().is_none());
        renderer.present(frame(1)).expect("new stream");
    }
    #[test]
    fn shared_json_roundtrip_and_bounds() {
        let frame = frame(1);
        let json = crate::encode_json(&frame).expect("encode");
        assert_eq!(
            crate::decode_json(&json, Limits::default()).expect("decode"),
            frame
        );
        let limits = Limits {
            json_bytes: 2,
            ..Limits::default()
        };
        assert!(matches!(
            crate::decode_json(&json, limits),
            Err(DecodeError::Limit(_))
        ));
        let unknown = json.replace("\"version\":1", "\"version\":42");
        assert_eq!(
            crate::decode_json(&unknown, Limits::default()),
            Err(DecodeError::Version)
        );
        let mut value: serde_json::Value = serde_json::from_str(&json).expect("JSON");
        value["buffer"]["cells"][0]["width"] = 2.into();
        assert!(crate::decode_json(&value.to_string(), Limits::default()).is_err());
        value["buffer"]["cells"][0]["width"] = 1.into();
        value["buffer"]["cells"][0]["symbol"] = "\u{1b}".into();
        assert!(crate::decode_json(&value.to_string(), Limits::default()).is_err());
    }
}
