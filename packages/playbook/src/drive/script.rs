//! Drive scripts: the commands an agent ran, one per line, in the same words
//! as the CLI (`press Down`, `wait --text Saved`), so a session becomes a
//! regression test by saving it, and a test is read and edited as easily as
//! it was written.
//!
//! ```text
//! # bmux drive script
//! start --size 80x24 -- ./target/debug/my-tui
//! press Down Down Enter
//! wait --text 'Saved'
//! expect --no-text 'error'
//! ```

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use super::cli::{self, Command};
use super::{Expect, Input, StartOptions, WaitFor};

const HEADER: &str = "# bmux drive script: replay with `bmux drive run FILE [--size 80x24,40x12]`\n";

/// One parsed script line.
#[derive(Debug, Clone)]
pub struct Step {
    pub line: usize,
    pub text: String,
    pub command: Command,
}

pub fn append(dir: &Path, line: &str) -> Result<()> {
    use std::io::Write as _;
    let path = super::session::script_path(dir);
    let fresh = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .context("failed recording the drive script")?;
    if fresh {
        file.write_all(HEADER.as_bytes())?;
    }
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn read(dir: &Path) -> Result<String> {
    match std::fs::read_to_string(super::session::script_path(dir)) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HEADER.to_owned()),
        Err(error) => Err(error.into()),
    }
}

pub fn start_line(options: &StartOptions, cwd: &Path) -> String {
    let mut line = format!("start --size {}x{}", options.cols, options.rows);
    // Scripts replay relative to where they are run unless started elsewhere.
    if std::env::current_dir().ok().as_deref() != Some(cwd) {
        let _ = write!(line, " --cwd {}", quote(&cwd.display().to_string()));
    }
    for (key, value) in &options.env {
        let _ = write!(line, " --env {}", quote(&format!("{key}={value}")));
    }
    line.push_str(" --");
    for word in &options.command {
        line.push(' ');
        line.push_str(&quote(word));
    }
    line
}

pub fn input_line(input: &Input) -> String {
    match input {
        Input::Type { text, enter } => {
            format!("type {}{}", quote(text), if *enter { " --enter" } else { "" })
        }
        Input::Press(keys) => {
            let words: Vec<String> = keys.iter().map(|key| quote(key)).collect();
            format!("press {}", words.join(" "))
        }
        Input::Paste(text) => format!("paste {}", quote(text)),
        Input::Click {
            row,
            col,
            button,
            double,
        } => {
            let mut line = format!("click {row} {col}");
            if button != "left" {
                let _ = write!(line, " --button {button}");
            }
            if *double {
                line.push_str(" --double");
            }
            line
        }
        Input::Scroll {
            up,
            count,
            row,
            col,
        } => {
            let mut line = format!("scroll {} {count}", if *up { "up" } else { "down" });
            if let (Some(row), Some(col)) = (row, col) {
                let _ = write!(line, " --at {row} {col}");
            }
            line
        }
        Input::Drag { from, to } => format!("drag {} {} {} {}", from.0, from.1, to.0, to.1),
    }
}

pub fn wait_line(condition: &WaitFor, timeout: Duration) -> String {
    let timeout = format!(" --timeout {}", timeout.as_secs_f64());
    match condition {
        WaitFor::Text(pattern) => format!("wait --text {}{timeout}", quote(pattern)),
        WaitFor::Gone(pattern) => format!("wait --gone {}{timeout}", quote(pattern)),
        WaitFor::Idle => format!("wait --idle{timeout}"),
        WaitFor::Exit => format!("wait --exit{timeout}"),
        WaitFor::Change => format!("wait --change{timeout}"),
    }
}

pub fn expect_line(expectation: &Expect) -> String {
    match expectation {
        Expect::Text(text) => format!("expect --text {}", quote(text)),
        Expect::NoText(text) => format!("expect --no-text {}", quote(text)),
        Expect::Cursor { row, col } => format!("expect --cursor {row} {col}"),
        Expect::Running => "expect --running".into(),
        Expect::Exited(Some(code)) => format!("expect --exit-code {code}"),
        Expect::Exited(None) => "expect --exited".into(),
        Expect::Style { text, style } => {
            format!("expect --styled {} --style {}", quote(text), quote(style))
        }
    }
}

/// Quote a word so a shell (and the script parser) reads it back unchanged.
pub fn quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,*^".contains(c));
    if safe {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Split a line into words as a POSIX shell would (quotes and backslashes,
/// no expansion).
pub fn split_words(line: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => bail!("unterminated single quote"),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => bail!("unterminated double quote"),
                        },
                        Some(c) => word.push(c),
                        None => bail!("unterminated double quote"),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(c) = chars.next() {
                    word.push(c);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// Parse a whole script.
pub fn parse(source: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let text = line.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let words = split_words(text).with_context(|| format!("line {}", index + 1))?;
        let command = cli::parse_script_command(&words)
            .with_context(|| format!("line {}: {text}", index + 1))?;
        if !cli::scriptable(&command) {
            bail!("line {}: `{}` cannot be used in a script", index + 1, words[0]);
        }
        steps.push(Step {
            line: index + 1,
            text: text.to_owned(),
            command,
        });
    }
    match steps.first() {
        None => bail!("the script has no commands"),
        Some(step) if !matches!(step.command, Command::Start(_)) => {
            bail!("line {}: a script must begin with `start -- <program>`", step.line)
        }
        Some(_) => Ok(steps),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_round_trips_through_splitting() {
        for word in ["plain", "two words", "it's", "", "a\"b", "$HOME", "C-c", "Down*3"] {
            let quoted = quote(word);
            assert_eq!(split_words(&quoted).unwrap(), vec![word.to_owned()], "{quoted}");
        }
    }

    #[test]
    fn splitting_handles_both_quote_styles() {
        assert_eq!(
            split_words(r#"type "a \"b\"" 'c d' e\ f"#).unwrap(),
            vec!["type", "a \"b\"", "c d", "e f"]
        );
        assert!(split_words("type 'open").is_err());
    }

    #[test]
    fn recorded_lines_parse_back() {
        let lines = [
            input_line(&Input::Type {
                text: "hello world".into(),
                enter: true,
            }),
            input_line(&Input::Press(vec!["Down*2".into(), "C-c".into()])),
            input_line(&Input::Click {
                row: 3,
                col: 4,
                button: "right".into(),
                double: true,
            }),
            input_line(&Input::Scroll {
                up: false,
                count: 2,
                row: Some(5),
                col: Some(6),
            }),
            input_line(&Input::Drag {
                from: (1, 2),
                to: (3, 4),
            }),
            wait_line(&WaitFor::Text("Saved.*".into()), Duration::from_secs(3)),
            expect_line(&Expect::NoText("error".into())),
            expect_line(&Expect::Exited(Some(0))),
            expect_line(&Expect::Style {
                text: "Save".into(),
                style: "inverse".into(),
            }),
            "resize 40x12".to_owned(),
        ];
        let source = format!("start --size 80x24 -- my-app --flag\n{}", lines.join("\n"));
        let steps = parse(&source).unwrap();
        assert_eq!(steps.len(), lines.len() + 1);
    }

    #[test]
    fn scripts_must_start_a_program() {
        assert!(parse("press Down").is_err());
        assert!(parse("# only a comment").is_err());
        assert!(parse("start -- app\nlook").is_err());
    }
}
