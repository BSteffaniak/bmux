use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_plugin_sdk::{
    NativeCommandContext, PluginCliCommandRequest, PluginCliCommandResponse, TypedDispatchClient,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum ClusterGatewayMode {
    #[default]
    Auto,
    Direct,
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum GatewayPolicyPreset {
    Balanced,
    Aggressive,
    Conservative,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct ClusterGatewayDefinition {
    targets: Vec<String>,
    hosts: Vec<ClusterGatewayHostRef>,
    gateway_mode: ClusterGatewayMode,
    gateway_candidates: Vec<String>,
    gateway_target: Option<String>,
    gateway_policy: Option<GatewayPolicyPreset>,
    breaker_open_after_failures: Option<u32>,
    breaker_half_open_after_ms: Option<u64>,
    breaker_half_open_required_successes: Option<u32>,
    probe_timeout_ms: Option<u64>,
    cooldown_ms: Option<u64>,
    cooldown_max_ms: Option<u64>,
    cooldown_jitter_pct: Option<u32>,
    success_ttl_ms: Option<u64>,
    history_max_entries: Option<usize>,
    history_retention_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ClusterGatewayHostRef {
    Target(String),
    Object {
        target: Option<String>,
        host: Option<String>,
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct ClusterGatewaySettings {
    clusters: BTreeMap<String, ClusterGatewayDefinition>,
}

#[derive(Debug, Clone, Default)]
struct ClusterGatewayRuntimeState {
    last_good: Option<GatewayLastGood>,
    cooldown_until: BTreeMap<String, Instant>,
    candidate_health: BTreeMap<String, GatewayCandidateHealth>,
    history: Vec<GatewayHistoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct PersistedClusterGatewayRuntimeState {
    version: u32,
    clusters: BTreeMap<String, PersistedClusterGatewayState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct PersistedClusterGatewayState {
    last_good: Option<PersistedGatewayLastGood>,
    cooldown_until_unix_ms: BTreeMap<String, u64>,
    candidate_health: BTreeMap<String, PersistedGatewayCandidateHealth>,
    history: Vec<PersistedGatewayHistoryEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GatewayBreakerState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct PersistedGatewayCandidateHealth {
    successes: u32,
    failures: u32,
    consecutive_failures: u32,
    last_latency_ms: Option<u64>,
    breaker_state: Option<GatewayBreakerState>,
    breaker_open_until_unix_ms: Option<u64>,
    adaptive_cooldown_level: u32,
    half_open_success_streak: u32,
    last_failure_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct PersistedGatewayHistoryEntry {
    observed_at_unix_ms: u64,
    command: String,
    candidate: Option<String>,
    execution_mode: Option<GatewayExecutionMode>,
    latency_ms: Option<u64>,
    result: String,
    #[serde(alias = "reason")]
    reason_code: Option<String>,
    selected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedGatewayLastGood {
    target: String,
    observed_at_unix_ms: u64,
}

#[derive(Debug, Clone)]
struct GatewayLastGood {
    target: String,
    observed_at: Instant,
}

#[derive(Debug, Clone)]
struct GatewayCandidateHealth {
    successes: u32,
    failures: u32,
    consecutive_failures: u32,
    last_latency_ms: Option<u64>,
    breaker_state: GatewayBreakerState,
    breaker_open_until: Option<Instant>,
    adaptive_cooldown_level: u32,
    half_open_success_streak: u32,
    last_failure_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct GatewayHistoryEntry {
    observed_at: Instant,
    command: String,
    candidate: Option<String>,
    execution_mode: GatewayExecutionMode,
    latency_ms: Option<u64>,
    result: String,
    reason_code: Option<String>,
    selected: bool,
}

#[derive(Debug, Clone)]
struct GatewayAttemptFailure {
    candidate: String,
    reason_code: &'static str,
    detail: String,
}

enum GatewayBatchOutcome {
    Success(u8),
    Exhausted { attempted: bool },
}

struct GatewayBatchRequest<'a> {
    caller: &'a NativeCommandContext,
    cluster_name: &'a str,
    definition: &'a ClusterGatewayDefinition,
    candidates: &'a [String],
    plugin_id: &'a str,
    command_name: &'a str,
    arguments: &'a [String],
    respect_cooldown: bool,
    no_failover: bool,
    execution_mode: GatewayExecutionMode,
}

struct GatewayDryRunRequest<'a> {
    caller: &'a NativeCommandContext,
    cluster_name: &'a str,
    definition: &'a ClusterGatewayDefinition,
    command_name: &'a str,
    candidates: &'a [String],
    output_format: GatewayOutputFormat,
    respect_cooldown: bool,
    no_failover: bool,
    why: bool,
}

struct GatewayExplainJsonPayloadInput<'a> {
    cluster_name: &'a str,
    definition: &'a ClusterGatewayDefinition,
    overrides: &'a GatewayCommandOverrides,
    probes: &'a [GatewayExplainCandidateProbe],
    preferred: Option<&'a String>,
    failures: &'a [GatewayAttemptFailure],
    selected_candidate: Option<&'a String>,
    command_name: Option<&'a str>,
    observational: bool,
    include_decision_summary: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct GatewayPolicyValues {
    breaker_open_after_failures: u32,
    breaker_half_open_after_ms: u64,
    breaker_half_open_required_successes: u32,
    probe_timeout_ms: u64,
    cooldown_ms: u64,
    cooldown_max_ms: u64,
    cooldown_jitter_pct: u32,
    success_ttl_ms: u64,
    history_max_entries: usize,
    history_retention_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum GatewayDoctorSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
struct GatewayDoctorFinding {
    severity: GatewayDoctorSeverity,
    candidate: Option<String>,
    reason_code: &'static str,
    detail: String,
    recommended_action: String,
    priority: u8,
    confidence: f32,
    next_command: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct GatewayDoctorSloSnapshot {
    #[serde(rename = "success_rate_5m")]
    success_rate: f64,
    #[serde(rename = "failover_rate_5m")]
    failover_rate: f64,
    #[serde(rename = "p95_probe_latency_ms_5m")]
    p95_probe_latency_ms: u64,
    #[serde(rename = "breaker_open_ratio_5m")]
    breaker_open_ratio: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GatewayExecutionMode {
    Mutating,
    Observational,
}

#[derive(Debug, Clone, Default)]
struct GatewayHistoryQuery {
    since: Option<Duration>,
    limit: Option<usize>,
    result: Option<String>,
    reason: Option<String>,
    candidate: Option<String>,
    command: Option<String>,
}

#[derive(Debug, Clone)]
struct GatewayHistoryRecordInput<'a> {
    command_name: &'a str,
    candidate: Option<&'a str>,
    execution_mode: GatewayExecutionMode,
    latency_ms: Option<u64>,
    result: &'a str,
    reason_code: Option<&'a str>,
    selected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GatewayHistoryClearScope {
    Cluster,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GatewayExportFormat {
    Json,
    Ndjson,
}

#[derive(Debug, Default)]
struct GatewayCommandOverrides {
    gateway_target: Option<String>,
    gateway_mode: Option<ClusterGatewayMode>,
    gateway_policy: Option<GatewayPolicyPreset>,
    no_failover: bool,
    dry_run: bool,
    why: bool,
    passthrough_arguments: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GatewayOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GatewayResetScope {
    Cluster(String),
    All,
}

#[derive(Debug, Clone)]
struct GatewayProbeResult {
    ok: bool,
    reason_code: &'static str,
    detail: String,
    latency_ms: u128,
}

#[derive(Debug, Clone)]
struct GatewayExplainCandidateProbe {
    candidate: String,
    cooldown_ms: Option<u128>,
    breaker_state: GatewayBreakerState,
    skip_reason: Option<&'static str>,
    stability_score: u64,
    last_latency_ms: Option<u64>,
    probe: GatewayProbeResult,
}

const CLUSTER_GATEWAY_STATE_SCHEMA_VERSION: u32 = 3;
const DEFAULT_CLUSTER_GATEWAY_LAST_GOOD_TTL: Duration = Duration::from_secs(90);
const DEFAULT_CLUSTER_GATEWAY_FAILURE_COOLDOWN: Duration = Duration::from_secs(20);
const DEFAULT_CLUSTER_GATEWAY_BREAKER_OPEN_AFTER_FAILURES: u32 = 3;
const DEFAULT_CLUSTER_GATEWAY_BREAKER_HALF_OPEN_AFTER: Duration = Duration::from_secs(30);
const DEFAULT_CLUSTER_GATEWAY_BREAKER_HALF_OPEN_REQUIRED_SUCCESSES: u32 = 2;
const DEFAULT_CLUSTER_GATEWAY_PROBE_TIMEOUT_MS: u64 = 7000;
const DEFAULT_CLUSTER_GATEWAY_COOLDOWN_MAX: Duration = Duration::from_secs(90);
const DEFAULT_CLUSTER_GATEWAY_COOLDOWN_JITTER_PCT: u32 = 10;
const DEFAULT_CLUSTER_GATEWAY_HISTORY_RETENTION_MS: u64 = 86_400_000;
const MAX_CLUSTER_GATEWAY_HISTORY_ENTRIES: usize = 200;
const GATEWAY_TABLE_CANDIDATE_WIDTH: usize = 24;

fn default_gateway_policy_values() -> GatewayPolicyValues {
    GatewayPolicyValues {
        breaker_open_after_failures: DEFAULT_CLUSTER_GATEWAY_BREAKER_OPEN_AFTER_FAILURES,
        breaker_half_open_after_ms: duration_millis_u64(
            DEFAULT_CLUSTER_GATEWAY_BREAKER_HALF_OPEN_AFTER,
        ),
        breaker_half_open_required_successes:
            DEFAULT_CLUSTER_GATEWAY_BREAKER_HALF_OPEN_REQUIRED_SUCCESSES,
        probe_timeout_ms: DEFAULT_CLUSTER_GATEWAY_PROBE_TIMEOUT_MS,
        cooldown_ms: duration_millis_u64(DEFAULT_CLUSTER_GATEWAY_FAILURE_COOLDOWN),
        cooldown_max_ms: duration_millis_u64(DEFAULT_CLUSTER_GATEWAY_COOLDOWN_MAX),
        cooldown_jitter_pct: DEFAULT_CLUSTER_GATEWAY_COOLDOWN_JITTER_PCT,
        success_ttl_ms: duration_millis_u64(DEFAULT_CLUSTER_GATEWAY_LAST_GOOD_TTL),
        history_max_entries: MAX_CLUSTER_GATEWAY_HISTORY_ENTRIES,
        history_retention_ms: DEFAULT_CLUSTER_GATEWAY_HISTORY_RETENTION_MS,
    }
}

fn gateway_policy_values_for_preset(preset: GatewayPolicyPreset) -> GatewayPolicyValues {
    match preset {
        GatewayPolicyPreset::Balanced => default_gateway_policy_values(),
        GatewayPolicyPreset::Aggressive => GatewayPolicyValues {
            breaker_open_after_failures: 2,
            breaker_half_open_after_ms: 15_000,
            breaker_half_open_required_successes: 1,
            probe_timeout_ms: 4_000,
            cooldown_ms: 8_000,
            cooldown_max_ms: 30_000,
            cooldown_jitter_pct: 5,
            success_ttl_ms: 45_000,
            history_max_entries: 300,
            history_retention_ms: 21_600_000,
        },
        GatewayPolicyPreset::Conservative => GatewayPolicyValues {
            breaker_open_after_failures: 4,
            breaker_half_open_after_ms: 45_000,
            breaker_half_open_required_successes: 3,
            probe_timeout_ms: 10_000,
            cooldown_ms: 30_000,
            cooldown_max_ms: 180_000,
            cooldown_jitter_pct: 15,
            success_ttl_ms: 180_000,
            history_max_entries: 500,
            history_retention_ms: 259_200_000,
        },
    }
}

fn gateway_effective_policy_values(definition: &ClusterGatewayDefinition) -> GatewayPolicyValues {
    let mut values = definition.gateway_policy.map_or_else(
        default_gateway_policy_values,
        gateway_policy_values_for_preset,
    );
    if let Some(value) = definition.breaker_open_after_failures {
        values.breaker_open_after_failures = value.max(1);
    }
    if let Some(value) = definition.breaker_half_open_after_ms {
        values.breaker_half_open_after_ms = value.max(1);
    }
    if let Some(value) = definition.breaker_half_open_required_successes {
        values.breaker_half_open_required_successes = value.max(1);
    }
    if let Some(value) = definition.probe_timeout_ms {
        values.probe_timeout_ms = value.max(1);
    }
    if let Some(value) = definition.cooldown_ms {
        values.cooldown_ms = value.max(1);
    }
    if let Some(value) = definition.cooldown_max_ms {
        values.cooldown_max_ms = value.max(values.cooldown_ms);
    }
    if let Some(value) = definition.cooldown_jitter_pct {
        values.cooldown_jitter_pct = value.min(100);
    }
    if let Some(value) = definition.success_ttl_ms {
        values.success_ttl_ms = value.max(1);
    }
    if let Some(value) = definition.history_max_entries {
        values.history_max_entries = value.max(1);
    }
    if let Some(value) = definition.history_retention_ms {
        values.history_retention_ms = value.max(1);
    }
    values
}

static CLUSTER_GATEWAY_RUNTIME_STATE: OnceLock<
    Mutex<BTreeMap<String, ClusterGatewayRuntimeState>>,
> = OnceLock::new();
static CLUSTER_GATEWAY_PATHS: OnceLock<ConfigPaths> = OnceLock::new();

impl Default for ClusterGatewayDefinition {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            hosts: Vec::new(),
            gateway_mode: ClusterGatewayMode::Auto,
            gateway_candidates: Vec::new(),
            gateway_target: None,
            gateway_policy: None,
            breaker_open_after_failures: None,
            breaker_half_open_after_ms: None,
            breaker_half_open_required_successes: None,
            probe_timeout_ms: None,
            cooldown_ms: None,
            cooldown_max_ms: None,
            cooldown_jitter_pct: None,
            success_ttl_ms: None,
            history_max_entries: None,
            history_retention_ms: None,
        }
    }
}

impl Default for GatewayCandidateHealth {
    fn default() -> Self {
        Self {
            successes: 0,
            failures: 0,
            consecutive_failures: 0,
            last_latency_ms: None,
            breaker_state: GatewayBreakerState::Closed,
            breaker_open_until: None,
            adaptive_cooldown_level: 0,
            half_open_success_streak: 0,
            last_failure_reason: None,
        }
    }
}

impl GatewayCandidateHealth {
    fn stability_score(&self) -> u64 {
        let samples = u64::from(self.successes) + u64::from(self.failures);
        let failure_rate_bps = (u64::from(self.failures) * 10_000)
            .checked_div(samples)
            .unwrap_or(5000);
        let breaker_penalty: u64 = match self.breaker_state {
            GatewayBreakerState::Closed => 0,
            GatewayBreakerState::HalfOpen => 80_000,
            GatewayBreakerState::Open => 200_000,
        };
        breaker_penalty
            .saturating_add(u64::from(self.consecutive_failures).saturating_mul(10_000))
            .saturating_add(failure_rate_bps)
    }
}

impl ClusterGatewayDefinition {
    fn declared_targets(&self) -> Vec<String> {
        let mut merged = Vec::new();
        for target in &self.targets {
            if !target.trim().is_empty() {
                merged.push(target.trim().to_string());
            }
        }
        for host in &self.hosts {
            if let Some(target) = cluster_gateway_target_from_host_ref(host) {
                merged.push(target);
            }
        }
        dedupe_preserve_order(merged)
    }
}

fn cluster_gateway_runtime_state_path(paths: &ConfigPaths) -> PathBuf {
    paths.runtime_dir.join("cluster-gateway-state.json")
}
fn save_cluster_gateway_runtime_state(
    paths: &ConfigPaths,
    state_map: &BTreeMap<String, ClusterGatewayRuntimeState>,
) -> Result<()> {
    std::fs::create_dir_all(&paths.runtime_dir).with_context(|| {
        format!(
            "failed creating runtime dir {}",
            paths.runtime_dir.display()
        )
    })?;
    let now_instant = Instant::now();
    let now_unix_ms = current_unix_timestamp_ms_u64();
    let clusters: BTreeMap<_, _> = state_map
        .iter()
        .filter_map(|(cluster_name, cluster_state)| {
            persist_gateway_cluster_state(cluster_name, cluster_state, now_instant, now_unix_ms)
        })
        .collect();
    let encoded = serde_json::to_string_pretty(&PersistedClusterGatewayRuntimeState {
        version: CLUSTER_GATEWAY_STATE_SCHEMA_VERSION,
        clusters,
    })
    .context("failed serializing cluster gateway runtime state")?;
    let path = cluster_gateway_runtime_state_path(paths);
    std::fs::write(&path, encoded).with_context(|| format!("failed writing {}", path.display()))
}

fn load_cluster_gateway_runtime_state(
    paths: &ConfigPaths,
) -> Result<BTreeMap<String, ClusterGatewayRuntimeState>> {
    let path = cluster_gateway_runtime_state_path(paths);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed reading {}", path.display()));
        }
    };

    let persisted = serde_json::from_str::<PersistedClusterGatewayRuntimeState>(&content)
        .with_context(|| {
            format!(
                "failed parsing cluster gateway runtime state {}",
                path.display()
            )
        })?;
    if persisted.version > CLUSTER_GATEWAY_STATE_SCHEMA_VERSION {
        tracing::warn!(
            event = "cluster_gateway_state_version_newer_than_runtime",
            file_version = persisted.version,
            supported_version = CLUSTER_GATEWAY_STATE_SCHEMA_VERSION,
            "cluster gateway state file version is newer than runtime schema"
        );
    }
    let now_instant = Instant::now();
    let now_unix_ms = current_unix_timestamp_ms_u64();
    Ok(persisted
        .clusters
        .into_iter()
        .filter_map(|(cluster_name, persisted_state)| {
            hydrate_gateway_cluster_state(cluster_name, persisted_state, now_instant, now_unix_ms)
        })
        .collect())
}

fn persist_gateway_cluster_state(
    cluster_name: &str,
    cluster_state: &ClusterGatewayRuntimeState,
    now_instant: Instant,
    now_unix_ms: u64,
) -> Option<(String, PersistedClusterGatewayState)> {
    let last_good = cluster_state
        .last_good
        .as_ref()
        .map(|last_good| PersistedGatewayLastGood {
            target: last_good.target.clone(),
            observed_at_unix_ms: now_unix_ms
                .saturating_sub(duration_millis_u64(last_good.observed_at.elapsed())),
        });
    let cooldown_until_unix_ms = cluster_state
        .cooldown_until
        .iter()
        .filter_map(|(candidate, until)| {
            if *until <= now_instant {
                return None;
            }
            let remaining_ms = duration_millis_u64(*until - now_instant);
            Some((candidate.clone(), now_unix_ms.saturating_add(remaining_ms)))
        })
        .collect::<BTreeMap<_, _>>();
    let candidate_health = cluster_state
        .candidate_health
        .iter()
        .filter_map(|(candidate, health)| {
            persist_gateway_candidate_health(candidate, health, now_instant, now_unix_ms)
        })
        .collect::<BTreeMap<_, _>>();
    let history = persist_gateway_history_entries(&cluster_state.history, now_unix_ms);
    if last_good.is_none()
        && cooldown_until_unix_ms.is_empty()
        && candidate_health.is_empty()
        && history.is_empty()
    {
        return None;
    }
    Some((
        cluster_name.to_string(),
        PersistedClusterGatewayState {
            last_good,
            cooldown_until_unix_ms,
            candidate_health,
            history,
        },
    ))
}

fn persist_gateway_candidate_health(
    candidate: &str,
    health: &GatewayCandidateHealth,
    now_instant: Instant,
    now_unix_ms: u64,
) -> Option<(String, PersistedGatewayCandidateHealth)> {
    let breaker_open_until_unix_ms = health.breaker_open_until.and_then(|until| {
        (until > now_instant).then(|| {
            let remaining_ms = duration_millis_u64(until - now_instant);
            now_unix_ms.saturating_add(remaining_ms)
        })
    });
    if health.successes == 0
        && health.failures == 0
        && health.consecutive_failures == 0
        && health.last_latency_ms.is_none()
        && health.adaptive_cooldown_level == 0
        && health.half_open_success_streak == 0
        && health.last_failure_reason.is_none()
        && breaker_open_until_unix_ms.is_none()
        && health.breaker_state == GatewayBreakerState::Closed
    {
        return None;
    }
    Some((
        candidate.to_string(),
        PersistedGatewayCandidateHealth {
            successes: health.successes,
            failures: health.failures,
            consecutive_failures: health.consecutive_failures,
            last_latency_ms: health.last_latency_ms,
            breaker_state: Some(health.breaker_state),
            breaker_open_until_unix_ms,
            adaptive_cooldown_level: health.adaptive_cooldown_level,
            half_open_success_streak: health.half_open_success_streak,
            last_failure_reason: health.last_failure_reason.clone(),
        },
    ))
}

fn persist_gateway_history_entries(
    history: &[GatewayHistoryEntry],
    now_unix_ms: u64,
) -> Vec<PersistedGatewayHistoryEntry> {
    history
        .iter()
        .skip(
            history
                .len()
                .saturating_sub(MAX_CLUSTER_GATEWAY_HISTORY_ENTRIES),
        )
        .map(|entry| PersistedGatewayHistoryEntry {
            observed_at_unix_ms: now_unix_ms
                .saturating_sub(duration_millis_u64(entry.observed_at.elapsed())),
            command: entry.command.clone(),
            candidate: entry.candidate.clone(),
            execution_mode: Some(entry.execution_mode),
            latency_ms: entry.latency_ms,
            result: entry.result.clone(),
            reason_code: entry.reason_code.clone(),
            selected: entry.selected,
        })
        .collect()
}

fn hydrate_gateway_cluster_state(
    cluster_name: String,
    persisted_state: PersistedClusterGatewayState,
    now_instant: Instant,
    now_unix_ms: u64,
) -> Option<(String, ClusterGatewayRuntimeState)> {
    let last_good = persisted_state.last_good.map(|last_good| GatewayLastGood {
        target: last_good.target,
        observed_at: instant_from_unix_ms(now_instant, now_unix_ms, last_good.observed_at_unix_ms),
    });
    let cooldown_until = persisted_state
        .cooldown_until_unix_ms
        .into_iter()
        .filter_map(|(candidate, until_unix_ms)| {
            (until_unix_ms > now_unix_ms).then(|| {
                let remaining_ms = until_unix_ms.saturating_sub(now_unix_ms);
                (candidate, now_instant + Duration::from_millis(remaining_ms))
            })
        })
        .collect::<BTreeMap<_, _>>();
    let candidate_health = persisted_state
        .candidate_health
        .into_iter()
        .map(|(candidate, health)| {
            (
                candidate,
                hydrate_gateway_candidate_health(&health, now_instant, now_unix_ms),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let history = persisted_state
        .history
        .into_iter()
        .map(|entry| GatewayHistoryEntry {
            observed_at: instant_from_unix_ms(now_instant, now_unix_ms, entry.observed_at_unix_ms),
            command: entry.command,
            candidate: entry.candidate,
            execution_mode: entry
                .execution_mode
                .unwrap_or(GatewayExecutionMode::Mutating),
            latency_ms: entry.latency_ms,
            result: entry.result,
            reason_code: entry.reason_code,
            selected: entry.selected,
        })
        .collect::<Vec<_>>();
    if last_good.is_none()
        && cooldown_until.is_empty()
        && candidate_health.is_empty()
        && history.is_empty()
    {
        return None;
    }
    Some((
        cluster_name,
        ClusterGatewayRuntimeState {
            last_good,
            cooldown_until,
            candidate_health,
            history,
        },
    ))
}

fn hydrate_gateway_candidate_health(
    persisted_health: &PersistedGatewayCandidateHealth,
    now_instant: Instant,
    now_unix_ms: u64,
) -> GatewayCandidateHealth {
    let breaker_open_until =
        persisted_health
            .breaker_open_until_unix_ms
            .and_then(|until_unix_ms| {
                (until_unix_ms > now_unix_ms).then(|| {
                    let remaining_ms = until_unix_ms.saturating_sub(now_unix_ms);
                    now_instant + Duration::from_millis(remaining_ms)
                })
            });
    GatewayCandidateHealth {
        successes: persisted_health.successes,
        failures: persisted_health.failures,
        consecutive_failures: persisted_health.consecutive_failures,
        last_latency_ms: persisted_health.last_latency_ms,
        breaker_state: persisted_health
            .breaker_state
            .unwrap_or(GatewayBreakerState::Closed),
        breaker_open_until,
        adaptive_cooldown_level: persisted_health.adaptive_cooldown_level,
        half_open_success_streak: persisted_health.half_open_success_streak,
        last_failure_reason: persisted_health.last_failure_reason.clone(),
    }
}

fn instant_from_unix_ms(now_instant: Instant, now_unix_ms: u64, target_unix_ms: u64) -> Instant {
    let age_ms = now_unix_ms.saturating_sub(target_unix_ms);
    now_instant
        .checked_sub(Duration::from_millis(age_ms))
        .unwrap_or(now_instant)
}

fn clear_cluster_gateway_runtime_state(paths: &ConfigPaths) -> Result<bool> {
    let path = cluster_gateway_runtime_state_path(paths);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed removing {}", path.display())),
    }
}
pub async fn run_command(
    caller: &NativeCommandContext,
    plugin_id: &str,
    command_name: &str,
    arguments: &[String],
) -> Result<Option<u8>> {
    if plugin_id != "bmux.cluster" {
        return Ok(None);
    }

    let overrides = parse_gateway_overrides(arguments)?;
    let paths = ConfigPaths::new(
        PathBuf::from(&caller.connection.config_dir),
        PathBuf::from(&caller.connection.runtime_dir),
        PathBuf::from(&caller.connection.data_dir),
        PathBuf::from(&caller.connection.state_dir),
    );
    let _ = CLUSTER_GATEWAY_PATHS.set(paths);
    let config_path = caller
        .connection
        .probe_config_file("bmux.toml")
        .unwrap_or_else(|| PathBuf::from(&caller.connection.config_dir).join("bmux.toml"));
    let config = BmuxConfig::load_from_path(&config_path)?;
    let settings = cluster_gateway_settings_from_context(caller, &config)?;

    if let Some(code) = maybe_run_cluster_gateway_special_command(
        caller,
        command_name,
        &config,
        &settings,
        &overrides,
    )
    .await?
    {
        return Ok(Some(code));
    }

    if overrides.gateway_mode == Some(ClusterGatewayMode::Direct) {
        return Ok(None);
    }

    run_cluster_gateway_routed_command(
        caller,
        plugin_id,
        command_name,
        &config,
        &settings,
        &overrides,
    )
    .await
}

async fn maybe_run_cluster_gateway_special_command(
    caller: &NativeCommandContext,
    command_name: &str,
    config: &BmuxConfig,
    settings: &ClusterGatewaySettings,
    overrides: &GatewayCommandOverrides,
) -> Result<Option<u8>> {
    if command_name == "cluster-gateway-reset" {
        return run_cluster_gateway_reset_command(settings, &overrides.passthrough_arguments)
            .map(Some);
    }
    if command_name == "cluster-gateway-history-clear" {
        return run_cluster_gateway_history_clear_command(
            settings,
            &overrides.passthrough_arguments,
        )
        .map(Some);
    }

    if !matches!(
        command_name,
        "cluster-gateway-status"
            | "cluster-gateway-explain"
            | "cluster-gateway-doctor"
            | "cluster-gateway-history"
            | "cluster-gateway-history-export"
            | "cluster-gateway-why"
    ) {
        return Ok(None);
    }

    let Some(cluster_name) =
        resolve_cluster_name_for_gateway(command_name, &overrides.passthrough_arguments, settings)?
    else {
        return Ok(None);
    };
    let base_definition = settings
        .clusters
        .get(cluster_name.as_str())
        .ok_or_else(|| anyhow::anyhow!("unknown cluster '{cluster_name}'"))?;
    let definition = apply_gateway_overrides(base_definition.clone(), overrides)?;
    if command_name == "cluster-gateway-status" {
        let output_format = parse_gateway_output_format(&overrides.passthrough_arguments)?;
        print_cluster_gateway_status(&cluster_name, &definition, overrides, output_format)?;
        return Ok(Some(0));
    }
    if command_name == "cluster-gateway-doctor" {
        let output_format = parse_gateway_output_format(&overrides.passthrough_arguments)?;
        return run_cluster_gateway_doctor(
            caller,
            config,
            &cluster_name,
            &definition,
            overrides,
            output_format,
        )
        .await
        .map(Some);
    }
    if command_name == "cluster-gateway-history" {
        let output_format = parse_gateway_output_format(&overrides.passthrough_arguments)?;
        return run_cluster_gateway_history_command(
            &cluster_name,
            &definition,
            &overrides.passthrough_arguments,
            output_format,
        )
        .map(Some);
    }
    if command_name == "cluster-gateway-history-export" {
        return run_cluster_gateway_history_export_command(
            &cluster_name,
            &definition,
            &overrides.passthrough_arguments,
        )
        .map(Some);
    }
    if command_name == "cluster-gateway-why" {
        let output_format = parse_gateway_output_format(&overrides.passthrough_arguments)?;
        return run_cluster_gateway_why_command(
            caller,
            config,
            &cluster_name,
            &definition,
            overrides,
            output_format,
        )
        .await
        .map(Some);
    }

    let output_format = parse_gateway_output_format(&overrides.passthrough_arguments)?;
    run_cluster_gateway_explain(
        caller,
        config,
        &cluster_name,
        &definition,
        overrides,
        output_format,
    )
    .await
}

async fn run_cluster_gateway_routed_command(
    caller: &NativeCommandContext,
    plugin_id: &str,
    command_name: &str,
    config: &BmuxConfig,
    settings: &ClusterGatewaySettings,
    overrides: &GatewayCommandOverrides,
) -> Result<Option<u8>> {
    let Some(cluster_name) =
        resolve_cluster_name_for_gateway(command_name, &overrides.passthrough_arguments, settings)?
    else {
        return Ok(None);
    };
    let base_definition = settings
        .clusters
        .get(cluster_name.as_str())
        .ok_or_else(|| anyhow::anyhow!("unknown cluster '{cluster_name}'"))?;
    let definition = apply_gateway_overrides(base_definition.clone(), overrides)?;

    if let Some(code) = maybe_run_gateway_routed_dry_run(
        caller,
        command_name,
        config,
        &cluster_name,
        &definition,
        overrides,
    )
    .await?
    {
        return Ok(Some(code));
    }

    if definition.gateway_mode == ClusterGatewayMode::Direct {
        return Ok(None);
    }

    let candidates = ordered_gateway_candidates_for_cluster(&cluster_name, &definition)?;

    tracing::info!(
        event = "cluster_gateway_selection_start",
        cluster = %cluster_name,
        mode = ?definition.gateway_mode,
        candidates = %candidates.join(","),
        command = %command_name,
        "selecting cluster gateway"
    );

    let mut failures = Vec::new();
    let attempted = match run_gateway_candidate_batch(
        GatewayBatchRequest {
            caller,
            cluster_name: &cluster_name,
            definition: &definition,
            candidates: &candidates,
            plugin_id,
            command_name,
            arguments: &overrides.passthrough_arguments,
            respect_cooldown: definition.gateway_mode == ClusterGatewayMode::Auto,
            no_failover: overrides.no_failover,
            execution_mode: GatewayExecutionMode::Mutating,
        },
        &mut failures,
    )
    .await?
    {
        GatewayBatchOutcome::Success(code) => return Ok(Some(code)),
        GatewayBatchOutcome::Exhausted { attempted } => attempted,
    };

    if !attempted && definition.gateway_mode == ClusterGatewayMode::Auto {
        tracing::warn!(
            event = "cluster_gateway_cooldown_override",
            cluster = %cluster_name,
            "all gateway candidates were in cooldown; retrying immediately"
        );
        if let GatewayBatchOutcome::Success(code) = run_gateway_candidate_batch(
            GatewayBatchRequest {
                caller,
                cluster_name: &cluster_name,
                definition: &definition,
                candidates: &candidates,
                plugin_id,
                command_name,
                arguments: &overrides.passthrough_arguments,
                respect_cooldown: false,
                no_failover: overrides.no_failover,
                execution_mode: GatewayExecutionMode::Mutating,
            },
            &mut failures,
        )
        .await?
        {
            return Ok(Some(code));
        }
    }

    emit_gateway_batch_failure_summary(&cluster_name, command_name, &failures);

    anyhow::bail!(
        "all gateway candidates failed for cluster '{cluster_name}': {}",
        format_gateway_failures(&failures)
    )
}

async fn maybe_run_gateway_routed_dry_run(
    caller: &NativeCommandContext,
    command_name: &str,
    _config: &BmuxConfig,
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    overrides: &GatewayCommandOverrides,
) -> Result<Option<u8>> {
    if !overrides.dry_run {
        return Ok(None);
    }
    let output_format = parse_gateway_output_format(&overrides.passthrough_arguments)?;
    if definition.gateway_mode == ClusterGatewayMode::Direct {
        print_direct_gateway_dry_run(
            command_name,
            cluster_name,
            definition,
            output_format,
            overrides.why,
        )?;
        return Ok(Some(0));
    }
    let candidates = ordered_gateway_candidates_for_cluster(cluster_name, definition)?;
    let code = run_cluster_gateway_dry_run(GatewayDryRunRequest {
        caller,
        cluster_name,
        definition,
        command_name,
        candidates: &candidates,
        output_format,
        respect_cooldown: definition.gateway_mode == ClusterGatewayMode::Auto,
        no_failover: overrides.no_failover,
        why: overrides.why,
    })
    .await?;
    Ok(Some(code))
}

fn print_direct_gateway_dry_run(
    command_name: &str,
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    output_format: GatewayOutputFormat,
    why: bool,
) -> Result<()> {
    let policy = gateway_effective_policy_values(definition);
    if output_format == GatewayOutputFormat::Json {
        let mut payload = serde_json::json!({
            "cluster": cluster_name,
            "command": command_name,
            "mode": gateway_mode_label(definition.gateway_mode),
            "policy": {
                "preset": definition.gateway_policy.map(gateway_policy_label),
                "breaker_open_after_failures": policy.breaker_open_after_failures,
                "breaker_half_open_after_ms": policy.breaker_half_open_after_ms,
                "breaker_half_open_required_successes": policy.breaker_half_open_required_successes,
                "probe_timeout_ms": policy.probe_timeout_ms,
                "cooldown_ms": policy.cooldown_ms,
                "cooldown_max_ms": policy.cooldown_max_ms,
                "cooldown_jitter_pct": policy.cooldown_jitter_pct,
                "success_ttl_ms": policy.success_ttl_ms,
                "history_max_entries": policy.history_max_entries,
                "history_retention_ms": policy.history_retention_ms,
            },
            "result": "success",
            "selected_candidate": serde_json::Value::Null,
            "failures": [],
            "probes": [],
            "would_mutate": {
                "enabled": false,
                "last_good": false,
                "cooldown": false,
                "breaker": false,
                "persistence_write": false,
            },
        });
        if why {
            payload["decision_summary"] = build_gateway_decision_summary(None, &[]);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed encoding direct dry-run json")?
        );
    } else {
        println!(
            "cluster gateway dry-run: mode=direct for cluster='{cluster_name}' command='{command_name}' (gateway bypass)"
        );
        println!(
            "policy: preset={} breaker_open_after_failures={} breaker_half_open_after_ms={} breaker_half_open_required_successes={} probe_timeout_ms={} cooldown_ms={} cooldown_max_ms={} cooldown_jitter_pct={} success_ttl_ms={} history_max_entries={} history_retention_ms={}",
            definition
                .gateway_policy
                .map_or("custom", gateway_policy_label),
            policy.breaker_open_after_failures,
            policy.breaker_half_open_after_ms,
            policy.breaker_half_open_required_successes,
            policy.probe_timeout_ms,
            policy.cooldown_ms,
            policy.cooldown_max_ms,
            policy.cooldown_jitter_pct,
            policy.success_ttl_ms,
            policy.history_max_entries,
            policy.history_retention_ms
        );
        println!(
            "would mutate: last_good=false cooldown=false breaker=false persistence_write=false"
        );
        if why {
            print_gateway_decision_summary_text(None, &[]);
        }
    }
    Ok(())
}

async fn run_cluster_gateway_dry_run(request: GatewayDryRunRequest<'_>) -> Result<u8> {
    let preferred = preferred_gateway_candidate(
        request.cluster_name,
        gateway_success_ttl(request.definition),
    );
    let mut probes = Vec::with_capacity(request.candidates.len());
    for candidate in request.candidates {
        let cooldown_ms = gateway_cooldown_remaining_ms(request.cluster_name, candidate);
        let health = gateway_effective_candidate_health(
            request.cluster_name,
            candidate,
            request.definition,
            GatewayExecutionMode::Observational,
        );
        let skip_reason = gateway_candidate_skip_reason(
            request.cluster_name,
            candidate,
            request.definition,
            request.respect_cooldown,
            GatewayExecutionMode::Observational,
        );
        let probe = probe_gateway_candidate(
            request.caller,
            candidate,
            request.cluster_name,
            request.definition,
        )
        .await;
        probes.push(GatewayExplainCandidateProbe {
            candidate: candidate.clone(),
            cooldown_ms,
            breaker_state: health.breaker_state,
            skip_reason,
            stability_score: health.stability_score(),
            last_latency_ms: health.last_latency_ms,
            probe,
        });
    }

    let mut failures = Vec::new();
    let (selected, _) = evaluate_gateway_explain_selection(
        &probes,
        request.respect_cooldown,
        request.no_failover,
        &mut failures,
    );
    emit_gateway_probe_observation(
        request.cluster_name,
        request.command_name,
        "dry_run",
        &probes,
        selected,
        &failures,
    );

    if request.output_format == GatewayOutputFormat::Json {
        print_gateway_dry_run_json(&request, preferred.as_ref(), &probes, &failures, selected)?;
    } else {
        print_gateway_dry_run_text(&request, preferred.as_ref(), &probes, &failures, selected);
    }

    Ok(u8::from(selected.is_none()))
}

fn print_gateway_dry_run_json(
    request: &GatewayDryRunRequest<'_>,
    preferred: Option<&String>,
    probes: &[GatewayExplainCandidateProbe],
    failures: &[GatewayAttemptFailure],
    selected: Option<&GatewayExplainCandidateProbe>,
) -> Result<()> {
    let payload_input = GatewayExplainJsonPayloadInput {
        cluster_name: request.cluster_name,
        definition: request.definition,
        overrides: &GatewayCommandOverrides {
            no_failover: request.no_failover,
            ..GatewayCommandOverrides::default()
        },
        probes,
        preferred,
        failures,
        selected_candidate: selected.map(|value| &value.candidate),
        command_name: Some(request.command_name),
        observational: true,
        include_decision_summary: request.why,
    };
    let payload = build_gateway_explain_json_payload(&payload_input);
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).context("failed encoding dry-run gateway json")?
    );
    Ok(())
}

fn print_gateway_dry_run_text(
    request: &GatewayDryRunRequest<'_>,
    preferred: Option<&String>,
    probes: &[GatewayExplainCandidateProbe],
    failures: &[GatewayAttemptFailure],
    selected: Option<&GatewayExplainCandidateProbe>,
) {
    let policy = gateway_effective_policy_values(request.definition);
    println!(
        "cluster gateway dry-run: cluster='{cluster_name}' command='{command_name}' mode={:?} no_failover={}",
        request.definition.gateway_mode,
        request.no_failover,
        cluster_name = request.cluster_name,
        command_name = request.command_name,
    );
    println!(
        "policy: preset={} breaker_open_after_failures={} breaker_half_open_after_ms={} breaker_half_open_required_successes={} probe_timeout_ms={} cooldown_ms={} cooldown_max_ms={} cooldown_jitter_pct={} success_ttl_ms={} history_max_entries={} history_retention_ms={}",
        request
            .definition
            .gateway_policy
            .map_or("custom", gateway_policy_label),
        policy.breaker_open_after_failures,
        policy.breaker_half_open_after_ms,
        policy.breaker_half_open_required_successes,
        policy.probe_timeout_ms,
        policy.cooldown_ms,
        policy.cooldown_max_ms,
        policy.cooldown_jitter_pct,
        policy.success_ttl_ms,
        policy.history_max_entries,
        policy.history_retention_ms
    );
    print_gateway_text_table_header();
    for probe in probes {
        println!(
            "  {:<24} {:<9} {:<10} {:<10} {:<12} {:<5} {:<14} {:<10} {:<14} {}",
            gateway_table_candidate_label(&probe.candidate),
            gateway_bool_label(preferred.is_some_and(|value| value == &probe.candidate)),
            probe.stability_score,
            gateway_breaker_state_label(probe.breaker_state),
            gateway_optional_u128_label(probe.cooldown_ms),
            gateway_bool_label(probe.probe.ok),
            probe.probe.reason_code,
            probe.probe.latency_ms,
            probe.skip_reason.unwrap_or("-"),
            probe.probe.detail
        );
    }
    if let Some(selected) = selected {
        println!(
            "selection result: candidate '{}' is executable (dry-run, command not executed)",
            selected.candidate
        );
    } else {
        println!("selection result: no executable gateway candidate");
        println!("failures: {}", format_gateway_failures(failures));
    }
    println!("would mutate: last_good=false cooldown=false breaker=false persistence_write=false");
    if request.why {
        print_gateway_decision_summary_text(
            selected.map(|value| value.candidate.as_str()),
            failures,
        );
    }
}

#[allow(clippy::too_many_lines)] // Control flow is clearer as one selection loop.
async fn run_gateway_candidate_batch(
    request: GatewayBatchRequest<'_>,
    failures: &mut Vec<GatewayAttemptFailure>,
) -> Result<GatewayBatchOutcome> {
    let mut attempted = false;
    for candidate in request.candidates {
        if let Some(skip_reason) = gateway_candidate_skip_reason(
            request.cluster_name,
            candidate,
            request.definition,
            request.respect_cooldown,
            request.execution_mode,
        ) {
            tracing::debug!(
                event = "cluster_gateway_candidate_skipped",
                cluster = %request.cluster_name,
                candidate = %candidate,
                reason = skip_reason,
                "skipping gateway candidate"
            );
            failures.push(GatewayAttemptFailure {
                candidate: candidate.clone(),
                reason_code: skip_reason,
                detail: format!("candidate skipped due to {skip_reason}"),
            });
            continue;
        }

        attempted = true;
        let started = Instant::now();
        match run_plugin_command_on_target(
            request.caller,
            candidate,
            request.plugin_id,
            request.command_name,
            request.arguments,
        )
        .await
        {
            Ok(code) => {
                if request.execution_mode == GatewayExecutionMode::Mutating {
                    record_gateway_success(
                        request.cluster_name,
                        candidate,
                        request.definition,
                        started.elapsed().as_millis(),
                    );
                    record_gateway_history_entry(
                        request.cluster_name,
                        request.definition,
                        &GatewayHistoryRecordInput {
                            command_name: request.command_name,
                            candidate: Some(candidate),
                            execution_mode: GatewayExecutionMode::Mutating,
                            latency_ms: Some(u128_to_u64_saturating(started.elapsed().as_millis())),
                            result: "success",
                            reason_code: None,
                            selected: true,
                        },
                    );
                }
                tracing::info!(
                    event = "cluster_gateway_selected",
                    cluster = %request.cluster_name,
                    candidate = %candidate,
                    command = %request.command_name,
                    "cluster gateway command succeeded"
                );
                return Ok(GatewayBatchOutcome::Success(code));
            }
            Err(error) => {
                let classified = classify_gateway_error(&error);
                if request.execution_mode == GatewayExecutionMode::Mutating {
                    record_gateway_failure(
                        request.cluster_name,
                        candidate,
                        request.definition,
                        started.elapsed().as_millis(),
                        classified.0,
                    );
                    record_gateway_history_entry(
                        request.cluster_name,
                        request.definition,
                        &GatewayHistoryRecordInput {
                            command_name: request.command_name,
                            candidate: Some(candidate),
                            execution_mode: GatewayExecutionMode::Mutating,
                            latency_ms: Some(u128_to_u64_saturating(started.elapsed().as_millis())),
                            result: "failure",
                            reason_code: Some(classified.0),
                            selected: false,
                        },
                    );
                }
                tracing::warn!(
                    event = "cluster_gateway_candidate_failed",
                    cluster = %request.cluster_name,
                    candidate = %candidate,
                    reason_code = classified.0,
                    detail = %classified.1,
                    "cluster gateway candidate failed"
                );
                failures.push(GatewayAttemptFailure {
                    candidate: candidate.clone(),
                    reason_code: classified.0,
                    detail: classified.1,
                });
            }
        }

        if request.no_failover {
            break;
        }
    }
    Ok(GatewayBatchOutcome::Exhausted { attempted })
}

fn cluster_gateway_settings_from_context(
    context: &NativeCommandContext,
    config: &BmuxConfig,
) -> Result<ClusterGatewaySettings> {
    let settings = context
        .settings
        .clone()
        .or_else(|| config.plugins.settings.get("bmux.cluster").cloned())
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
    settings
        .try_into()
        .map_err(|error| anyhow::anyhow!("invalid bmux.cluster settings: {error}"))
}

fn resolve_cluster_name_for_gateway(
    command_name: &str,
    arguments: &[String],
    settings: &ClusterGatewaySettings,
) -> Result<Option<String>> {
    let cluster_flag =
        value_after_flag(arguments, "--cluster").or_else(|| value_after_flag(arguments, "-c"));
    let explicit = match command_name {
        "cluster-up" => first_positional_argument(arguments),
        "cluster-events" => cluster_flag,
        "cluster-gateway-status"
        | "cluster-gateway-explain"
        | "cluster-gateway-doctor"
        | "cluster-gateway-history"
        | "cluster-gateway-history-export"
        | "cluster-gateway-why" => {
            let cluster = cluster_flag.or_else(|| first_positional_argument(arguments));
            if cluster.is_none() && settings.clusters.len() > 1 {
                anyhow::bail!("{command_name} requires --cluster in multi-cluster setups");
            }
            cluster
        }
        "cluster-pane-retry" => {
            if cluster_flag.is_none() && settings.clusters.len() > 1 {
                anyhow::bail!(
                    "cluster-pane-retry requires --cluster when multiple clusters are configured"
                );
            }
            cluster_flag
        }
        "cluster-pane-new" | "cluster-pane-move" => {
            if let Some(cluster) = cluster_flag {
                Some(cluster)
            } else if let Some(host) = extract_host_argument(arguments) {
                let matches = infer_cluster_names_from_target(settings, host.as_str());
                match matches.as_slice() {
                    [single] => Some(single.clone()),
                    [] => {
                        if settings.clusters.len() > 1 {
                            anyhow::bail!(
                                "{command_name} cannot infer cluster for host '{host}'; pass --cluster"
                            );
                        }
                        None
                    }
                    _ => {
                        anyhow::bail!(
                            "{command_name} host '{host}' matches multiple clusters ({}) - pass --cluster",
                            matches.join(",")
                        );
                    }
                }
            } else {
                if settings.clusters.len() > 1 {
                    anyhow::bail!(
                        "{command_name} requires --cluster in multi-cluster setups when host inference is unavailable"
                    );
                }
                None
            }
        }
        "cluster-status" | "cluster-hosts" | "cluster-doctor" => {
            let candidate = first_positional_argument(arguments);
            candidate.filter(|value| settings.clusters.contains_key(value))
        }
        _ => None,
    };

    Ok(explicit.or_else(|| {
        if settings.clusters.len() == 1 {
            settings.clusters.keys().next().cloned()
        } else {
            None
        }
    }))
}

fn first_positional_argument(arguments: &[String]) -> Option<String> {
    let mut index = 0usize;
    while index < arguments.len() {
        let value = arguments[index].trim();
        if value.is_empty() {
            index += 1;
            continue;
        }
        if value.starts_with('-') {
            index += 1;
            if index < arguments.len() && !arguments[index].starts_with('-') {
                index += 1;
            }
            continue;
        }
        return Some(value.to_string());
    }
    None
}

fn value_after_flag(arguments: &[String], flag: &str) -> Option<String> {
    let inline_prefix = format!("{flag}=");
    arguments
        .iter()
        .find_map(|argument| {
            if argument == flag {
                return None;
            }
            argument
                .strip_prefix(inline_prefix.as_str())
                .map(|value| value.trim().to_string())
        })
        .filter(|value| !value.is_empty())
        .or_else(|| {
            arguments
                .windows(2)
                .find_map(|pair| (pair[0] == flag).then(|| pair[1].trim().to_string()))
                .filter(|value| !value.is_empty())
        })
}

fn extract_host_argument(arguments: &[String]) -> Option<String> {
    value_after_flag(arguments, "--host")
        .or_else(|| value_after_flag(arguments, "-h"))
        .or_else(|| {
            let positional = arguments
                .iter()
                .filter(|value| !value.starts_with('-'))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>();
            positional.last().cloned()
        })
}

fn infer_cluster_names_from_target(settings: &ClusterGatewaySettings, target: &str) -> Vec<String> {
    settings
        .clusters
        .iter()
        .filter(|(_, definition)| {
            definition
                .declared_targets()
                .iter()
                .any(|value| value == target)
        })
        .map(|(cluster, _)| cluster.clone())
        .collect()
}

fn gateway_success_ttl(definition: &ClusterGatewayDefinition) -> Duration {
    Duration::from_millis(gateway_effective_policy_values(definition).success_ttl_ms)
}

fn gateway_failure_cooldown_for_level(
    definition: &ClusterGatewayDefinition,
    candidate: &str,
    adaptive_level: u32,
    reason_code: &str,
) -> Duration {
    let policy = gateway_effective_policy_values(definition);
    let exponent = adaptive_level.saturating_sub(1).min(16);
    let multiplier = 1_u64 << exponent;
    let class_multiplier = gateway_reason_class_cooldown_multiplier(reason_code);
    let scaled = policy
        .cooldown_ms
        .saturating_mul(multiplier)
        .saturating_mul(class_multiplier);
    let bounded = scaled.min(policy.cooldown_max_ms);
    let jittered = apply_gateway_jitter_ms(bounded, policy.cooldown_jitter_pct, candidate);
    Duration::from_millis(jittered)
}

fn gateway_reason_class_cooldown_multiplier(reason_code: &str) -> u64 {
    match reason_code {
        "auth_failed" | "service_denied" => 3,
        "dns_failed" | "protocol_mismatch" => 2,
        _ => 1,
    }
}

fn gateway_reason_breaker_threshold(
    definition: &ClusterGatewayDefinition,
    reason_code: &str,
) -> u32 {
    match reason_code {
        "auth_failed" | "service_denied" => 1,
        _ => gateway_breaker_open_after_failures(definition),
    }
}

fn apply_gateway_jitter_ms(base_ms: u64, jitter_pct: u32, seed_hint: &str) -> u64 {
    if base_ms == 0 || jitter_pct == 0 {
        return base_ms;
    }
    let spread = base_ms
        .saturating_mul(u64::from(jitter_pct))
        .saturating_div(100);
    if spread == 0 {
        return base_ms;
    }
    let mut seed = current_unix_timestamp_ms_u64();
    for byte in seed_hint.as_bytes() {
        seed = seed.wrapping_mul(109).wrapping_add(u64::from(*byte));
    }
    let bucket = spread.saturating_mul(2).saturating_add(1);
    let delta = seed % bucket;
    if delta <= spread {
        base_ms.saturating_sub(spread - delta)
    } else {
        base_ms.saturating_add(delta - spread)
    }
}

fn gateway_breaker_half_open_after(definition: &ClusterGatewayDefinition) -> Duration {
    Duration::from_millis(gateway_effective_policy_values(definition).breaker_half_open_after_ms)
}

fn gateway_breaker_open_after_failures(definition: &ClusterGatewayDefinition) -> u32 {
    gateway_effective_policy_values(definition).breaker_open_after_failures
}

fn gateway_probe_timeout_ms(definition: &ClusterGatewayDefinition) -> u64 {
    gateway_effective_policy_values(definition).probe_timeout_ms
}

fn gateway_effective_candidate_health(
    cluster_name: &str,
    candidate: &str,
    _definition: &ClusterGatewayDefinition,
    execution_mode: GatewayExecutionMode,
) -> GatewayCandidateHealth {
    ensure_gateway_runtime_state_loaded();
    let Ok(mut state_map) = cluster_gateway_state_map().lock() else {
        return GatewayCandidateHealth::default();
    };
    let (health, persist_needed) = gateway_effective_candidate_health_in_state(
        &mut state_map,
        cluster_name,
        candidate,
        execution_mode,
    );
    if persist_needed {
        let snapshot = state_map.clone();
        drop(state_map);
        persist_gateway_runtime_state_snapshot(&snapshot);
    }
    health
}

fn gateway_effective_candidate_health_in_state(
    state_map: &mut BTreeMap<String, ClusterGatewayRuntimeState>,
    cluster_name: &str,
    candidate: &str,
    execution_mode: GatewayExecutionMode,
) -> (GatewayCandidateHealth, bool) {
    let now = Instant::now();
    let mut persist_needed = false;
    let mut health = state_map
        .get(cluster_name)
        .and_then(|cluster_state| cluster_state.candidate_health.get(candidate).cloned())
        .unwrap_or_default();

    if health.breaker_state == GatewayBreakerState::Open
        && let Some(until) = health.breaker_open_until
        && now >= until
    {
        health.breaker_state = GatewayBreakerState::HalfOpen;
        health.breaker_open_until = None;
        health.half_open_success_streak = 0;
        if execution_mode == GatewayExecutionMode::Mutating {
            let cluster_state = state_map.entry(cluster_name.to_string()).or_default();
            cluster_state
                .candidate_health
                .insert(candidate.to_string(), health.clone());
            persist_needed = true;
        }
    }

    (health, persist_needed)
}

fn gateway_candidate_skip_reason(
    cluster_name: &str,
    candidate: &str,
    definition: &ClusterGatewayDefinition,
    respect_cooldown: bool,
    execution_mode: GatewayExecutionMode,
) -> Option<&'static str> {
    let health =
        gateway_effective_candidate_health(cluster_name, candidate, definition, execution_mode);
    if health.breaker_state == GatewayBreakerState::Open {
        return Some("breaker_open");
    }
    if respect_cooldown && gateway_cooldown_remaining_ms(cluster_name, candidate).is_some() {
        return Some("cooldown");
    }
    None
}

fn candidate_stability_score(
    cluster_name: &str,
    candidate: &str,
    definition: &ClusterGatewayDefinition,
) -> u64 {
    gateway_effective_candidate_health(
        cluster_name,
        candidate,
        definition,
        GatewayExecutionMode::Observational,
    )
    .stability_score()
}

fn ordered_gateway_candidates_for_cluster(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
) -> Result<Vec<String>> {
    let candidates = gateway_candidates_for_cluster(cluster_name, definition)?;
    if definition.gateway_mode != ClusterGatewayMode::Auto {
        return Ok(candidates);
    }

    let preferred = preferred_gateway_candidate(cluster_name, gateway_success_ttl(definition));
    let mut ordered = candidates;
    ordered.sort_by_key(|candidate| {
        let stability = candidate_stability_score(cluster_name, candidate, definition);
        let latency = gateway_effective_candidate_health(
            cluster_name,
            candidate,
            definition,
            GatewayExecutionMode::Observational,
        )
        .last_latency_ms
        .unwrap_or(u64::MAX);
        let preferred_rank = u8::from(preferred.as_ref().is_none_or(|value| value != candidate));
        (stability, latency, preferred_rank)
    });
    Ok(ordered)
}

fn gateway_paths() -> &'static ConfigPaths {
    CLUSTER_GATEWAY_PATHS.get_or_init(ConfigPaths::default)
}

fn cluster_gateway_state_map() -> &'static Mutex<BTreeMap<String, ClusterGatewayRuntimeState>> {
    CLUSTER_GATEWAY_RUNTIME_STATE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn ensure_gateway_runtime_state_loaded() {
    let should_load = cluster_gateway_state_map()
        .lock()
        .is_ok_and(|state_map| state_map.is_empty());
    if !should_load {
        return;
    }

    let loaded = match load_cluster_gateway_runtime_state(gateway_paths()) {
        Ok(loaded) => loaded,
        Err(error) => {
            tracing::warn!(
                event = "cluster_gateway_state_load_failed",
                error = %error,
                "failed loading persisted cluster gateway runtime state"
            );
            BTreeMap::new()
        }
    };
    if loaded.is_empty() {
        return;
    }

    if let Ok(mut state_map) = cluster_gateway_state_map().lock()
        && state_map.is_empty()
    {
        *state_map = loaded;
    }
}

fn persist_gateway_runtime_state_snapshot(
    state_map: &BTreeMap<String, ClusterGatewayRuntimeState>,
) {
    if let Err(error) = save_cluster_gateway_runtime_state(gateway_paths(), state_map) {
        tracing::warn!(
            event = "cluster_gateway_state_save_failed",
            error = %error,
            "failed persisting cluster gateway runtime state"
        );
    }
}

fn preferred_gateway_candidate(cluster_name: &str, success_ttl: Duration) -> Option<String> {
    ensure_gateway_runtime_state_loaded();
    let state = {
        let state_map = cluster_gateway_state_map().lock().ok()?;
        state_map.get(cluster_name)?.clone()
    };
    let last_good = state.last_good?;
    if last_good.observed_at.elapsed() > success_ttl {
        if let Ok(mut state_map) = cluster_gateway_state_map().lock()
            && let Some(cluster_state) = state_map.get_mut(cluster_name)
        {
            cluster_state.last_good = None;
            let snapshot = state_map.clone();
            drop(state_map);
            persist_gateway_runtime_state_snapshot(&snapshot);
        }
        None
    } else {
        Some(last_good.target)
    }
}

fn record_gateway_success(
    cluster_name: &str,
    candidate: &str,
    definition: &ClusterGatewayDefinition,
    latency_ms: u128,
) {
    ensure_gateway_runtime_state_loaded();
    if let Ok(mut state_map) = cluster_gateway_state_map().lock() {
        record_gateway_success_in_state(
            &mut state_map,
            cluster_name,
            candidate,
            definition,
            latency_ms,
        );
        let snapshot = state_map.clone();
        drop(state_map);
        persist_gateway_runtime_state_snapshot(&snapshot);
    }
}

fn record_gateway_success_in_state(
    state_map: &mut BTreeMap<String, ClusterGatewayRuntimeState>,
    cluster_name: &str,
    candidate: &str,
    definition: &ClusterGatewayDefinition,
    latency_ms: u128,
) {
    let cluster_state = state_map.entry(cluster_name.to_string()).or_default();
    cluster_state.last_good = Some(GatewayLastGood {
        target: candidate.to_string(),
        observed_at: Instant::now(),
    });
    cluster_state.cooldown_until.remove(candidate);
    let health = cluster_state
        .candidate_health
        .entry(candidate.to_string())
        .or_default();
    if health.breaker_state == GatewayBreakerState::HalfOpen {
        health.half_open_success_streak = health.half_open_success_streak.saturating_add(1);
        if health.half_open_success_streak
            >= gateway_effective_policy_values(definition).breaker_half_open_required_successes
        {
            health.breaker_state = GatewayBreakerState::Closed;
            health.breaker_open_until = None;
            health.half_open_success_streak = 0;
        }
    } else {
        health.breaker_state = GatewayBreakerState::Closed;
        health.breaker_open_until = None;
        health.half_open_success_streak = 0;
    }
    health.successes = health.successes.saturating_add(1);
    health.consecutive_failures = 0;
    health.adaptive_cooldown_level = 0;
    health.last_failure_reason = None;
    health.last_latency_ms = Some(u128_to_u64_saturating(latency_ms));
}

fn record_gateway_failure(
    cluster_name: &str,
    candidate: &str,
    definition: &ClusterGatewayDefinition,
    latency_ms: u128,
    reason_code: &str,
) {
    ensure_gateway_runtime_state_loaded();
    if let Ok(mut state_map) = cluster_gateway_state_map().lock() {
        record_gateway_failure_in_state(
            &mut state_map,
            cluster_name,
            candidate,
            definition,
            latency_ms,
            reason_code,
        );
        let snapshot = state_map.clone();
        drop(state_map);
        persist_gateway_runtime_state_snapshot(&snapshot);
    }
}

fn record_gateway_failure_in_state(
    state_map: &mut BTreeMap<String, ClusterGatewayRuntimeState>,
    cluster_name: &str,
    candidate: &str,
    definition: &ClusterGatewayDefinition,
    latency_ms: u128,
    reason_code: &str,
) {
    let cluster_state = state_map.entry(cluster_name.to_string()).or_default();
    let health = cluster_state
        .candidate_health
        .entry(candidate.to_string())
        .or_default();
    let failed_from_half_open = health.breaker_state == GatewayBreakerState::HalfOpen;
    health.failures = health.failures.saturating_add(1);
    health.consecutive_failures = health.consecutive_failures.saturating_add(1);
    let level_increment = gateway_reason_class_cooldown_multiplier(reason_code);
    health.adaptive_cooldown_level = health
        .adaptive_cooldown_level
        .saturating_add(u32::try_from(level_increment).unwrap_or(u32::MAX));
    health.half_open_success_streak = 0;
    health.last_failure_reason = Some(reason_code.to_string());
    health.last_latency_ms = Some(u128_to_u64_saturating(latency_ms));
    if failed_from_half_open
        || health.consecutive_failures >= gateway_reason_breaker_threshold(definition, reason_code)
    {
        health.breaker_state = GatewayBreakerState::Open;
        health.breaker_open_until =
            Some(Instant::now() + gateway_breaker_half_open_after(definition));
    }
    cluster_state.cooldown_until.insert(
        candidate.to_string(),
        Instant::now()
            + gateway_failure_cooldown_for_level(
                definition,
                candidate,
                health.adaptive_cooldown_level,
                reason_code,
            ),
    );
}

fn record_gateway_history_entry(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    input: &GatewayHistoryRecordInput<'_>,
) {
    ensure_gateway_runtime_state_loaded();
    if let Ok(mut state_map) = cluster_gateway_state_map().lock() {
        let cluster_state = state_map.entry(cluster_name.to_string()).or_default();
        cluster_state.history.push(GatewayHistoryEntry {
            observed_at: Instant::now(),
            command: input.command_name.to_string(),
            candidate: input.candidate.map(str::to_string),
            execution_mode: input.execution_mode,
            latency_ms: input.latency_ms,
            result: input.result.to_string(),
            reason_code: input.reason_code.map(str::to_string),
            selected: input.selected,
        });
        trim_gateway_history_entries(cluster_state, definition);
        let snapshot = state_map.clone();
        drop(state_map);
        persist_gateway_runtime_state_snapshot(&snapshot);
    }
}

fn gateway_history_entries(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    query: &GatewayHistoryQuery,
) -> Vec<GatewayHistoryEntry> {
    ensure_gateway_runtime_state_loaded();
    let mut entries = cluster_gateway_state_map()
        .lock()
        .ok()
        .and_then(|state_map| {
            state_map
                .get(cluster_name)
                .map(|state| state.history.clone())
        })
        .unwrap_or_default();
    trim_gateway_history_entries_vec(&mut entries, definition);
    apply_gateway_history_query(&mut entries, query);
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.observed_at));
    if let Some(limit) = query.limit {
        entries.truncate(limit);
    }
    entries
}

fn trim_gateway_history_entries(
    cluster_state: &mut ClusterGatewayRuntimeState,
    definition: &ClusterGatewayDefinition,
) {
    trim_gateway_history_entries_vec(&mut cluster_state.history, definition);
}

fn trim_gateway_history_entries_vec(
    entries: &mut Vec<GatewayHistoryEntry>,
    definition: &ClusterGatewayDefinition,
) {
    let policy = gateway_effective_policy_values(definition);
    let retention = Duration::from_millis(policy.history_retention_ms);
    entries.retain(|entry| entry.observed_at.elapsed() <= retention);
    if entries.len() > policy.history_max_entries {
        let drain_count = entries.len().saturating_sub(policy.history_max_entries);
        entries.drain(..drain_count);
    }
}

fn apply_gateway_history_query(
    entries: &mut Vec<GatewayHistoryEntry>,
    query: &GatewayHistoryQuery,
) {
    if let Some(since_window) = query.since {
        entries.retain(|entry| entry.observed_at.elapsed() <= since_window);
    }
    if let Some(result) = query.result.as_ref() {
        entries.retain(|entry| entry.result == *result);
    }
    if let Some(reason) = query.reason.as_ref() {
        entries.retain(|entry| entry.reason_code.as_deref() == Some(reason.as_str()));
    }
    if let Some(candidate) = query.candidate.as_ref() {
        entries.retain(|entry| entry.candidate.as_deref() == Some(candidate.as_str()));
    }
    if let Some(command) = query.command.as_ref() {
        entries.retain(|entry| entry.command == *command);
    }
}

fn gateway_history_entry_observed_unix_ms(entry: &GatewayHistoryEntry) -> u64 {
    current_unix_timestamp_ms_u64().saturating_sub(duration_millis_u64(entry.observed_at.elapsed()))
}

fn parse_gateway_history_limit(arguments: &[String]) -> Result<Option<usize>> {
    let Some(raw) = value_after_flag(arguments, "--limit") else {
        return Ok(None);
    };
    let parsed = raw
        .parse::<usize>()
        .with_context(|| format!("invalid --limit value '{raw}'"))?;
    if parsed == 0 {
        anyhow::bail!("--limit must be greater than zero");
    }
    Ok(Some(parsed))
}

fn parse_gateway_history_since(arguments: &[String]) -> Result<Option<Duration>> {
    let Some(raw) = value_after_flag(arguments, "--since") else {
        return Ok(None);
    };
    parse_duration_literal(raw.as_str()).map(Some)
}

fn parse_gateway_history_query(arguments: &[String]) -> Result<GatewayHistoryQuery> {
    let result = value_after_flag(arguments, "--result").map(|value| value.trim().to_string());
    let reason = value_after_flag(arguments, "--reason").map(|value| value.trim().to_string());
    let candidate =
        value_after_flag(arguments, "--candidate").map(|value| value.trim().to_string());
    let command = value_after_flag(arguments, "--command").map(|value| value.trim().to_string());
    if let Some(value) = result.as_ref()
        && !matches!(
            value.as_str(),
            "success" | "failure" | "observed_success" | "observed_failure"
        )
    {
        anyhow::bail!(
            "unsupported --result '{value}' (expected success|failure|observed_success|observed_failure)"
        );
    }
    Ok(GatewayHistoryQuery {
        since: parse_gateway_history_since(arguments)?,
        limit: parse_gateway_history_limit(arguments)?,
        result,
        reason,
        candidate,
        command,
    })
}

fn parse_gateway_confirm_flag(arguments: &[String]) -> bool {
    arguments.iter().any(|value| value == "--confirm")
}

fn parse_gateway_all_flag(arguments: &[String]) -> bool {
    arguments.iter().any(|value| value == "--all")
}

fn parse_gateway_export_format(arguments: &[String]) -> Result<GatewayExportFormat> {
    let Some(value) = value_after_flag(arguments, "--format") else {
        return Ok(GatewayExportFormat::Json);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "json" => Ok(GatewayExportFormat::Json),
        "ndjson" => Ok(GatewayExportFormat::Ndjson),
        other => anyhow::bail!("unsupported --format '{other}' (expected json|ndjson)"),
    }
}

fn parse_duration_literal(value: &str) -> Result<Duration> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        anyhow::bail!("duration value cannot be empty");
    }
    let split_at = trimmed
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (digits, unit) = trimmed.split_at(split_at);
    if digits.is_empty() {
        anyhow::bail!("invalid duration '{value}'");
    }
    let amount = digits
        .parse::<u64>()
        .with_context(|| format!("invalid duration '{value}'"))?;
    let normalized = unit.trim().to_ascii_lowercase();
    let millis = match normalized.as_str() {
        "" | "s" => amount.saturating_mul(1000),
        "ms" => amount,
        "m" => amount.saturating_mul(60_000),
        "h" => amount.saturating_mul(3_600_000),
        "d" => amount.saturating_mul(86_400_000),
        _ => anyhow::bail!("unsupported duration unit in '{value}' (use ms|s|m|h|d)"),
    };
    if millis == 0 {
        anyhow::bail!("duration must be greater than zero");
    }
    Ok(Duration::from_millis(millis))
}

fn classify_gateway_error(error: &anyhow::Error) -> (&'static str, String) {
    let message = error.to_string();
    let lowered = message.to_ascii_lowercase();
    let code = if lowered.contains("denied") || lowered.contains("forbidden") {
        "service_denied"
    } else if lowered.contains("permission denied")
        || lowered.contains("publickey")
        || lowered.contains("authentication")
        || lowered.contains("unauthorized")
    {
        "auth_failed"
    } else if lowered.contains("protocol")
        || lowered.contains("handshake")
        || lowered.contains("version mismatch")
    {
        "protocol_mismatch"
    } else if lowered.contains("dns")
        || lowered.contains("name or service not known")
        || lowered.contains("no such host")
        || lowered.contains("failed to lookup")
    {
        "dns_failed"
    } else if lowered.contains("connection refused") || lowered.contains("refused") {
        "connection_refused"
    } else if lowered.contains("auth") || lowered.contains("permission") {
        "auth_failed"
    } else if lowered.contains("timeout") || lowered.contains("timed out") {
        "timeout"
    } else if lowered.contains("not found") || lowered.contains("unreachable") {
        "unreachable"
    } else {
        "connect"
    };
    (code, message)
}

fn format_gateway_failures(failures: &[GatewayAttemptFailure]) -> String {
    failures
        .iter()
        .map(|failure| {
            format!(
                "{}[{}]={}",
                failure.candidate, failure.reason_code, failure.detail
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn parse_gateway_output_format(arguments: &[String]) -> Result<GatewayOutputFormat> {
    let Some(value) = value_after_flag(arguments, "--format") else {
        return Ok(GatewayOutputFormat::Text);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "text" => Ok(GatewayOutputFormat::Text),
        "json" => Ok(GatewayOutputFormat::Json),
        other => anyhow::bail!("unsupported --format '{other}' (expected text|json)"),
    }
}

fn parse_gateway_reset_scope(arguments: &[String]) -> Result<GatewayResetScope> {
    let all = arguments.iter().any(|value| value == "--all");
    let cluster_flag =
        value_after_flag(arguments, "--cluster").or_else(|| value_after_flag(arguments, "-c"));
    let cluster_positional = first_positional_argument(arguments);

    if all {
        if cluster_flag.is_some() || cluster_positional.is_some() {
            anyhow::bail!("cluster gateway reset accepts either --all or --cluster, not both");
        }
        return Ok(GatewayResetScope::All);
    }

    if let (Some(flag_cluster), Some(positional_cluster)) =
        (cluster_flag.as_ref(), cluster_positional.as_ref())
        && flag_cluster != positional_cluster
    {
        anyhow::bail!(
            "cluster gateway reset cluster mismatch between --cluster='{flag_cluster}' and positional '{positional_cluster}'"
        );
    }

    let cluster = cluster_flag.or(cluster_positional).ok_or_else(|| {
        anyhow::anyhow!("cluster gateway reset requires --cluster unless --all is passed")
    })?;
    Ok(GatewayResetScope::Cluster(cluster))
}

fn run_cluster_gateway_reset_command(
    settings: &ClusterGatewaySettings,
    arguments: &[String],
) -> Result<u8> {
    let scope = parse_gateway_reset_scope(arguments)?;
    match scope {
        GatewayResetScope::All => {
            let removed = clear_gateway_runtime_state_all()?;
            println!("cluster gateway reset: scope=all removed={removed}");
            Ok(0)
        }
        GatewayResetScope::Cluster(cluster_name) => {
            if !settings.clusters.contains_key(cluster_name.as_str()) {
                anyhow::bail!("unknown cluster '{cluster_name}'");
            }
            let removed = clear_gateway_runtime_state_cluster(cluster_name.as_str())?;
            println!(
                "cluster gateway reset: scope=cluster cluster='{cluster_name}' removed={removed}"
            );
            Ok(0)
        }
    }
}

fn run_cluster_gateway_history_command(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    arguments: &[String],
    output_format: GatewayOutputFormat,
) -> Result<u8> {
    let query = parse_gateway_history_query(arguments)?;
    let entries = gateway_history_entries(cluster_name, definition, &query);

    if output_format == GatewayOutputFormat::Json {
        let payload_entries = entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "observed_at_unix_ms": gateway_history_entry_observed_unix_ms(entry),
                    "command": entry.command,
                    "candidate": entry.candidate,
                    "execution_mode": entry.execution_mode,
                    "latency_ms": entry.latency_ms,
                    "result": entry.result,
                    "reason_code": entry.reason_code,
                    "selected": entry.selected,
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "cluster": cluster_name,
                "filters": {
                    "since": value_after_flag(arguments, "--since"),
                    "limit": query.limit,
                    "result": query.result,
                    "reason": query.reason,
                    "candidate": query.candidate,
                    "command": query.command,
                },
                "count": payload_entries.len(),
                "entries": payload_entries,
            }))
            .context("failed encoding gateway history json")?
        );
        return Ok(0);
    }

    println!(
        "cluster gateway history: cluster='{cluster_name}' entries={}",
        entries.len()
    );
    println!(
        "{:<16}  {:<24}  {:<8}  {:<24}  reason",
        "observed_unix_ms", "command", "result", "candidate"
    );
    if entries.is_empty() {
        println!("(no entries)");
        return Ok(0);
    }
    for entry in entries {
        println!(
            "{:<16}  {:<24}  {:<8}  {:<24}  {}",
            gateway_history_entry_observed_unix_ms(&entry),
            entry.command,
            entry.result,
            entry.candidate.unwrap_or_else(|| "-".to_string()),
            entry.reason_code.unwrap_or_else(|| "-".to_string())
        );
    }
    Ok(0)
}

fn run_cluster_gateway_history_export_command(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    arguments: &[String],
) -> Result<u8> {
    let query = parse_gateway_history_query(arguments)?;
    let entries = gateway_history_entries(cluster_name, definition, &query);
    let export_format = parse_gateway_export_format(arguments)?;
    match export_format {
        GatewayExportFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "cluster": cluster_name,
                    "entries": entries.iter().map(|entry| serde_json::json!({
                        "observed_at_unix_ms": gateway_history_entry_observed_unix_ms(entry),
                        "command": entry.command,
                        "candidate": entry.candidate,
                        "execution_mode": entry.execution_mode,
                        "latency_ms": entry.latency_ms,
                        "result": entry.result,
                        "reason_code": entry.reason_code,
                        "selected": entry.selected,
                    })).collect::<Vec<_>>()
                }))
                .context("failed encoding gateway history export json")?
            );
        }
        GatewayExportFormat::Ndjson => {
            for entry in entries {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "cluster": cluster_name,
                        "observed_at_unix_ms": gateway_history_entry_observed_unix_ms(&entry),
                        "command": entry.command,
                        "candidate": entry.candidate,
                        "execution_mode": entry.execution_mode,
                        "latency_ms": entry.latency_ms,
                        "result": entry.result,
                        "reason_code": entry.reason_code,
                        "selected": entry.selected,
                    }))
                    .context("failed encoding gateway history export ndjson")?
                );
            }
        }
    }
    Ok(0)
}

