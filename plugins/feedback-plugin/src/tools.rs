//! The tools bmux offers the intake agent while it triages bmux's feedback.

use bcode_session_bridge::models::bcode_feedback_models::{
    AttachFeedbackRequest, FeedbackId, FeedbackItem,
};
use bcode_session_bridge::models::{ToolDescriptor, ToolInvocation, ToolOutcome, ToolPolicy};
use bcode_session_bridge::{BridgeClient, BridgeClientError};
use bmux_plugin_sdk::PluginHostServices;
use serde::Deserialize;

use crate::declaration::requires::{recording_commands, recording_state};
use crate::server::{OPENED_FACET, epoch_millis};
use crate::settings::Settings;

/// Which span of the rolling recording is available around a feedback item.
pub const TIMELINE: &str = "bmux.recording_timeline";
/// Cut a span of the rolling recording and attach it to a feedback item.
pub const CAPTURE: &str = "bmux.capture_recording";

/// Media type of an attached bmux recording directory (replay it with `bmux recording`).
const RECORDING_MEDIA_TYPE: &str = "application/vnd.bmux.recording";
/// Longest a tool call may take.
const TOOL_DEADLINE_MS: u32 = 60_000;

/// What the intake agent reads about bmux's feedback, next to each item.
pub const INSTRUCTIONS: &str = "\
This feedback was given in bmux, a terminal multiplexer, about what was on screen. \
`bmux.opened_epoch_ms` is when the person started writing it; the other `bmux.*` facets \
describe the focused session, tab and pane. bmux keeps a rolling recording of the screen. \
When seeing the screen would help understand or reproduce the feedback, call \
`bmux.recording_timeline` to see which span is still recorded, then \
`bmux.capture_recording` with the narrowest span that shows the problem, usually the \
seconds before `bmux.opened_epoch_ms`; the recording is attached to the item. Do not \
capture when the text alone is enough. Typed input is never captured.";

/// bmux's tools, registered under the `bmux` provider.
fn descriptors(max_capture_seconds: u64) -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: TIMELINE.to_owned(),
            description: "Which span of bmux's rolling screen recording is still available \
                          around a bmux feedback item, in Unix epoch milliseconds."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "feedback_id": {"type": "string", "description": "The item, such as fb-12"},
                },
                "required": ["feedback_id"],
                "additionalProperties": false,
            }),
            policy: ToolPolicy::ReadOnly,
            max_duration_ms: TOOL_DEADLINE_MS,
        },
        ToolDescriptor {
            name: CAPTURE.to_owned(),
            description: format!(
                "Cut a span of bmux's rolling screen recording (at most {max_capture_seconds} \
                 seconds, without typed input) and attach it to a bmux feedback item."
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "feedback_id": {"type": "string", "description": "The item, such as fb-12"},
                    "start_epoch_ms": {"type": "integer", "minimum": 0},
                    "end_epoch_ms": {"type": "integer", "minimum": 0},
                    "description": {
                        "type": "string",
                        "description": "What the span shows, for whoever reads the item",
                    },
                },
                "required": ["feedback_id", "start_epoch_ms", "end_epoch_ms"],
                "additionalProperties": false,
            }),
            // It reads the screen recording and adds it to the item being triaged; nothing of
            // the person's changes, so it never waits for permission in the intake session.
            policy: ToolPolicy::ReadOnly,
            max_duration_ms: TOOL_DEADLINE_MS,
        },
    ]
}

/// Register bmux's tools with Bcode.
///
/// # Errors
/// Bcode's refusal or a transport failure.
pub async fn register(client: &BridgeClient, settings: &Settings) -> Result<(), BridgeClientError> {
    client
        .register_tools(
            crate::APP_ID,
            descriptors(settings.max_capture_seconds),
            Vec::new(),
        )
        .await
}

/// Answer one tool call.
pub async fn answer(
    host: PluginHostServices,
    settings: Settings,
    client: BridgeClient,
    call: ToolInvocation,
) {
    let outcome = match call.tool_name.as_str() {
        TIMELINE => timeline(&host, &client, &call.arguments_json).await,
        CAPTURE => capture(&host, &settings, &client, &call.arguments_json).await,
        other => Err(failure("unknown_tool", format!("bmux has no tool {other}"))),
    };
    let outcome = outcome.unwrap_or_else(|failed| failed);
    if let Err(error) = client.tool_result(&call.invocation_id, outcome).await {
        tracing::warn!(%error, tool = %call.tool_name, "bmux.feedback: tool answer not delivered");
    }
}

type Answer = Result<ToolOutcome, ToolOutcome>;

