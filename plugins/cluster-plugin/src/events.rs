#![allow(clippy::wildcard_imports)] // Private domain modules share crate-private models.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterEventsFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterEventsArgs {
    pub format: ClusterEventsFormat,
    pub cluster: Option<String>,
    pub target: Option<String>,
    pub state: Option<ClusterConnectionState>,
    pub since_unix_ms: Option<u64>,
    pub limit: Option<usize>,
}

pub fn filter_cluster_events(
    events: Vec<ClusterConnectionEvent>,
    args: &ClusterEventsArgs,
) -> Vec<ClusterConnectionEvent> {
    let mut filtered = events
        .into_iter()
        .filter(|event| {
            if let Some(cluster) = args.cluster.as_deref()
                && event.cluster.as_deref() != Some(cluster)
            {
                return false;
            }
            if let Some(target) = args.target.as_deref()
                && event.target.as_deref() != Some(target)
            {
                return false;
            }
            if let Some(state) = args.state.as_ref()
                && &event.state != state
            {
                return false;
            }
            if let Some(since_unix_ms) = args.since_unix_ms
                && event.ts_unix_ms < since_unix_ms
            {
                return false;
            }
            true
        })
        .collect::<Vec<_>>();
    if let Some(limit) = args.limit
        && filtered.len() > limit
    {
        let to_drop = filtered.len() - limit;
        filtered.drain(0..to_drop);
    }
    filtered
}

pub fn parse_cluster_events_args(arguments: &[String]) -> Result<ClusterEventsArgs, String> {
    let mut format = ClusterEventsFormat::Text;
    let mut cluster = None;
    let mut target = None;
    let mut state = None;
    let mut since_unix_ms = None;
    let mut limit = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--format" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--format requires a value".to_string())?;
            format = parse_cluster_events_format_value(value)?;
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--format=") {
            format = parse_cluster_events_format_value(value)?;
            index += 1;
            continue;
        }
        if argument == "--cluster" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--cluster requires a value".to_string())?;
            cluster = normalized_non_empty(value);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--cluster=") {
            cluster = normalized_non_empty(value);
            index += 1;
            continue;
        }
        if argument == "--target" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--target requires a value".to_string())?;
            target = normalized_non_empty(value);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--target=") {
            target = normalized_non_empty(value);
            index += 1;
            continue;
        }
        if argument == "--state" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--state requires a value".to_string())?;
            state = Some(parse_cluster_connection_state(value)?);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--state=") {
            state = Some(parse_cluster_connection_state(value)?);
            index += 1;
            continue;
        }
        if argument == "--limit" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--limit requires a value".to_string())?;
            limit = Some(parse_cluster_events_limit(value)?);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--limit=") {
            limit = Some(parse_cluster_events_limit(value)?);
            index += 1;
            continue;
        }
        if argument == "--since" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| "--since requires a value".to_string())?;
            since_unix_ms = Some(parse_cluster_events_since(value)?);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--since=") {
            since_unix_ms = Some(parse_cluster_events_since(value)?);
            index += 1;
            continue;
        }
        index += 1;
    }
    Ok(ClusterEventsArgs {
        format,
        cluster,
        target,
        state,
        since_unix_ms,
        limit,
    })
}

pub fn parse_cluster_events_format_value(value: &str) -> Result<ClusterEventsFormat, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "text" => Ok(ClusterEventsFormat::Text),
        "json" => Ok(ClusterEventsFormat::Json),
        _ => Err(format!(
            "invalid --format value '{value}' (expected: text|json)"
        )),
    }
}

pub fn parse_cluster_connection_state(value: &str) -> Result<ClusterConnectionState, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "connecting" => Ok(ClusterConnectionState::Connecting),
        "ready" => Ok(ClusterConnectionState::Ready),
        "degraded" => Ok(ClusterConnectionState::Degraded),
        "retrying" => Ok(ClusterConnectionState::Retrying),
        "failed" => Ok(ClusterConnectionState::Failed),
        _ => Err(format!(
            "invalid --state value '{value}' (expected: connecting|ready|degraded|retrying|failed)"
        )),
    }
}

pub fn parse_cluster_events_limit(value: &str) -> Result<usize, String> {
    let parsed = value
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("invalid --limit value '{value}' (expected positive integer)"))?;
    if parsed == 0 {
        return Err("--limit must be greater than zero".to_string());
    }
    Ok(parsed)
}

pub fn parse_cluster_events_since(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("now") || trimmed == "0" {
        return Ok(now_unix_ms());
    }
    if let Ok(absolute_unix_ms) = trimmed.parse::<u64>() {
        return Ok(absolute_unix_ms);
    }

    let duration_ms = parse_relative_duration_ms(trimmed).map_err(|reason| {
        format!(
            "invalid --since value '{value}' ({reason}; expected 'now', '0', unix ms integer, or relative duration like 500ms, 30s, 15m, 2h, 1d, 1h30m)"
        )
    })?;
    Ok(now_unix_ms().saturating_sub(duration_ms))
}

pub fn parse_relative_duration_ms(value: &str) -> Result<u64, &'static str> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err("duration is empty");
    }

    let bytes = normalized.as_bytes();
    let mut index = 0_usize;
    let mut total_ms = 0_u64;

    while index < bytes.len() {
        let number_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if number_start == index {
            return Err("duration segment is missing a numeric value");
        }

        let amount = normalized[number_start..index]
            .parse::<u64>()
            .map_err(|_| "duration segment numeric value is invalid")?;

        let (unit_ms, unit_len) = relative_duration_unit(&bytes[index..])?;
        index += unit_len;
        let segment_ms = amount
            .checked_mul(unit_ms)
            .ok_or("duration segment overflows supported range")?;
        total_ms = total_ms
            .checked_add(segment_ms)
            .ok_or("duration overflows supported range")?;
    }

    Ok(total_ms)
}

pub fn relative_duration_unit(remaining: &[u8]) -> Result<(u64, usize), &'static str> {
    if remaining.starts_with(b"ms") {
        return Ok((1_u64, 2));
    }
    let Some(first) = remaining.first().copied() else {
        return Err("duration segment is missing a unit");
    };
    match first {
        b's' => Ok((1_000_u64, 1)),
        b'm' => Ok((60_000_u64, 1)),
        b'h' => Ok((3_600_000_u64, 1)),
        b'd' => Ok((86_400_000_u64, 1)),
        _ => Err("duration segment has an unsupported unit"),
    }
}

pub fn normalized_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub const fn connection_state_label(state: ClusterConnectionState) -> &'static str {
    match state {
        ClusterConnectionState::Connecting => "connecting",
        ClusterConnectionState::Ready => "ready",
        ClusterConnectionState::Degraded => "degraded",
        ClusterConnectionState::Retrying => "retrying",
        ClusterConnectionState::Failed => "failed",
    }
}