fn run_cluster_gateway_history_clear_command(
    settings: &ClusterGatewaySettings,
    arguments: &[String],
) -> Result<u8> {
    let all = parse_gateway_all_flag(arguments);
    let confirm = parse_gateway_confirm_flag(arguments);
    if all
        && (value_after_flag(arguments, "--cluster").is_some()
            || value_after_flag(arguments, "-c").is_some()
            || first_positional_argument(arguments).is_some())
    {
        anyhow::bail!("cluster gateway history clear accepts either --all or --cluster, not both");
    }
    let scope = if all {
        GatewayHistoryClearScope::All
    } else {
        GatewayHistoryClearScope::Cluster
    };
    let query = parse_gateway_history_query(arguments)?;
    let broad_clear = query.since.is_none()
        && query.result.is_none()
        && query.reason.is_none()
        && query.candidate.is_none()
        && query.command.is_none();
    if broad_clear && !confirm && !io::stdin().is_terminal() {
        anyhow::bail!(
            "cluster gateway history clear requires --confirm for non-interactive broad clear"
        );
    }

    ensure_gateway_runtime_state_loaded();
    let mut removed = 0usize;
    if let Ok(mut state_map) = cluster_gateway_state_map().lock() {
        match scope {
            GatewayHistoryClearScope::All => {
                for (cluster_name, state) in state_map.iter_mut() {
                    let definition = settings
                        .clusters
                        .get(cluster_name)
                        .cloned()
                        .unwrap_or_default();
                    let before = state.history.len();
                    removed = removed.saturating_add(clear_gateway_history_for_cluster(
                        state,
                        &definition,
                        &query,
                    ));
                    if before != state.history.len() {
                        trim_gateway_history_entries(state, &definition);
                    }
                }
            }
            GatewayHistoryClearScope::Cluster => {
                let Some(cluster_name) = resolve_cluster_name_for_history(arguments, settings)?
                else {
                    anyhow::bail!(
                        "cluster gateway history clear requires --cluster unless --all is passed"
                    );
                };
                let definition = settings
                    .clusters
                    .get(cluster_name.as_str())
                    .cloned()
                    .unwrap_or_default();
                let state = state_map.entry(cluster_name).or_default();
                removed = clear_gateway_history_for_cluster(state, &definition, &query);
                trim_gateway_history_entries(state, &definition);
            }
        }
        let snapshot = state_map.clone();
        drop(state_map);
        persist_gateway_runtime_state_snapshot(&snapshot);
    }

    println!("cluster gateway history clear: removed={removed}");
    Ok(0)
}

