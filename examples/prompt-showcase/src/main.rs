use anyhow::{Context, Result, bail};
use bmux_cli::attach::{
    self, AttachExitReason, PromptOption, PromptRequest, PromptResponse, PromptSubmitError,
    PromptValue,
};
use bmux_client::BmuxClient;
use bmux_sandbox_harness::SandboxHarness;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Typed dispatch for `sessions-commands:new-session`. `BmuxClient`
/// no longer exposes a `new_session` convenience method; callers route
/// through the typed sessions-plugin API.
async fn typed_new_session(client: &mut BmuxClient, name: Option<String>) -> Result<Uuid> {
    #[derive(serde::Serialize)]
    struct Args {
        name: Option<String>,
    }
    let payload = bmux_codec::to_vec(&Args { name }).context("encoding new-session args")?;
    let bytes = client
        .invoke_service_raw(
            "bmux.sessions.write",
            bmux_ipc::InvokeServiceKind::Command,
            "sessions-commands",
            "new-session",
            payload,
        )
        .await
        .context("new-session invoke failed")?;
    let outcome: std::result::Result<
        bmux_sessions_plugin_api::sessions_commands::SessionAck,
        bmux_sessions_plugin_api::sessions_commands::NewSessionError,
    > = bmux_codec::from_bytes(&bytes).context("decoding new-session response")?;
    outcome
        .map(|ack| ack.id)
        .map_err(|err| anyhow::anyhow!("new-session failed: {err:?}"))
}

const PROMPT_HOST_WAIT_TIMEOUT: Duration = Duration::from_secs(8);
const PROMPT_HOST_WAIT_POLL: Duration = Duration::from_millis(75);

#[tokio::main]
async fn main() -> Result<()> {
    let sandbox = SandboxHarness::start()
        .await
        .context("failed to start sandbox harness")?;

    let run_result = run_showcase(&sandbox).await;
    let shutdown_result = sandbox
        .shutdown(false)
        .await
        .context("failed shutting down sandbox harness");

    run_result?;
    shutdown_result
}

async fn run_showcase(sandbox: &SandboxHarness) -> Result<()> {
    let mut attach_client = sandbox
        .connect("bmux-prompt-showcase")
        .await
        .context("failed connecting to sandbox")?;
    let session_id = typed_new_session(&mut attach_client, Some("prompt-showcase".to_string()))
        .await
        .context("failed creating prompt showcase session")?;

    let target = session_id.to_string();

    println!("bmux prompt showcase");
    println!("sandbox root: {}", sandbox.root_dir().display());
    println!("session id: {session_id}");
    println!("detach when done with: Ctrl+b then d");
    println!();

    let prompt_task = tokio::spawn(async { run_prompt_sequence().await });

    let attach_outcome = attach::run_with_client(attach_client, Some(&target), None, false)
        .await
        .context("attach runtime failed")?;

    let prompt_result = prompt_task
        .await
        .context("prompt sequence task join failed")?;

    println!();
    println!(
        "attach exit reason: {}",
        exit_reason_name(attach_outcome.exit_reason)
    );

    match prompt_result {
        Ok(lines) => {
            println!("prompt results:");
            for line in lines {
                println!("- {line}");
            }
        }
        Err(error) => {
            println!("prompt sequence ended early: {error:#}");
        }
    }

    Ok(())
}

