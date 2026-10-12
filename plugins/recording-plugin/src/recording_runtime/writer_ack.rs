//! Retained, bounded acknowledgments from the original recording writer.
//! These are live synchronization only; recovery validates canonical disk evidence.
use anyhow::{Result, bail, ensure};
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Clone, Debug, Default)]
pub(crate) struct WriterProgress {
    pub(crate) watermark: u64,
    pub(crate) ready: bool,
    pub(crate) closed: bool,
    pub(crate) error: Option<String>,
}

pub(crate) type WriterAck = watch::Receiver<WriterProgress>;
pub(super) type WriterPublisher = watch::Sender<WriterProgress>;

pub(super) fn channel() -> (WriterPublisher, WriterAck) {
    watch::channel(WriterProgress::default())
}

pub(super) fn fail(publisher: &WriterPublisher, reason: &str) {
    publisher.send_modify(|state| {
        if state.error.is_none() {
            // A single bounded failure is sufficient; this is not a diagnostic log.
            state.error = Some(reason.chars().take(1024).collect());
        }
    });
}

/// The retained initial state and watch version eliminate subscribe/check races.
/// One absolute deadline bounds the whole wait, including unrelated progress.
pub(crate) async fn wait_for_watermark(
    mut acknowledgment: WriterAck,
    watermark: u64,
    deadline: Instant,
) -> Result<()> {
    loop {
        {
            let state = acknowledgment.borrow_and_update();
            if let Some(error) = &state.error {
                bail!("recording writer failed: {error}");
            }
            if state.ready && state.watermark >= watermark {
                return Ok(());
            }
            ensure!(
                !state.closed,
                "recording writer closed before durable output watermark"
            );
        }
        ensure!(
            Instant::now() < deadline,
            "capture writer has not reached drained output watermark"
        );
        tokio::time::timeout_at(deadline, acknowledgment.changed())
            .await
            .map_err(|_| {
                anyhow::anyhow!("capture writer has not reached drained output watermark")
            })?
            .map_err(|_| {
                anyhow::anyhow!("recording writer acknowledgment closed without completion")
            })?;
    }
}

/// Even panic/unexpected return publishes a terminal failure, not a healthy sink.
pub(super) struct WriterGuard {
    publisher: WriterPublisher,
    completed: bool,
}

impl WriterGuard {
    pub(super) const fn new(publisher: WriterPublisher) -> Self {
        Self {
            publisher,
            completed: false,
        }
    }

    pub(super) fn complete(&mut self, result: &Result<bmux_recording_protocol::RecordingSummary>) {
        if let Err(error) = result {
            fail(&self.publisher, &error.to_string());
        }
        self.publisher.send_modify(|state| state.closed = true);
        self.completed = true;
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        if !self.completed {
            fail(&self.publisher, "writer exited without completion");
            self.publisher.send_modify(|state| state.closed = true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn retained_progress_and_closed_success_are_visible_to_late_waiters() {
        let (publisher, acknowledgment) = channel();
        publisher.send_modify(|state| {
            state.ready = true;
            state.watermark = 7;
            state.closed = true;
        });
        wait_for_watermark(acknowledgment.clone(), 7, Instant::now())
            .await
            .unwrap();
        assert!(
            wait_for_watermark(acknowledgment, 8, Instant::now())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn progress_between_check_and_wait_is_not_lost() {
        let (publisher, acknowledgment) = channel();
        let deadline = Instant::now() + Duration::from_secs(1);
        let waiter = wait_for_watermark(acknowledgment, 4, deadline);
        let producer = async {
            tokio::task::yield_now().await;
            publisher.send_modify(|state| {
                state.ready = true;
                state.watermark = 4;
            });
        };
        let (result, ()) = tokio::join!(waiter, producer);
        result.unwrap();
    }

    #[tokio::test]
    async fn failure_overrides_watermark_and_missing_publisher_is_not_success() {
        let (publisher, acknowledgment) = channel();
        publisher.send_modify(|state| {
            state.ready = true;
            state.watermark = 9;
        });
        fail(&publisher, "failed commit");
        assert!(
            wait_for_watermark(acknowledgment, 9, Instant::now())
                .await
                .is_err()
        );
        let (publisher, acknowledgment) = channel();
        drop(publisher);
        assert!(
            wait_for_watermark(acknowledgment, 0, Instant::now())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unrelated_progress_does_not_reset_expired_deadline() {
        let (publisher, acknowledgment) = channel();
        let deadline = Instant::now();
        publisher.send_modify(|state| {
            state.ready = true;
            state.watermark = 1;
        });
        let error = wait_for_watermark(acknowledgment, 2, deadline)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not reached drained output watermark")
        );
    }

    #[tokio::test]
    async fn multiple_waiters_share_retained_progress() {
        let (publisher, acknowledgment) = channel();
        let deadline = Instant::now() + Duration::from_secs(1);
        let first = wait_for_watermark(acknowledgment.clone(), 3, deadline);
        let second = wait_for_watermark(acknowledgment, 5, deadline);
        let producer = async {
            tokio::task::yield_now().await;
            publisher.send_modify(|state| {
                state.ready = true;
                state.watermark = 5;
            });
        };
        let (first, second, ()) = tokio::join!(first, second, producer);
        first.unwrap();
        second.unwrap();
    }

    #[tokio::test]
    async fn deadline_and_cancellation_do_not_leave_background_waiters() {
        let (publisher, acknowledgment) = channel();
        assert!(
            wait_for_watermark(acknowledgment.clone(), 1, Instant::now())
                .await
                .is_err()
        );
        let task = tokio::spawn(wait_for_watermark(
            acknowledgment.clone(),
            1,
            Instant::now() + Duration::from_mins(1),
        ));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        publisher.send_modify(|state| {
            state.ready = true;
            state.watermark = 1;
        });
        wait_for_watermark(acknowledgment, 1, Instant::now())
            .await
            .unwrap();
    }
}