fn clear_gateway_history_for_cluster(
    state: &mut ClusterGatewayRuntimeState,
    definition: &ClusterGatewayDefinition,
    query: &GatewayHistoryQuery,
) -> usize {
    let mut filtered = state.history.clone();
    trim_gateway_history_entries_vec(&mut filtered, definition);
    apply_gateway_history_query(&mut filtered, query);
    if filtered.is_empty() {
        return 0;
    }
    let before = state.history.len();
    state.history.retain(|entry| {
        !filtered.iter().any(|matched| {
            matched.command == entry.command
                && matched.candidate == entry.candidate
                && matched.result == entry.result
                && matched.reason_code == entry.reason_code
                && matched.observed_at == entry.observed_at
        })
    });
    before.saturating_sub(state.history.len())
}

fn resolve_cluster_name_for_history(
    arguments: &[String],
    settings: &ClusterGatewaySettings,
) -> Result<Option<String>> {
    let cluster_flag =
        value_after_flag(arguments, "--cluster").or_else(|| value_after_flag(arguments, "-c"));
    let explicit = cluster_flag.or_else(|| first_positional_argument(arguments));
    if explicit.is_none() && settings.clusters.len() > 1 {
        anyhow::bail!("cluster gateway history clear requires --cluster in multi-cluster setups");
    }
    Ok(explicit.or_else(|| settings.clusters.keys().next().cloned()))
}

