//! Interactive command policy; PTY execution and terminal ownership stay separate.
use anyhow::{Context, Result, bail, ensure};
use bmux_plugin_sdk::NativeCommandContext;
use bmux_recording_plugin_api::recording_process_v4 as process;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Default)]
struct Options {
    argv: Vec<String>,
    output: Option<PathBuf>,
    gif: Option<PathBuf>,
    exports: Vec<String>,
    json: bool,
}

impl Options {
    fn parse(arguments: &[String]) -> Result<Self> {
        let separator = arguments
            .iter()
            .position(|arg| arg == "--")
            .context("record requires -- PROGRAM ARGS (literal argv, no implicit shell)")?;
        let mut options = Self {
            argv: arguments[separator + 1..].to_vec(),
            ..Self::default()
        };
        ensure!(
            !options.argv.is_empty(),
            "record requires a program after --"
        );
        let mut flags = arguments[..separator].iter();
        while let Some(flag) = flags.next() {
            match flag.as_str() {
                "--json" => {
                    ensure!(!options.json, "duplicate --json");
                    options.json = true;
                }
                "--output" => {
                    ensure!(options.output.is_none(), "duplicate --output");
                    options.output = Some(new_destination(
                        flags.next().context("--output requires a new directory")?,
                    )?);
                }
                "--gif" => {
                    ensure!(options.gif.is_none(), "duplicate --gif");
                    options.gif = Some(new_destination(
                        flags.next().context("--gif requires a new GIF path")?,
                    )?);
                }
                "--export" => {
                    let format = flags
                        .next()
                        .context("--export requires text, raw, events or gif")?;
                    ensure!(
                        ["text", "raw", "events", "gif"].contains(&format.as_str()),
                        "unsupported export {format}"
                    );
                    ensure!(
                        !options.exports.contains(format),
                        "duplicate export {format}"
                    );
                    options.exports.push(format.clone());
                }
                _ => bail!("unsupported record option {flag}"),
            }
        }
        ensure!(
            !(options.exports.iter().any(|format| format == "gif") && options.gif.is_some()),
            "choose --export gif or --gif PATH, not both"
        );
        ensure!(
            options.exports.is_empty() || options.output.is_some(),
            "--export requires --output"
        );
        Ok(options)
    }
}

fn new_destination(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    ensure!(!path.as_os_str().is_empty(), "destination is empty");
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    ensure!(
        !absolute.try_exists()?,
        "destination already exists: {}",
        absolute.display()
    );
    let parent = absolute.parent().context("destination requires a parent")?;
    let name = absolute
        .file_name()
        .context("destination requires a file name")?;
    Ok(std::fs::canonicalize(parent)?.join(name))
}

