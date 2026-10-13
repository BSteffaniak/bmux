//! Optional neutral prepared-TUI to browser publication bridge.
use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use bmux_tui::presentation::PreparedPresentation;
use bmux_web_renderer::BrowserFrame;

use crate::{BinaryPresentation, Resource, SnapshotPublisher, ViewControl};

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod tests;

/// Publishes complete browser representations, retaining only the last accepted frame.
/// Caller metadata lives in `BrowserFrame`; host authentication and sizing are separate.
/// Use one publisher per feed: transport revisions are local, monotonic commit identities.
pub struct BrowserPublisher {
    feed: SnapshotPublisher,
    control: ViewControl,
    revision: u64,
    previous: Option<BrowserFrame>,
}

impl BrowserPublisher {
    #[must_use]
    pub fn new(feed: SnapshotPublisher, control: ViewControl) -> Self {
        Self {
            feed,
            control,
            revision: 0,
            previous: None,
        }
    }

    /// Projects a prepared frame with explicit caller-owned source identity.
    /// # Errors
    /// Rejects unsupported or over-budget output without advancing committed authority.
    pub fn publish_prepared(
        &mut self,
        generation: &str,
        source_revision: u64,
        prepared: &PreparedPresentation,
    ) -> Result<u64> {
        let frame =
            BrowserFrame::from_prepared_in_generation(generation, source_revision, prepared)
                .context("browser prepared projection rejected");
        match frame {
            Ok(frame) => self.publish_frame(frame),
            Err(error) => {
                self.report_error(Some(&error));
                Err(error)
            }
        }
    }

    /// Commits full bytes, optional exact-predecessor delta and all resources together.
    /// Diagnostic JSON and delta encoding are optional and cannot veto a full frame.
    /// # Errors
    /// Rejection preserves the last complete output and reports bounded stage/details.
    /// A subsequent successful publication clears the status and repairs delivery.
    pub fn publish_frame(&mut self, frame: BrowserFrame) -> Result<u64> {
        let result = self.commit_frame(frame);
        self.report_error(result.as_ref().err());
        result
    }

    fn report_error(&self, error: Option<&anyhow::Error>) {
        // Both construction and retained status are bounded; no frame data is retained here.
        // Renderer/host errors describe rejection stages, budget counts and contract failures.
        struct BoundedStatus(String);
        impl std::fmt::Write for BoundedStatus {
            fn write_str(&mut self, value: &str) -> std::fmt::Result {
                let remaining = 1024_usize.saturating_sub(self.0.len());
                let end = value
                    .char_indices()
                    .map(|(index, _)| index)
                    .chain(std::iter::once(value.len()))
                    .take_while(|index| *index <= remaining)
                    .last()
                    .unwrap_or(0);
                self.0.push_str(&value[..end]);
                Ok(())
            }
        }
        let status = error.map(|error| {
            let mut status = BoundedStatus(String::new());
            let _ = std::fmt::write(&mut status, format_args!("{error:#}"));
            status.0
        });
        self.control.set_presentation_status(status.as_deref());
    }

    fn commit_frame(&mut self, frame: BrowserFrame) -> Result<u64> {
        anyhow::ensure!(
            !self.previous.as_ref().is_some_and(|previous| {
                previous.generation() == frame.generation()
                    && frame.revision() <= previous.revision()
            }),
            "browser source revision rejected: revision must advance within its generation"
        );
        let next_revision = self
            .revision
            .checked_add(1)
            .context("browser transport revision exhausted")?;
        let binary = bmux_web_renderer::wire::encode_binary_with_resources(&frame)
            .context("browser full encoding rejected")?;
        let delta = self
            .previous
            .as_ref()
            .filter(|base| {
                base.generation() == frame.generation()
                    && base.buffer().area() == frame.buffer().area()
            })
            .and_then(|base| {
                bmux_web_renderer::wire::encode_delta_with_resources(&frame, base).ok()
            })
            .filter(|bytes| bytes.len() < binary.len())
            .map(|bytes| (self.revision, Arc::<[u8]>::from(bytes)));
        let resources = bmux_web_renderer::wire::extract_resources(&frame)
            .context("browser resource extraction rejected")?
            .into_iter()
            .map(|resource| {
                let (bytes, mime_type) = match resource.payload {
                    bmux_tui::image::ImagePayload::Png { bytes, .. } => (bytes, "image/png"),
                    bmux_tui::image::ImagePayload::Pixels { bytes, .. } => {
                        (bytes, "application/octet-stream")
                    }
                };
                (
                    resource.id,
                    Resource {
                        bytes: bytes.into(),
                        mime_type: mime_type.to_owned(),
                    },
                )
            })
            .collect();
        let json = bmux_web_renderer::encode_json(&frame)
            .ok()
            .map(|json| Arc::<[u8]>::from(json.into_bytes()))
            .filter(|json| !json.is_empty() && json.len() <= self.feed.max_bytes);
        self.publish_encoded(
            frame.generation(),
            next_revision,
            json,
            BinaryPresentation {
                snapshot: binary.into(),
                delta,
            },
            resources,
        )?;
        self.revision = next_revision;
        self.previous = Some(frame);
        Ok(next_revision)
    }

    fn publish_encoded(
        &self,
        generation: &str,
        revision: u64,
        json: Option<Arc<[u8]>>,
        mut binary: BinaryPresentation,
        resources: BTreeMap<String, Resource>,
    ) -> Result<()> {
        self.control
            .publish_generation(generation, || {
                if binary.delta.is_some() {
                    if self
                        .feed
                        .publish_representations(
                            revision,
                            json.clone(),
                            binary.clone(),
                            resources.clone(),
                        )
                        .is_ok()
                    {
                        return Ok(());
                    }
                    // Admission of the optional delta must not veto the full representation.
                    binary.delta = None;
                }
                self.feed
                    .publish_representations(revision, json, binary, resources)
            })
            .context("browser atomic publication rejected (generation/full/resources)")
    }
}