fn clear_gateway_runtime_state_all() -> Result<bool> {
    ensure_gateway_runtime_state_loaded();
    let had_entries = cluster_gateway_state_map()
        .lock()
        .is_ok_and(|state_map| !state_map.is_empty());
    if let Ok(mut state_map) = cluster_gateway_state_map().lock() {
        state_map.clear();
    }

    let paths = gateway_paths();
    let removed_file = clear_cluster_gateway_runtime_state(paths)?;
    Ok(had_entries || removed_file)
}

fn clear_gateway_runtime_state_cluster(cluster_name: &str) -> Result<bool> {
    ensure_gateway_runtime_state_loaded();
    let snapshot = {
        let mut state_map = cluster_gateway_state_map()
            .lock()
            .map_err(|_| anyhow::anyhow!("failed locking gateway runtime state"))?;
        let removed = state_map.remove(cluster_name).is_some();
        (removed, state_map.clone())
    };

    let paths = gateway_paths();
    if snapshot.1.is_empty() {
        let _ = clear_cluster_gateway_runtime_state(paths)?;
    } else {
        save_cluster_gateway_runtime_state(paths, &snapshot.1)?;
    }
    Ok(snapshot.0)
}

fn status_selected_candidate(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    ordered_candidates: &[String],
) -> Option<String> {
    if definition.gateway_mode == ClusterGatewayMode::Direct {
        return None;
    }
    ordered_candidates
        .iter()
        .find(|candidate| {
            gateway_candidate_skip_reason(
                cluster_name,
                candidate,
                definition,
                definition.gateway_mode == ClusterGatewayMode::Auto,
                GatewayExecutionMode::Observational,
            )
            .is_none()
        })
        .cloned()
        .or_else(|| ordered_candidates.first().cloned())
}

