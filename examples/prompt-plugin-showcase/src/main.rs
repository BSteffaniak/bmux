use anyhow::{Context, Result};
use bmux_cli::attach::{self, AttachExitReason};
use bmux_sandbox_harness::SandboxHarness;

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
        .connect("bmux-plugin-prompt-showcase")
        .await
        .context("failed connecting to sandbox")?;
    let session_id = attach_client
        .new_session(Some("plugin-prompt-showcase".to_string()))
        .await
        .context("failed creating plugin prompt showcase session")?;

    let target = session_id.to_string();

    println!("bmux plugin prompt showcase");
    println!("sandbox root: {}", sandbox.root_dir().display());
    println!("session id: {session_id}");
    println!("prompts are requested from examples/native-plugin");
    println!("detach when done with: Ctrl+b then d");
    println!();

    let prompt_task =
        tokio::spawn(async { bmux_example_native_plugin::run_prompt_showcase_sequence().await });

    let attach_outcome = attach::run_with_client(attach_client, Some(&target), None, false)
        .await
        .context("attach runtime failed")?;

    let prompt_result = prompt_task
        .await
        .context("plugin prompt sequence task join failed")?;

    println!();
    println!(
        "attach exit reason: {}",
        exit_reason_name(attach_outcome.exit_reason)
    );

    match prompt_result {
        Ok(lines) => {
            println!("plugin prompt results:");
            for line in lines {
                println!("- {line}");
            }
        }
        Err(error) => {
            println!("plugin prompt sequence ended early: {error}");
        }
    }

    Ok(())
}

const fn exit_reason_name(reason: AttachExitReason) -> &'static str {
    match reason {
        AttachExitReason::Detached => "detached",
        AttachExitReason::StreamClosed => "stream_closed",
        AttachExitReason::Quit => "quit",
    }
}