fn failure(code: &str, message: impl Into<String>) -> ToolOutcome {
    ToolOutcome::Failed {
        code: code.to_owned(),
        message: message.into(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelineArgs {
    feedback_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureArgs {
    feedback_id: String,
    start_epoch_ms: u64,
    end_epoch_ms: u64,
    #[serde(default)]
    description: Option<String>,
}

fn arguments<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, ToolOutcome> {
    serde_json::from_str(json).map_err(|error| failure("invalid_arguments", error.to_string()))
}

/// The bmux feedback item `id` names.
async fn item(client: &BridgeClient, id: &str) -> Result<FeedbackItem, ToolOutcome> {
    let id: FeedbackId = id
        .parse()
        .map_err(|_| failure("invalid_arguments", format!("{id} is not a feedback id")))?;
    let item = client
        .feedback_get(id)
        .await
        .map_err(|error| failure("feedback_unavailable", error.to_string()))?;
    if item.app != crate::APP_ID {
        return Err(failure(
            "not_bmux_feedback",
            format!("{id} was not given in bmux, so bmux has no recording of it"),
        ));
    }
    Ok(item)
}

fn opened_epoch_ms(item: &FeedbackItem) -> Option<u64> {
    item.facets
        .iter()
        .find(|facet| facet.key == OPENED_FACET)
        .and_then(|facet| facet.value.parse().ok())
}

/// The recorded span still available now, as `(start, end)` epoch milliseconds, or why
/// there is none.
async fn available(host: &PluginHostServices) -> Result<(u64, u64), String> {
    let status = recording_state::v1::Client::remote(host.clone())
        .rolling_status()
        .await
        .map_err(|error| format!("the recording is unavailable: {}", error.message))?;
    let active = status.active.filter(|_| status.available).ok_or_else(|| {
        "bmux's rolling recording is off, so there is nothing to capture".to_owned()
    })?;
    let now = epoch_millis();
    let window_start = status.rolling_window_secs.map_or(0, |seconds| {
        now.saturating_sub(seconds.saturating_mul(1000))
    });
    Ok((active.started_epoch_ms.max(window_start), now))
}

async fn timeline(host: &PluginHostServices, client: &BridgeClient, json: &str) -> Answer {
    let args: TimelineArgs = arguments(json)?;
    let item = item(client, &args.feedback_id).await?;
    let recording = match available(host).await {
        Ok((start, end)) => serde_json::json!({
            "available": true,
            "start_epoch_ms": start,
            "end_epoch_ms": end,
        }),
        Err(reason) => serde_json::json!({"available": false, "reason": reason}),
    };
    let answer = serde_json::json!({
        "feedback_id": item.id.to_string(),
        "opened_epoch_ms": opened_epoch_ms(&item),
        "now_epoch_ms": epoch_millis(),
        "recording": recording,
    });
    Ok(ToolOutcome::Completed {
        content: answer.to_string(),
    })
}

/// Why a requested span cannot be captured, if it cannot.
fn check_span(start: u64, end: u64, now: u64, max_seconds: u64) -> Result<(), String> {
    if start >= end {
        return Err("start_epoch_ms must be before end_epoch_ms".to_owned());
    }
    if end > now {
        return Err(format!(
            "end_epoch_ms {end} is in the future (now {now}); capture after it has passed"
        ));
    }
    if end - start > max_seconds.saturating_mul(1000) {
        return Err(format!(
            "the span is longer than {max_seconds} seconds; capture less"
        ));
    }
    Ok(())
}

async fn capture(
    host: &PluginHostServices,
    settings: &Settings,
    client: &BridgeClient,
    json: &str,
) -> Answer {
    let args: CaptureArgs = arguments(json)?;
    check_span(
        args.start_epoch_ms,
        args.end_epoch_ms,
        epoch_millis(),
        settings.max_capture_seconds,
    )
    .map_err(|message| failure("invalid_span", message))?;
    let item = item(client, &args.feedback_id).await?;
    let commands = recording_commands::v3::Client::remote(host.clone());
    let cut = commands
        .cut(
            None,
            Some(format!("feedback {}", item.id)),
            Some(args.start_epoch_ms),
            Some(args.end_epoch_ms),
            Some(true),
        )
        .await
        .map_err(|error| failure("recording_unavailable", error.message))?
        .map_err(|error| failure("capture_failed", format!("{error:?}")))?;
    let name = format!(
        "bmux-recording-{}-{}",
        args.start_epoch_ms, args.end_epoch_ms
    );
    let attached = client
        .feedback_attach(AttachFeedbackRequest {
            id: item.id,
            source: cut.path.clone(),
            name: name.clone(),
            media_type: RECORDING_MEDIA_TYPE.to_owned(),
            description: args.description,
            sensitive: false,
        })
        .await;
    // The item keeps its own copy; bmux's cut is no longer needed.
    if let Err(error) = commands.delete(cut.id).await {
        tracing::debug!(?error, "bmux.feedback: captured cut not removed");
    }
    attached.map_err(|error| failure("attach_failed", error.to_string()))?;
    Ok(ToolOutcome::Completed {
        content: format!(
            "Attached {name} to {}: {} events from {} to {} (typed input left out).",
            item.id, cut.event_count, args.start_epoch_ms, args.end_epoch_ms
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_must_be_past_ordered_and_short() {
        assert!(check_span(1_000, 5_000, 10_000, 120).is_ok());
        assert!(check_span(5_000, 5_000, 10_000, 120).is_err());
        assert!(check_span(1_000, 11_000, 10_000, 120).is_err());
        assert!(check_span(0, 9_000, 10_000, 5).is_err());
    }

    #[test]
    fn tools_take_closed_argument_objects() {
        for tool in descriptors(120) {
            assert!(tool.name.starts_with("bmux."));
            assert_eq!(tool.input_schema["additionalProperties"], false);
        }
        assert!(arguments::<CaptureArgs>(r#"{"feedback_id":"fb-1"}"#).is_err());
        let args: CaptureArgs =
            arguments(r#"{"feedback_id":"fb-1","start_epoch_ms":1,"end_epoch_ms":2}"#).unwrap();
        assert_eq!(args.end_epoch_ms, 2);
    }
}