fn resolve_program(program: &str) -> Result<PathBuf> {
    let candidate = if program.contains(std::path::MAIN_SEPARATOR) {
        PathBuf::from(program)
    } else {
        let path = std::env::var_os("PATH").context("PATH is not set")?;
        std::env::split_paths(&path)
            .map(|entry| entry.join(program))
            .find(|candidate| executable(candidate))
            .with_context(|| format!("program not found on caller PATH: {program}"))?
    };
    ensure!(
        executable(&candidate),
        "program is not an executable file: {}",
        candidate.display()
    );
    std::fs::canonicalize(candidate).context("resolving executable")
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn launch_arguments(options: &Options) -> Result<Vec<String>> {
    let program = resolve_program(&options.argv[0])?;
    let (cols, rows) = crossterm::terminal::size().context("reading outer terminal viewport")?;
    ensure!(
        cols > 0 && rows > 0,
        "outer terminal has zero-sized viewport"
    );
    let mut args = vec![
        "--profile".into(),
        "full".into(),
        "--viewport".into(),
        format!("{cols}x{rows}"),
        "--cwd".into(),
        std::env::current_dir()?
            .to_str()
            .context("cwd is not UTF-8")?
            .into(),
        "--idempotency-key".into(),
        Uuid::new_v4().to_string(),
        "--json".into(),
    ];
    // Capture the invoking environment before isolated startup. Never pass
    // BMUX routing variables or override runtime-owned terminal/identity fields.
    for (key, value) in std::env::vars_os() {
        let key = key
            .into_string()
            .map_err(|_| anyhow::anyhow!("environment key is not UTF-8"))?;
        if key.starts_with("BMUX_")
            || matches!(
                key.as_str(),
                "TERM"
                    | "COLORTERM"
                    | "TERM_PROGRAM"
                    | "TERM_PROGRAM_VERSION"
                    | "APPLICATION_RESURRECTION_HOST"
            )
        {
            continue;
        }
        let value = value
            .into_string()
            .map_err(|_| anyhow::anyhow!("environment value is not UTF-8 for {key}"))?;
        args.extend(["--env".into(), format!("{key}={value}")]);
    }
    args.push("--".into());
    args.push(
        program
            .to_str()
            .context("program path is not UTF-8")?
            .into(),
    );
    args.extend_from_slice(&options.argv[1..]);
    Ok(args)
}

pub(crate) fn run(context: &NativeCommandContext) -> i32 {
    if context
        .arguments
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--help" || arg == "-h")
    {
        eprintln!(
            "bmux record [--output NEW_DIR] [--export text|raw|events|gif]... [--gif NEW_PATH] [--json] -- PROGRAM ARGS\nRequires stdin/stdout TTY. Uses full profile, caller cwd/PATH/environment (excluding BMUX routing and runtime-owned terminal identity), actual terminal size. No implicit shell. Canonical evidence is retained; GIF is opt-in. Detachment preserves work; outer job suspension is unsupported (Ctrl-Z reaches the child, external SIGTSTP restores and retains the execution)."
        );
        return 0;
    }
    let json = context
        .arguments
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--json");
    let owned = context.clone();
    let result = std::thread::spawn(move || -> Result<(serde_json::Value, i32)> {
        let options = Options::parse(&owned.arguments)?;
        ensure!(std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            "bmux record requires an interactive terminal on stdin and stdout; use bmux recording process run for headless execution");
        let launch_args = launch_arguments(&options)?;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(execute(&owned, options, launch_args))
    }).join().map_err(|_| anyhow::anyhow!("record command worker panicked")).and_then(|result| result);
    let (value, code) = match result {
        Ok(result) => result,
        Err(error) => (
            serde_json::json!({"schema_version":4,"ok":false,"error":error.to_string(),"artifacts":[]}),
            1,
        ),
    };
    if json {
        println!("{value}");
    } else {
        eprintln!("record: {value}");
    }
    code
}

async fn execute(
    context: &NativeCommandContext,
    options: Options,
    launch_args: Vec<String>,
) -> Result<(serde_json::Value, i32)> {
    let mut launch = context.clone();
    launch.arguments = launch_args;
    let (value, _) = crate::process_commands::execute_interactive_start(&launch).await?;
    let mut status: process::RunStatus = serde_json::from_value(value)?;
    let mut artifacts = Vec::<PathBuf>::new();
    let mut terminal_restored = None;
    let mut owned_server_cleanup_acknowledged = None;
    let outcome = complete(
        context,
        &options,
        &mut status,
        &mut artifacts,
        &mut terminal_restored,
        &mut owned_server_cleanup_acknowledged,
    )
    .await;
    let code = if outcome.is_ok() {
        status
            .exit_code
            .map_or(1, |code| i32::try_from(code.min(255)).unwrap_or(1))
    } else {
        1
    };
    let recording_path = Path::new(&status.runtime_root)
        .join("state/recordings")
        .join(status.recording_id.to_string());
    let mut report = serde_json::json!({"schema_version":4,"ok":outcome.is_ok(),"status":status,"artifacts":artifacts,"recording_path":recording_path,"owned_server_cleanup_acknowledged":owned_server_cleanup_acknowledged});
    if let Err(error) = outcome {
        report["error"] = serde_json::Value::String(format!("{error:#}"));
        report["error_kind"] = serde_json::Value::String(format!(
            "{:?}",
            crate::interactive_terminal::failure_kind(&error)
        ));
        report["terminal_restored"] = serde_json::to_value(terminal_restored)?;
        report["recovery_required"] = serde_json::Value::Bool(true);
        report["automatic_relaunch"] = serde_json::Value::Bool(false);
    } else {
        report["terminal_restored"] = serde_json::Value::Bool(true);
    }
    Ok((report, code))
}

async fn complete(
    context: &NativeCommandContext,
    options: &Options,
    status: &mut process::RunStatus,
    artifacts: &mut Vec<PathBuf>,
    terminal_restored: &mut Option<bool>,
    owned_server_cleanup_acknowledged: &mut Option<bool>,
) -> Result<()> {
    if !matches!(
        status.phase,
        process::RunPhase::Running | process::RunPhase::Exited
    ) {
        bail!(
            "recorded launch did not produce an available execution: {:?}; retained run {}",
            status.phase,
            status.run_id
        );
    }
    let mut dedicated = crate::process_bootstrap::resolve(context, status.run_id).await?;
    let execution = status
        .execution
        .clone()
        .context("recorded run has no execution identity")?;
    match crate::interactive_terminal::interact(&mut dedicated.client, status.run_id, execution)
        .await
    {
        Ok(observed) => {
            *terminal_restored = Some(true);
            *status = observed;
        }
        Err(error) => {
            *terminal_restored = crate::interactive_terminal::terminal_restored(&error);
            // Retain the actual child outcome without replacing the original
            // attachment/restoration error or authorizing termination.
            if let Ok(Ok(observed)) =
                process::client::status(&mut dedicated.client, status.run_id).await
            {
                *status = observed;
            }
            return Err(error);
        }
    }
    *status = process::client::status(&mut dedicated.client, status.run_id)
        .await?
        .map_err(|error| anyhow::anyhow!("recording status: {error:?}"))?;
    ensure!(
        !matches!(status.phase, process::RunPhase::Failed) && status.errors.is_empty(),
        "recorded target failed; refusing successful artifact publication: {:?}",
        status.errors
    );
    if !status.output_drained || !status.terminal_session_ended {
        bail!(
            "terminal restored; capture pending after target exit; run {} retained at {}; surviving terminal descriptors may still produce output; use recording process status/wait and finish after real EOF",
            status.run_id,
            status.runtime_root
        );
    }
    *status = process::client::finish(&mut dedicated.client, status.run_id, status.revision, false)
        .await?
        .map_err(|error| anyhow::anyhow!("recording finalization: {error:?}"))?;
    ensure!(
        status.capture_finalized && status.durable_recording_complete && status.timing_complete,
        "recording is not durably complete"
    );
    let exported = export(context, options, status, artifacts).await;
    // Finalized evidence survives export failure; cleanup targets only this
    // finalized owned runtime. Preserve export's original failure if both fail.
    let cleaned = crate::process_bootstrap::cleanup(context, status.run_id).await;
    // ServerStopping plus the durable bootstrap marker acknowledges this
    // owned server's stop request, not process-group guardian retirement.
    // Never rewrite status.cleanup_complete to stand in for server cleanup.
    *owned_server_cleanup_acknowledged = Some(cleaned.is_ok());
    match (exported, cleaned) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("owned server cleanup also failed: {cleanup}")))
        }
    }
}

