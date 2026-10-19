//! Async native session driver using the same neutral attach provider contract.
//!
//! The driver owns one provider session, validates continuity before exposing
//! state, and explicitly fences controls on disconnect. Hosts schedule it on
//! their executor and project through the retained compositor; no window or
//! terminal lifecycle, serialization, ANSI output, or runtime bootstrap lives here.

use anyhow::{Context, Result};
use bmux_client::{
    AttachControlValidator, AttachDetachOutcome, AttachProviderAck, AttachProviderAction,
    AttachProviderDisconnect, AttachProviderEvent, AttachProviderInput, AttachProviderViewport,
    AttachResumeState, AttachSession,
};

use crate::state::AttachState;

/// Host scheduling outcome; a disconnect is never silently reattached.
#[derive(Debug)]
pub enum DriverEvent {
    Changed,
    Disconnected(AttachProviderDisconnect),
    Detached,
}

/// One validated provider attachment, independent of its presentation backend.
pub struct NativeAttachDriver {
    session: Box<dyn AttachSession>,
    state: AttachState,
    controls: AttachControlValidator,
    connected: bool,
}

impl NativeAttachDriver {
    /// Obtain and validate initial parser repair before allowing controls.
    pub async fn open(mut session: Box<dyn AttachSession>) -> Result<Self> {
        let snapshot = session
            .initial_snapshot()
            .await
            .context("attach provider snapshot failed")?;
        let state = AttachState::from_snapshot(&snapshot)?;
        Ok(Self {
            session,
            state,
            controls: AttachControlValidator::default(),
            connected: true,
        })
    }

    #[must_use]
    pub fn state(&self) -> &AttachState {
        &self.state
    }

    /// Execution state only; mutation does not commit any interaction metadata.
    pub fn state_mut(&mut self) -> &mut AttachState {
        &mut self.state
    }

    #[must_use]
    pub fn resume_state(&self) -> AttachResumeState {
        self.state.resume_state()
    }

    /// Wait for one cancellation-safe provider event and validate its continuity.
    pub async fn next_event(&mut self) -> Result<DriverEvent> {
        anyhow::ensure!(self.connected, "attach is disconnected");
        let event = match self.session.next_event().await {
            Ok(event) => event,
            Err(error) => {
                self.connected = false;
                return Err(error.into());
            }
        };
        match event {
            AttachProviderEvent::Delta(delta) => {
                if let Err(error) = self.state.apply_delta(&delta) {
                    self.connected = false;
                    return Err(error);
                }
                Ok(DriverEvent::Changed)
            }
            AttachProviderEvent::Disconnected(mut disconnect) => {
                self.connected = false;
                let validated = self.state.resume_state();
                if let Some(resume) = &disconnect.resume {
                    anyhow::ensure!(
                        *resume == validated,
                        "provider disconnect resume differs from validated state"
                    );
                }
                disconnect.resume = disconnect.recoverable.then_some(validated);
                Ok(DriverEvent::Disconnected(disconnect))
            }
            AttachProviderEvent::Detached => {
                self.connected = false;
                let _ = self.controls.detach();
                Ok(DriverEvent::Detached)
            }
        }
    }

    /// Install an already authenticated/reconciled replacement session.
    ///
    /// The caller must reopen the provider with `resume_state()` through its
    /// normal authentication path. The replacement's complete snapshot is
    /// validated before installation; a failed repair leaves controls fenced.
    /// This does not restore old presentation authority (hosts must rebind it).
    pub async fn reconnect(&mut self, mut replacement: Box<dyn AttachSession>) -> Result<()> {
        anyhow::ensure!(!self.connected, "cannot replace a connected attach session");
        let snapshot = replacement.initial_snapshot().await?;
        let repaired = AttachState::from_snapshot(&snapshot)?;
        let old_resume = self.state.resume_state();
        let new_resume = repaired.resume_state();
        anyhow::ensure!(
            new_resume.view_revision >= old_resume.view_revision,
            "replacement attach view revision regressed"
        );
        self.session = replacement;
        self.state = repaired;
        self.controls = AttachControlValidator::default();
        self.connected = true;
        Ok(())
    }

    /// Inputs are provider controls, not terminal encoding. Native text/IME
    /// routing must use the input adapter and the last committed presentation.
    pub async fn send_input(&mut self, input: AttachProviderInput) -> Result<AttachProviderAck> {
        self.require_connected()?;
        self.controls
            .validate_input(&input, self.state.continuity())?;
        accepted(self.session.send_input(input).await?)
    }

    pub async fn update_viewport(
        &mut self,
        viewport: AttachProviderViewport,
    ) -> Result<AttachProviderAck> {
        self.require_connected()?;
        self.controls.validate_viewport(&viewport)?;
        accepted(self.session.update_viewport(viewport).await?)
    }

    /// Actions stay in the provider/plugin vocabulary; the driver does not
    /// create resources or implement a second domain action dispatcher.
    pub async fn execute_action(
        &mut self,
        action: AttachProviderAction,
    ) -> Result<AttachProviderAck> {
        self.require_connected()?;
        self.controls.validate_action(&action)?;
        accepted(self.session.execute_action(action).await?)
    }

    /// Explicit and idempotent. Dropping a driver never runs async cleanup.
    pub async fn detach(&mut self) -> Result<AttachDetachOutcome> {
        if self.controls.detach() == AttachDetachOutcome::AlreadyDetached {
            return Ok(AttachDetachOutcome::AlreadyDetached);
        }
        self.connected = false;
        Ok(self.session.detach().await?)
    }

    fn require_connected(&self) -> Result<()> {
        anyhow::ensure!(
            self.connected,
            "attach controls are fenced until authenticated reconnect"
        );
        Ok(())
    }
}

fn accepted(ack: AttachProviderAck) -> Result<AttachProviderAck> {
    anyhow::ensure!(
        ack.accepted,
        "attach control rejected: {}",
        ack.message.as_deref().unwrap_or("no reason supplied")
    );
    Ok(ack)
}
