//! Native recording-process commands. Programs are always direct argv, never shell text.
use anyhow::{Context, Result, bail, ensure};
use bmux_plugin_sdk::NativeCommandContext;
use bmux_recording_plugin_api::{
    recording_process_v4 as process, recording_types::RecordingProfile,
};
use std::collections::BTreeMap;
use std::io::Read;
use uuid::Uuid;

pub(crate) fn run(context: &NativeCommandContext) -> i32 {
    let operation = context.command.trim_start_matches("recording-process-");
    if context
        .arguments
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--help" || arg == "-h")
    {
        help(operation);
        return 0;
    }
    let owned = context.clone();
    let operation_owned = operation.to_owned();
    let result = std::thread::spawn(move || -> Result<(serde_json::Value, i32)> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(execute(&owned, &operation_owned))
    })
    .join()
    .map_err(|_| anyhow::anyhow!("recording command worker panicked"))
    .and_then(|result| result);
    match result {
        Ok((value, code)) => {
            println!("{value}");
            code
        }
        Err(error) => {
            eprintln!("recording process {operation}: {error}");
            println!(
                "{}",
                serde_json::json!({"schema_version":4,"ok":false,"error":error.to_string()})
            );
            1
        }
    }
}

pub(crate) async fn execute_interactive_start(
    context: &NativeCommandContext,
) -> Result<(serde_json::Value, i32)> {
    let args = Arguments::parse(&context.arguments)?;
    program_options(&args)?;
    let launch_key = args
        .one("idempotency-key")?
        .context("interactive launch identity missing")?
        .parse::<Uuid>()?;
    crate::process_bootstrap::record_launch(context, launch_key, &context.arguments, None)?;
    let mut dedicated = crate::process_bootstrap::bootstrap(context).await?;
    // Probe this exact isolated server before target admission. An advertised
    // facade alone cannot prove its pane provider has coherent presentation.
    let capabilities =
        bmux_recording_plugin_api::recording_process_presentation_v1::client::capabilities(
            &mut dedicated.client,
        )
        .await
        .context(
            "interactive presentation capability negotiation unavailable; target not launched",
        )?
        .map_err(|error| {
            anyhow::anyhow!(
                "interactive presentation capability negotiation: {error:?}; target not launched"
            )
        })?;
    validate_interactive_capabilities(&capabilities)?;
    let wait =
        bmux_recording_plugin_api::recording_process_presentation_wait_v1::client::capabilities(
            &mut dedicated.client,
        )
        .await
        .context("interactive wait capability unavailable; target not launched")?;
    ensure!(
        wait.schema_version == 1
            && wait.token_bytes == 32
            && wait.max_timeout_ms == 1000
            && wait.max_response_bytes == 4 * 1024 * 1024,
        "unsupported interactive wait capability; target not launched"
    );
    command_start(context, "start", &args, &mut dedicated, launch_key).await
}

fn validate_interactive_capabilities(
    capabilities: &bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::PresentationCapabilities,
) -> Result<()> {
    ensure!(
        capabilities.schema_version == 1
            && capabilities.coherent_screen
            && capabilities.canonical_resize,
        "unsupported interactive presentation capabilities; requires schema1 coherent-screen and canonical-resize; target not launched"
    );
    Ok(())
}

