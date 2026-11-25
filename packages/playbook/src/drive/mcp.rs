//! `bmux drive mcp`: the drive commands as Model Context Protocol tools over
//! stdio (newline-delimited JSON-RPC 2.0), for agents that prefer tools to a
//! shell. Each tool builds the same command line the CLI parses, so both
//! surfaces behave identically; sessions are shared with the CLI.

use std::fmt::Write as _;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::cli;
use super::{Driver, ScreenDetail};

const PROTOCOL_VERSION: &str = "2025-06-18";

const INSTRUCTIONS: &str = "\
Drive terminal programs (TUIs and CLIs) in private terminals and see what they do. Start one \
with `start`, then act with `press`, `type`, `click`, `scroll`, `resize`. Every action waits \
until the program settles (no sleeps needed) and returns the screen with numbered rows (changed \
rows marked *), styled cells that stand out (selection, errors), cursor, terminal modes, whether \
the program exited, and new terminal-protocol problems. Use `wait` for slow work, `expect` for \
assertions, `screenshot` to see colors and layout, `stop` to quit and check the program restored \
the terminal, and `script` to get a replayable regression test of the session.";

pub async fn serve(driver: Driver) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(&driver, &message).await,
            Err(error) => Some(error_response(&Value::Null, -32700, &format!("parse error: {error}"))),
        };
        if let Some(response) = response {
            let mut text = serde_json::to_string(&response)?;
            text.push('\n');
            stdout.write_all(text.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

async fn handle(driver: &Driver, message: &Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    // Notifications carry no id and get no response.
    let id = id?;
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "bmux-drive", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => Ok(call_tool(driver, &params).await),
        other => Err((-32601, format!("method not found: {other}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => error_response(&id, code, &message),
    })
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

async fn call_tool(driver: &Driver, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    match run_tool(driver, name, &arguments).await {
        Ok(result) => result,
        Err(error) => text_result(format!("error: {error:#}"), true),
    }
}

async fn run_tool(driver: &Driver, name: &str, args: &Value) -> Result<Value> {
    let session = args.get("session").and_then(Value::as_str);
    let detail = if args.get("changes_only").and_then(Value::as_bool) == Some(true) {
        ScreenDetail::Changes
    } else {
        ScreenDetail::Full
    };
    let all_styles = args.get("all_styles").and_then(Value::as_bool) == Some(true);
    match name {
        "list" => {
            let sessions = driver.list();
            let mut text = String::new();
            if sessions.is_empty() {
                text.push_str("no drive sessions running");
            }
            for session in sessions {
                let _ = writeln!(
                    text,
                    "{}  {}  {}  (in {})",
                    session.name,
                    session.size,
                    super::shell_join(&session.command),
                    session.cwd
                );
            }
            return Ok(text_result(text, false));
        }
        "screenshot" => {
            let (session, png) = driver.screenshot_bytes(session).await?;
            return Ok(json!({
                "content": [
                    { "type": "image", "data": base64(&png), "mimeType": "image/png" },
                    { "type": "text", "text": format!("[{session}] screenshot") },
                ],
                "isError": false,
            }));
        }
        "run_script" => {
            let source = match (str_arg(args, "script"), str_arg(args, "file")) {
                (Some(script), _) => script.to_owned(),
                (None, Some(file)) => std::fs::read_to_string(file)?,
                (None, None) => anyhow::bail!("give `script` (its text) or `file` (a path)"),
            };
            let sizes = args
                .get("sizes")
                .and_then(Value::as_array)
                .map(|sizes| {
                    sizes
                        .iter()
                        .filter_map(Value::as_str)
                        .map(cli::parse_size)
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?
                .unwrap_or_default();
            let runs = driver
                .run_script(&source, &sizes, super::DEFAULT_QUIET)
                .await?;
            let failed = runs.iter().any(|run| !run.passed);
            let text: String = runs.iter().map(super::ScriptRun::render).collect();
            return Ok(text_result(text, failed));
        }
        _ => {}
    }
    let words = tool_words(name, args)?;
    let command = cli::parse_script_command(&words)?;
    let report = match &command {
        cli::Command::Start(start) => {
            let options = start.start_options(None)?;
            driver.start(options).await?
        }
        other => cli::execute_in_session(driver, other, session).await?,
    };
    let failed = !report.ok;
    Ok(text_result(report.render(detail, all_styles), failed))
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn number_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|value| match value {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => Some(text.clone()),
        _ => None,
    })
}

fn required(args: &Value, key: &str) -> Result<String> {
    number_arg(args, key).ok_or_else(|| anyhow::anyhow!("missing argument `{key}`"))
}

fn string_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(text)) => vec![text.clone()],
        _ => Vec::new(),
    }
}

/// The CLI words a tool call stands for.
fn tool_words(name: &str, args: &Value) -> Result<Vec<String>> {
    let mut words = vec![name.to_owned()];
    let flag = |words: &mut Vec<String>, key: &str, flag: &str| {
        if let Some(value) = number_arg(args, key) {
            words.push(flag.to_owned());
            words.push(value);
        }
    };
    let switch = |words: &mut Vec<String>, key: &str, flag: &str| {
        if args.get(key).and_then(Value::as_bool) == Some(true) {
            words.push(flag.to_owned());
        }
    };
    match name {
        "start" => {
            flag(&mut words, "name", "--name");
            flag(&mut words, "size", "--size");
            flag(&mut words, "cwd", "--cwd");
            flag(&mut words, "timeout_secs", "--timeout");
            if let Some(env) = args.get("env").and_then(Value::as_object) {
                for (key, value) in env {
                    words.push("--env".into());
                    words.push(format!("{key}={}", value.as_str().unwrap_or_default()));
                }
            }
            let command = string_list(args, "command");
            anyhow::ensure!(!command.is_empty(), "`command` must list the program and its arguments");
            words.push("--".into());
            words.extend(command);
        }
        "type" => {
            words.push(required(args, "text")?);
            switch(&mut words, "enter", "--enter");
        }
        "press" => {
            let keys = string_list(args, "keys");
            anyhow::ensure!(!keys.is_empty(), "`keys` must list at least one key");
            words.extend(keys);
        }
        "paste" => words.push(required(args, "text")?),
        "click" => {
            words.push(required(args, "row")?);
            words.push(required(args, "col")?);
            flag(&mut words, "button", "--button");
            switch(&mut words, "double", "--double");
        }
        "scroll" => {
            words.push(required(args, "direction")?);
            if let Some(count) = number_arg(args, "count") {
                words.push(count);
            }
            if let (Some(row), Some(col)) = (number_arg(args, "row"), number_arg(args, "col")) {
                words.extend(["--at".to_owned(), row, col]);
            }
        }
        "drag" => {
            for key in ["from_row", "from_col", "to_row", "to_col"] {
                words.push(required(args, key)?);
            }
        }
        "resize" => words.push(required(args, "size")?),
        "look" => flag(&mut words, "history_lines", "--history"),
        "lint" | "script" => {}
        "wait" => {
            flag(&mut words, "text", "--text");
            flag(&mut words, "gone", "--gone");
            switch(&mut words, "idle", "--idle");
            switch(&mut words, "exit", "--exit");
            switch(&mut words, "change", "--change");
            flag(&mut words, "timeout_secs", "--timeout");
        }
        "expect" => {
            flag(&mut words, "text", "--text");
            flag(&mut words, "no_text", "--no-text");
            if let (Some(row), Some(col)) =
                (number_arg(args, "cursor_row"), number_arg(args, "cursor_col"))
            {
                words.extend(["--cursor".to_owned(), row, col]);
            }
            switch(&mut words, "running", "--running");
            switch(&mut words, "exited", "--exited");
            flag(&mut words, "exit_code", "--exit-code");
            flag(&mut words, "styled", "--styled");
            flag(&mut words, "style", "--style");
        }
        "stop" => flag(&mut words, "grace_secs", "--grace"),
        other => anyhow::bail!("unknown tool `{other}`"),
    }
    if matches!(
        name,
        "type" | "press" | "paste" | "click" | "scroll" | "drag" | "resize"
    ) {
        flag(&mut words, "timeout_secs", "--timeout");
    }
    Ok(words)
}

fn tools() -> Value {
    let session = json!({ "type": "string", "description": "Session name; needed only when several sessions run" });
    let timeout = json!({ "type": "number", "description": "Seconds to wait for the program to settle (default 5)" });
    let changes = json!({ "type": "boolean", "description": "Show only rows that changed instead of the whole screen" });
    let action = |description: &str, mut properties: Value, required: &[&str]| {
        properties["session"] = session.clone();
        properties["timeout_secs"] = timeout.clone();
        properties["changes_only"] = changes.clone();
        json!({ "description": description, "properties": properties, "required": required })
    };
    let specs = [
        ("start", json!({
            "description": "Start a terminal program in a new private terminal and return its first settled screen. The program inherits your environment; TERM is xterm-256color.",
            "properties": {
                "command": { "type": "array", "items": { "type": "string" }, "description": "Program and arguments, e.g. [\"htop\", \"-d\", \"10\"] or [\"./target/debug/app\"]" },
                "name": { "type": "string", "description": "Session name (default: the program's name)" },
                "size": { "type": "string", "description": "COLSxROWS, default 80x24" },
                "cwd": { "type": "string", "description": "Working directory (default: the server's current directory)" },
                "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Extra environment variables" },
                "timeout_secs": { "type": "number", "description": "Seconds to wait for the first screen (default 15)" },
            },
            "required": ["command"],
        })),
        ("type", action("Type text into the program (newlines are Enter) and return the settled screen.", json!({
            "text": { "type": "string" },
            "enter": { "type": "boolean", "description": "Press Enter after the text" },
        }), &["text"])),
        ("press", action("Press keys and return the settled screen. Names: Enter Esc Tab S-Tab Backspace Space Up Down Left Right Home End PgUp PgDn Insert Delete F1-F12, single characters (q Q /), C-x ctrl, M-x alt, S-x shift, repeats like Down*3.", json!({
            "keys": { "type": "array", "items": { "type": "string" }, "description": "e.g. [\"Down\", \"Down\", \"Enter\"] or [\"C-c\"]" },
        }), &["keys"])),
        ("paste", action("Paste text (bracketed paste when the program enabled it).", json!({
            "text": { "type": "string" },
        }), &["text"])),
        ("click", action("Click a cell; rows and columns start at 1, as numbered in screens. Reports when the program has not enabled mouse reporting.", json!({
            "row": { "type": "integer" }, "col": { "type": "integer" },
            "button": { "type": "string", "enum": ["left", "right", "middle"] },
            "double": { "type": "boolean" },
        }), &["row", "col"])),
        ("scroll", action("Scroll the mouse wheel over a cell (default: the screen's middle).", json!({
            "direction": { "type": "string", "enum": ["up", "down"] },
            "count": { "type": "integer" },
            "row": { "type": "integer" }, "col": { "type": "integer" },
        }), &["direction"])),
        ("drag", action("Drag with the left button between two cells.", json!({
            "from_row": { "type": "integer" }, "from_col": { "type": "integer" },
            "to_row": { "type": "integer" }, "to_col": { "type": "integer" },
        }), &["from_row", "from_col", "to_row", "to_col"])),
        ("resize", action("Resize the terminal (the program receives SIGWINCH) and return the settled screen.", json!({
            "size": { "type": "string", "description": "COLSxROWS, e.g. 40x12" },
        }), &["size"])),
        ("look", json!({
            "description": "Return the current screen without acting. With history_lines, return that many of the last lines of output instead, including lines that scrolled off the screen.",
            "properties": {
                "session": session,
                "changes_only": changes,
                "all_styles": { "type": "boolean" },
                "history_lines": { "type": "integer", "description": "Show the last N lines of output, including scrolled-off ones" },
            },
        })),
        ("wait", json!({
            "description": "Wait for a regex to appear (text) or disappear (gone), the program to go idle, exit, or the screen to change. Use for slow work instead of sleeping.",
            "properties": {
                "text": { "type": "string" }, "gone": { "type": "string" },
                "idle": { "type": "boolean" }, "exit": { "type": "boolean" }, "change": { "type": "boolean" },
                "timeout_secs": { "type": "number", "description": "Default 30" },
                "session": session,
            },
        })),
        ("expect", json!({
            "description": "Check the screen now (an assertion recorded into the script). Fails with the reason and screen when the check fails.",
            "properties": {
                "text": { "type": "string" }, "no_text": { "type": "string" },
                "cursor_row": { "type": "integer" }, "cursor_col": { "type": "integer" },
                "running": { "type": "boolean" }, "exited": { "type": "boolean" },
                "exit_code": { "type": "integer" },
                "styled": { "type": "string", "description": "Text that must be styled as `style` says" },
                "style": { "type": "string", "description": "e.g. inverse, bold, fg=red, bg=#303030" },
                "session": session,
            },
        })),
        ("screenshot", json!({
            "description": "A PNG image of the terminal, to check colors, alignment and layout.",
            "properties": { "session": session },
        })),
        ("lint", json!({
            "description": "What the program did to the terminal: modes left on at exit (alternate screen, mouse, hidden cursor, bracketed paste), unbalanced synchronized updates, flicker from full clears, invalid UTF-8, terminal queries.",
            "properties": { "session": session },
        })),
        ("stop", json!({
            "description": "Stop the program (SIGHUP, then kill) and the session; returns exit status, final screen, lint and the recorded script. Quit the program with its own keys first to test its exit path.",
            "properties": { "session": session, "grace_secs": { "type": "number" } },
        })),
        ("list", json!({ "description": "List running sessions.", "properties": {} })),
        ("script", json!({
            "description": "The session's actions as a replayable script (a regression test); save it and replay with run_script.",
            "properties": { "session": session },
        })),
        ("run_script", json!({
            "description": "Replay a drive script, optionally at several sizes; reports pass/fail per size with the failing step, screen and lint findings.",
            "properties": {
                "script": { "type": "string", "description": "Script text" },
                "file": { "type": "string", "description": "Path to a script file" },
                "sizes": { "type": "array", "items": { "type": "string" }, "description": "e.g. [\"80x24\", \"40x12\"]" },
            },
        })),
    ];
    Value::Array(
        specs
            .into_iter()
            .map(|(name, spec)| {
                let mut schema = json!({ "type": "object", "properties": spec["properties"] });
                if let Some(required) = spec.get("required") {
                    schema["required"] = required.clone();
                }
                json!({ "name": name, "description": spec["description"], "inputSchema": schema })
            })
            .collect(),
    )
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(char::from(TABLE[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn every_tool_call_maps_to_a_valid_command() {
        let calls = [
            ("start", json!({ "command": ["htop", "-d", "10"], "size": "100x30", "env": { "A": "b" } })),
            ("type", json!({ "text": "hello", "enter": true })),
            ("press", json!({ "keys": ["Down", "C-c"] })),
            ("paste", json!({ "text": "x" })),
            ("click", json!({ "row": 3, "col": 4, "double": true })),
            ("scroll", json!({ "direction": "down", "count": 2, "row": 1, "col": 1 })),
            ("drag", json!({ "from_row": 1, "from_col": 1, "to_row": 2, "to_col": 2 })),
            ("resize", json!({ "size": "40x12" })),
            ("look", json!({})),
            ("look", json!({ "history_lines": 50 })),
            ("wait", json!({ "text": "Saved", "timeout_secs": 3 })),
            ("expect", json!({ "styled": "Save", "style": "inverse" })),
            ("expect", json!({ "cursor_row": 1, "cursor_col": 2 })),
            ("lint", json!({})),
            ("stop", json!({ "grace_secs": 1 })),
            ("script", json!({})),
        ];
        for (name, args) in calls {
            let words = tool_words(name, &args).unwrap();
            cli::parse_script_command(&words)
                .unwrap_or_else(|error| panic!("{name} {words:?}: {error:#}"));
        }
    }

    #[test]
    fn tool_list_has_schemas_and_the_tools_the_dispatcher_knows() {
        let tools = tools();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        for name in names {
            let tool = tools
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap();
            assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
            if !matches!(name, "list" | "screenshot" | "run_script") {
                let _ = tool_words(name, &json!({}));
            }
        }
    }

    #[tokio::test]
    async fn initialize_and_list_respond_without_sessions() {
        let driver = Driver::new(std::path::PathBuf::from("bmux"));
        let init = handle(
            &driver,
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } }),
        )
        .await
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(handle(&driver, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await
            .is_none());
        let list = handle(&driver, &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            .await
            .unwrap();
        assert!(list["result"]["tools"].as_array().unwrap().len() > 10);
        let unknown = handle(&driver, &json!({ "jsonrpc": "2.0", "id": 3, "method": "nope" }))
            .await
            .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }
}
