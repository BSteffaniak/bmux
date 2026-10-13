//! Generic viewport negotiation. No application resource or input semantics live here.
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

const LEASE: Duration = Duration::from_secs(30);
const MIN_INTERVAL: Duration = Duration::from_millis(50);

/// Cloneable generation control without serving capabilities or shutdown authority.
#[derive(Clone)]
pub struct ViewControl {
    viewport: Arc<Viewport>,
}

impl ViewControl {
    pub(crate) fn new(viewport: Arc<Viewport>) -> Self {
        Self { viewport }
    }

    /// Updates authoritative output identity after successful snapshot publication.
    ///
    /// # Errors
    /// Rejects invalid generations and closed hosts.
    pub fn set_generation(&self, generation: &str) -> io::Result<()> {
        self.viewport.set_generation(generation)
    }

    /// Report bounded content-free publication status; does not change output authority.
    pub fn set_presentation_status(&self, status: Option<&str>) {
        if let Ok(mut control) = self.viewport.control.lock() {
            control.presentation_status = status.map(|value| value.chars().take(1024).collect());
        }
    }

    /// Validates a queued request at the eventual authoritative sizing effect.
    /// Checks output generation, live controller ownership, latest ID and host lifetime.
    #[must_use]
    pub fn is_current(&self, request: &ViewportRequest) -> bool {
        !self.viewport.sender.is_closed()
            && self
                .viewport
                .control
                .lock()
                .is_ok_and(|control| control.valid(request))
    }

    /// Revokes controller ownership and pending requests on source loss.
    pub fn invalidate_view(&self) {
        self.viewport.invalidate();
    }
}

/// Observer is the default; resizable only enables transport, not product authorization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewportMode {
    #[default]
    Observer,
    Resizable,
}

/// Bounded geometry request. Generation identifies authoritative output, not the lease.
#[derive(Clone, PartialEq, Eq)]
pub struct ViewportRequest {
    pub generation: String,
    pub request_id: u64,
    pub cols: u16,
    pub rows: u16,
    controller: String,
}

impl std::fmt::Debug for ViewportRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewportRequest")
            .field("generation", &self.generation)
            .field("request_id", &self.request_id)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .finish_non_exhaustive()
    }
}

/// Single-consumer, latest-only requests. Call `is_current` at the side effect too.
pub struct ViewportRequests {
    receiver: watch::Receiver<Option<ViewportRequest>>,
    control: Arc<Mutex<Control>>,
}

impl ViewportRequests {
    /// Waits for the latest live request; closure discards retained stale output.
    pub async fn recv(&mut self) -> Option<ViewportRequest> {
        loop {
            if !self.control.lock().is_ok_and(|control| !control.closed) {
                return None;
            }
            self.receiver.changed().await.ok()?;
            if !self.control.lock().is_ok_and(|control| !control.closed) {
                return None;
            }
            let request = self.receiver.borrow_and_update().clone();
            if let Some(request) = request
                && self.is_current(&request)
            {
                return Some(request);
            }
        }
    }

    /// Ingress authority is not enough: callers must check immediately before resizing.
    #[must_use]
    pub fn is_current(&self, request: &ViewportRequest) -> bool {
        if self.receiver.has_changed().is_err() {
            return false;
        }
        self.control
            .lock()
            .is_ok_and(|control| control.valid(request))
    }

    /// Runs a short synchronous effect while holding lease/generation authority.
    /// Returns `None` for stale requests. The closure must not call back into this host.
    /// Async effects must revalidate at their own authoritative application point.
    pub fn with_current<R>(
        &self,
        request: &ViewportRequest,
        effect: impl FnOnce() -> R,
    ) -> Option<R> {
        if self.receiver.has_changed().is_err() {
            return None;
        }
        let control = self.control.lock().ok()?;
        if !control.valid(request) {
            return None;
        }
        Some(effect())
    }
}

impl Control {
    fn valid(&self, request: &ViewportRequest) -> bool {
        !self.closed
            && self.lease.as_ref().is_some_and(|lease| {
                lease.deadline > Instant::now()
                    && self.generation.as_deref() == Some(request.generation.as_str())
                    && lease.controller == request.controller
                    && lease.last_id == Some(request.request_id)
            })
    }
}

struct Lease {
    controller: String,
    deadline: Instant,
    last_id: Option<u64>,
    last_request: Option<Instant>,
}

pub(crate) struct Control {
    closed: bool,
    mode: ViewportMode,
    generation: Option<String>,
    presentation_status: Option<String>,
    lease: Option<Lease>,
}

pub(crate) struct Viewport {
    control: Arc<Mutex<Control>>,
    sender: watch::Sender<Option<ViewportRequest>>,
}

