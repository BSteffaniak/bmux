//! Deterministic presentation policy, shared by the real executor only.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use unicode_segmentation::UnicodeSegmentation;

use crate::{PlaybackSpeed, managed::ManagedRun};

#[derive(Debug)]
pub(crate) struct UnresolvedInput;

impl std::fmt::Display for UnresolvedInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("paced input acknowledgement unresolved; refusing further input")
    }
}

impl std::error::Error for UnresolvedInput {}

pub(crate) const MAX_OVERHEAD: Duration = Duration::from_secs(3600);

pub(crate) fn delay(speed: PlaybackSpeed, typing: bool) -> Duration {
    let normal = if typing { 40 } else { 120 };
    let millis = match speed {
        PlaybackSpeed::Instant => 0,
        PlaybackSpeed::Half => normal * 2,
        PlaybackSpeed::Normal => normal,
        PlaybackSpeed::Double => normal / 2,
    };
    Duration::from_millis(millis)
}

/// Control-bearing payloads stay atomic, including ESC protocols, chords,
/// bracketed paste and unknown encodings. Printable UTF-8 plus one trailing
/// CR/LF is chunked into graphemes followed by an atomic Enter.
pub(crate) fn chunks(bytes: &[u8]) -> Vec<&[u8]> {
    let (text_bytes, enter) = match bytes.last() {
        Some(b'\r' | b'\n') => (&bytes[..bytes.len() - 1], Some(&bytes[bytes.len() - 1..])),
        _ => (bytes, None),
    };
    match std::str::from_utf8(text_bytes) {
        Ok(text) if !text.chars().any(char::is_control) => {
            let mut chunks: Vec<_> = text.graphemes(true).map(str::as_bytes).collect();
            if let Some(enter) = enter {
                chunks.push(enter);
            }
            chunks
        }
        _ => vec![bytes],
    }
}

pub(crate) async fn checkpoint(control: Option<&ManagedRun>) -> Result<()> {
    if let Some(control) = control {
        control.checkpoint().await?;
    }
    Ok(())
}

pub(crate) fn overhead(overhead: &AtomicU64) -> Duration {
    Duration::from_millis(overhead.load(Ordering::Relaxed)).min(MAX_OVERHEAD)
}

pub(crate) async fn wait(
    control: Option<&ManagedRun>,
    fallback: PlaybackSpeed,
    typing: bool,
    credit: &AtomicU64,
) -> Result<()> {
    let mut elapsed = Duration::ZERO;
    loop {
        checkpoint(control).await?;
        let speed = control.map_or(Ok(fallback), ManagedRun::speed)?;
        let remaining = delay(speed, typing).saturating_sub(elapsed);
        if remaining.is_zero() {
            return Ok(());
        }
        let slice = remaining.min(Duration::from_millis(25));
        // Credit only intentional pacing, before sleeping so a deadline landing
        // inside this slice cannot truncate typing. Pauses earn no credit.
        let millis = u64::try_from(slice.as_millis()).unwrap_or(25);
        let _ = credit.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            Some(value.saturating_add(millis).min(3_600_000))
        });
        tokio::time::sleep(slice).await;
        elapsed += slice;
    }
}

pub(crate) async fn deadline(base: Instant, credit: &AtomicU64) {
    loop {
        let remaining = (base + overhead(credit)).saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        tokio::time::sleep(remaining).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphemes_and_protocol_payloads() {
        assert_eq!(
            chunks("a👩‍💻e\u{301}".as_bytes()),
            vec!["a".as_bytes(), "👩‍💻".as_bytes(), "e\u{301}".as_bytes()]
        );
        for bytes in [
            b"\x1b[200~hello\x1b[201~".as_slice(),
            b"\x1b[A",
            b"\x03",
            b"text\r\n",
            &[0xff],
        ] {
            assert_eq!(chunks(bytes), vec![bytes]);
        }
    }

    #[test]
    fn real_demo_shell_lines_type_before_atomic_enter() {
        let (deck, _) = crate::parse_dsl::parse_dsl(include_str!(
            "../../../plugins/presenter-plugin/examples/live-demo.dsl"
        ))
        .unwrap();
        let mut lines = 0;
        for step in deck.steps {
            if let crate::types::Action::SendKeys { keys, .. } = step.action {
                let pieces = chunks(&keys);
                assert!(pieces.len() > 10, "shell command must visibly type");
                assert_eq!(pieces.last().copied(), Some(b"\r".as_slice()));
                assert_eq!(pieces.concat(), keys);
                assert!(!pieces[..pieces.len() - 1].concat().contains(&b'\r'));
                lines += 1;
            }
        }
        assert_eq!(lines, 2);
    }

    #[test]
    fn unicode_shell_prefix_and_final_enter() {
        let bytes = "printf '👩‍💻e\u{301}'\r".as_bytes();
        let pieces = chunks(bytes);
        assert!(pieces.contains(&"👩‍💻".as_bytes()));
        assert!(pieces.contains(&"e\u{301}".as_bytes()));
        assert_eq!(pieces.last().copied(), Some(b"\r".as_slice()));
        assert_eq!(pieces.concat(), bytes);
    }

    #[test]
    fn deterministic_speed_policy() {
        assert_eq!(delay(PlaybackSpeed::Instant, true), Duration::ZERO);
        assert_eq!(
            delay(PlaybackSpeed::Half, true),
            delay(PlaybackSpeed::Normal, true) * 2
        );
        assert_eq!(
            delay(PlaybackSpeed::Normal, false),
            delay(PlaybackSpeed::Double, false) * 2
        );
    }

    #[test]
    fn only_bounded_pacing_credit_extends_run_budget() {
        let credit = AtomicU64::new(u64::MAX);
        assert_eq!(overhead(&credit), MAX_OVERHEAD);
        let credit = AtomicU64::new(0);
        assert_eq!(overhead(&credit), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn standalone_instant_has_no_pacing_credit() {
        // Standalone instant never adds artificial latency.
        let credit = AtomicU64::new(0);
        wait(None, PlaybackSpeed::Instant, false, &credit)
            .await
            .unwrap();
        assert_eq!(overhead(&credit), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn virtual_clock_delay() {
        let start = tokio::time::Instant::now();
        let credit = AtomicU64::new(0);
        wait(None, PlaybackSpeed::Normal, true, &credit)
            .await
            .unwrap();
        assert_eq!(start.elapsed(), Duration::from_millis(40));
        assert_eq!(overhead(&credit), Duration::from_millis(40));
        wait(None, PlaybackSpeed::Instant, true, &credit)
            .await
            .unwrap();
        assert_eq!(start.elapsed(), Duration::from_millis(40));
    }
}