fn u128_to_u64_saturating(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

const fn gateway_mode_label(mode: ClusterGatewayMode) -> &'static str {
    match mode {
        ClusterGatewayMode::Auto => "auto",
        ClusterGatewayMode::Direct => "direct",
        ClusterGatewayMode::Pinned => "pinned",
    }
}

const fn gateway_policy_label(policy: GatewayPolicyPreset) -> &'static str {
    match policy {
        GatewayPolicyPreset::Balanced => "balanced",
        GatewayPolicyPreset::Aggressive => "aggressive",
        GatewayPolicyPreset::Conservative => "conservative",
    }
}

fn print_gateway_policy_header(
    title: &str,
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    no_failover: bool,
) {
    let policy = gateway_effective_policy_values(definition);
    println!(
        "{title}: cluster='{cluster_name}' mode={:?} no_failover={no_failover}",
        definition.gateway_mode
    );
    println!(
        "policy: preset={} breaker_open_after_failures={} breaker_half_open_after_ms={} breaker_half_open_required_successes={} probe_timeout_ms={} cooldown_ms={} cooldown_max_ms={} cooldown_jitter_pct={} success_ttl_ms={} history_max_entries={} history_retention_ms={}",
        definition
            .gateway_policy
            .map_or("custom", gateway_policy_label),
        policy.breaker_open_after_failures,
        policy.breaker_half_open_after_ms,
        policy.breaker_half_open_required_successes,
        policy.probe_timeout_ms,
        policy.cooldown_ms,
        policy.cooldown_max_ms,
        policy.cooldown_jitter_pct,
        policy.success_ttl_ms,
        policy.history_max_entries,
        policy.history_retention_ms
    );
}

const fn gateway_breaker_state_label(state: GatewayBreakerState) -> &'static str {
    match state {
        GatewayBreakerState::Closed => "closed",
        GatewayBreakerState::Open => "open",
        GatewayBreakerState::HalfOpen => "half_open",
    }
}

const fn gateway_bool_label(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn gateway_optional_u128_label(value: Option<u128>) -> String {
    value.map_or_else(|| "-".to_string(), |value| value.to_string())
}

fn parse_gateway_overrides(arguments: &[String]) -> Result<GatewayCommandOverrides> {
    let mut overrides = GatewayCommandOverrides::default();
    let mut index = 0usize;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--gateway-no-failover" {
            overrides.no_failover = true;
            index += 1;
            continue;
        }
        if argument == "--dry-run" {
            overrides.dry_run = true;
            index += 1;
            continue;
        }
        if argument == "--why" {
            overrides.why = true;
            index += 1;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--gateway=") {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                anyhow::bail!("--gateway requires a non-empty target value");
            }
            overrides.gateway_target = Some(trimmed.to_string());
            index += 1;
            continue;
        }
        if argument == "--gateway" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| anyhow::anyhow!("--gateway requires a target value"))?
                .trim()
                .to_string();
            if value.is_empty() || value.starts_with('-') {
                anyhow::bail!("--gateway requires a non-empty target value");
            }
            overrides.gateway_target = Some(value);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--gateway-mode=") {
            overrides.gateway_mode = Some(parse_gateway_mode_value(value)?);
            index += 1;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--gateway-policy=") {
            overrides.gateway_policy = Some(parse_gateway_policy_value(value)?);
            index += 1;
            continue;
        }
        if argument == "--gateway-mode" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| anyhow::anyhow!("--gateway-mode requires a value"))?;
            overrides.gateway_mode = Some(parse_gateway_mode_value(value)?);
            index += 2;
            continue;
        }
        if argument == "--gateway-policy" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| anyhow::anyhow!("--gateway-policy requires a value"))?;
            overrides.gateway_policy = Some(parse_gateway_policy_value(value)?);
            index += 2;
            continue;
        }
        overrides.passthrough_arguments.push(argument.clone());
        index += 1;
    }
    Ok(overrides)
}

fn parse_gateway_policy_value(value: &str) -> Result<GatewayPolicyPreset> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "balanced" => Ok(GatewayPolicyPreset::Balanced),
        "aggressive" => Ok(GatewayPolicyPreset::Aggressive),
        "conservative" => Ok(GatewayPolicyPreset::Conservative),
        _ => anyhow::bail!(
            "unsupported gateway policy '{value}' (expected balanced|aggressive|conservative)"
        ),
    }
}

fn parse_gateway_mode_value(value: &str) -> Result<ClusterGatewayMode> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "auto" => Ok(ClusterGatewayMode::Auto),
        "direct" => Ok(ClusterGatewayMode::Direct),
        "pinned" => Ok(ClusterGatewayMode::Pinned),
        _ => anyhow::bail!("unsupported gateway mode '{value}' (expected auto|direct|pinned)"),
    }
}

fn apply_gateway_overrides(
    mut definition: ClusterGatewayDefinition,
    overrides: &GatewayCommandOverrides,
) -> Result<ClusterGatewayDefinition> {
    if let Some(policy) = overrides.gateway_policy {
        definition.gateway_policy = Some(policy);
    }
    if let Some(mode) = overrides.gateway_mode {
        definition.gateway_mode = mode;
    }
    if let Some(target) = overrides.gateway_target.as_ref() {
        definition.gateway_mode = ClusterGatewayMode::Pinned;
        definition.gateway_target = Some(target.clone());
        definition.gateway_candidates = vec![target.clone()];
    }
    if definition.gateway_mode == ClusterGatewayMode::Pinned
        && definition
            .gateway_target
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
    {
        anyhow::bail!("gateway_mode='pinned' requires gateway_target or --gateway");
    }
    Ok(definition)
}

fn print_cluster_gateway_status(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    overrides: &GatewayCommandOverrides,
    output_format: GatewayOutputFormat,
) -> Result<()> {
    let candidates = ordered_gateway_candidates_for_cluster(cluster_name, definition)?;
    let preferred = preferred_gateway_candidate(cluster_name, gateway_success_ttl(definition));
    let candidate_rows =
        gateway_status_candidate_rows(cluster_name, definition, preferred.as_ref(), &candidates);
    let selected_candidate = status_selected_candidate(cluster_name, definition, &candidates);

    if output_format == GatewayOutputFormat::Json {
        print_cluster_gateway_status_json(
            cluster_name,
            definition,
            overrides,
            selected_candidate.as_deref(),
            &candidate_rows,
        )?;
        return Ok(());
    }

    print_cluster_gateway_status_text(
        cluster_name,
        definition,
        overrides,
        selected_candidate.as_deref(),
        &candidate_rows,
    );
    Ok(())
}

fn gateway_status_candidate_rows(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    preferred: Option<&String>,
    candidates: &[String],
) -> Vec<serde_json::Value> {
    candidates
        .iter()
        .map(|candidate| {
            let preferred_marker = preferred.is_some_and(|value| value == candidate);
            let cooldown = gateway_cooldown_remaining_ms(cluster_name, candidate);
            let health = gateway_effective_candidate_health(
                cluster_name,
                candidate,
                definition,
                GatewayExecutionMode::Observational,
            );
            let skip_reason = gateway_candidate_skip_reason(
                cluster_name,
                candidate,
                definition,
                definition.gateway_mode == ClusterGatewayMode::Auto,
                GatewayExecutionMode::Observational,
            );
            serde_json::json!({
                "candidate": candidate,
                "preferred": preferred_marker,
                "cooldown_ms": cooldown.map(u128_to_u64_saturating),
                "breaker_state": gateway_breaker_state_label(health.breaker_state),
                "stability_score": health.stability_score(),
                "historical_latency_ms": health.last_latency_ms,
                "skip_reason": skip_reason,
            })
        })
        .collect()
}

fn print_cluster_gateway_status_json(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    overrides: &GatewayCommandOverrides,
    selected_candidate: Option<&str>,
    candidate_rows: &[serde_json::Value],
) -> Result<()> {
    let policy = gateway_effective_policy_values(definition);
    let mut payload = serde_json::json!({
        "cluster": cluster_name,
        "mode": gateway_mode_label(definition.gateway_mode),
        "no_failover": overrides.no_failover,
        "overrides": {
            "mode": overrides.gateway_mode.map(gateway_mode_label),
            "gateway": overrides.gateway_target,
            "policy": overrides.gateway_policy.map(gateway_policy_label),
        },
        "policy": {
            "preset": definition.gateway_policy.map(gateway_policy_label),
            "breaker_open_after_failures": policy.breaker_open_after_failures,
            "breaker_half_open_after_ms": policy.breaker_half_open_after_ms,
            "breaker_half_open_required_successes": policy.breaker_half_open_required_successes,
            "probe_timeout_ms": policy.probe_timeout_ms,
            "cooldown_ms": policy.cooldown_ms,
            "cooldown_max_ms": policy.cooldown_max_ms,
            "cooldown_jitter_pct": policy.cooldown_jitter_pct,
            "success_ttl_ms": policy.success_ttl_ms,
            "history_max_entries": policy.history_max_entries,
            "history_retention_ms": policy.history_retention_ms,
        },
        "selected_candidate": selected_candidate,
        "would_mutate": {
            "enabled": false,
            "last_good": false,
            "cooldown": false,
            "breaker": false,
            "persistence_write": false,
        },
        "candidates": candidate_rows,
    });
    if overrides.why {
        payload["decision_summary"] =
            build_gateway_status_decision_summary(selected_candidate, candidate_rows);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).context("failed encoding gateway status json")?
    );
    Ok(())
}

fn print_cluster_gateway_status_text(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    overrides: &GatewayCommandOverrides,
    selected_candidate: Option<&str>,
    candidate_rows: &[serde_json::Value],
) {
    let policy = gateway_effective_policy_values(definition);

    println!(
        "cluster gateway status: cluster='{cluster_name}' mode={:?} no_failover={}",
        definition.gateway_mode, overrides.no_failover
    );
    println!(
        "policy: preset={} breaker_open_after_failures={} breaker_half_open_after_ms={} breaker_half_open_required_successes={} probe_timeout_ms={} cooldown_ms={} cooldown_max_ms={} cooldown_jitter_pct={} success_ttl_ms={} history_max_entries={} history_retention_ms={}",
        definition
            .gateway_policy
            .map_or("custom", gateway_policy_label),
        policy.breaker_open_after_failures,
        policy.breaker_half_open_after_ms,
        policy.breaker_half_open_required_successes,
        policy.probe_timeout_ms,
        policy.cooldown_ms,
        policy.cooldown_max_ms,
        policy.cooldown_jitter_pct,
        policy.success_ttl_ms,
        policy.history_max_entries,
        policy.history_retention_ms
    );
    if overrides.gateway_mode.is_some() || overrides.gateway_target.is_some() {
        println!(
            "overrides: mode={:?} gateway={:?}",
            overrides.gateway_mode, overrides.gateway_target
        );
    }
    println!(
        "selected candidate: {}",
        selected_candidate.unwrap_or("none")
    );
    println!("candidates:");
    print_gateway_text_table_header();
    for row in candidate_rows {
        let unavailable = "-";
        println!(
            "  {:<24} {:<9} {:<10} {:<10} {:<12} {:<5} {:<14} {:<10} {:<14} {}",
            gateway_table_candidate_label(row["candidate"].as_str().unwrap_or("-")),
            gateway_bool_label(row["preferred"].as_bool().unwrap_or(false)),
            row["stability_score"].as_u64().unwrap_or(0),
            row["breaker_state"].as_str().unwrap_or("closed"),
            row["cooldown_ms"]
                .as_u64()
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            unavailable,
            unavailable,
            row["historical_latency_ms"]
                .as_u64()
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            row["skip_reason"].as_str().unwrap_or("-"),
            unavailable
        );
    }
    if overrides.why {
        let summary = build_gateway_status_decision_summary(selected_candidate, candidate_rows);
        print_gateway_decision_summary_line(&summary);
    }
}

async fn run_cluster_gateway_explain(
    caller: &NativeCommandContext,
    config: &BmuxConfig,
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    overrides: &GatewayCommandOverrides,
    output_format: GatewayOutputFormat,
) -> Result<Option<u8>> {
    let candidates = ordered_gateway_candidates_for_cluster(cluster_name, definition)?;
    let preferred = preferred_gateway_candidate(cluster_name, gateway_success_ttl(definition));
    if output_format == GatewayOutputFormat::Text {
        print_gateway_policy_header(
            "cluster gateway explain",
            cluster_name,
            definition,
            overrides.no_failover,
        );
        print_gateway_text_table_header();
    }

    let probes = collect_gateway_explain_probes(
        caller,
        config,
        cluster_name,
        definition,
        &candidates,
        preferred.as_ref(),
        output_format,
    )
    .await;

    let mut failures = Vec::new();
    let (mut selected, attempted) = evaluate_gateway_explain_selection(
        &probes,
        definition.gateway_mode == ClusterGatewayMode::Auto,
        overrides.no_failover,
        &mut failures,
    );
    if selected.is_none() && !attempted && definition.gateway_mode == ClusterGatewayMode::Auto {
        if output_format == GatewayOutputFormat::Text {
            println!("selection note: all candidates in cooldown; simulating immediate retry");
        }
        let (retry_selected, _) = evaluate_gateway_explain_selection(
            &probes,
            false,
            overrides.no_failover,
            &mut failures,
        );
        selected = retry_selected;
    }
    emit_gateway_probe_observation(
        cluster_name,
        "cluster-gateway-explain",
        "explain",
        &probes,
        selected,
        &failures,
    );

    let selected_candidate = selected.map(|value| value.candidate.clone());
    if output_format == GatewayOutputFormat::Json {
        let payload_input = GatewayExplainJsonPayloadInput {
            cluster_name,
            definition,
            overrides,
            probes: &probes,
            preferred: preferred.as_ref(),
            failures: &failures,
            selected_candidate: selected_candidate.as_ref(),
            command_name: Some("cluster-gateway-explain"),
            observational: true,
            include_decision_summary: overrides.why,
        };
        let payload = build_gateway_explain_json_payload(&payload_input);
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed encoding gateway explain json")?
        );
        return Ok(Some(u8::from(selected.is_none())));
    }

    Ok(Some(print_gateway_explain_text_result(
        selected,
        &failures,
        overrides.why,
    )))
}

