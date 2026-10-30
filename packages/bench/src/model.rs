//! Versioned, portable benchmark contracts. No terminal runtime or server is required.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;
pub const SOURCE_SHA256: &str = env!("BMUX_BENCH_SOURCE_SHA256");
pub const GIT_REVISION: &str = env!("BMUX_BENCH_GIT_REV");
pub type BenchResult<T> = Result<T, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    pub columns: u16,
    pub rows: u16,
}
impl Default for Geometry {
    fn default() -> Self {
        Self {
            columns: 80,
            rows: 24,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseSpec {
    pub id: String,
    pub revision: u32,
    pub category: String,
    pub title: String,
    pub description: String,
    pub diagnostic: String,
    pub suites: Vec<String>,
    pub requires_sync: bool,
}

#[derive(Debug, Clone)]
pub struct Workload {
    pub spec: CaseSpec,
    pub setup: Vec<u8>,
    pub chunks: Vec<Vec<u8>>,
    pub teardown: Vec<u8>,
    pub interval_ns: u64,
    pub logical_operations: u64,
    pub payload_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub profile: String,
    pub suites: Vec<String>,
    pub cases: Vec<String>,
    pub exclude: Vec<String>,
    pub geometry: Geometry,
    pub seed: u64,
    pub samples: usize,
    pub warmup: usize,
    pub max_case_ms: u64,
    pub max_bytes_per_sample: usize,
    pub probe_timeout_ms: u64,
    pub probe: bool,
    pub synchronized_updates: bool,
    pub practical_change_percent: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Complete,
    Partial,
    Cancelled,
    Failed,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub case_id: String,
    pub index: usize,
    pub metric: String,
    pub value: f64,
    pub unit: String,
    pub elapsed_ns: u64,
    pub bytes: u64,
    pub operations: u64,
    pub write_calls: u64,
    pub missed_deadlines: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseOutcome {
    pub spec: CaseSpec,
    pub payload_hash: String,
    pub status: Status,
    pub reason: Option<String>,
    pub samples: Vec<Sample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub schema_version: u32,
    pub run_id: String,
    pub harness_version: String,
    pub label: String,
    pub started_unix_ms: u64,
    pub status: Status,
    pub config: RunConfig,
    /// Allowlisted diagnostics only; never copy the process environment wholesale.
    pub environment: BTreeMap<String, String>,
    pub capabilities: BTreeMap<String, String>,
    pub case_order: Vec<String>,
    pub outcomes: Vec<CaseOutcome>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSummary {
    pub case_id: String,
    pub metric: String,
    pub unit: String,
    pub count: usize,
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub mad: f64,
    pub ci_low: f64,
    pub ci_high: f64,
    pub outliers: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSummary {
    pub schema_version: u32,
    pub method: String,
    pub metrics: Vec<MetricSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Faster,
    Slower,
    NoMeaningfulChange,
    Inconclusive,
    Incompatible,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comparison {
    pub case_id: String,
    pub metric: String,
    pub change: Change,
    pub percent_change: Option<f64>,
    pub ci_low: Option<f64>,
    pub ci_high: Option<f64>,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub enum Progress {
    CaseStarted {
        id: String,
        index: usize,
        total: usize,
    },
    Warmup {
        id: String,
    },
    Sampling {
        id: String,
    },
    CaseFinished {
        id: String,
        status: Status,
    },
}
