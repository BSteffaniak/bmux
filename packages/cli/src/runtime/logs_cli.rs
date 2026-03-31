use super::*;

pub(super) fn run_logs_path(as_json: bool) -> Result<u8> {
    let path = active_log_file_path();
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "path": path }))
                .context("failed to encode log path json")?
        );
        return Ok(0);
    }
    println!("{}", path.display());
    Ok(0)
}

pub(super) fn run_logs_level(as_json: bool) -> Result<u8> {
    let level = EFFECTIVE_LOG_LEVEL.get().copied().unwrap_or(Level::INFO);
    let value = match level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    };
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "level": value }))
                .context("failed to encode log level json")?
        );
        return Ok(0);
    }
    println!("{value}");
    Ok(0)
}

pub(super) fn run_logs_tail(lines: usize, since: Option<&str>, follow: bool) -> Result<u8> {
    let path = active_log_file_path();
    if !path.exists() {
        println!(
            "no log file in {} (expected prefix: bmux.log)",
            ConfigPaths::default().logs_dir().display()
        );
        return Ok(0);
    }

    let since_cutoff = match since {
        Some(value) => Some(parse_since_cutoff(value)?),
        None => None,
    };

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed reading log file {}", path.display()))?;
    let all_lines = content
        .lines()
        .filter(|line| line_matches_since(line, since_cutoff))
        .collect::<Vec<_>>();
    let start = all_lines.len().saturating_sub(lines.max(1));
    for line in &all_lines[start..] {
        println!("{line}");
    }

    if !follow {
        return Ok(0);
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .with_context(|| format!("failed opening log file {}", path.display()))?;
    let mut read_offset = file
        .metadata()
        .with_context(|| format!("failed reading metadata for {}", path.display()))?
        .len();

    loop {
        let metadata = file
            .metadata()
            .with_context(|| format!("failed reading metadata for {}", path.display()))?;
        let file_len = metadata.len();
        if file_len < read_offset {
            read_offset = 0;
        }
        if file_len > read_offset {
            file.seek(std::io::SeekFrom::Start(read_offset))
                .with_context(|| format!("failed seeking {}", path.display()))?;
            let mut chunk = String::new();
            file.read_to_string(&mut chunk)
                .with_context(|| format!("failed reading appended logs from {}", path.display()))?;
            if !chunk.is_empty() {
                print!("{chunk}");
                io::stdout().flush().context("failed flushing log output")?;
            }
            read_offset = file_len;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub(super) fn parse_since_cutoff(raw: &str) -> Result<OffsetDateTime> {
    let duration = parse_since_duration(raw)?;
    let now = OffsetDateTime::now_utc();
    Ok(now - duration)
}

pub(super) fn parse_since_duration(raw: &str) -> Result<TimeDuration> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("--since must be a non-empty duration like 30s, 10m, 2h, or 1d");
    }

    let split_at = trimmed
        .find(|char: char| !char.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (value_part, unit_part) = trimmed.split_at(split_at);
    if value_part.is_empty() {
        anyhow::bail!("--since must start with a number");
    }

    let amount = value_part
        .parse::<i64>()
        .with_context(|| format!("invalid --since value '{raw}'"))?;
    if amount < 0 {
        anyhow::bail!("--since must be non-negative");
    }

    let duration = match unit_part {
        "" | "s" => TimeDuration::seconds(amount),
        "m" => TimeDuration::minutes(amount),
        "h" => TimeDuration::hours(amount),
        "d" => TimeDuration::days(amount),
        _ => {
            anyhow::bail!(
                "invalid --since unit '{unit_part}' (use s, m, h, d; example: 30s, 10m, 2h, 1d)"
            )
        }
    };
    Ok(duration)
}

pub(super) fn line_matches_since(line: &str, cutoff: Option<OffsetDateTime>) -> bool {
    let Some(cutoff) = cutoff else {
        return true;
    };
    let Some(timestamp) = line.split_whitespace().next() else {
        return false;
    };
    let Ok(parsed) = OffsetDateTime::parse(timestamp, &Rfc3339) else {
        return false;
    };
    parsed >= cutoff
}