fn print_gateway_explain_text_result(
    selected: Option<&GatewayExplainCandidateProbe>,
    failures: &[GatewayAttemptFailure],
    why: bool,
) -> u8 {
    selected.map_or_else(
        || {
            println!("selection result: no executable gateway candidate");
            println!("failures: {}", format_gateway_failures(failures));
            if why {
                print_gateway_decision_summary_text(None, failures);
            }
            1
        },
        |candidate| {
            println!(
                "selection result: candidate '{}' is executable",
                candidate.candidate
            );
            if why {
                print_gateway_decision_summary_text(Some(candidate.candidate.as_str()), failures);
            }
            0
        },
    )
}

async fn run_cluster_gateway_doctor(
    caller: &NativeCommandContext,
    config: &BmuxConfig,
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    overrides: &GatewayCommandOverrides,
    output_format: GatewayOutputFormat,
) -> Result<u8> {
    let candidates = ordered_gateway_candidates_for_cluster(cluster_name, definition)?;
    let preferred = preferred_gateway_candidate(cluster_name, gateway_success_ttl(definition));
    let probes = collect_gateway_explain_probes(
        caller,
        config,
        cluster_name,
        definition,
        &candidates,
        preferred.as_ref(),
        GatewayOutputFormat::Json,
    )
    .await;
    let mut findings = build_gateway_doctor_findings(cluster_name, &probes);
    findings.sort_by_key(|finding| finding.priority);
    let slo = gateway_doctor_slo_snapshot(cluster_name, definition);
    let has_critical = findings
        .iter()
        .any(|finding| matches!(finding.severity, GatewayDoctorSeverity::Critical));
    if output_format == GatewayOutputFormat::Json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "cluster": cluster_name,
                "result": if has_critical { "critical" } else if findings.is_empty() { "healthy" } else { "warning" },
                "policy": {
                    "preset": definition.gateway_policy.map(gateway_policy_label),
                    "effective": gateway_effective_policy_values(definition),
                },
                "slo": slo,
                "findings": findings,
                "checked_candidates": probes.len(),
                "no_failover": overrides.no_failover,
            }))
            .context("failed encoding gateway doctor json")?
        );
    } else {
        let policy = gateway_effective_policy_values(definition);
        println!("cluster gateway doctor: cluster='{cluster_name}'");
        println!(
            "policy: preset={} breaker_open_after_failures={} breaker_half_open_after_ms={} breaker_half_open_required_successes={} probe_timeout_ms={} cooldown_ms={} cooldown_max_ms={} cooldown_jitter_pct={} success_ttl_ms={} history_max_entries={} history_retention_ms={}",
            definition
                .gateway_policy
                .map_or("custom", gateway_policy_label),
            policy.breaker_open_after_failures,
            policy.breaker_half_open_after_ms,
            policy.breaker_half_open_required_successes,
            policy.probe_timeout_ms,
            policy.cooldown_ms,
            policy.cooldown_max_ms,
            policy.cooldown_jitter_pct,
            policy.success_ttl_ms,
            policy.history_max_entries,
            policy.history_retention_ms
        );
        println!(
            "slo(5m): success_rate={:.2}% failover_rate={:.2}% p95_probe_latency_ms={} breaker_open_ratio={:.2}%",
            slo.success_rate, slo.failover_rate, slo.p95_probe_latency_ms, slo.breaker_open_ratio
        );
        if findings.is_empty() {
            println!("doctor result: healthy");
        } else {
            println!(
                "doctor result: {} ({} finding{})",
                if has_critical { "critical" } else { "warning" },
                findings.len(),
                if findings.len() == 1 { "" } else { "s" }
            );
            for finding in &findings {
                println!(
                    "  - priority={} severity={} candidate={} reason={} action={} next={} confidence={:.2} detail={}",
                    finding.priority,
                    gateway_doctor_severity_label(&finding.severity),
                    finding.candidate.as_deref().unwrap_or("-"),
                    finding.reason_code,
                    finding.recommended_action,
                    finding.next_command,
                    finding.confidence,
                    finding.detail
                );
            }
        }
    }

    Ok(u8::from(has_critical))
}

async fn run_cluster_gateway_why_command(
    caller: &NativeCommandContext,
    config: &BmuxConfig,
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    overrides: &GatewayCommandOverrides,
    output_format: GatewayOutputFormat,
) -> Result<u8> {
    let candidates = ordered_gateway_candidates_for_cluster(cluster_name, definition)?;
    let preferred = preferred_gateway_candidate(cluster_name, gateway_success_ttl(definition));
    let probes = collect_gateway_explain_probes(
        caller,
        config,
        cluster_name,
        definition,
        &candidates,
        preferred.as_ref(),
        GatewayOutputFormat::Json,
    )
    .await;
    let mut failures = Vec::new();
    let (selected, _) = evaluate_gateway_explain_selection(
        &probes,
        definition.gateway_mode == ClusterGatewayMode::Auto,
        overrides.no_failover,
        &mut failures,
    );
    let mut findings = build_gateway_doctor_findings(cluster_name, &probes);
    findings.sort_by_key(|finding| finding.priority);
    let policy = gateway_effective_policy_values(definition);
    let query = GatewayHistoryQuery {
        limit: Some(parse_gateway_history_limit(&overrides.passthrough_arguments)?.unwrap_or(5)),
        ..GatewayHistoryQuery::default()
    };
    let history = gateway_history_entries(cluster_name, definition, &query);
    let slo = gateway_doctor_slo_snapshot(cluster_name, definition);

    if output_format == GatewayOutputFormat::Json {
        let payload = serde_json::json!({
            "cluster": cluster_name,
            "mode": gateway_mode_label(definition.gateway_mode),
            "selected_candidate": selected.map(|value| value.candidate.clone()),
            "decision_summary": build_gateway_decision_summary(selected.map(|value| value.candidate.as_str()), &failures),
            "policy": {
                "preset": definition.gateway_policy.map(gateway_policy_label),
                "effective": policy,
            },
            "slo": slo,
            "findings": findings,
            "recent_history": history.iter().map(|entry| serde_json::json!({
                "observed_at_unix_ms": gateway_history_entry_observed_unix_ms(entry),
                "command": entry.command,
                "candidate": entry.candidate,
                "result": entry.result,
                "reason_code": entry.reason_code,
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).context("failed encoding gateway why json")?
        );
    } else {
        println!(
            "cluster gateway why: cluster='{cluster_name}' mode={}",
            gateway_mode_label(definition.gateway_mode)
        );
        println!(
            "selected: {}",
            selected.map_or_else(|| "none".to_string(), |value| value.candidate.clone())
        );
        print_gateway_decision_summary_text(
            selected.map(|value| value.candidate.as_str()),
            &failures,
        );
        println!(
            "slo(5m): success_rate={:.2}% failover_rate={:.2}% p95_probe_latency_ms={} breaker_open_ratio={:.2}%",
            slo.success_rate, slo.failover_rate, slo.p95_probe_latency_ms, slo.breaker_open_ratio
        );
        if findings.is_empty() {
            println!("actions: none (healthy)");
        } else {
            println!("top actions:");
            for finding in findings.iter().take(3) {
                println!(
                    "  - [{}] {} (next: {})",
                    gateway_doctor_severity_label(&finding.severity),
                    finding.recommended_action,
                    finding.next_command
                );
            }
        }
        if history.is_empty() {
            println!("recent history: none");
        } else {
            println!("recent history:");
            for entry in history {
                println!(
                    "  - command={} result={} candidate={} reason={}",
                    entry.command,
                    entry.result,
                    entry.candidate.as_deref().unwrap_or("-"),
                    entry.reason_code.as_deref().unwrap_or("-")
                );
            }
        }
    }

    Ok(u8::from(selected.is_none()))
}

fn build_gateway_doctor_findings(
    cluster_name: &str,
    probes: &[GatewayExplainCandidateProbe],
) -> Vec<GatewayDoctorFinding> {
    let mut findings = Vec::new();
    let all_skipped = probes.iter().all(|probe| probe.skip_reason.is_some());
    let all_unhealthy = probes.iter().all(|probe| !probe.probe.ok);
    if all_skipped {
        findings.push(GatewayDoctorFinding {
            severity: GatewayDoctorSeverity::Critical,
            candidate: None,
            reason_code: "all_candidates_skipped",
            detail: "all candidates are currently skipped by breaker/cooldown".to_string(),
            recommended_action:
                "run `cluster gateway reset --cluster <name>` or wait for cooldown/half-open"
                    .to_string(),
            priority: 1,
            confidence: 0.95,
            next_command: format!("cluster gateway why --cluster {cluster_name}"),
        });
    }
    if all_unhealthy {
        findings.push(GatewayDoctorFinding {
            severity: GatewayDoctorSeverity::Critical,
            candidate: None,
            reason_code: "all_candidates_unhealthy",
            detail: format!("all probes failed for cluster '{cluster_name}'"),
            recommended_action:
                "verify network reachability and credentials for at least one gateway target"
                    .to_string(),
            priority: 2,
            confidence: 0.95,
            next_command: format!("cluster gateway explain --cluster {cluster_name} --format json"),
        });
    }

    for probe in probes {
        if probe.skip_reason == Some("breaker_open") {
            findings.push(GatewayDoctorFinding {
                severity: GatewayDoctorSeverity::Warning,
                candidate: Some(probe.candidate.clone()),
                reason_code: "breaker_open",
                detail: "candidate is blocked by open breaker".to_string(),
                recommended_action:
                    "wait for half-open window or inspect recurring failures for this target"
                        .to_string(),
                priority: 4,
                confidence: 0.80,
                next_command: format!(
                    "cluster gateway history --cluster {cluster_name} --candidate {} --limit 10",
                    probe.candidate
                ),
            });
        }
        if probe.skip_reason == Some("cooldown") {
            findings.push(GatewayDoctorFinding {
                severity: GatewayDoctorSeverity::Info,
                candidate: Some(probe.candidate.clone()),
                reason_code: "cooldown",
                detail: "candidate is cooling down after a recent failure".to_string(),
                recommended_action:
                    "retry after cooldown or use --gateway to force a specific target".to_string(),
                priority: 6,
                confidence: 0.70,
                next_command: format!("cluster gateway status --cluster {cluster_name}"),
            });
        }
        if !probe.probe.ok
            && let Some(finding) = gateway_doctor_finding_from_reason(probe)
        {
            findings.push(finding);
        }
    }

    findings
}

fn gateway_doctor_finding_from_reason(
    probe: &GatewayExplainCandidateProbe,
) -> Option<GatewayDoctorFinding> {
    let (severity, action) = match probe.probe.reason_code {
        "auth_failed" => (
            GatewayDoctorSeverity::Critical,
            "check auth material/permissions for this target",
        ),
        "service_denied" => (
            GatewayDoctorSeverity::Critical,
            "verify service capability permissions and policy rules",
        ),
        "dns_failed" => (
            GatewayDoctorSeverity::Warning,
            "verify DNS resolution and target hostname",
        ),
        "protocol_mismatch" => (
            GatewayDoctorSeverity::Warning,
            "confirm bmux versions and protocol compatibility",
        ),
        "connection_refused" | "connect" | "unreachable" => (
            GatewayDoctorSeverity::Warning,
            "verify target service is running and reachable on network",
        ),
        "timeout" => (
            GatewayDoctorSeverity::Warning,
            "increase probe_timeout_ms or investigate high latency",
        ),
        "ok" => return None,
        _ => (
            GatewayDoctorSeverity::Warning,
            "inspect gateway logs for this target",
        ),
    };
    Some(GatewayDoctorFinding {
        severity,
        candidate: Some(probe.candidate.clone()),
        reason_code: probe.probe.reason_code,
        detail: probe.probe.detail.clone(),
        recommended_action: action.to_string(),
        priority: gateway_reason_priority(probe.probe.reason_code),
        confidence: gateway_reason_confidence(probe.probe.reason_code),
        next_command: gateway_reason_next_command(probe.probe.reason_code, &probe.candidate),
    })
}

fn gateway_reason_priority(reason_code: &str) -> u8 {
    match reason_code {
        "auth_failed" | "service_denied" => 1,
        "protocol_mismatch" | "dns_failed" => 2,
        "connection_refused" | "connect" | "unreachable" => 3,
        "timeout" => 4,
        _ => 5,
    }
}

fn gateway_reason_confidence(reason_code: &str) -> f32 {
    match reason_code {
        "auth_failed" | "service_denied" => 0.95,
        "protocol_mismatch" | "dns_failed" => 0.9,
        "connection_refused" | "connect" | "unreachable" => 0.8,
        "timeout" => 0.75,
        _ => 0.6,
    }
}

fn gateway_reason_next_command(reason_code: &str, candidate: &str) -> String {
    match reason_code {
        "auth_failed" | "service_denied" => {
            format!(
                "cluster gateway history --candidate {candidate} --reason {reason_code} --limit 10"
            )
        }
        "protocol_mismatch" => "cluster gateway doctor --format json".to_string(),
        "dns_failed" => "cluster gateway explain --format json".to_string(),
        "connection_refused" | "connect" | "unreachable" => {
            format!("cluster gateway status --gateway {candidate}")
        }
        "timeout" => "cluster gateway explain --format json --why".to_string(),
        _ => "cluster gateway why".to_string(),
    }
}

fn gateway_doctor_slo_snapshot(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
) -> GatewayDoctorSloSnapshot {
    let entries = gateway_history_entries(
        cluster_name,
        definition,
        &GatewayHistoryQuery {
            since: Some(Duration::from_mins(5)),
            ..GatewayHistoryQuery::default()
        },
    );
    let mut success = 0usize;
    let mut failure = 0usize;
    let mut failover = 0usize;
    let mut latencies = Vec::new();
    for entry in &entries {
        if entry.result == "success" {
            success = success.saturating_add(1);
        } else if entry.result == "failure" {
            failure = failure.saturating_add(1);
            if entry
                .reason_code
                .as_deref()
                .is_some_and(|code| code != "auth_failed")
            {
                failover = failover.saturating_add(1);
            }
        }
        if let Some(latency_ms) = entry.latency_ms {
            latencies.push(latency_ms);
        }
    }
    latencies.sort_unstable();
    let p95 = if latencies.is_empty() {
        0
    } else {
        let idx = (latencies.len().saturating_sub(1) * 95) / 100;
        latencies[idx]
    };
    let total = success.saturating_add(failure);
    let success_rate = if total == 0 {
        100.0
    } else {
        (count_to_f64(success) * 100.0) / count_to_f64(total)
    };
    let failover_rate = if total == 0 {
        0.0
    } else {
        (count_to_f64(failover) * 100.0) / count_to_f64(total)
    };
    let ordered =
        ordered_gateway_candidates_for_cluster(cluster_name, definition).unwrap_or_default();
    let mut open = 0usize;
    for candidate in &ordered {
        let health = gateway_effective_candidate_health(
            cluster_name,
            candidate,
            definition,
            GatewayExecutionMode::Observational,
        );
        if health.breaker_state == GatewayBreakerState::Open {
            open = open.saturating_add(1);
        }
    }
    let breaker_open_ratio = if ordered.is_empty() {
        0.0
    } else {
        (count_to_f64(open) * 100.0) / count_to_f64(ordered.len())
    };

    GatewayDoctorSloSnapshot {
        success_rate,
        failover_rate,
        p95_probe_latency_ms: p95,
        breaker_open_ratio,
    }
}

fn count_to_f64(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

const fn gateway_doctor_severity_label(severity: &GatewayDoctorSeverity) -> &'static str {
    match severity {
        GatewayDoctorSeverity::Info => "info",
        GatewayDoctorSeverity::Warning => "warning",
        GatewayDoctorSeverity::Critical => "critical",
    }
}

async fn collect_gateway_explain_probes(
    caller: &NativeCommandContext,
    _config: &BmuxConfig,
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
    candidates: &[String],
    preferred: Option<&String>,
    output_format: GatewayOutputFormat,
) -> Vec<GatewayExplainCandidateProbe> {
    let mut probes = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let cooldown = gateway_cooldown_remaining_ms(cluster_name, candidate);
        let health = gateway_effective_candidate_health(
            cluster_name,
            candidate,
            definition,
            GatewayExecutionMode::Observational,
        );
        let skip_reason = gateway_candidate_skip_reason(
            cluster_name,
            candidate,
            definition,
            definition.gateway_mode == ClusterGatewayMode::Auto,
            GatewayExecutionMode::Observational,
        );
        let probe = probe_gateway_candidate(caller, candidate, cluster_name, definition).await;
        let probe_row = GatewayExplainCandidateProbe {
            candidate: candidate.clone(),
            cooldown_ms: cooldown,
            breaker_state: health.breaker_state,
            skip_reason,
            stability_score: health.stability_score(),
            last_latency_ms: health.last_latency_ms,
            probe,
        };
        if output_format == GatewayOutputFormat::Text {
            print_gateway_explain_probe_line(&probe_row, preferred);
        }
        probes.push(probe_row);
    }
    probes
}

fn print_gateway_explain_probe_line(
    probe_row: &GatewayExplainCandidateProbe,
    preferred: Option<&String>,
) {
    println!(
        "  {:<24} {:<9} {:<10} {:<10} {:<12} {:<5} {:<14} {:<10} {:<14} {}",
        gateway_table_candidate_label(&probe_row.candidate),
        gateway_bool_label(preferred.is_some_and(|value| value == &probe_row.candidate)),
        probe_row.stability_score,
        gateway_breaker_state_label(probe_row.breaker_state),
        gateway_optional_u128_label(probe_row.cooldown_ms),
        gateway_bool_label(probe_row.probe.ok),
        probe_row.probe.reason_code,
        probe_row.probe.latency_ms,
        probe_row.skip_reason.unwrap_or("-"),
        probe_row.probe.detail
    );
}

fn emit_gateway_probe_observation(
    cluster_name: &str,
    command_name: &str,
    phase: &str,
    probes: &[GatewayExplainCandidateProbe],
    selected: Option<&GatewayExplainCandidateProbe>,
    failures: &[GatewayAttemptFailure],
) {
    let mut probe_ok = 0u64;
    let mut skip_cooldown = 0u64;
    let mut skip_breaker_open = 0u64;
    for probe in probes {
        if probe.probe.ok {
            probe_ok = probe_ok.saturating_add(1);
        }
        match probe.skip_reason {
            Some("cooldown") => skip_cooldown = skip_cooldown.saturating_add(1),
            Some("breaker_open") => skip_breaker_open = skip_breaker_open.saturating_add(1),
            _ => {}
        }
    }
    let mut failure_connect = 0u64;
    let mut failure_connection_refused = 0u64;
    let mut failure_dns = 0u64;
    let mut failure_auth = 0u64;
    let mut failure_protocol = 0u64;
    let mut failure_probe_timeout = 0u64;
    let mut failure_cooldown = 0u64;
    let mut failure_breaker_open = 0u64;
    for failure in failures {
        match failure.reason_code {
            "connect" => failure_connect = failure_connect.saturating_add(1),
            "connection_refused" => {
                failure_connection_refused = failure_connection_refused.saturating_add(1);
            }
            "dns_failed" => failure_dns = failure_dns.saturating_add(1),
            "auth_failed" => failure_auth = failure_auth.saturating_add(1),
            "protocol_mismatch" => failure_protocol = failure_protocol.saturating_add(1),
            "probe_timeout" | "timeout" => {
                failure_probe_timeout = failure_probe_timeout.saturating_add(1);
            }
            "cooldown" => failure_cooldown = failure_cooldown.saturating_add(1),
            "breaker_open" => failure_breaker_open = failure_breaker_open.saturating_add(1),
            _ => {}
        }
    }
    tracing::info!(
        event = "cluster_gateway_selection_observation",
        cluster = %cluster_name,
        command = %command_name,
        phase = %phase,
        candidates_total = probes.len(),
        probes_ok = probe_ok,
        skip_cooldown,
        skip_breaker_open,
        failure_connect,
        failure_connection_refused,
        failure_dns,
        failure_auth,
        failure_protocol,
        failure_probe_timeout,
        failure_cooldown,
        failure_breaker_open,
        selected = selected.map_or("none", |value| value.candidate.as_str()),
        result = if selected.is_some() { "success" } else { "failure" },
        "observed gateway candidate evaluation"
    );
}

fn gateway_table_candidate_label(candidate: &str) -> String {
    let char_count = candidate.chars().count();
    if char_count <= GATEWAY_TABLE_CANDIDATE_WIDTH {
        return candidate.to_string();
    }
    let keep = GATEWAY_TABLE_CANDIDATE_WIDTH.saturating_sub(3);
    let mut shortened = candidate.chars().take(keep).collect::<String>();
    shortened.push_str("...");
    shortened
}

fn emit_gateway_batch_failure_summary(
    cluster_name: &str,
    command_name: &str,
    failures: &[GatewayAttemptFailure],
) {
    let mut skipped_cooldown = 0u64;
    let mut skipped_breaker_open = 0u64;
    let mut failed_connect = 0u64;
    let mut failed_connection_refused = 0u64;
    let mut failed_dns = 0u64;
    let mut failed_auth = 0u64;
    let mut failed_protocol = 0u64;
    let mut failed_probe_timeout = 0u64;
    let mut failed_other = 0u64;
    for failure in failures {
        match failure.reason_code {
            "cooldown" => skipped_cooldown = skipped_cooldown.saturating_add(1),
            "breaker_open" => skipped_breaker_open = skipped_breaker_open.saturating_add(1),
            "connect" => failed_connect = failed_connect.saturating_add(1),
            "connection_refused" => {
                failed_connection_refused = failed_connection_refused.saturating_add(1);
            }
            "dns_failed" => failed_dns = failed_dns.saturating_add(1),
            "auth_failed" => failed_auth = failed_auth.saturating_add(1),
            "protocol_mismatch" => failed_protocol = failed_protocol.saturating_add(1),
            "probe_timeout" | "timeout" => {
                failed_probe_timeout = failed_probe_timeout.saturating_add(1);
            }
            _ => failed_other = failed_other.saturating_add(1),
        }
    }
    tracing::warn!(
        event = "cluster_gateway_selection_failed",
        cluster = %cluster_name,
        command = %command_name,
        failures_total = failures.len(),
        skipped_cooldown,
        skipped_breaker_open,
        failed_connect,
        failed_connection_refused,
        failed_dns,
        failed_auth,
        failed_protocol,
        failed_probe_timeout,
        failed_other,
        "all gateway candidates exhausted"
    );
}

fn print_gateway_text_table_header() {
    println!(
        "  {:<24} {:<9} {:<10} {:<10} {:<12} {:<5} {:<14} {:<10} {:<14} detail",
        "candidate",
        "preferred",
        "stability",
        "breaker",
        "cooldown_ms",
        "ok",
        "reason",
        "latency_ms",
        "skip"
    );
}

fn print_gateway_decision_summary_text(
    selected_candidate: Option<&str>,
    failures: &[GatewayAttemptFailure],
) {
    let summary = build_gateway_decision_summary(selected_candidate, failures);
    print_gateway_decision_summary_line(&summary);
}

fn print_gateway_decision_summary_line(summary: &serde_json::Value) {
    println!(
        "why: selected={} attempted_failures={} top_reasons={}",
        summary["selected_candidate"].as_str().unwrap_or("none"),
        summary["attempted_failures"].as_u64().unwrap_or(0),
        summary["top_reasons"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "none".to_string())
    );
}

fn build_gateway_decision_summary(
    selected_candidate: Option<&str>,
    failures: &[GatewayAttemptFailure],
) -> serde_json::Value {
    let mut reason_counts = BTreeMap::<&'static str, u64>::new();
    for failure in failures {
        let count = reason_counts.entry(failure.reason_code).or_insert(0);
        *count = count.saturating_add(1);
    }
    let mut top_reasons = reason_counts
        .iter()
        .map(|(reason, count)| (*reason, *count))
        .collect::<Vec<_>>();
    top_reasons.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    serde_json::json!({
        "selected_candidate": selected_candidate.unwrap_or("none"),
        "attempted_failures": failures.len(),
        "top_reasons": top_reasons
            .iter()
            .take(3)
            .map(|(reason, _)| *reason)
            .collect::<Vec<_>>(),
    })
}

fn build_gateway_status_decision_summary(
    selected_candidate: Option<&str>,
    candidate_rows: &[serde_json::Value],
) -> serde_json::Value {
    let mut pseudo_failures = Vec::new();
    for row in candidate_rows {
        if let Some(reason) = row["skip_reason"].as_str()
            && reason != "-"
        {
            pseudo_failures.push(GatewayAttemptFailure {
                candidate: row["candidate"].as_str().unwrap_or("-").to_string(),
                reason_code: match reason {
                    "cooldown" => "cooldown",
                    "breaker_open" => "breaker_open",
                    _ => "skipped",
                },
                detail: format!("candidate skipped due to {reason}"),
            });
        }
    }
    build_gateway_decision_summary(selected_candidate, &pseudo_failures)
}

fn build_gateway_explain_json_payload(
    input: &GatewayExplainJsonPayloadInput<'_>,
) -> serde_json::Value {
    let policy = gateway_effective_policy_values(input.definition);
    let selected_ok = input.selected_candidate.is_some();
    let probe_rows = input
        .probes
        .iter()
        .map(|probe| {
            serde_json::json!({
                "candidate": probe.candidate,
                "preferred": input.preferred.is_some_and(|value| value == &probe.candidate),
                "stability_score": probe.stability_score,
                "breaker_state": gateway_breaker_state_label(probe.breaker_state),
                "cooldown_ms": probe.cooldown_ms.map(u128_to_u64_saturating),
                "skip_reason": probe.skip_reason,
                "historical_latency_ms": probe.last_latency_ms,
                "ok": probe.probe.ok,
                "reason": probe.probe.reason_code,
                "latency_ms": u128_to_u64_saturating(probe.probe.latency_ms),
                "detail": probe.probe.detail,
            })
        })
        .collect::<Vec<_>>();
    let failure_rows = input
        .failures
        .iter()
        .map(|failure| {
            serde_json::json!({
                "candidate": failure.candidate,
                "reason": failure.reason_code,
                "detail": failure.detail,
            })
        })
        .collect::<Vec<_>>();

    let mut payload = serde_json::json!({
        "cluster": input.cluster_name,
        "command": input.command_name,
        "mode": gateway_mode_label(input.definition.gateway_mode),
        "policy": {
            "preset": input.definition.gateway_policy.map(gateway_policy_label),
            "breaker_open_after_failures": policy.breaker_open_after_failures,
            "breaker_half_open_after_ms": policy.breaker_half_open_after_ms,
            "breaker_half_open_required_successes": policy.breaker_half_open_required_successes,
            "probe_timeout_ms": policy.probe_timeout_ms,
            "cooldown_ms": policy.cooldown_ms,
            "cooldown_max_ms": policy.cooldown_max_ms,
            "cooldown_jitter_pct": policy.cooldown_jitter_pct,
            "success_ttl_ms": policy.success_ttl_ms,
            "history_max_entries": policy.history_max_entries,
            "history_retention_ms": policy.history_retention_ms,
        },
        "no_failover": input.overrides.no_failover,
        "overrides": {
            "policy": input.overrides.gateway_policy.map(gateway_policy_label),
        },
        "selected_candidate": input.selected_candidate,
        "result": if selected_ok {
            "success"
        } else {
            "failure"
        },
        "would_mutate": {
            "enabled": !input.observational,
            "last_good": !input.observational,
            "cooldown": !input.observational,
            "breaker": !input.observational,
            "persistence_write": !input.observational,
        },
        "probes": probe_rows,
        "failures": failure_rows,
    });
    if input.include_decision_summary {
        payload["decision_summary"] = build_gateway_decision_summary(
            input.selected_candidate.map(String::as_str),
            input.failures,
        );
    }
    payload
}

fn evaluate_gateway_explain_selection<'a>(
    probes: &'a [GatewayExplainCandidateProbe],
    respect_cooldown: bool,
    no_failover: bool,
    failures: &mut Vec<GatewayAttemptFailure>,
) -> (Option<&'a GatewayExplainCandidateProbe>, bool) {
    let mut attempted = false;
    for candidate in probes {
        if candidate.breaker_state == GatewayBreakerState::Open {
            failures.push(GatewayAttemptFailure {
                candidate: candidate.candidate.clone(),
                reason_code: "breaker_open",
                detail: "candidate skipped due to breaker_open".to_string(),
            });
            continue;
        }
        if respect_cooldown && candidate.cooldown_ms.is_some() {
            failures.push(GatewayAttemptFailure {
                candidate: candidate.candidate.clone(),
                reason_code: "cooldown",
                detail: "candidate skipped due to cooldown".to_string(),
            });
            continue;
        }

        attempted = true;
        if candidate.probe.ok {
            return (Some(candidate), attempted);
        }

        failures.push(GatewayAttemptFailure {
            candidate: candidate.candidate.clone(),
            reason_code: candidate.probe.reason_code,
            detail: candidate.probe.detail.clone(),
        });
        if no_failover {
            break;
        }
    }
    (None, attempted)
}

async fn probe_gateway_candidate(
    caller: &NativeCommandContext,
    candidate: &str,
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
) -> GatewayProbeResult {
    let started = Instant::now();
    let probe_timeout_ms = gateway_probe_timeout_ms(definition);
    let result = tokio::time::timeout(
        Duration::from_millis(probe_timeout_ms),
        run_plugin_command_on_target(
            caller,
            candidate,
            "bmux.cluster",
            "cluster-status",
            &[cluster_name.to_string()],
        ),
    )
    .await;
    match result {
        Ok(Ok(_)) => GatewayProbeResult {
            ok: true,
            reason_code: "ok",
            detail: "gateway command bridge reachable".to_string(),
            latency_ms: started.elapsed().as_millis(),
        },
        Ok(Err(error)) => {
            let classified = classify_gateway_error(&error);
            GatewayProbeResult {
                ok: false,
                reason_code: classified.0,
                detail: classified.1,
                latency_ms: started.elapsed().as_millis(),
            }
        }
        Err(_) => GatewayProbeResult {
            ok: false,
            reason_code: "timeout",
            detail: format!("probe timed out after {probe_timeout_ms}ms"),
            latency_ms: started.elapsed().as_millis(),
        },
    }
}

fn gateway_cooldown_remaining_ms(cluster_name: &str, candidate: &str) -> Option<u128> {
    ensure_gateway_runtime_state_loaded();
    let until = cluster_gateway_state_map()
        .lock()
        .ok()?
        .get(cluster_name)?
        .cooldown_until
        .get(candidate)
        .copied()?;
    let now = Instant::now();
    if until <= now {
        return None;
    }
    Some((until - now).as_millis())
}

#[cfg(test)]
fn clear_gateway_runtime_state_for_tests() {
    if let Ok(mut state_map) = cluster_gateway_state_map().lock() {
        state_map.clear();
    }
    let _ = clear_cluster_gateway_runtime_state(gateway_paths());
}

fn gateway_candidates_for_cluster(
    cluster_name: &str,
    definition: &ClusterGatewayDefinition,
) -> Result<Vec<String>> {
    let mut ordered = match definition.gateway_mode {
        ClusterGatewayMode::Direct => Vec::new(),
        ClusterGatewayMode::Pinned => {
            let target = definition
                .gateway_target
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "cluster '{cluster_name}' uses gateway_mode='pinned' but gateway_target is missing"
                    )
                })?;
            vec![target.to_string()]
        }
        ClusterGatewayMode::Auto => {
            if definition.gateway_candidates.is_empty() {
                definition.declared_targets()
            } else {
                definition.gateway_candidates.clone()
            }
        }
    };

    let mut seen = BTreeSet::new();
    ordered.retain(|candidate| {
        let trimmed = candidate.trim();
        !trimmed.is_empty() && seen.insert(trimmed.to_string())
    });
    if ordered.is_empty() {
        anyhow::bail!(
            "cluster '{cluster_name}' has no gateway candidates; configure targets or gateway_candidates"
        );
    }
    Ok(ordered)
}

