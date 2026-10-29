//! Bounded, caller-polled diagnostics. Never records presentation contents.
use std::time::Instant;

const WINDOW: usize = 128;

// Keep aggregation and percentile selection exhaustive when adding a measurement.
macro_rules! measurements {
    ($($field:ident),+ $(,)?) => {
        /// CPU wall times in nanoseconds and resource counters for one presentation attempt.
        /// Text/image times include projection and upload enqueue work, not GPU execution.
        /// GPU wait measures device polling; validation measures subsequent error-scope
        /// resolution. Vertex byte counters are logical/requested sizes, not resident
        /// memory; allocation bytes sum requested capacities, including reallocations.
        /// Upload counts include initialization of the four-byte solid-color texel.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct FrameTelemetry { $(pub $field: u64,)+ }
        impl FrameTelemetry {
            fn add(&mut self, other: Self) {
                $(self.$field = self.$field.saturating_add(other.$field);)+
            }
            fn percentile(samples: &[Self], percentile: usize) -> Self {
                Self { $($field: percentile_value(samples, percentile, |s| s.$field),)+ }
            }
        }
    };
}
measurements!(
    text_cpu_ns,
    image_cpu_ns,
    gpu_wait_ns,
    gpu_validation_ns,
    total_ns,
    atlas_upload_count,
    atlas_upload_bytes,
    cpu_vertex_allocations,
    cpu_vertex_allocation_bytes,
    vertex_count,
    vertex_bytes,
    gpu_vertex_allocations,
    gpu_vertex_bytes,
    image_cache_hits,
    image_cache_misses,
    image_decode_count,
    glyph_rgba_cache_hits,
    glyph_rgba_cache_misses,
);

/// Lifetime saturating totals plus exact nearest-rank percentiles over the last
/// `window_samples` attempts (at most 128), including failures. No automatic logging.
/// Percentiles are selected independently per field; zero means no work in that stage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TelemetrySummary {
    pub attempts: u64,
    pub failures: u64,
    pub window_samples: usize,
    pub totals: FrameTelemetry,
    pub p50: FrameTelemetry,
    pub p95: FrameTelemetry,
    pub p99: FrameTelemetry,
}

fn percentile_value(
    samples: &[FrameTelemetry],
    percentile: usize,
    field: fn(&FrameTelemetry) -> u64,
) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut values = [0; WINDOW];
    for (value, sample) in values.iter_mut().zip(samples) {
        *value = field(sample);
    }
    let values = &mut values[..samples.len()];
    values.sort_unstable();
    values[(values.len() * percentile).div_ceil(100) - 1]
}

pub(super) struct Telemetry {
    samples: [FrameTelemetry; WINDOW],
    next: usize,
    summary: TelemetrySummary,
}
impl Default for Telemetry {
    fn default() -> Self {
        Self {
            samples: [FrameTelemetry::default(); WINDOW],
            next: 0,
            summary: TelemetrySummary::default(),
        }
    }
}
impl Telemetry {
    pub fn record(&mut self, sample: FrameTelemetry, failed: bool) {
        self.samples[self.next] = sample;
        self.next = (self.next + 1) % WINDOW;
        self.summary.attempts = self.summary.attempts.saturating_add(1);
        self.summary.failures = self.summary.failures.saturating_add(u64::from(failed));
        self.summary.window_samples = (self.summary.window_samples + 1).min(WINDOW);
        self.summary.totals.add(sample);
    }
    pub fn summary(&self) -> TelemetrySummary {
        let samples = &self.samples[..self.summary.window_samples];
        TelemetrySummary {
            p50: FrameTelemetry::percentile(samples, 50),
            p95: FrameTelemetry::percentile(samples, 95),
            p99: FrameTelemetry::percentile(samples, 99),
            ..self.summary
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Stage {
    Text,
    Image,
}

/// One optional clock per attempt/stage, never one per cell or glyph.
pub(super) struct Measurement {
    pub sample: FrameTelemetry,
    started: Option<Instant>,
    stage: Option<(Stage, Instant)>,
}
impl Measurement {
    pub fn new(enabled: bool) -> Self {
        Self {
            sample: FrameTelemetry::default(),
            started: enabled.then(Instant::now),
            stage: None,
        }
    }
    pub fn stage(&mut self, stage: Option<Stage>) {
        if let Some((previous, start)) = self.stage.take() {
            let elapsed = elapsed_ns(Some(start));
            match previous {
                Stage::Text => self.sample.text_cpu_ns = elapsed,
                Stage::Image => self.sample.image_cpu_ns = elapsed,
            }
        }
        if self.started.is_some() {
            self.stage = stage.map(|stage| (stage, Instant::now()));
        }
    }
    pub fn finish(mut self, gpu: FrameTelemetry) -> FrameTelemetry {
        self.stage(None);
        self.sample.add(gpu);
        self.sample.total_ns = elapsed_ns(self.started);
        self.sample
    }
}

pub(crate) fn elapsed_ns(start: Option<Instant>) -> u64 {
    start.map_or(0, |start| {
        u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_disabled_measurements_are_zero() {
        assert_eq!(Telemetry::default().summary(), TelemetrySummary::default());
        let mut measurement = Measurement::new(false);
        assert!(measurement.started.is_none());
        measurement.stage(Some(Stage::Text));
        assert!(measurement.stage.is_none());
        measurement.stage(Some(Stage::Image));
        assert!(measurement.stage.is_none());
        assert_eq!(
            measurement.finish(FrameTelemetry::default()),
            FrameTelemetry::default()
        );
    }

    #[test]
    fn window_is_bounded_but_totals_include_evicted_failures() {
        let mut telemetry = Telemetry::default();
        for total_ns in 1..=256 {
            telemetry.record(
                FrameTelemetry {
                    total_ns,
                    atlas_upload_bytes: 4,
                    ..FrameTelemetry::default()
                },
                total_ns % 2 == 0,
            );
        }
        let summary = telemetry.summary();
        assert_eq!(summary.attempts, 256);
        assert_eq!(summary.failures, 128);
        assert_eq!(summary.window_samples, WINDOW);
        assert_eq!(summary.totals.total_ns, 256 * 257 / 2);
        assert_eq!(summary.totals.atlas_upload_bytes, 1024);
        assert_eq!(summary.p50.total_ns, 192);
        assert_eq!(summary.p95.total_ns, 250);
        assert_eq!(summary.p99.total_ns, 255);
    }

    #[test]
    fn totals_saturate_and_single_sample_percentiles_are_exact() {
        let mut telemetry = Telemetry::default();
        let sample = FrameTelemetry {
            vertex_bytes: u64::MAX,
            ..FrameTelemetry::default()
        };
        telemetry.record(sample, true);
        assert_eq!(telemetry.summary().p99, sample);
        telemetry.record(sample, false);
        assert_eq!(telemetry.summary().totals.vertex_bytes, u64::MAX);
    }
}