#[derive(Serialize)]
struct ViewInfo<'a> {
    protocol_version: u8,
    presentation_versions: [u8; 1],
    generation: Option<&'a str>,
    presentation_status: Option<&'a str>,
    viewport_allowed: bool,
    controller: Option<&'a str>,
    lease_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    generation: String,
    request_id: String,
    cols: u16,
    rows: u16,
}

fn secret() -> io::Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| io::Error::other("OS randomness unavailable"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl Viewport {
    pub(crate) fn new(mode: ViewportMode) -> (Self, ViewportRequests) {
        let control = Arc::new(Mutex::new(Control {
            closed: false,
            mode,
            generation: None,
            presentation_status: None,
            lease: None,
        }));
        let (sender, receiver) = watch::channel(None);
        (
            Self {
                control: Arc::clone(&control),
                sender,
            },
            ViewportRequests { receiver, control },
        )
    }

    pub(crate) fn set_generation(&self, generation: &str) -> io::Result<()> {
        if generation.is_empty()
            || generation.len() > 128
            || !generation.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid generation",
            ));
        }
        let mut control = self
            .control
            .lock()
            .map_err(|_| io::Error::other("control lock poisoned"))?;
        if control.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "view closed"));
        }
        if control.generation.as_deref() != Some(generation) {
            control.generation = Some(generation.to_owned());
            self.sender.send_replace(None);
        }
        Ok(())
    }

    pub(crate) fn close(&self) {
        if let Ok(mut control) = self.control.lock() {
            control.closed = true;
            control.generation = None;
            control.lease = None;
            self.sender.send_replace(None);
        }
    }

    pub(crate) fn invalidate(&self) {
        if let Ok(mut control) = self.control.lock() {
            control.generation = None;
            control.lease = None;
            self.sender.send_replace(None);
        }
    }

    pub(crate) fn claim(&self, controller: Option<&str>) -> Result<Vec<u8>, u16> {
        let mut control = self.control.lock().map_err(|_| 503_u16)?;
        let now = Instant::now();
        if control.closed {
            return Err(503);
        }
        let Some(generation) = control.generation.clone() else {
            return if control.presentation_status.is_some() {
                info(None, None, false, control.presentation_status.as_deref())
            } else {
                Err(503)
            };
        };
        if control
            .lease
            .as_ref()
            .is_some_and(|lease| lease.deadline <= now)
        {
            control.lease = None;
            self.sender.send_replace(None);
        }
        if control.mode == ViewportMode::Resizable && !self.sender.is_closed() {
            if let Some(lease) = &mut control.lease {
                if controller == Some(lease.controller.as_str()) {
                    lease.deadline = now + LEASE;
                }
            } else if controller.is_none() {
                control.lease = Some(Lease {
                    controller: secret().map_err(|_| 503_u16)?,
                    deadline: now + LEASE,
                    last_id: None,
                    last_request: None,
                });
                let lease = control.lease.as_ref().ok_or(503_u16)?;
                return info(
                    Some(lease),
                    Some(&generation),
                    true,
                    control.presentation_status.as_deref(),
                );
            }
        }
        if self.sender.is_closed() {
            control.lease = None;
        }
        let owner = control
            .lease
            .as_ref()
            .filter(|lease| controller == Some(lease.controller.as_str()));
        info(
            owner,
            Some(&generation),
            owner.is_some(),
            control.presentation_status.as_deref(),
        )
    }

    pub(crate) fn release(&self, controller: Option<&str>) -> Result<(), u16> {
        let mut control = self.control.lock().map_err(|_| 503_u16)?;
        let lease = control.lease.as_ref().ok_or(409_u16)?;
        if controller != Some(lease.controller.as_str()) {
            return Err(403);
        }
        control.lease = None;
        self.sender.send_replace(None);
        Ok(())
    }

    pub(crate) fn submit(&self, controller: Option<&str>, body: &[u8]) -> Result<(), u16> {
        let request: WireRequest = serde_json::from_slice(body).map_err(|_| 400_u16)?;
        if request.generation.is_empty()
            || request.generation.len() > 128
            || request.request_id.is_empty()
            || request.request_id.len() > 20
            || !request.request_id.bytes().all(|byte| byte.is_ascii_digit())
            || (request.request_id.len() > 1 && request.request_id.starts_with('0'))
            || request.cols == 0
            || request.rows == 0
            || u32::from(request.cols) * u32::from(request.rows) > 262_144
        {
            return Err(400);
        }
        let request_id = request.request_id.parse::<u64>().map_err(|_| 400_u16)?;
        let mut control = self.control.lock().map_err(|_| 503_u16)?;
        if control.mode != ViewportMode::Resizable {
            return Err(403);
        }
        if control.generation.as_deref() != Some(request.generation.as_str()) {
            return Err(409);
        }
        let lease = control.lease.as_mut().ok_or(409_u16)?;
        if controller != Some(lease.controller.as_str()) {
            return Err(403);
        }
        let now = Instant::now();
        if lease.deadline <= now || lease.last_id.is_some_and(|id| request_id <= id) {
            return Err(409);
        }
        if lease
            .last_request
            .is_some_and(|last| now.duration_since(last) < MIN_INTERVAL)
        {
            return Err(429);
        }
        self.sender
            .send(Some(ViewportRequest {
                generation: request.generation,
                request_id,
                cols: request.cols,
                rows: request.rows,
                controller: lease.controller.clone(),
            }))
            .map_err(|_| 503_u16)?;
        lease.last_id = Some(request_id);
        lease.last_request = Some(now);
        lease.deadline = now + LEASE;
        Ok(())
    }
}

