//! Generated state contract backed by the same instance-owned operations as
//! native dispatch. No independent transport or runtime state lives here.
use std::future::Future;
use std::pin::Pin;

use bmux_recording_plugin_api::{recording_state::RecordingStateService, recording_types as types};
use uuid::Uuid;

use crate::RecordingOwner;

type Reply<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl RecordingStateService for RecordingOwner {
    fn status(&self) -> Reply<'_, types::RecordingStatus> {
        Box::pin(async move { self.recording_status_generated() })
    }
    fn list_recordings(&self) -> Reply<'_, Vec<types::RecordingSummary>> {
        Box::pin(async move { self.recording_list_generated() })
    }
    fn rolling_status(&self) -> Reply<'_, types::RecordingRollingStatus> {
        Box::pin(async move { self.recording_rolling_status_generated() })
    }
    fn capture_targets(&self) -> Reply<'_, Vec<types::RecordingCaptureTarget>> {
        Box::pin(async move { self.recording_capture_targets_generated() })
    }
    fn job_status(&self, job_id: Uuid) -> Reply<'_, Option<types::RecordingJob>> {
        Box::pin(async move { self.recording_job_status_generated(job_id) })
    }
    fn list_jobs(&self) -> Reply<'_, Vec<types::RecordingJob>> {
        Box::pin(async move { self.recording_list_jobs_generated() })
    }
}