async fn export(
    context: &NativeCommandContext,
    options: &Options,
    status: &process::RunStatus,
    artifacts: &mut Vec<PathBuf>,
) -> Result<()> {
    if options.output.is_none() && options.gif.is_none() {
        return Ok(());
    }
    let temporary =
        Path::new(&status.runtime_root).join(format!("record-export-{}", Uuid::new_v4()));
    let output = options.output.as_ref().unwrap_or(&temporary);
    let mut finish = context.clone();
    finish.arguments = vec![
        status.run_id.to_string(),
        "--output".into(),
        output.to_str().context("output path is not UTF-8")?.into(),
    ];
    let gif = options.gif.is_some() || options.exports.iter().any(|format| format == "gif");
    for format in &options.exports {
        finish.arguments.extend(["--export".into(), format.clone()]);
    }
    if gif && !options.exports.iter().any(|format| format == "gif") {
        finish.arguments.extend(["--export".into(), "gif".into()]);
    }
    let (report, _) = crate::process_commands::execute(&finish, "finish").await?;
    *artifacts = serde_json::from_value(report["artifacts"].clone())?;
    if let Some(destination) = &options.gif {
        publish_gif(&output.join("output.gif"), destination)?;
        artifacts.retain(|path| path != &output.join("output.gif"));
        artifacts.push(destination.clone());
    }
    Ok(())
}

fn publish_gif(source: &Path, destination: &Path) -> Result<()> {
    use std::io::Write;
    let parent = destination.parent().context("GIF parent missing")?;
    let temporary = parent.join(format!(".bmux-gif-{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut source = std::fs::File::open(source)?;
        let mut target = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            target.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        std::io::copy(&mut source, &mut target)?;
        target.flush()?;
        target.sync_all()?;
        // Link publishes only the fully committed file and fails if the requested
        // path exists. Unlike rename, this cannot overwrite a racing destination.
        std::fs::hard_link(&temporary, destination)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let cleanup = std::fs::remove_file(&temporary);
    result?;
    cleanup.context("removing GIF publication staging file")?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_manifest_uses_generic_plugin_discovery() {
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(include_str!("../plugin.toml")).unwrap();
        let command = manifest
            .commands
            .iter()
            .find(|command| command.name == "record")
            .unwrap();
        assert_eq!(command.path, ["record"]);
        assert!(command.expose_in_cli);
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn literal_program_flags_are_not_record_options() {
        let options = Options::parse(&args(&[
            "--json",
            "--",
            "/bin/echo",
            "--gif",
            "a b",
            "$(false)",
        ]))
        .unwrap();
        assert!(options.json);
        assert_eq!(
            options.argv,
            args(&["/bin/echo", "--gif", "a b", "$(false)"])
        );
        assert!(options.gif.is_none());
        assert!(options.output.is_none());
    }

    #[test]
    fn malformed_options_are_rejected_without_launch() {
        for invalid in [
            args(&["/bin/echo"]),
            args(&["--"]),
            args(&["--export", "raw", "--", "/bin/echo"]),
            args(&["--profile", "visual", "--", "/bin/echo"]),
            args(&["--json", "--json", "--", "/bin/echo"]),
            args(&["--gif", "--", "/bin/echo"]),
        ] {
            assert!(Options::parse(&invalid).is_err());
        }
    }

    #[test]
    fn gif_publication_is_complete_and_never_overwrites() {
        let root = std::env::temp_dir().join(format!("bmux-gif-publication-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("source.gif");
        let destination = root.join("result.gif");
        std::fs::write(&source, b"GIF89a evidence").unwrap();
        publish_gif(&source, &destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"GIF89a evidence");
        std::fs::write(&source, b"replacement").unwrap();
        assert!(publish_gif(&source, &destination).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"GIF89a evidence");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
}
