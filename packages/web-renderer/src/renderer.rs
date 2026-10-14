use crate::{BrowserFrame, DecodeError, Limits};

/// Explicit fidelity requirements, independent of any DOM or GPU API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredFeature {
    Cells,
    Cursor,
    Images,
    BlinkingText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities {
    pub cells: bool,
    pub cursor: bool,
    pub images: bool,
    pub blinking_text: bool,
}
impl BackendCapabilities {
    pub const COMPLETE: Self = Self {
        cells: true,
        cursor: true,
        images: true,
        blinking_text: true,
    };

    pub fn check(self, frame: &BrowserFrame) -> Result<(), RequiredFeature> {
        if !self.cells {
            return Err(RequiredFeature::Cells);
        }
        if !self.cursor && frame.cursor().is_some_and(|cursor| cursor.visible) {
            return Err(RequiredFeature::Cursor);
        }
        if !self.images && frame.image_scene().placements().len() != 0 {
            return Err(RequiredFeature::Images);
        }
        if !self.blinking_text
            && frame.buffer().cells().iter().any(|cell| {
                cell.style
                    .modifiers
                    .contains(bmux_tui::style::Modifier::SLOW_BLINK)
            })
        {
            return Err(RequiredFeature::BlinkingText);
        }
        Ok(())
    }
}

/// Replaceable output backend. Implementations accept a complete frame before
/// success; failure preserves visible output and committed interaction authority.
/// Full replacement releases omitted resources. Reset and Drop release all resources.
pub trait BrowserBackend {
    type Error;
    fn capabilities(&self) -> BackendCapabilities;
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
    Unsupported(RequiredFeature),
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
        self.present_snapshot(frame, false)
    }
    /// Explicit complete-snapshot repair, including replacement generations.
    /// Unlike reset followed by present, failure preserves the old output authority.
    pub fn repair(&mut self, frame: BrowserFrame) -> Result<(), PresentError<B::Error>> {
        self.present_snapshot(frame, true)
    }
    fn present_snapshot(
        &mut self,
        frame: BrowserFrame,
        repair: bool,
    ) -> Result<(), PresentError<B::Error>> {
        frame.validate(self.limits).map_err(PresentError::Invalid)?;
        self.backend
            .capabilities()
            .check(&frame)
            .map_err(PresentError::Unsupported)?;
        if !repair
            && self
                .committed
                .as_ref()
                .is_some_and(|previous| frame.generation() != previous.generation())
        {
            return Err(PresentError::GenerationChanged);
        }
        if self.committed.as_ref().is_some_and(|previous| {
            frame.generation() == previous.generation() && frame.revision() <= previous.revision()
        }) {
            return Err(PresentError::StaleRevision);
        }
        let previous = self
            .committed
            .as_ref()
            .filter(|previous| previous.generation() == frame.generation());
        self.backend
            .render(&frame, previous)
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
        reject_cells: bool,
        fail: bool,
        revisions: Vec<u64>,
        resets: usize,
    }
    impl BrowserBackend for Recording {
        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                cells: !self.reject_cells,
                ..BackendCapabilities::COMPLETE
            }
        }
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
    fn missing_feature_fails_before_output_or_commit() {
        let mut renderer = Renderer::new(Recording::default(), Limits::default());
        renderer.present(frame(1)).expect("initial output");
        renderer.backend_mut().reject_cells = true;
        assert!(matches!(
            renderer.present(frame(2)),
            Err(PresentError::Unsupported(RequiredFeature::Cells))
        ));
        assert_eq!(renderer.backend().revisions, vec![1]);
        assert_eq!(renderer.committed().expect("retained").revision(), 1);
    }
    #[test]
    fn generation_repair_commits_only_successful_output() {
        let mut renderer = Renderer::new(Recording::default(), Limits::default());
        renderer.present(frame(4)).expect("initial");
        let prepared = PresentationState::new(Rect::new(0, 0, 4, 2))
            .prepare(Damage::Full, |_| {})
            .expect("prepared");
        let replacement =
            BrowserFrame::from_prepared_in_generation("replacement", 1, &prepared).expect("frame");
        assert!(matches!(
            renderer.present(replacement.clone()),
            Err(PresentError::GenerationChanged)
        ));
        renderer.backend_mut().fail = true;
        assert!(renderer.repair(replacement.clone()).is_err());
        assert_eq!(
            renderer.committed().expect("retained").generation(),
            "standalone"
        );
        assert_eq!(renderer.committed().expect("retained").revision(), 4);
        renderer.backend_mut().fail = false;
        renderer.repair(replacement).expect("repair");
        assert_eq!(
            renderer.committed().expect("replacement").generation(),
            "replacement"
        );
        assert_eq!(renderer.committed().expect("replacement").revision(), 1);
    }
    #[test]
    fn required_cursor_and_blink_features_are_explicit() {
        let cursor = BackendCapabilities {
            cursor: false,
            ..BackendCapabilities::COMPLETE
        };
        assert_eq!(cursor.check(&frame(1)), Err(RequiredFeature::Cursor));
        let prepared = PresentationState::new(Rect::new(0, 0, 4, 2))
            .prepare(Damage::Full, |cx| {
                cx.fill(
                    LocalRect::new(0, 0, 4, 2),
                    "x",
                    Style::new().add_modifier(bmux_tui::style::Modifier::SLOW_BLINK),
                );
            })
            .expect("prepared");
        let blinking = BrowserFrame::from_prepared(1, &prepared).expect("frame");
        let static_text = BackendCapabilities {
            blinking_text: false,
            ..BackendCapabilities::COMPLETE
        };
        assert_eq!(
            static_text.check(&blinking),
            Err(RequiredFeature::BlinkingText)
        );
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
        let mut versioned: serde_json::Value = serde_json::from_str(&json).expect("JSON");
        for version in [crate::WIRE_VERSION + 1, 2] {
            versioned["version"] = version.into();
            assert_eq!(
                crate::decode_json(&versioned.to_string(), Limits::default()),
                Err(DecodeError::Version)
            );
        }
        let mut value: serde_json::Value = serde_json::from_str(&json).expect("JSON");
        value["buffer"]["cells"][0]["width"] = 2.into();
        assert!(crate::decode_json(&value.to_string(), Limits::default()).is_err());
        value["buffer"]["cells"][0]["width"] = 1.into();
        value["buffer"]["cells"][0]["symbol"] = "\u{1b}".into();
        assert!(crate::decode_json(&value.to_string(), Limits::default()).is_err());
    }
}
