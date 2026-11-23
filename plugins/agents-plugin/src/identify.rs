//! Which agent a pane's foreground job is.
//!
//! The pane runtime lists each foreground process's command line. An agent is
//! named by its program (`claude`, `codex`), or by the script a runtime runs
//! (`node …/@anthropic-ai/claude-code/cli.js`, `npx @openai/codex`), matched
//! against every rule file's id and aliases and the names configured without
//! rules.

use std::collections::HashMap;

/// Other names agents run as, beyond their rule files' aliases.
const EXTRA_NAMES: &[(&str, &str)] = &[
    (".cline", "cline"),
    ("opencode2", "opencode"),
    ("github-copilot", "copilot"),
    ("antigravity", "agy"),
];

/// Agents known by name only: no screen rules, so their state comes from
/// their reports or their output.
pub const DEFAULT_RULELESS_AGENTS: &[&str] = &[
    "aider",
    "goose",
    "crush",
    "omp",
    "mastracode",
    "openhands",
    "plandex",
];

/// Programs that run another program named among their arguments.
const RUNTIMES: &[&str] = &[
    "node", "nodejs", "bun", "deno", "python", "python3", "npx", "bunx", "pnpx", "pnpm", "yarn",
    "uv", "uvx", "pipx", "env", "sh", "bash", "zsh", "dash", "fish",
];

/// Options of those runtimes whose argument is code, not a program.
const CODE_OPTIONS: &[&str] = &["-e", "--eval", "-p", "--print", "-c", "-m"];

/// Every agent name, lowercased, and the agent it names.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    names: HashMap<String, String>,
}

impl Catalog {
    /// Names from `(id, aliases)` pairs, then rule-less `extra` agents.
    pub fn new<'a>(
        agents: impl IntoIterator<Item = (&'a str, &'a [String])>,
        extra: &[String],
    ) -> Self {
        let mut names = HashMap::new();
        for (id, aliases) in agents {
            names.insert(id.to_lowercase(), id.to_string());
            for alias in aliases {
                names.insert(alias.to_lowercase(), id.to_string());
            }
        }
        for (name, id) in EXTRA_NAMES {
            if names.values().any(|known| known == id) {
                names
                    .entry((*name).to_string())
                    .or_insert_with(|| (*id).to_string());
            }
        }
        for name in extra {
            let name = name.trim().to_lowercase();
            if !name.is_empty() {
                names.entry(name.clone()).or_insert(name);
            }
        }
        Self { names }
    }

    /// The agent the foreground job's command lines (leader first) run.
    #[must_use]
    pub fn identify(&self, commands: &[String]) -> Option<String> {
        commands
            .iter()
            .find_map(|command| self.identify_command(command))
    }

    fn identify_command(&self, command: &str) -> Option<String> {
        let words = command.split_whitespace().collect::<Vec<_>>();
        let program = words.first()?;
        if let Some(agent) = self.lookup(program) {
            return Some(agent);
        }
        let runtime = program_name(program);
        if !RUNTIMES.contains(&runtime.as_str()) {
            return None;
        }
        // The first argument that is neither an option nor an assignment is
        // what the runtime runs; code given inline names no program.
        for arg in &words[1..] {
            if CODE_OPTIONS.contains(arg) {
                return None;
            }
            if arg.starts_with('-') || (runtime == "env" && arg.contains('=')) {
                continue;
            }
            // `pnpm dlx`, `uv run`, `yarn dlx`: the next argument.
            if matches!(*arg, "dlx" | "run" | "exec" | "x") && runtime != "node" {
                continue;
            }
            return self.lookup_target(arg);
        }
        None
    }

    /// A runtime's target: a program, a script path (any of its last
    /// components may name the agent's package) or a package spec.
    fn lookup_target(&self, target: &str) -> Option<String> {
        if let Some(agent) = self.lookup(target) {
            return Some(agent);
        }
        let package = target.strip_prefix('@').map_or(target, |scoped| {
            scoped.split_once('/').map_or(scoped, |(_, name)| name)
        });
        let package = package.split('@').next().unwrap_or(package);
        if let Some(agent) = self.lookup(package) {
            return Some(agent);
        }
        target
            .split(['/', '\\'])
            .rev()
            .skip(1)
            .take(4)
            .find_map(|component| self.lookup(component))
    }

    fn lookup(&self, word: &str) -> Option<String> {
        let name = program_name(word);
        let name = [".js", ".mjs", ".cjs", ".ts", ".py", ".exe", ".cmd"]
            .iter()
            .find_map(|extension| name.strip_suffix(extension))
            .unwrap_or(&name);
        if let Some(agent) = self.names.get(name) {
            return Some(agent.clone());
        }
        // Muse's launcher runs `muse-bin-<version>`.
        name.strip_prefix("muse-bin-")
            .filter(|version| version.starts_with(|c: char| c.is_ascii_digit()))
            .and_then(|_| self.names.get("muse").cloned())
    }
}

/// A program's name: its path's last component, lowercased, without a login
/// shell's leading `-`.
fn program_name(word: &str) -> String {
    word.rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .trim_start_matches('-')
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        let claude = vec!["claude-code".to_string()];
        let codex = Vec::new();
        let cline = Vec::new();
        Catalog::new(
            [
                ("claude", claude.as_slice()),
                ("codex", codex.as_slice()),
                ("cline", cline.as_slice()),
            ],
            &["aider".to_string(), "MyBot".to_string()],
        )
    }

    fn identify(command: &str) -> Option<String> {
        catalog().identify(&[command.to_string()])
    }

    #[test]
    fn programs_name_their_agents() {
        assert_eq!(identify("claude --resume").as_deref(), Some("claude"));
        assert_eq!(identify("/opt/bin/Claude-Code").as_deref(), Some("claude"));
        assert_eq!(identify("codex.exe").as_deref(), Some("codex"));
        assert_eq!(identify("aider --model x").as_deref(), Some("aider"));
        assert_eq!(identify("mybot").as_deref(), Some("mybot"));
        assert_eq!(identify(".cline").as_deref(), Some("cline"));
        assert_eq!(identify("vim notes.md"), None);
    }

    #[test]
    fn runtimes_name_the_agent_they_run() {
        for (command, agent) in [
            (
                "node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
                "claude",
            ),
            (
                "node --no-warnings /home/me/.local/bin/codex --full-auto",
                "codex",
            ),
            ("npx -y @openai/codex@latest", "codex"),
            ("pnpm dlx claude-code", "claude"),
            ("env FOO=1 claude", "claude"),
            ("bun /x/claude.js", "claude"),
            ("uv run aider", "aider"),
        ] {
            assert_eq!(identify(command).as_deref(), Some(agent), "{command}");
        }
        // Code given inline, and programs that are not agents, name none.
        assert_eq!(identify("node -e claude()"), None);
        assert_eq!(identify("bash -c claude"), None);
        assert_eq!(identify("python3 manage.py runserver"), None);
        assert_eq!(identify("node"), None);
    }

    #[test]
    fn later_processes_of_the_job_count_too() {
        let commands = ["tee log".to_string(), "claude".to_string()];
        assert_eq!(catalog().identify(&commands).as_deref(), Some("claude"));
    }
}