fn info(
    lease: Option<&Lease>,
    generation: Option<&str>,
    allowed: bool,
    presentation_status: Option<&str>,
) -> Result<Vec<u8>, u16> {
    serde_json::to_vec(&ViewInfo {
        protocol_version: 1,
        presentation_versions: [1],
        generation,
        presentation_status,
        viewport_allowed: allowed,
        controller: lease.map(|lease| lease.controller.as_str()),
        lease_ms: if allowed { 30_000 } else { 0 },
    })
    .map_err(|_| 503)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(view: &Viewport) -> String {
        let response: serde_json::Value =
            serde_json::from_slice(&view.claim(None).unwrap()).unwrap();
        response["controller"].as_str().unwrap().to_owned()
    }

    #[test]
    fn publication_status_is_bounded_and_independent_of_output_generation() {
        let (viewport, _requests) = Viewport::new(ViewportMode::Resizable);
        let control = ViewControl::new(Arc::new(viewport));
        control.set_presentation_status(Some(&"x".repeat(2048)));
        let info: serde_json::Value =
            serde_json::from_slice(&control.viewport.claim(None).unwrap()).unwrap();
        assert!(info["generation"].is_null());
        assert!(!info["viewport_allowed"].as_bool().unwrap());
        assert_eq!(info["presentation_status"].as_str().unwrap().len(), 1024);
        control.set_generation("committed").unwrap();
        control.set_presentation_status(None);
        let info: serde_json::Value =
            serde_json::from_slice(&control.viewport.claim(None).unwrap()).unwrap();
        assert_eq!(info["generation"], "committed");
        assert!(info["presentation_status"].is_null());
    }

    fn body(id: u64, generation: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "generation": generation, "request_id": id.to_string(), "cols": 80, "rows": 24
        }))
        .unwrap()
    }

    fn unthrottle(view: &Viewport) {
        view.control
            .lock()
            .unwrap()
            .lease
            .as_mut()
            .unwrap()
            .last_request = None;
    }

    #[tokio::test]
    async fn ownership_coalescing_generation_and_transfer() {
        let (view, mut receiver) = Viewport::new(ViewportMode::Resizable);
        assert_eq!(view.claim(None), Err(503));
        view.set_generation("g1").unwrap();
        let controller = claim(&view);
        let extra: serde_json::Value = serde_json::from_slice(&view.claim(None).unwrap()).unwrap();
        assert_eq!(extra["viewport_allowed"], false);
        assert!(extra["controller"].is_null());
        assert_eq!(extra["generation"], "g1");
        assert_eq!(view.submit(None, &body(1, "g1")), Err(403));
        view.submit(Some(&controller), &body(1, "g1")).unwrap();
        assert_eq!(view.submit(Some(&controller), &body(1, "g1")), Err(409));
        assert_eq!(view.submit(Some(&controller), &body(2, "g1")), Err(429));
        unthrottle(&view);
        view.submit(Some(&controller), &body(2, "g1")).unwrap();
        let latest = receiver.recv().await.unwrap();
        assert_eq!(latest.request_id, 2);
        assert!(receiver.is_current(&latest));
        let renewed: serde_json::Value =
            serde_json::from_slice(&view.claim(Some(&controller)).unwrap()).unwrap();
        assert_eq!(renewed["controller"], controller);
        view.set_generation("g2").unwrap();
        assert!(!receiver.is_current(&latest));
        assert_eq!(view.submit(Some(&controller), &body(3, "g1")), Err(409));
        unthrottle(&view);
        view.submit(Some(&controller), &body(3, "g2")).unwrap();
        let old = receiver.recv().await.unwrap();
        view.release(Some(&controller)).unwrap();
        let new_controller = claim(&view);
        assert_ne!(controller, new_controller);
        assert!(!receiver.is_current(&old));
        assert_eq!(view.submit(Some(&controller), &body(4, "g2")), Err(403));
        view.submit(Some(&new_controller), &body(1, "g2")).unwrap();
        let current = receiver.recv().await.unwrap();
        assert!(receiver.is_current(&current));
    }

    #[tokio::test]
    async fn expiry_invalidation_disconnect_and_mode() {
        let (view, mut receiver) = Viewport::new(ViewportMode::Resizable);
        view.set_generation("g").unwrap();
        let controller = claim(&view);
        view.submit(Some(&controller), &body(1, "g")).unwrap();
        let queued = receiver.recv().await.unwrap();
        view.control
            .lock()
            .unwrap()
            .lease
            .as_mut()
            .unwrap()
            .deadline = Instant::now();
        assert!(!receiver.is_current(&queued));
        assert_eq!(view.submit(Some(&controller), &body(2, "g")), Err(409));
        assert_ne!(controller, claim(&view));
        view.invalidate();
        assert_eq!(view.claim(None), Err(503));
        view.set_generation("next").unwrap();
        view.close();
        assert!(receiver.recv().await.is_none());
        assert!(view.set_generation("next").is_err());
        let (observer, _receiver) = Viewport::new(ViewportMode::Observer);
        observer.set_generation("g").unwrap();
        let info: serde_json::Value =
            serde_json::from_slice(&observer.claim(None).unwrap()).unwrap();
        assert_eq!(info["viewport_allowed"], false);
        assert_eq!(observer.submit(None, &body(1, "g")), Err(403));
        let (view, receiver) = Viewport::new(ViewportMode::Resizable);
        view.set_generation("g").unwrap();
        let controller = claim(&view);
        drop(receiver);
        assert_eq!(view.submit(Some(&controller), &body(1, "g")), Err(503));
    }

    #[tokio::test]
    async fn cloned_control_validates_eventual_effect() {
        let (view, mut receiver) = Viewport::new(ViewportMode::Resizable);
        let control = ViewControl::new(Arc::new(view));
        let cloned = control.clone();
        control.set_generation("g").unwrap();
        let controller = claim(&control.viewport);
        control
            .viewport
            .submit(Some(&controller), &body(1, "g"))
            .unwrap();
        let queued = receiver.recv().await.unwrap();
        assert!(cloned.is_current(&queued));
        unthrottle(&control.viewport);
        control
            .viewport
            .submit(Some(&controller), &body(2, "g"))
            .unwrap();
        assert!(!cloned.is_current(&queued));
        let queued = receiver.recv().await.unwrap();
        assert!(cloned.is_current(&queued));
        control.set_generation("next").unwrap();
        assert!(!cloned.is_current(&queued));
        unthrottle(&control.viewport);
        control
            .viewport
            .submit(Some(&controller), &body(3, "next"))
            .unwrap();
        let queued = receiver.recv().await.unwrap();
        control.viewport.release(Some(&controller)).unwrap();
        let replacement = claim(&control.viewport);
        assert!(!cloned.is_current(&queued));
        control
            .viewport
            .submit(Some(&replacement), &body(1, "next"))
            .unwrap();
        let queued = receiver.recv().await.unwrap();
        control
            .viewport
            .control
            .lock()
            .unwrap()
            .lease
            .as_mut()
            .unwrap()
            .deadline = Instant::now();
        assert!(!cloned.is_current(&queued));
        let replacement = claim(&control.viewport);
        control
            .viewport
            .submit(Some(&replacement), &body(1, "next"))
            .unwrap();
        let queued = receiver.recv().await.unwrap();
        control.invalidate_view();
        assert!(!cloned.is_current(&queued));
        control.set_generation("next").unwrap();
        let replacement = claim(&control.viewport);
        control
            .viewport
            .submit(Some(&replacement), &body(1, "next"))
            .unwrap();
        let queued = receiver.recv().await.unwrap();
        drop(receiver);
        assert!(!cloned.is_current(&queued));
        control.viewport.close();
        assert!(!cloned.is_current(&queued));
    }

    #[test]
    fn strict_wire_bounds() {
        let (view, _receiver) = Viewport::new(ViewportMode::Resizable);
        view.set_generation("g").unwrap();
        let controller = claim(&view);
        for body in [
            r#"{"generation":"g","request_id":1,"cols":80,"rows":24}"#,
            r#"{"generation":"g","request_id":"01","cols":80,"rows":24}"#,
            r#"{"generation":"g","request_id":"18446744073709551616","cols":80,"rows":24}"#,
            r#"{"generation":"g","request_id":"1","cols":0,"rows":24}"#,
            r#"{"generation":"g","request_id":"1","cols":65535,"rows":65535}"#,
            r#"{"generation":"g","request_id":"1","cols":80,"rows":24,"input":"x"}"#,
            r#"{"generation":"g","request_id":"1","cols":80,"cols":81,"rows":24}"#,
        ] {
            assert_eq!(
                view.submit(Some(&controller), body.as_bytes()),
                Err(400),
                "{body}"
            );
        }
        for generation in ["", "\n", &"a".repeat(129)] {
            assert!(view.set_generation(generation).is_err());
        }
    }
}