pub(crate) async fn execute(
    context: &NativeCommandContext,
    operation: &str,
) -> Result<(serde_json::Value, i32)> {
    let args = Arguments::parse(&context.arguments)?;
    validate_operation(operation, &args)?;
    // Reject unsupported selectors and malformed launch options before admitting
    // any durable key or creating a dedicated runtime.
    if matches!(operation, "start" | "run") {
        program_options(&args)?;
    }
    if !matches!(operation, "start" | "run")
        && let Some((root, status)) = crate::process_bootstrap::offline(context, args.id()?)?
    {
        let status = if matches!(operation, "status" | "wait") && !status.cleanup_complete {
            let runtime = std::path::Path::new(&status.runtime_root);
            match crate::process_bootstrap::connect_finalized(runtime).await {
                Ok(mut client) => {
                    crate::process_runtime::refresh_finalized_cleanup(
                        &mut client,
                        &crate::process_store::ProcessStore::new(&root)?,
                        &std::sync::Mutex::new(()),
                        status,
                    )
                    .await?
                }
                Err(_) => status, // Durable evidence remains readable offline.
            }
        } else {
            status
        };
        return offline_command(context, operation, &args, &root, status).await;
    }
    let launch_key = args
        .one("idempotency-key")?
        .map(Uuid::parse_str)
        .transpose()?
        .unwrap_or_else(Uuid::new_v4);
    if matches!(operation, "start" | "run") {
        if let Some(id) =
            crate::process_bootstrap::launch_receipt(context, launch_key, &context.arguments)?
        {
            if let Some((_, status)) = crate::process_bootstrap::offline(context, id)? {
                return completed_command(operation, status);
            }
            let mut existing = crate::process_bootstrap::resolve(context, id).await?;
            let options = program_options(&args)?;
            let root = existing.root.to_string_lossy().into_owned();
            // Replay the exact admission, not an unversioned status shortcut.
            // Service-side durable intent decides whether launch is still Reserved;
            // unknown outcomes cannot authorize another execution.
            let status =
                process::client::start(&mut existing.client, id, launch_key, options, root)
                    .await?
                    .map_err(|error| anyhow::anyhow!("launch reconciliation: {error:?}"))?;
            if operation == "run" {
                return complete_native_run(&mut existing.client, status).await;
            }
            return completed_command(operation, status);
        }
        crate::process_bootstrap::record_launch(context, launch_key, &context.arguments, None)?;
    }
    let mut dedicated = if matches!(operation, "start" | "run") {
        args.validate(
            &[
                "profile",
                "viewport",
                "cwd",
                "env",
                "idempotency-key",
                "json",
            ],
            true,
        )?;
        ensure!(!args.argv.is_empty(), "start/run requires -- PROGRAM ARGS");
        viewport(args.one("viewport")?.unwrap_or("80x24"))?;
        crate::process_bootstrap::bootstrap(context).await?
    } else {
        crate::process_bootstrap::resolve(context, args.id()?).await?
    };
    match operation {
        "start" | "run" => {
            command_start(context, operation, &args, &mut dedicated, launch_key).await
        }
        "status" => command_status(&args, &mut dedicated).await,
        "read" => command_read(&args, &mut dedicated).await,
        "screen" => command_screen(&args, &mut dedicated).await,
        "send" => command_send(&args, &mut dedicated, launch_key).await,
        "wait" => command_wait(&args, &mut dedicated).await,
        "terminate" => command_terminate(&args, &mut dedicated).await,
        "finish" => command_finish(&args, &mut dedicated).await,
        _ => bail!("unknown process operation {operation}"),
    }
}