fn cluster_gateway_target_from_host_ref(host: &ClusterGatewayHostRef) -> Option<String> {
    match host {
        ClusterGatewayHostRef::Target(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        ClusterGatewayHostRef::Object { target, host, name } => target
            .as_deref()
            .or(host.as_deref())
            .or(name.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
    }
}

fn dedupe_preserve_order(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::with_capacity(values.len());
    for value in values {
        if seen.insert(value.clone()) {
            deduped.push(value);
        }
    }
    deduped
}

async fn run_plugin_command_on_target(
    caller: &NativeCommandContext,
    target: &str,
    plugin_id: &str,
    command_name: &str,
    arguments: &[String],
) -> Result<u8> {
    let mut remote_arguments = arguments.to_vec();
    remote_arguments.push("--gateway-mode".to_string());
    remote_arguments.push("direct".to_string());
    let command = PluginCliCommandRequest::new(
        plugin_id.to_string(),
        command_name.to_string(),
        remote_arguments,
    );
    let payload = bmux_plugin_sdk::encode_service_message(&command)
        .context("failed encoding plugin command request")?;
    let response = crate::endpoint::EndpointDispatchClient::new(caller, target)
        .invoke_service_raw(
            bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY,
            bmux_ipc::InvokeServiceKind::Command,
            bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1,
            bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
            payload,
        )
        .await
        .context("connections plugin endpoint dispatch failed")?;
    let response: PluginCliCommandResponse = bmux_plugin_sdk::decode_service_message(&response)
        .context("failed decoding remote plugin command response")?;
    if let Some(error) = response.error {
        anyhow::bail!(
            "gateway plugin command failed on target '{target}': {error} (exit_code={})",
            response.exit_code
        );
    }
    u8::try_from(response.exit_code.clamp(0, i32::from(u8::MAX)))
        .context("gateway plugin command returned out-of-range exit code")
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn current_unix_timestamp_ms_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, duration_millis_u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    use serial_test::serial;

    fn definition(mode: ClusterGatewayMode) -> ClusterGatewayDefinition {
        ClusterGatewayDefinition {
            gateway_mode: mode,
            targets: vec!["first".to_string(), "second".to_string()],
            ..ClusterGatewayDefinition::default()
        }
    }

    #[test]
    fn direct_override_bypasses_gateway_recursion() {
        let parsed = parse_gateway_overrides(&[
            "--gateway-mode".to_string(),
            "direct".to_string(),
            "cluster-a".to_string(),
        ])
        .expect("parse overrides");
        assert_eq!(parsed.gateway_mode, Some(ClusterGatewayMode::Direct));
        assert_eq!(parsed.passthrough_arguments, vec!["cluster-a"]);
    }

    #[test]
    fn auto_candidates_default_to_declared_targets() {
        let candidates =
            gateway_candidates_for_cluster("cluster-a", &definition(ClusterGatewayMode::Auto))
                .expect("candidates");
        assert_eq!(candidates, vec!["first", "second"]);
    }

    #[test]
    fn pinned_mode_requires_explicit_target() {
        let error =
            gateway_candidates_for_cluster("cluster-a", &definition(ClusterGatewayMode::Pinned))
                .expect_err("missing target");
        assert!(error.to_string().contains("gateway_target is missing"));
    }

    #[test]
    fn gateway_policy_presets_are_distinct() {
        let aggressive = gateway_policy_values_for_preset(GatewayPolicyPreset::Aggressive);
        let conservative = gateway_policy_values_for_preset(GatewayPolicyPreset::Conservative);
        assert!(aggressive.probe_timeout_ms < conservative.probe_timeout_ms);
        assert!(aggressive.cooldown_ms < conservative.cooldown_ms);
    }

    #[test]
    fn success_and_failure_update_breaker_state() {
        let definition = definition(ClusterGatewayMode::Auto);
        let mut state = BTreeMap::new();
        for _ in 0..DEFAULT_CLUSTER_GATEWAY_BREAKER_OPEN_AFTER_FAILURES {
            record_gateway_failure_in_state(
                &mut state,
                "cluster-a",
                "first",
                &definition,
                10,
                "transport",
            );
        }
        assert_eq!(
            state["cluster-a"].candidate_health["first"].breaker_state,
            GatewayBreakerState::Open
        );
        state
            .get_mut("cluster-a")
            .unwrap()
            .candidate_health
            .get_mut("first")
            .unwrap()
            .breaker_state = GatewayBreakerState::HalfOpen;
        record_gateway_success_in_state(&mut state, "cluster-a", "first", &definition, 5);
        assert_eq!(
            state["cluster-a"].candidate_health["first"].breaker_state,
            GatewayBreakerState::HalfOpen
        );
    }

    #[test]
    fn persisted_state_round_trips() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = ConfigPaths::new(
            root.path().join("config"),
            root.path().join("runtime"),
            root.path().join("data"),
            root.path().join("state"),
        );
        let definition = definition(ClusterGatewayMode::Auto);
        let mut state = BTreeMap::new();
        record_gateway_success_in_state(&mut state, "cluster-a", "first", &definition, 7);
        save_cluster_gateway_runtime_state(&paths, &state).expect("save");
        let loaded = load_cluster_gateway_runtime_state(&paths).expect("load");
        assert_eq!(
            loaded["cluster-a"]
                .last_good
                .as_ref()
                .map(|entry| entry.target.as_str()),
            Some("first")
        );
    }
    #[test]
    fn resolve_cluster_name_for_gateway_prefers_explicit_cluster_flag() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([("prod".to_string(), ClusterGatewayDefinition::default())]),
        };

        let cluster = resolve_cluster_name_for_gateway(
            "cluster-events",
            &["--cluster".to_string(), "prod".to_string()],
            &settings,
        )
        .expect("cluster flag should resolve");
        assert_eq!(cluster.as_deref(), Some("prod"));

        let cluster_inline = resolve_cluster_name_for_gateway(
            "cluster-events",
            &["--cluster=prod".to_string()],
            &settings,
        )
        .expect("inline cluster flag should resolve");
        assert_eq!(cluster_inline.as_deref(), Some("prod"));
    }

    #[test]
    fn resolve_cluster_name_for_gateway_uses_single_cluster_default() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([("prod".to_string(), ClusterGatewayDefinition::default())]),
        };

        let cluster = resolve_cluster_name_for_gateway("cluster-pane-retry", &[], &settings)
            .expect("single-cluster default should resolve");
        assert_eq!(cluster.as_deref(), Some("prod"));
    }

    #[test]
    fn resolve_cluster_name_for_gateway_infers_cluster_from_host_when_unique() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([
                (
                    "prod".to_string(),
                    ClusterGatewayDefinition {
                        targets: vec!["prod-a".to_string()],
                        ..ClusterGatewayDefinition::default()
                    },
                ),
                (
                    "staging".to_string(),
                    ClusterGatewayDefinition {
                        targets: vec!["staging-a".to_string()],
                        ..ClusterGatewayDefinition::default()
                    },
                ),
            ]),
        };

        let cluster = resolve_cluster_name_for_gateway(
            "cluster-pane-new",
            &["--host".to_string(), "prod-a".to_string()],
            &settings,
        )
        .expect("unique host should infer cluster");
        assert_eq!(cluster.as_deref(), Some("prod"));
    }

    #[test]
    fn resolve_cluster_name_for_gateway_requires_cluster_for_retry_in_multi_cluster() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([
                ("prod".to_string(), ClusterGatewayDefinition::default()),
                ("staging".to_string(), ClusterGatewayDefinition::default()),
            ]),
        };

        let error = resolve_cluster_name_for_gateway("cluster-pane-retry", &[], &settings)
            .expect_err("retry should require --cluster in multi-cluster mode");
        assert!(error.to_string().contains("requires --cluster"));
    }

    #[test]
    fn resolve_cluster_name_for_gateway_rejects_ambiguous_host_mapping() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([
                (
                    "prod".to_string(),
                    ClusterGatewayDefinition {
                        targets: vec!["db-a".to_string()],
                        ..ClusterGatewayDefinition::default()
                    },
                ),
                (
                    "staging".to_string(),
                    ClusterGatewayDefinition {
                        targets: vec!["db-a".to_string()],
                        ..ClusterGatewayDefinition::default()
                    },
                ),
            ]),
        };

        let error = resolve_cluster_name_for_gateway(
            "cluster-pane-new",
            &["--host".to_string(), "db-a".to_string()],
            &settings,
        )
        .expect_err("ambiguous host mapping should hard fail");
        assert!(error.to_string().contains("matches multiple clusters"));
    }

    #[test]
    fn resolve_cluster_name_for_gateway_status_accepts_positional_cluster() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([("prod".to_string(), ClusterGatewayDefinition::default())]),
        };

        let cluster = resolve_cluster_name_for_gateway(
            "cluster-gateway-status",
            &["prod".to_string()],
            &settings,
        )
        .expect("gateway status positional cluster should resolve");
        assert_eq!(cluster.as_deref(), Some("prod"));
    }

    #[test]
    fn resolve_cluster_name_for_gateway_status_requires_cluster_in_multi_cluster() {
        let settings = ClusterGatewaySettings {
            clusters: BTreeMap::from([
                ("prod".to_string(), ClusterGatewayDefinition::default()),
                ("staging".to_string(), ClusterGatewayDefinition::default()),
            ]),
        };

        let error = resolve_cluster_name_for_gateway("cluster-gateway-status", &[], &settings)
            .expect_err("gateway status should require --cluster in multi-cluster mode");
        assert!(error.to_string().contains("requires --cluster"));
    }

    #[test]
    fn parse_gateway_overrides_strips_gateway_flags() {
        let overrides = parse_gateway_overrides(&[
            "--gateway".to_string(),
            "db-b".to_string(),
            "--gateway-mode=auto".to_string(),
            "--gateway-policy".to_string(),
            "aggressive".to_string(),
            "--gateway-no-failover".to_string(),
            "--dry-run".to_string(),
            "--why".to_string(),
            "--cluster".to_string(),
            "prod".to_string(),
        ])
        .expect("gateway overrides should parse");

        assert_eq!(overrides.gateway_target.as_deref(), Some("db-b"));
        assert_eq!(overrides.gateway_mode, Some(ClusterGatewayMode::Auto));
        assert_eq!(
            overrides.gateway_policy,
            Some(GatewayPolicyPreset::Aggressive)
        );
        assert!(overrides.no_failover);
        assert!(overrides.dry_run);
        assert!(overrides.why);
        assert_eq!(
            overrides.passthrough_arguments,
            vec!["--cluster".to_string(), "prod".to_string()]
        );
    }

    #[test]
    fn parse_gateway_policy_value_supports_presets() {
        assert_eq!(
            parse_gateway_policy_value("balanced").expect("balanced should parse"),
            GatewayPolicyPreset::Balanced
        );
        assert_eq!(
            parse_gateway_policy_value("aggressive").expect("aggressive should parse"),
            GatewayPolicyPreset::Aggressive
        );
        assert_eq!(
            parse_gateway_policy_value("conservative").expect("conservative should parse"),
            GatewayPolicyPreset::Conservative
        );
    }

    #[test]
    fn classify_gateway_error_detects_enriched_reasons() {
        let dns = anyhow::anyhow!("failed to lookup address: no such host");
        assert_eq!(classify_gateway_error(&dns).0, "dns_failed");

        let auth = anyhow::anyhow!("permission denied (publickey)");
        assert_eq!(classify_gateway_error(&auth).0, "service_denied");

        let protocol = anyhow::anyhow!("protocol version mismatch");
        assert_eq!(classify_gateway_error(&protocol).0, "protocol_mismatch");

        let refused = anyhow::anyhow!("connection refused");
        assert_eq!(classify_gateway_error(&refused).0, "connection_refused");
    }

    #[test]
    fn parse_gateway_output_format_defaults_to_text() {
        let format = parse_gateway_output_format(&[]).expect("default gateway format should parse");
        assert_eq!(format, GatewayOutputFormat::Text);
    }

    #[test]
    fn parse_gateway_output_format_supports_json() {
        let format = parse_gateway_output_format(&["--format".to_string(), "json".to_string()])
            .expect("json gateway format should parse");
        assert_eq!(format, GatewayOutputFormat::Json);
    }

    #[test]
    fn parse_duration_literal_supports_gateway_history_since_units() {
        assert_eq!(
            parse_duration_literal("250ms")
                .expect("ms duration should parse")
                .as_millis(),
            250
        );
        assert_eq!(
            parse_duration_literal("15")
                .expect("plain duration should parse as seconds")
                .as_secs(),
            15
        );
        assert_eq!(
            parse_duration_literal("2m")
                .expect("minute duration should parse")
                .as_secs(),
            120
        );
    }

    #[test]
    fn gateway_table_candidate_label_keeps_short_names() {
        let value = gateway_table_candidate_label("db-a");
        assert_eq!(value, "db-a");
    }

    #[test]
    fn gateway_table_candidate_label_truncates_long_names() {
        let value = gateway_table_candidate_label("very-long-gateway-candidate-name-prod-a");
        assert_eq!(value.chars().count(), GATEWAY_TABLE_CANDIDATE_WIDTH);
        assert!(value.ends_with("..."));
    }

    #[test]
    fn parse_gateway_reset_scope_requires_cluster_without_all() {
        let error = parse_gateway_reset_scope(&[])
            .expect_err("reset scope should require --cluster unless --all");
        assert!(
            error
                .to_string()
                .contains("requires --cluster unless --all is passed")
        );
    }

    #[test]
    fn parse_gateway_reset_scope_accepts_all() {
        let scope = parse_gateway_reset_scope(&["--all".to_string()])
            .expect("--all reset scope should parse");
        assert!(matches!(scope, GatewayResetScope::All));
    }

    #[test]
    fn parse_gateway_reset_scope_rejects_mixed_all_and_cluster() {
        let error = parse_gateway_reset_scope(&[
            "--all".to_string(),
            "--cluster".to_string(),
            "prod".to_string(),
        ])
        .expect_err("mixed reset scope should fail");
        assert!(error.to_string().contains("either --all or --cluster"));
    }

    #[test]
    fn evaluate_gateway_explain_selection_respects_no_failover() {
        let probes = vec![
            GatewayExplainCandidateProbe {
                candidate: "db-a".to_string(),
                cooldown_ms: None,
                breaker_state: GatewayBreakerState::Closed,
                skip_reason: None,
                stability_score: 0,
                last_latency_ms: None,
                probe: GatewayProbeResult {
                    ok: false,
                    reason_code: "unreachable",
                    detail: "failed reaching db-a".to_string(),
                    latency_ms: 11,
                },
            },
            GatewayExplainCandidateProbe {
                candidate: "db-b".to_string(),
                cooldown_ms: None,
                breaker_state: GatewayBreakerState::Closed,
                skip_reason: None,
                stability_score: 0,
                last_latency_ms: None,
                probe: GatewayProbeResult {
                    ok: true,
                    reason_code: "ok",
                    detail: "reachable".to_string(),
                    latency_ms: 9,
                },
            },
        ];
        let mut failures = Vec::new();
        let (selected, attempted) =
            evaluate_gateway_explain_selection(&probes, false, true, &mut failures);

        assert!(attempted);
        assert!(selected.is_none());
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].candidate, "db-a");
    }

    #[test]
    fn evaluate_gateway_explain_selection_skips_cooldown_then_retries() {
        let probes = vec![
            GatewayExplainCandidateProbe {
                candidate: "db-a".to_string(),
                cooldown_ms: Some(3000),
                breaker_state: GatewayBreakerState::Closed,
                skip_reason: Some("cooldown"),
                stability_score: 0,
                last_latency_ms: None,
                probe: GatewayProbeResult {
                    ok: false,
                    reason_code: "timeout",
                    detail: "timeout".to_string(),
                    latency_ms: 15,
                },
            },
            GatewayExplainCandidateProbe {
                candidate: "db-b".to_string(),
                cooldown_ms: Some(5000),
                breaker_state: GatewayBreakerState::Closed,
                skip_reason: Some("cooldown"),
                stability_score: 0,
                last_latency_ms: None,
                probe: GatewayProbeResult {
                    ok: true,
                    reason_code: "ok",
                    detail: "reachable".to_string(),
                    latency_ms: 7,
                },
            },
        ];

        let mut failures = Vec::new();
        let (selected, attempted) =
            evaluate_gateway_explain_selection(&probes, true, false, &mut failures);
        assert!(selected.is_none());
        assert!(!attempted);
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].reason_code, "cooldown");

        let (retry_selected, retry_attempted) =
            evaluate_gateway_explain_selection(&probes, false, false, &mut failures);
        assert!(retry_attempted);
        assert_eq!(
            retry_selected.map(|value| value.candidate.as_str()),
            Some("db-b")
        );
    }

    #[test]
    #[serial]
    fn ordered_gateway_candidates_prioritizes_recent_success() {
        clear_gateway_runtime_state_for_tests();
        let definition = ClusterGatewayDefinition {
            targets: vec!["db-a".to_string(), "db-b".to_string()],
            gateway_mode: ClusterGatewayMode::Auto,
            ..ClusterGatewayDefinition::default()
        };
        record_gateway_success("prod", "db-b", &definition, 10);

        let ordered = ordered_gateway_candidates_for_cluster("prod", &definition)
            .expect("ordered candidates should resolve");
        assert_eq!(ordered, vec!["db-b".to_string(), "db-a".to_string()]);
    }

    #[test]
    fn breaker_opens_after_three_consecutive_failures() {
        let mut state = BTreeMap::new();
        let definition = ClusterGatewayDefinition {
            targets: vec!["db-a".to_string()],
            gateway_mode: ClusterGatewayMode::Auto,
            ..ClusterGatewayDefinition::default()
        };
        record_gateway_failure_in_state(&mut state, "prod", "db-a", &definition, 30, "timeout");
        record_gateway_failure_in_state(&mut state, "prod", "db-a", &definition, 40, "timeout");
        record_gateway_failure_in_state(&mut state, "prod", "db-a", &definition, 50, "timeout");

        let (health, _) = gateway_effective_candidate_health_in_state(
            &mut state,
            "prod",
            "db-a",
            GatewayExecutionMode::Observational,
        );
        assert_eq!(health.breaker_state, GatewayBreakerState::Open);
        assert_eq!(health.consecutive_failures, 3);
    }

    #[test]
    fn half_open_requires_multiple_successes_before_closing() {
        let mut state = BTreeMap::new();
        let definition = ClusterGatewayDefinition {
            targets: vec!["db-a".to_string()],
            gateway_mode: ClusterGatewayMode::Auto,
            breaker_open_after_failures: Some(1),
            breaker_half_open_after_ms: Some(1),
            breaker_half_open_required_successes: Some(2),
            ..ClusterGatewayDefinition::default()
        };

        record_gateway_failure_in_state(&mut state, "prod", "db-a", &definition, 30, "timeout");
        std::thread::sleep(Duration::from_millis(3));
        let (half_open, _) = gateway_effective_candidate_health_in_state(
            &mut state,
            "prod",
            "db-a",
            GatewayExecutionMode::Mutating,
        );
        assert_eq!(half_open.breaker_state, GatewayBreakerState::HalfOpen);

        record_gateway_success_in_state(&mut state, "prod", "db-a", &definition, 20);
        let (after_one_success, _) = gateway_effective_candidate_health_in_state(
            &mut state,
            "prod",
            "db-a",
            GatewayExecutionMode::Observational,
        );
        assert_eq!(
            after_one_success.breaker_state,
            GatewayBreakerState::HalfOpen
        );

        record_gateway_success_in_state(&mut state, "prod", "db-a", &definition, 20);
        let (after_second_success, _) = gateway_effective_candidate_health_in_state(
            &mut state,
            "prod",
            "db-a",
            GatewayExecutionMode::Observational,
        );
        assert_eq!(
            after_second_success.breaker_state,
            GatewayBreakerState::Closed
        );
    }

    #[test]
    fn adaptive_cooldown_level_increments_on_failure_and_resets_on_success() {
        let mut state = BTreeMap::new();
        let definition = ClusterGatewayDefinition {
            targets: vec!["db-a".to_string()],
            gateway_mode: ClusterGatewayMode::Auto,
            ..ClusterGatewayDefinition::default()
        };

        record_gateway_failure_in_state(&mut state, "prod", "db-a", &definition, 10, "timeout");
        record_gateway_failure_in_state(&mut state, "prod", "db-a", &definition, 11, "timeout");
        let (after_failures, _) = gateway_effective_candidate_health_in_state(
            &mut state,
            "prod",
            "db-a",
            GatewayExecutionMode::Observational,
        );
        assert_eq!(after_failures.adaptive_cooldown_level, 2);

        record_gateway_success_in_state(&mut state, "prod", "db-a", &definition, 5);
        let (after_success, _) = gateway_effective_candidate_health_in_state(
            &mut state,
            "prod",
            "db-a",
            GatewayExecutionMode::Observational,
        );
        assert_eq!(after_success.adaptive_cooldown_level, 0);
    }

    #[test]
    fn adaptive_cooldown_duration_caps_at_policy_max() {
        let definition = ClusterGatewayDefinition {
            cooldown_ms: Some(1_000),
            cooldown_max_ms: Some(4_000),
            cooldown_jitter_pct: Some(0),
            ..ClusterGatewayDefinition::default()
        };
        assert_eq!(
            gateway_failure_cooldown_for_level(&definition, "db-a", 1, "timeout").as_millis(),
            1_000
        );
        assert_eq!(
            gateway_failure_cooldown_for_level(&definition, "db-a", 2, "timeout").as_millis(),
            2_000
        );
        assert_eq!(
            gateway_failure_cooldown_for_level(&definition, "db-a", 4, "timeout").as_millis(),
            4_000
        );
    }

    #[test]
    #[serial]
    fn gateway_history_entries_respect_since_and_limit() {
        clear_gateway_runtime_state_for_tests();
        record_gateway_history_entry(
            "prod",
            &ClusterGatewayDefinition::default(),
            &GatewayHistoryRecordInput {
                command_name: "cluster-status",
                candidate: Some("db-a"),
                execution_mode: GatewayExecutionMode::Observational,
                latency_ms: Some(10),
                result: "observed_failure",
                reason_code: Some("timeout"),
                selected: false,
            },
        );
        record_gateway_history_entry(
            "prod",
            &ClusterGatewayDefinition::default(),
            &GatewayHistoryRecordInput {
                command_name: "cluster-status",
                candidate: Some("db-b"),
                execution_mode: GatewayExecutionMode::Observational,
                latency_ms: Some(5),
                result: "observed_success",
                reason_code: None,
                selected: true,
            },
        );
        if let Ok(mut state_map) = cluster_gateway_state_map().lock()
            && let Some(cluster_state) = state_map.get_mut("prod")
            && let Some(first) = cluster_state.history.first_mut()
        {
            first.observed_at = Instant::now()
                .checked_sub(Duration::from_hours(1))
                .expect("checked_sub should support one hour");
        }

        let recent = gateway_history_entries(
            "prod",
            &ClusterGatewayDefinition::default(),
            &GatewayHistoryQuery {
                since: Some(Duration::from_mins(1)),
                ..GatewayHistoryQuery::default()
            },
        );
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].candidate.as_deref(), Some("db-b"));

        let limited = gateway_history_entries(
            "prod",
            &ClusterGatewayDefinition::default(),
            &GatewayHistoryQuery {
                limit: Some(1),
                ..GatewayHistoryQuery::default()
            },
        );
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].candidate.as_deref(), Some("db-b"));
    }

    #[test]
    #[serial]
    fn ordered_gateway_candidates_prioritize_stability_before_latency() {
        clear_gateway_runtime_state_for_tests();
        let definition = ClusterGatewayDefinition {
            targets: vec!["db-a".to_string(), "db-b".to_string()],
            gateway_mode: ClusterGatewayMode::Auto,
            ..ClusterGatewayDefinition::default()
        };

        record_gateway_success("prod", "db-a", &definition, 120);
        record_gateway_success("prod", "db-b", &definition, 10);
        record_gateway_failure("prod", "db-b", &definition, 15, "timeout");

        let ordered = ordered_gateway_candidates_for_cluster("prod", &definition)
            .expect("ordered candidates should resolve");
        assert_eq!(ordered.first().map(String::as_str), Some("db-a"));
    }

    #[test]
    fn gateway_candidates_auto_accepts_hosts_object_entries() {
        let definition = ClusterGatewayDefinition {
            hosts: vec![
                ClusterGatewayHostRef::Object {
                    target: Some("db-a".to_string()),
                    host: None,
                    name: None,
                },
                ClusterGatewayHostRef::Target("db-b".to_string()),
            ],
            gateway_mode: ClusterGatewayMode::Auto,
            ..ClusterGatewayDefinition::default()
        };

        let candidates = gateway_candidates_for_cluster("prod", &definition)
            .expect("hosts entries should be valid candidates");
        assert_eq!(candidates, vec!["db-a".to_string(), "db-b".to_string()]);
    }
}