async fn run_prompt_sequence() -> Result<Vec<String>> {
    let mut lines = Vec::new();

    let confirm = request_prompt_with_retry(|| {
        PromptRequest::confirm("Prompt Showcase")
            .message("We will run confirm, text input, single select, and multi toggle prompts.")
            .submit_label("Continue")
            .cancel_label("Stop")
            .confirm_default(true)
            .confirm_labels("Continue", "Stop")
    })
    .await?;
    lines.push(format_prompt_line("confirm", &confirm));

    if !is_confirmed(&confirm) {
        lines.push("showcase cancelled before running remaining prompt types".to_string());
        return Ok(lines);
    }

    let text_input = request_prompt_with_retry(|| {
        PromptRequest::text_input("Project Label")
            .message("Enter a short label to tag this demo run.")
            .input_placeholder("demo-123")
            .input_required(true)
            .submit_label("Save")
            .cancel_label("Skip")
    })
    .await?;
    lines.push(format_prompt_line("text_input", &text_input));

    let single_select = request_prompt_with_retry(|| {
        PromptRequest::single_select(
            "Preferred Layout",
            vec![
                PromptOption::new("tall", "Tall stack"),
                PromptOption::new("wide", "Wide split"),
                PromptOption::new("grid", "Grid"),
            ],
        )
        .message("Pick a layout style for this session.")
        .single_default_index(1)
        .submit_label("Select")
        .cancel_label("Skip")
    })
    .await?;
    lines.push(format_prompt_line("single_select", &single_select));

    let multi_toggle = request_prompt_with_retry(|| {
        PromptRequest::multi_toggle(
            "Enable Features",
            vec![
                PromptOption::new("line-numbers", "Line numbers"),
                PromptOption::new("timestamps", "Timestamps"),
                PromptOption::new("soft-wrap", "Soft wrap"),
                PromptOption::new("status-icons", "Status icons"),
            ],
        )
        .message("Toggle one or more simulated features.")
        .multi_defaults(vec![0, 2])
        .multi_min_selected(1)
        .submit_label("Apply")
        .cancel_label("Skip")
    })
    .await?;
    lines.push(format_prompt_line("multi_toggle", &multi_toggle));

    let done = request_prompt_with_retry(|| {
        PromptRequest::confirm("Showcase Complete")
            .message("Prompt showcase is complete. Press Ctrl+b then d to detach.")
            .submit_label("Got it")
            .cancel_label("Repeat")
            .confirm_default(true)
            .confirm_labels("Got it", "Repeat")
    })
    .await?;
    lines.push(format_prompt_line("completion", &done));

    Ok(lines)
}

async fn request_prompt_with_retry<F>(build: F) -> Result<PromptResponse>
where
    F: Fn() -> PromptRequest,
{
    let started = Instant::now();

    loop {
        match attach::request_prompt(build()).await {
            Ok(response) => return Ok(response),
            Err(PromptSubmitError::HostUnavailable)
                if started.elapsed() < PROMPT_HOST_WAIT_TIMEOUT =>
            {
                tokio::time::sleep(PROMPT_HOST_WAIT_POLL).await;
            }
            Err(PromptSubmitError::HostUnavailable) => {
                bail!("prompt host did not become available in time")
            }
            Err(PromptSubmitError::HostDisconnected) => {
                bail!("prompt host disconnected during showcase")
            }
        }
    }
}

fn is_confirmed(response: &PromptResponse) -> bool {
    matches!(
        response,
        PromptResponse::Submitted(PromptValue::Confirm(true))
    )
}

fn format_prompt_line(label: &str, response: &PromptResponse) -> String {
    let value = match response {
        PromptResponse::Submitted(PromptValue::Confirm(value)) => format!("confirm={value}"),
        PromptResponse::Submitted(PromptValue::Text(value)) => format!("text={value}"),
        PromptResponse::Submitted(PromptValue::Single(value)) => format!("single={value}"),
        PromptResponse::Submitted(PromptValue::Multi(values)) => {
            format!("multi={}", values.join(", "))
        }
        PromptResponse::Cancelled => "cancelled".to_string(),
        PromptResponse::RejectedBusy => "rejected_busy".to_string(),
    };
    format!("{label}: {value}")
}

const fn exit_reason_name(reason: AttachExitReason) -> &'static str {
    match reason {
        AttachExitReason::Detached => "detached",
        AttachExitReason::StreamClosed => "stream_closed",
        AttachExitReason::Quit => "quit",
    }
}