async fn command_start(
    context: &NativeCommandContext,
    operation: &str,
    args: &Arguments,
    dedicated: &mut crate::process_bootstrap::DedicatedRuntime,
    launch_key: Uuid,
) -> Result<(serde_json::Value, i32)> {
    let runtime_root = dedicated.root.to_string_lossy().into_owned();
    let options = program_options(args)?;
    let retained = {
        let path =
            crate::process_bootstrap::retain_launch_root(context, launch_key, &dedicated.root)?;
        // Durable caller identity and routing precede the first service effect.
        // A lost reply can reconnect only this exact admitted isolated runtime.
        crate::process_bootstrap::remember(context, launch_key, &dedicated.root)?;
        crate::process_bootstrap::record_launch(
            context,
            launch_key,
            &context.arguments,
            Some(launch_key),
        )?;
        dedicated.armed = false;
        Some(path)
    };
    let client = &mut dedicated.client;
    let value = {
        let root = runtime_root.clone();
        let status = process::client::start(&mut *client, launch_key, launch_key, options, root)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))
            .and_then(|result| result.map_err(|error| anyhow::anyhow!("start failed: {error:?}")))
            .map_err(|error| if let Some(path) = &retained {
                anyhow::anyhow!("launch outcome unresolved: {error}; recovery_required=true; launch_mutation_id={launch_key}; retained_runtime_root={runtime_root}; recovery_record={}; refusing relaunch or implicit cleanup", path.display())
            } else { error })?;
        let indexed = (|| -> Result<()> {
            if let Some(path) = &retained {
                crate::process_bootstrap::record_native_launch_identity(
                    path,
                    launch_key,
                    &dedicated.root,
                    status.run_id,
                    true,
                )?;
            }
            crate::process_bootstrap::remember(context, status.run_id, &dedicated.root)?;
            crate::process_bootstrap::record_launch(
                context,
                launch_key,
                &context.arguments,
                Some(status.run_id),
            )?;
            if let Some(path) = &retained {
                crate::process_bootstrap::record_native_launch_identity(
                    path,
                    launch_key,
                    &dedicated.root,
                    status.run_id,
                    false,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = indexed {
            if let Some(path) = &retained {
                return Err(error.context(format!("metadata publication incomplete; recovery_required=true; launch_mutation_id={launch_key}; run_id={}; retained_runtime_root={runtime_root}; recovery_record={}; refusing implicit termination", status.run_id, path.display())));
            }
            let cleanup = process::client::terminate(&mut *client, status.run_id, 0).await;
            return Err(error.context(format!("metadata failure; process cleanup: {cleanup:?}")));
        }
        dedicated.armed = false;
        if matches!(status.phase, process::RunPhase::Failed) {
            let cleanup = crate::process_bootstrap::cleanup(context, status.run_id).await;
            ensure!(cleanup.is_ok(), "failed launch cleanup failed: {cleanup:?}");
            return Ok((serde_json::to_value(status)?, 1));
        }
        if operation == "run" {
            return complete_native_run(client, status).await;
        }
        serde_json::to_value(status)?
    };
    Ok((value, 0))
}

async fn complete_native_run(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    mut status: process::RunStatus,
) -> Result<(serde_json::Value, i32)> {
    // Native foreground completion joins the same run until the terminal session
    // has ended and canonical capture is ready; leader exit alone is insufficient.
    while !status.output_drained && !status.capture_finalized {
        ensure!(
            status.errors.is_empty(),
            "native capture incomplete: {:?}",
            status.errors
        );
        status = process::client::wait(client, status.run_id, 1000)
            .await?
            .map_err(|error| anyhow::anyhow!("native wait failed: {error:?}"))?;
    }
    complete_run(client, status).await
}

async fn complete_run(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    mut status: process::RunStatus,
) -> Result<(serde_json::Value, i32)> {
    // Initial launch and matching retries join the same execution authority.
    // Disconnect never requests termination or admits another launch.
    while matches!(
        status.phase,
        process::RunPhase::Running | process::RunPhase::Starting
    ) {
        status = process::client::wait(client, status.run_id, 1000)
            .await?
            .map_err(|error| anyhow::anyhow!("wait failed: {error:?}"))?;
    }
    if !status.capture_finalized {
        status = process::client::finish(client, status.run_id, status.revision, false)
            .await?
            .map_err(|error| anyhow::anyhow!("finish failed: {error:?}"))?;
    }
    completed_command("run", status)
}

fn completed_command(
    operation: &str,
    status: process::RunStatus,
) -> Result<(serde_json::Value, i32)> {
    let code = if operation == "run" {
        ensure!(
            status.capture_finalized,
            "foreground run capture is not finalized"
        );
        status
            .exit_code
            .map_or(1, |code| i32::try_from(code.min(255)).unwrap_or(1))
    } else {
        0
    };
    Ok((serde_json::to_value(status)?, code))
}

fn program_options(args: &Arguments) -> Result<process::ProgramOptions> {
    args.validate(
        &[
            "profile",
            "viewport",
            "cwd",
            "env",
            "idempotency-key",
            "json",
        ],
        true,
    )?;
    let program = args
        .argv
        .first()
        .context("start/run requires -- PROGRAM ARGS")?
        .clone();
    let (cols, rows) = viewport(args.one("viewport")?.unwrap_or("80x24"))?;
    let mut env = BTreeMap::new();
    for item in args.values("env") {
        let (key, value) = item.split_once('=').context("--env requires KEY=VALUE")?;
        ensure!(
            !key.is_empty() && !key.contains('\0') && !value.contains('\0'),
            "invalid environment entry"
        );
        ensure!(
            env.insert(key.into(), value.into()).is_none(),
            "duplicate environment key"
        );
    }
    let profile = match args.one("profile")?.unwrap_or("functional") {
        "full" => RecordingProfile::Full,
        "functional" => RecordingProfile::Functional,
        "visual" => RecordingProfile::Visual,
        value => bail!("unknown profile {value}"),
    };
    Ok(process::ProgramOptions {
        program,
        args: args.argv[1..].to_vec(),
        cwd: Some(
            match args.one("cwd")? {
                Some(path) => std::fs::canonicalize(path)?,
                None => std::env::current_dir()?,
            }
            .to_string_lossy()
            .into_owned(),
        ),
        env,
        cols,
        rows,
        profile,
    })
}

async fn command_status(
    args: &Arguments,
    dedicated: &mut crate::process_bootstrap::DedicatedRuntime,
) -> Result<(serde_json::Value, i32)> {
    let client = &mut dedicated.client;
    let value = {
        args.validate(&["json"], false)?;
        serde_json::to_value(
            process::client::status(&mut *client, args.id()?)
                .await?
                .map_err(|error| anyhow::anyhow!("status failed: {error:?}"))?,
        )?
    };
    Ok((value, 0))
}

async fn command_read(
    args: &Arguments,
    dedicated: &mut crate::process_bootstrap::DedicatedRuntime,
) -> Result<(serde_json::Value, i32)> {
    let client = &mut dedicated.client;
    let value = {
        args.validate(&["since", "max-bytes", "json"], false)?;
        serde_json::to_value(
            process::client::read(
                &mut *client,
                args.id()?,
                args.number("since", 0)?,
                args.number("max-bytes", 65536)?,
            )
            .await?
            .map_err(|error| anyhow::anyhow!("read failed: {error:?}"))?,
        )?
    };
    Ok((value, 0))
}

async fn command_screen(
    args: &Arguments,
    dedicated: &mut crate::process_bootstrap::DedicatedRuntime,
) -> Result<(serde_json::Value, i32)> {
    let client = &mut dedicated.client;
    let value = {
        args.validate(&["json"], false)?;
        let text = process::client::screen(&mut *client, args.id()?)
            .await?
            .map_err(|error| anyhow::anyhow!("screen failed: {error:?}"))?;
        serde_json::json!({"schema_version":4,"run_id":args.id()?,"text":text})
    };
    Ok((value, 0))
}

async fn command_send(
    args: &Arguments,
    dedicated: &mut crate::process_bootstrap::DedicatedRuntime,
    launch_key: Uuid,
) -> Result<(serde_json::Value, i32)> {
    let client = &mut dedicated.client;
    let value = {
        args.validate(&["text", "key", "stdin", "idempotency-key", "json"], false)?;
        let mut inputs = 0;
        let mut data = Vec::new();
        if let Some(text) = args.one("text")? {
            inputs += 1;
            data = text.as_bytes().to_vec();
        }
        if let Some(key) = args.one("key")? {
            inputs += 1;
            data = match key.to_ascii_lowercase().as_str() {
                "enter" => b"\r".to_vec(),
                "tab" => b"\t".to_vec(),
                "escape" => vec![27],
                "ctrl-c" => vec![3],
                "ctrl-d" => vec![4],
                "backspace" => vec![127],
                "up" => b"\x1b[A".to_vec(),
                "down" => b"\x1b[B".to_vec(),
                "right" => b"\x1b[C".to_vec(),
                "left" => b"\x1b[D".to_vec(),
                _ => bail!("unsupported key {key}"),
            };
        }
        if args.flag("stdin") {
            inputs += 1;
            std::io::stdin().take(1_048_577).read_to_end(&mut data)?;
        }
        ensure!(
            inputs == 1,
            "send requires exactly one of --text, --key, --stdin"
        );
        ensure!(data.len() <= 1_048_576, "input exceeds 1 MiB");
        let bytes = data.len();
        let receipt = process::client::send(&mut *client, args.id()?, data, launch_key)
            .await?
            .map_err(|error| anyhow::anyhow!("send failed: {error:?}"))?;
        serde_json::json!({"schema_version":4,"run_id":args.id()?,"mutation_id":launch_key,"accepted_bytes":bytes,"receipt":receipt})
    };
    Ok((value, 0))
}

async fn command_wait(
    args: &Arguments,
    dedicated: &mut crate::process_bootstrap::DedicatedRuntime,
) -> Result<(serde_json::Value, i32)> {
    let client = &mut dedicated.client;
    {
        args.validate(&["exit", "screen-contains", "timeout", "json"], false)?;
        let timeout: u32 = args.duration("timeout", 30_000)?;
        ensure!(timeout <= 30_000, "timeout exceeds 30000 milliseconds");
        if let Some(needle) = args.one("screen-contains")? {
            let start = std::time::Instant::now();
            loop {
                let screen = process::client::screen(&mut *client, args.id()?)
                    .await?
                    .map_err(|error| anyhow::anyhow!("screen failed: {error:?}"))?;
                if screen.contains(needle) {
                    return Ok((
                        serde_json::json!({"schema_version":4,"run_id":args.id()?,"matched":true}),
                        0,
                    ));
                }
                if start.elapsed().as_millis() >= u128::from(timeout) {
                    return Ok((
                        serde_json::json!({"schema_version":4,"run_id":args.id()?,"matched":false,"timed_out":true}),
                        124,
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        let status = process::client::wait(&mut *client, args.id()?, timeout)
            .await?
            .map_err(|error| anyhow::anyhow!("wait failed: {error:?}"))?;
        let code = if status.output_drained { 0 } else { 124 };
        Ok((serde_json::to_value(status)?, code))
    }
}

async fn command_terminate(
    args: &Arguments,
    dedicated: &mut crate::process_bootstrap::DedicatedRuntime,
) -> Result<(serde_json::Value, i32)> {
    let client = &mut dedicated.client;
    let value = {
        args.validate(&["grace-period", "json"], false)?;
        serde_json::to_value(
            process::client::terminate(
                &mut *client,
                args.id()?,
                args.duration("grace-period", 1000)?,
            )
            .await?
            .map_err(|error| anyhow::anyhow!("terminate failed: {error:?}"))?,
        )?
    };
    Ok((value, 0))
}

async fn command_finish(
    args: &Arguments,
    dedicated: &mut crate::process_bootstrap::DedicatedRuntime,
) -> Result<(serde_json::Value, i32)> {
    let recording_root = crate::process_bootstrap::recording_root(dedicated);
    let client = &mut dedicated.client;
    let value = {
        args.validate(
            &[
                "terminate",
                "cleanup-server",
                "expected-revision",
                "export",
                "output",
                "cleanup-server",
                "json",
            ],
            false,
        )?;
        let status = process::client::status(&mut *client, args.id()?)
            .await?
            .map_err(|error| anyhow::anyhow!("status failed: {error:?}"))?;
        let status = process::client::finish(
            &mut *client,
            args.id()?,
            args.number("expected-revision", status.revision)?,
            args.flag("terminate"),
        )
        .await?
        .map_err(|error| anyhow::anyhow!("finish failed: {error:?}"))?;
        if args.flag("cleanup-server") {
            client.stop_server().await?;
            crate::durable::write_atomic(&dedicated.root.join("server-stopped.json"), b"true")?;
        }
        if let Some(output) = args.one("output")? {
            for format in args.values("export") {
                ensure!(
                    ["text", "raw", "events", "gif"].contains(&format),
                    "unsupported export {format}"
                );
            }
            let root = &recording_root;
            prepare_gif(
                &root.join(status.recording_id.to_string()),
                status.recording_id,
                args,
            )?;
            let options = crate::process_artifacts::ProcessArtifactOptions {
                gif_fps: args.values("export").contains(&"gif").then_some(24),
                ..Default::default()
            };
            let screen = std::fs::read_to_string(
                root.join(status.recording_id.to_string())
                    .join("screen.txt"),
            )?;
            let report = crate::process_artifacts::export_process_artifacts(
                &root.join(status.recording_id.to_string()),
                std::path::Path::new(output),
                &options,
                Some(screen.as_bytes()),
            )?;
            return Ok((
                serde_json::json!({"schema_version":4,"status":status,"artifacts":report.artifacts}),
                0,
            ));
        }
        ensure!(
            args.values("export").is_empty(),
            "--export requires --output"
        );
        serde_json::to_value(status)?
    };
    Ok((value, 0))
}

fn prepare_gif(directory: &std::path::Path, recording_id: Uuid, args: &Arguments) -> Result<()> {
    if args.values("export").contains(&"gif") {
        let (cols, rows): (u16, u16) =
            serde_json::from_slice(&std::fs::read(directory.join("viewport.json"))?)?;
        let screen = std::fs::read(directory.join("screen.txt"))?;
        crate::process_display::replay(directory, recording_id, cols, rows)?;
        crate::durable::write_atomic(&directory.join("screen.txt"), &screen)?;
    }
    Ok(())
}

fn validate_operation(operation: &str, args: &Arguments) -> Result<()> {
    match operation {
        "status" | "screen" => args.validate(&["json"], false)?,
        "read" => args.validate(&["since", "max-bytes", "json"], false)?,
        "wait" => args.validate(&["exit", "screen-contains", "timeout", "json"], false)?,
        "terminate" => args.validate(&["grace-period", "json"], false)?,
        "send" => args.validate(&["text", "key", "stdin", "idempotency-key", "json"], false)?,
        "start" | "run" | "finish" => {}
        _ => bail!("unknown process operation"),
    }
    if operation == "finish" {
        args.validate(
            &[
                "terminate",
                "cleanup-server",
                "expected-revision",
                "export",
                "output",
                "json",
            ],
            false,
        )?;
        for format in args.values("export") {
            ensure!(
                ["text", "raw", "events", "gif"].contains(&format),
                "unsupported export {format}"
            );
        }
        ensure!(
            args.values("export").is_empty() || args.one("output")?.is_some(),
            "--export requires --output"
        );
        if let Some(output) = args.one("output")? {
            ensure!(
                !std::path::Path::new(output).try_exists()?,
                "artifact directory already exists"
            );
        }
    }
    Ok(())
}

async fn offline_command(
    context: &NativeCommandContext,
    operation: &str,
    args: &Arguments,
    root: &std::path::Path,
    status: process::RunStatus,
) -> Result<(serde_json::Value, i32)> {
    let directory = root.join(status.recording_id.to_string());
    if operation == "wait"
        && let Some(needle) = args.one("screen-contains")?
    {
        let text = std::fs::read_to_string(directory.join("screen.txt"))?;
        let matched = text.contains(needle);
        return Ok((
            serde_json::json!({"schema_version":4,"run_id":status.run_id,"matched":matched,"timed_out":!matched}),
            if matched { 0 } else { 124 },
        ));
    }
    if let Some(expected) = args.one("expected-revision")? {
        ensure!(
            expected.parse::<u64>()? == status.revision,
            "stale process run revision"
        );
    }
    let value = match operation {
        "read" => serde_json::to_value(crate::process_runtime::read_output(
            &directory,
            &status,
            args.number("since", 0)?,
            args.number("max-bytes", 65536)?,
        )?)?,
        "screen" => {
            serde_json::json!({"schema_version":4,"run_id":status.run_id,"text":std::fs::read_to_string(directory.join("screen.txt"))?})
        }
        "status" | "wait" | "terminate" | "finish" => {
            if operation == "finish" {
                if args.flag("cleanup-server") {
                    crate::process_bootstrap::cleanup(context, args.id()?).await?;
                }
                if let Some(output) = args.one("output")? {
                    prepare_gif(
                        &root.join(status.recording_id.to_string()),
                        status.recording_id,
                        args,
                    )?;
                    let options = crate::process_artifacts::ProcessArtifactOptions {
                        gif_fps: args.values("export").contains(&"gif").then_some(24),
                        ..Default::default()
                    };
                    let screen = std::fs::read(directory.join("screen.txt"))?;
                    let report = crate::process_artifacts::export_process_artifacts(
                        &directory,
                        std::path::Path::new(output),
                        &options,
                        Some(&screen),
                    )?;
                    return Ok((
                        serde_json::json!({"schema_version":4,"status":status,"artifacts":report.artifacts}),
                        0,
                    ));
                }
            }
            serde_json::to_value(status)?
        }
        _ => bail!("finalized process cannot accept input"),
    };
    Ok((value, 0))
}

#[derive(Default)]
struct Arguments {
    options: BTreeMap<String, Vec<String>>,
    positional: Vec<String>,
    argv: Vec<String>,
}
impl Arguments {
    fn parse(values: &[String]) -> Result<Self> {
        let mut result = Self::default();
        let mut iter = values.iter();
        while let Some(value) = iter.next() {
            if value == "--" {
                result.argv.extend(iter.cloned());
                break;
            }
            if let Some(flag) = value.strip_prefix("--") {
                let (key, item) = if let Some((key, item)) = flag.split_once('=') {
                    (key, item.to_owned())
                } else if ["json", "stdin", "exit", "terminate", "cleanup-server"].contains(&flag) {
                    (flag, String::new())
                } else {
                    (
                        flag,
                        iter.next().context("option requires a value")?.clone(),
                    )
                };
                result.options.entry(key.into()).or_default().push(item);
            } else {
                result.positional.push(value.clone());
            }
        }
        Ok(result)
    }
    fn validate(&self, allowed: &[&str], program: bool) -> Result<()> {
        for key in self.options.keys() {
            ensure!(
                allowed.contains(&key.as_str()),
                "unsupported option --{key}"
            );
        }
        ensure!(program || self.argv.is_empty(), "unexpected program argv");
        ensure!(
            self.positional.len() == usize::from(!program),
            "unexpected positional arguments"
        );
        Ok(())
    }
    fn values(&self, name: &str) -> Vec<&str> {
        self.options.get(name).map_or_else(Vec::new, |values| {
            values.iter().map(String::as_str).collect()
        })
    }
    fn one(&self, name: &str) -> Result<Option<&str>> {
        let values = self.values(name);
        ensure!(values.len() <= 1, "duplicate --{name}");
        Ok(values.first().copied())
    }
    fn flag(&self, name: &str) -> bool {
        self.options.contains_key(name)
    }
    fn number<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T> {
        self.one(name)?.map_or(Ok(default), |value| {
            value
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid --{name}"))
        })
    }
    fn duration(&self, name: &str, default: u32) -> Result<u32> {
        let Some(value) = self.one(name)? else {
            return Ok(default);
        };
        let (number, multiplier) = if let Some(ms) = value.strip_suffix("ms") {
            (ms, 1_u32)
        } else if let Some(seconds) = value.strip_suffix('s') {
            (seconds, 1000_u32)
        } else {
            (value, 1_u32)
        };
        let number: u32 = number
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid --{name} duration"))?;
        number.checked_mul(multiplier).context("duration overflow")
    }
    fn id(&self) -> Result<Uuid> {
        Uuid::parse_str(self.positional.first().context("run ID required")?)
            .context("invalid run ID")
    }
}
fn viewport(value: &str) -> Result<(u16, u16)> {
    let (cols, rows) = value.split_once('x').context("viewport requires WxH")?;
    let cols = cols.parse()?;
    let rows = rows.parse()?;
    ensure!(cols > 0 && rows > 0, "viewport must be positive");
    ensure!(
        u32::from(cols) * u32::from(rows) <= 262_144,
        "viewport exceeds bounded capture geometry"
    );
    Ok((cols, rows))
}
fn help(operation: &str) {
    let (flags, example) = match operation {
        "start" | "run" => (
            "[--profile functional|full|visual] [--viewport WxH] [--cwd DIR] [--env KEY=VALUE] [--idempotency-key UUID] [--json] -- PROGRAM ARGS",
            "--profile functional --viewport 80x24 -- /bin/echo hello",
        ),
        "status" | "screen" => (
            "RUN_ID [--json]",
            "00000000-0000-0000-0000-000000000000 --json",
        ),
        "read" => (
            "RUN_ID [--since BYTE_CURSOR] [--max-bytes 1..1048576] [--json]",
            "RUN_ID --since 0 --max-bytes 65536",
        ),
        "send" => (
            "RUN_ID (--text TEXT|--key KEY|--stdin) [--idempotency-key UUID] [--json]",
            "RUN_ID --text hello",
        ),
        "wait" => (
            "RUN_ID [--exit|--screen-contains TEXT] [--timeout MILLISECONDS] [--json]",
            "RUN_ID --exit --timeout 30000",
        ),
        "terminate" => (
            "RUN_ID [--grace-period MILLISECONDS] [--json]",
            "RUN_ID --grace-period 1000",
        ),
        "finish" => (
            "RUN_ID [--terminate] [--cleanup-server] [--expected-revision N] [--export text|raw|events|gif]... [--output NEW_DIR] [--json]",
            "RUN_ID --export text --export raw --output ./artifacts",
        ),
        _ => ("", ""),
    };
    println!(
        "bmux recording process {operation} {flags}\n\nExample: bmux recording process {operation} {example}\n\nSecurity: requires dedicated isolated runtime; never targets live BMUX. Input and output may contain secrets. Direct argv only. Disconnect does not kill the process. Results are JSON; diagnostics stderr. Exit 0 success, 1 error, 124 timeout; run propagates program exit (capped at 255)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_capabilities_require_exact_supported_representation() {
        use bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::PresentationCapabilities;
        for (schema_version, coherent_screen, canonical_resize, accepted) in [
            (1, true, true, true),
            (2, true, true, false),
            (1, false, true, false),
            (1, true, false, false),
        ] {
            assert_eq!(
                validate_interactive_capabilities(&PresentationCapabilities {
                    schema_version,
                    coherent_screen,
                    canonical_resize,
                })
                .is_ok(),
                accepted
            );
        }
    }

    #[test]
    fn retired_selectors_reject_before_launch_receipt_or_runtime_creation() {
        let root = std::env::temp_dir().join(format!("bmux-rejected-selectors-{}", Uuid::new_v4()));
        assert!(!root.exists());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for operation in ["start", "run"] {
            for (selector, value) in [
                ("--contract-version", "3"),
                ("--lifetime", "terminal-session"),
            ] {
                let mut context = crate::tests::command_context(
                    "recording-process-start",
                    &[selector, value, "--", "/bin/echo", "must-not-launch"],
                    bmux_plugin_sdk::NativeCommandInvocationSource::AttachKeybinding,
                );
                context.connection.data_dir = root.to_string_lossy().into_owned();
                let error = runtime.block_on(execute(&context, operation)).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains(selector.trim_start_matches("--")),
                    "unexpected validation error: {error}"
                );
                assert!(
                    !root.exists(),
                    "rejected selector created durable launch admission"
                );
            }
        }
    }

    fn run_status(phase: process::RunPhase, finalized: bool) -> process::RunStatus {
        process::RunStatus {
            schema_version: 4,
            run_id: Uuid::new_v4(),
            revision: 3,
            launch_mutation_id: Uuid::new_v4(),
            recording_id: Uuid::new_v4(),
            execution: None,
            runtime_root: "/owned".into(),
            phase,
            exit_code: Some(7),
            target_exited: finalized,
            terminal_session_ended: finalized,
            capture_eof: finalized,
            capture_watermark: 0,
            recording_watermark: 0,
            durable_recording_complete: finalized,
            timing_complete: true,
            cleanup_complete: false,
            recovery_required: false,
            output_drained: finalized,
            output_watermark: 4,
            capture_finalized: finalized,
            errors: vec![],
        }
    }

    #[test]
    fn completed_run_retry_preserves_nonzero_exit_and_identity() {
        let status = run_status(process::RunPhase::Finalized, true);
        let (first, code) = completed_command("run", status.clone()).unwrap();
        assert_eq!(code, 7);
        assert_eq!(
            completed_command("run", status.clone()).unwrap(),
            (first, 7)
        );
        assert_eq!(completed_command("start", status).unwrap().1, 0);
        assert!(completed_command("run", run_status(process::RunPhase::Running, false)).is_err());
    }

    struct RunRetryClient {
        status: process::RunStatus,
        operations: Vec<String>,
    }
    impl bmux_plugin_sdk::TypedDispatchClient for RunRetryClient {
        async fn invoke_service_raw(
            &mut self,
            _capability: &str,
            _kind: bmux_ipc::InvokeServiceKind,
            _interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>> {
            let run_id = match operation {
                "wait" => {
                    bmux_plugin_sdk::decode_service_message::<process::client::WaitRequest>(
                        &payload,
                    )
                    .unwrap()
                    .run_id
                }
                "finish" => {
                    bmux_plugin_sdk::decode_service_message::<process::client::FinishRequest>(
                        &payload,
                    )
                    .unwrap()
                    .run_id
                }
                _ => panic!("retry must not launch: {operation}"),
            };
            assert_eq!(run_id, self.status.run_id);
            self.operations.push(operation.into());
            match operation {
                "wait" => {
                    self.status.phase = process::RunPhase::Exited;
                    self.status.output_drained = true;
                }
                "finish" => {
                    self.status.phase = process::RunPhase::Finalized;
                    self.status.capture_finalized = true;
                }
                _ => panic!("retry must not launch: {operation}"),
            }
            Ok(
                bmux_plugin_sdk::encode_service_message(&Ok::<_, process::ProcessError>(
                    self.status.clone(),
                ))
                .unwrap(),
            )
        }
    }

    #[test]
    fn running_retry_waits_and_finalizes_once_without_launching() {
        let status = run_status(process::RunPhase::Running, false);
        let mut client = RunRetryClient {
            status: status.clone(),
            operations: vec![],
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (value, code) = runtime.block_on(complete_run(&mut client, status)).unwrap();
        assert_eq!(code, 7);
        assert_eq!(value["run_id"], serde_json::json!(client.status.run_id));
        assert_eq!(client.operations, ["wait", "finish"]);
        let finalized = client.status.clone();
        runtime
            .block_on(complete_run(&mut client, finalized))
            .unwrap();
        assert_eq!(client.operations, ["wait", "finish"]);
    }

    #[test]
    fn native_foreground_retry_waits_for_capture_after_leader_exit_without_launching() {
        let mut status = run_status(process::RunPhase::Exited, false);
        status.exit_code = Some(7);
        let mut client = RunRetryClient {
            status: status.clone(),
            operations: Vec::new(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (value, code) = runtime
            .block_on(complete_native_run(&mut client, status))
            .unwrap();
        assert_eq!(code, 7);
        assert_eq!(value["run_id"], serde_json::json!(client.status.run_id));
        assert_eq!(client.operations, ["wait", "finish"]);
        let finalized = client.status.clone();
        runtime
            .block_on(complete_native_run(&mut client, finalized))
            .unwrap();
        assert_eq!(client.operations, ["wait", "finish"]);
    }

    #[test]
    fn direct_argv_preserves_options_and_spaces() {
        let args = Arguments::parse(&[
            "--env".into(),
            "A=x=y".into(),
            "--".into(),
            "/bin/tool".into(),
            "--help".into(),
            "a b".into(),
        ])
        .unwrap();
        assert_eq!(args.argv, ["/bin/tool", "--help", "a b"]);
        assert_eq!(args.values("env"), ["A=x=y"]);
    }
    #[test]
    fn duplicate_scalar_unknown_options_and_bad_viewport_fail() {
        let args = Arguments::parse(&["--since".into(), "0".into(), "--since".into(), "1".into()])
            .unwrap();
        assert!(args.one("since").is_err());
        assert!(args.validate(&["json"], false).is_err());
        assert!(viewport("0x24").is_err());
        assert!(viewport("80").is_err());
    }
}
