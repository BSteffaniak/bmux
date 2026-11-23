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

/// Every agent name, lowercased, and every agent path, and the agent each
/// names.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    names: HashMap<String, String>,
    /// Aliases given as paths: they match that exact program path only.
    paths: HashMap<String, String>,
    /// Patterns of whole command lines, and the agent each names; they are
    /// tried before names.
    patterns: Vec<(regex::Regex, String)>,
}

impl Catalog {
    /// Names from `(id, aliases)` pairs (later pairs win), then rule-less
    /// `extra` agents. An alias containing a path separator is a path.
    pub fn new<'a>(
        agents: impl IntoIterator<Item = (&'a str, &'a [String])>,
        extra: &[String],
    ) -> Self {
        let mut names = HashMap::new();
        let mut paths = HashMap::new();
        for (id, aliases) in agents {
            names.insert(id.to_lowercase(), id.to_string());
            for alias in aliases.iter().map(|alias| alias.trim()) {
                if alias.contains(['/', '\\']) {
                    paths.insert(alias.to_string(), id.to_string());
                } else if !alias.is_empty() {
                    names.insert(alias.to_lowercase(), id.to_string());
                }
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
        Self {
            names,
            paths,
            patterns: Vec::new(),
        }
    }

    /// This catalog, also naming an agent by each `(pattern, agent)`: a
    /// command line the pattern matches anywhere in runs that agent.
    #[must_use]
    pub fn with_patterns(
        mut self,
        patterns: impl IntoIterator<Item = (regex::Regex, String)>,
    ) -> Self {
        self.patterns.extend(patterns);
        self
    }

    /// The agent the foreground job's command lines (leader first) run.
    #[must_use]
    pub fn identify(&self, commands: &[String]) -> Option<String> {
        commands.iter().find_map(|command| {
            self.patterns
                .iter()
                .find(|(pattern, _)| pattern.is_match(command))
                .map(|(_, agent)| agent.clone())
                .or_else(|| self.identify_command(command))
        })
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
        if let Some(agent) = self.paths.get(word) {
            return Some(agent.clone());
        }
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
/// shell's leading `-`, and seen through a Nix wrapper.
fn program_name(word: &str) -> String {
    let name = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .trim_start_matches('-')
        .to_lowercase();
    nix_wrapped(&name).map_or_else(|| name.clone(), str::to_string)
}

/// The program a Nix wrapper runs: `wrapProgram` renames `NAME` to
/// `.NAME-wrapped`, and each further wrapping appends `_`.
fn nix_wrapped(name: &str) -> Option<&str> {
    name.strip_prefix('.')?
        .trim_end_matches('_')
        .strip_suffix("-wrapped")
        .filter(|wrapped| !wrapped.is_empty())
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
    fn nix_wrapped_programs_name_their_agents() {
        for (command, agent) in [
            (
                "/nix/store/9jdr2bi3w9h9jc67lxa76vd84ygsgrp7-claude-code-with-ca-certs/bin/.claude-wrapped_ --resume",
                "claude",
            ),
            ("/nix/store/x-claude-code/bin/.claude-wrapped", "claude"),
            ("/nix/store/x-codex/bin/.codex-wrapped__", "codex"),
            (
                "/nix/store/x-nodejs/bin/.node-wrapped /x/@anthropic-ai/claude-code/cli.js",
                "claude",
            ),
        ] {
            assert_eq!(identify(command).as_deref(), Some(agent), "{command}");
        }
        assert_eq!(identify("/nix/store/x-vim/bin/.vim-wrapped"), None);
        assert_eq!(identify(".-wrapped"), None);
    }

    #[test]
    fn aliases_may_be_names_or_exact_paths() {
        let claude = vec!["my-claude".to_string(), "/opt/tools/bin/launch".to_string()];
        let catalog = Catalog::new([("claude", claude.as_slice())], &[]);
        let identify = |command: &str| catalog.identify(&[command.to_string()]);
        assert_eq!(identify("/usr/bin/My-Claude -c").as_deref(), Some("claude"));
        assert_eq!(identify("/opt/tools/bin/launch").as_deref(), Some("claude"));
        assert_eq!(
            identify("node /opt/tools/bin/launch --x").as_deref(),
            Some("claude")
        );
        // A path alias names that program only, not others called the same.
        assert_eq!(identify("/usr/bin/launch"), None);
    }

    #[test]
    fn later_processes_of_the_job_count_too() {
        let commands = ["tee log".to_string(), "claude".to_string()];
        assert_eq!(catalog().identify(&commands).as_deref(), Some("claude"));
    }
}
