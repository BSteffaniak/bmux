//! Process fixtures for the remote-access acceptance tests: an isolated host runtime, an
//! isolated daemonless client profile, real pseudo-terminals with a rendered screen, and the
//! interactive pairing ceremony exactly as a person performs it.
//!
//! Every process a fixture starts is owned by a handle that kills it on drop, and every
//! directory lives under one short private root that is removed afterwards.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use bmux_pty::{PtyCommand, PtyEvent, PtyExit, PtyProcess, PtySize, PtyWriter};
use bmux_terminal_grid::{GridLimits, TerminalGridStream, visible_text_lines};
use regex::bytes::Regex;

use crate::support::{ServerEnv, assert_success, bmux_binary, combined_output};

/// Terminal size used by every viewer, so screens of different clients compare cell for cell.
pub const COLS: u16 = 100;
pub const ROWS: u16 = 30;
/// Deadline for one interactive step (a prompt, a screen change, a pairing transition):
/// each step finishes on its own, so this only bounds a hang.
pub const STEP: Duration = crate::support::HANG_GUARD;
/// How long an assertion that something does NOT happen keeps watching.
pub const QUIET: Duration = Duration::from_secs(4);
/// The pane shell's prompt: seeing it means an attach has drawn the pane.
pub const PROMPT: &str = "bmux-test$";
/// The tmux-style prefix key (`ctrl+a`) of [`PREFIX_KEYMAP`].
pub const PREFIX: &[u8] = b"\x01";

/// Prefix chords for the insert mode the attach starts in. The default keymap is modal:
/// its prefix (`runtime`) table is unused, and insert mode binds no chords of its own.
pub const PREFIX_KEYMAP: &str = r#"
[keybindings.modes.insert]
label = "INSERT"
passthrough = true

[keybindings.modes.insert.bindings]
"ctrl+a %" = "plugin:bmux.tabs:split-pane --direction vertical"
"ctrl+a [" = "enter_scroll_mode"
"#;

/// The phrase both sides show: four groups of four lowercase hex digits.
pub const PHRASE: &str = "([0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4})";

fn regex(pattern: &str) -> Regex {
    // The workspace builds `regex` without Unicode classes; patterns here are ASCII only.
    Regex::new(&format!("(?-u){pattern}")).expect("valid test pattern")
}

/// A free UDP port on 127.0.0.1 (Iroh binds UDP). Racy by nature, like every reserve helper.
#[must_use]
pub fn reserve_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .and_then(|socket| socket.local_addr())
        .map(|addr| addr.port())
        .expect("reserve loopback UDP port")
}

/// Removes inherited `BMUX_*` variables and pins every directory and `HOME` into `root`, so
/// nothing reads or writes the developer's own bmux, shell or SSH state.
fn isolated_env(root: &Path, dirs: &Dirs) -> Vec<(OsString, Option<OsString>)> {
    let mut env: Vec<(OsString, Option<OsString>)> = std::env::vars_os()
        .filter(|(key, _)| key.to_string_lossy().starts_with("BMUX_"))
        .map(|(key, _)| (key, None))
        .collect();
    let home = root.join("home");
    for (key, value) in [
        ("HOME", home.clone()),
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("BMUX_CONFIG_DIR", dirs.config.clone()),
        ("BMUX_RUNTIME_DIR", dirs.runtime.clone()),
        ("BMUX_DATA_DIR", dirs.data.clone()),
        ("BMUX_STATE_DIR", dirs.state.clone()),
        ("BMUX_LOG_DIR", dirs.log.clone()),
    ] {
        env.push((key.into(), Some(value.into_os_string())));
    }
    // The clipboard: no display or native pasteboard reaches the developer's own, and
    // copies go through the platform's clipboard command, which lands them in a file.
    let bin = clipboard_capture(root);
    env.push((bmux_clipboard::BACKEND_ENV.into(), Some("command".into())));
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path)))
        .expect("PATH with the clipboard capture");
    env.push(("PATH".into(), Some(path)));
    env.push(("DISPLAY".into(), None));
    env.push(("WAYLAND_DISPLAY".into(), Some(NO_DISPLAY.into())));
    env.push(("BMUX_NO_BASE_CONFIG".into(), Some("1".into())));
    env.push(("BMUX_RUNTIME_NAME".into(), Some("default".into())));
    env.push(("TERM".into(), Some("xterm-256color".into())));
    env
}

/// A Wayland display name no compositor serves, so nothing reaches a real desktop even
/// when a native clipboard would be tried.
const NO_DISPLAY: &str = "bmux-test-no-display";

/// Where copies made by processes under `root` land.
fn clipboard_file(root: &Path) -> PathBuf {
    root.join("clipboard.txt")
}

/// A directory holding each platform's clipboard copy command (`wl-copy`, `pbcopy`), each
/// writing its input to [`clipboard_file`].
fn clipboard_capture(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let bin = root.join("clipboard-bin");
    if !bin.exists() {
        private_dir(&bin);
        let target = clipboard_file(root);
        for name in ["wl-copy", "pbcopy"] {
            let program = bin.join(name);
            std::fs::write(
                &program,
                format!(
                    "#!/bin/sh\ncat > {}.tmp && mv {}.tmp {}\n",
                    shell_quote(&target),
                    shell_quote(&target),
                    shell_quote(&target)
                ),
            )
            .expect("write the clipboard capture");
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
                .expect("make the clipboard capture executable");
        }
    }
    bin
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[derive(Clone, Debug)]
struct Dirs {
    config: PathBuf,
    runtime: PathBuf,
    data: PathBuf,
    state: PathBuf,
    log: PathBuf,
}

fn private_dir(path: &Path) {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .expect("create private fixture directory");
}

/// Settings for the connections plugin, written into a profile's `bmux.toml`.
#[derive(Clone, Debug, Default)]
pub struct RemoteSettings {
    /// `remote.bind_addr`: the address this process's endpoint binds (host and client).
    pub bind_addr: Option<SocketAddr>,
    /// `remote.external_addrs`: extra addresses the host advertises in links and hints.
    pub external_addrs: Vec<SocketAddr>,
    /// Custom relay URLs; `None` disables relays entirely (never public infrastructure).
    pub relay_urls: Option<Vec<String>>,
}

impl RemoteSettings {
    #[must_use]
    pub fn loopback_host(port: u16) -> Self {
        Self {
            bind_addr: Some(SocketAddr::from(([127, 0, 0, 1], port))),
            ..Self::default()
        }
    }

    fn toml(&self) -> String {
        let mut text = String::from("[plugins.settings.\"bmux.connections\".remote]\n");
        if let Some(addr) = self.bind_addr {
            let _ = writeln!(text, "bind_addr = \"{addr}\"");
        }
        if !self.external_addrs.is_empty() {
            let addrs: Vec<String> = self
                .external_addrs
                .iter()
                .map(|addr| format!("\"{addr}\""))
                .collect();
            let _ = writeln!(text, "external_addrs = [{}]", addrs.join(", "));
        }
        text.push_str("\n[plugins.settings.\"bmux.connections\".remote.relay]\n");
        match &self.relay_urls {
            None => text.push_str("mode = \"disabled\"\n"),
            Some(urls) => {
                let urls: Vec<String> = urls.iter().map(|url| format!("\"{url}\"")).collect();
                let _ = writeln!(text, "mode = \"custom\"\nurls = [{}]", urls.join(", "));
            }
        }
        text
    }
}

/// A shell that starts quietly (no rc files, the fixed prompt [`PROMPT`]), so screens only
/// show what the test typed and what it made the shell print.
fn write_quiet_shell(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let shell = root.join("quiet-shell");
    std::fs::write(
        &shell,
        "#!/bin/sh\nunset ENV BASH_ENV\nPS1='bmux-test$ '\nexport PS1\nexec /bin/sh -i\n",
    )
    .expect("write quiet shell");
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700))
        .expect("make quiet shell executable");
    shell
}

/// An isolated host runtime: a real `bmux server` with remote hosting configured.
pub struct Host {
    pub env: ServerEnv,
}

impl Host {
    /// Starts the server and enables hosting (`bmux remote enable --yes`).
    #[must_use]
    pub fn start(label: &str, settings: RemoteSettings) -> Self {
        Self::start_with(label, settings, "")
    }

    /// [`Host::start`] with `extra` appended to the server's configuration.
    #[must_use]
    pub fn start_with(label: &str, settings: RemoteSettings, extra: &str) -> Self {
        let mut env = ServerEnv::new(label);
        env.isolate_shell_config();
        private_dir(&env.root().join("home").join(".config"));
        let shell = write_quiet_shell(env.root());
        env.write_config(&format!(
            "[general]\ndefault_shell = {}\nserver_timeout = 30000\n[behavior]\nrestore_last_layout = false\npane_shell_integration = false\n[multi_client]\ndefault_follow_mode = false\n\n{}\n{extra}",
            serde_json::to_string(&shell.to_string_lossy()).expect("quote shell path"),
            settings.toml(),
        ));
        env.start();
        let host = Self { env };
        host.ok(&["remote", "enable", "--yes"], "enable hosting");
        host
    }

    fn dirs(&self) -> Dirs {
        Dirs {
            config: self.env.config_dir.clone(),
            runtime: self.env.runtime_dir.clone(),
            data: self.env.data_dir.clone(),
            state: self.env.state_dir.clone(),
            log: self.env.log_dir.clone(),
        }
    }

    /// Runs a host CLI command to completion.
    #[must_use]
    pub fn run(&self, args: &[&str]) -> Output {
        self.env.run(args)
    }

    /// Runs a host CLI command and requires success.
    pub fn ok(&self, args: &[&str], what: &str) -> String {
        let output = self.run(args);
        assert_success(&output, what);
        combined_output(&output)
    }

    /// Starts a host CLI command on its own terminal.
    #[must_use]
    pub fn terminal(&self, name: &str, args: &[&str]) -> Term {
        Term::spawn(name, self.env.root(), &self.dirs(), args)
    }

    /// Creates a detached session.
    pub fn new_session(&self, name: &str) {
        self.ok(&["new-session", name], "create session");
    }

    /// Creates a detached session as a tab of the workspace, which tab navigation
    /// reaches (a bare `new-session NAME` creates a session but no tab).
    pub fn new_tab(&self, name: &str) {
        self.ok(&["new-session", "--name", name], "create tab");
    }

    /// Attaches locally (the owner's own view) to `session`.
    #[must_use]
    pub fn attach(&self, session: &str) -> Term {
        let mut term = self.terminal("host-viewer", &["attach", session]);
        term.wait_attached();
        term
    }

    /// What the host's own processes last copied to the clipboard, if anything.
    #[must_use]
    pub fn clipboard(&self) -> Option<String> {
        std::fs::read_to_string(clipboard_file(self.env.root())).ok()
    }

    /// The host's endpoint id, read from `bmux remote status`.
    #[must_use]
    pub fn endpoint_id(&self) -> iroh::EndpointId {
        let status = self.ok(&["remote", "status"], "read host status");
        status
            .split(|c: char| !c.is_ascii_alphanumeric())
            .find_map(|word| (word.len() >= 52).then(|| word.parse().ok()).flatten())
            .unwrap_or_else(|| panic!("`bmux remote status` names no endpoint id: {status}"))
    }

    /// Whether the host server process is still running.
    #[must_use]
    pub fn is_running(&mut self) -> bool {
        self.env.is_running()
    }
}

/// An isolated client profile that never starts a server: its runtime directory is a path
/// that does not exist until something (wrongly) creates it.
pub struct Client {
    root: tempfile::TempDir,
    dirs: Dirs,
}

impl Client {
    #[must_use]
    pub fn new(label: &str, settings: &RemoteSettings) -> Self {
        let root = tempfile::Builder::new()
            .prefix(&format!("bra-{label}-"))
            .disable_cleanup(std::env::var_os("BMUX_TEST_KEEP_ARTIFACTS").is_some())
            .tempdir_in(short_tmp_root())
            .expect("create client root");
        let base = root.path();
        let dirs = Dirs {
            config: base.join("config"),
            runtime: base.join("runtime"),
            data: base.join("data"),
            state: base.join("state"),
            log: base.join("logs"),
        };
        for dir in [&dirs.config, &dirs.data, &dirs.state, &dirs.log] {
            private_dir(dir);
        }
        private_dir(&base.join("home").join(".config"));
        std::fs::write(dirs.config.join("bmux.toml"), settings.toml()).expect("client config");
        Self { root, dirs }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// What this client's processes last copied to the clipboard, if anything.
    #[must_use]
    pub fn clipboard(&self) -> Option<String> {
        std::fs::read_to_string(clipboard_file(self.root())).ok()
    }

    /// Adds `toml` to this client's configuration.
    pub fn configure(&self, toml: &str) {
        let path = self.dirs.config.join("bmux.toml");
        let mut config = std::fs::read_to_string(&path).expect("read client config");
        config.push_str(toml);
        std::fs::write(&path, config).expect("write client config");
    }

    #[must_use]
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(bmux_binary());
        for (key, value) in isolated_env(self.root(), &self.dirs) {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        command.args(args).current_dir(self.root());
        command
    }

    /// Runs a client CLI command to completion, bounded by [`STEP`].
    #[must_use]
    pub fn run(&self, args: &[&str]) -> Output {
        let stdout = tempfile::NamedTempFile::new_in(self.root()).expect("stdout capture");
        let stderr = tempfile::NamedTempFile::new_in(self.root()).expect("stderr capture");
        let mut child = self
            .command(args)
            .stdin(Stdio::null())
            .stdout(stdout.reopen().expect("reopen stdout"))
            .stderr(stderr.reopen().expect("reopen stderr"))
            .spawn()
            .expect("spawn client command");
        let deadline = Instant::now() + STEP;
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll client command") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("client command {args:?} did not finish within {STEP:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Output {
            status,
            stdout: std::fs::read(stdout.path()).unwrap_or_default(),
            stderr: std::fs::read(stderr.path()).unwrap_or_default(),
        }
    }

    pub fn ok(&self, args: &[&str], what: &str) -> String {
        let output = self.run(args);
        assert_success(&output, what);
        combined_output(&output)
    }

    /// Starts a client CLI command on its own terminal.
    #[must_use]
    pub fn terminal(&self, name: &str, args: &[&str]) -> Term {
        Term::spawn(name, self.root(), &self.dirs, args)
    }

    /// `bmux connect <alias> <session>`, waiting until the attach has drawn.
    #[must_use]
    pub fn connect(&self, alias: &str, session: &str) -> Term {
        let mut term = self.terminal("remote-viewer", &["connect", alias, session]);
        term.wait_attached();
        term
    }

    /// Asserts the daemonless-client invariant: no runtime directory, socket or server
    /// metadata exists anywhere under this profile.
    pub fn assert_daemonless(&self) {
        assert!(
            !self.dirs.runtime.exists(),
            "the connecting side created its runtime directory {}",
            self.dirs.runtime.display()
        );
        let mut stack = vec![self.root().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                let file_type = entry.file_type().expect("inspect client file");
                let name = entry.file_name().to_string_lossy().into_owned();
                {
                    use std::os::unix::fs::FileTypeExt as _;
                    assert!(
                        !file_type.is_socket(),
                        "the connecting side created a socket: {}",
                        path.display()
                    );
                }
                assert!(
                    !name.starts_with("server-meta") && !name.starts_with("server-lifetime"),
                    "the connecting side created server state: {}",
                    path.display()
                );
                if file_type.is_dir() {
                    stack.push(path);
                }
            }
        }
    }
}

/// `$TMPDIR` when nextest's setup script made it short (see `scripts/nextest-test-tmpdir.sh`),
/// otherwise `/tmp`: Unix socket paths must stay under 104 bytes on macOS.
fn short_tmp_root() -> PathBuf {
    let tmp = std::env::temp_dir();
    if tmp.as_os_str().len() <= 40 {
        tmp
    } else {
        PathBuf::from("/tmp")
    }
}

/// A process on a real pseudo-terminal, with the screen it drew.
pub struct Term {
    name: String,
    process: PtyProcess,
    writer: PtyWriter,
    screen: TerminalGridStream,
    /// Everything the process wrote, for prompts and escape sequences.
    raw: Vec<u8>,
    /// Where the next `expect` starts searching in `raw`.
    cursor: usize,
    exit: Option<PtyExit>,
}

impl Term {
    fn spawn(name: &str, cwd: &Path, dirs: &Dirs, args: &[&str]) -> Self {
        let root = dirs.config.parent().expect("profile root");
        Self::start(name, &bmux_binary(), args, cwd, &isolated_env(root, dirs))
    }

    /// Starts `program` on a new terminal of [`COLS`] x [`ROWS`].
    #[must_use]
    pub fn start(
        name: &str,
        program: &Path,
        args: &[&str],
        cwd: &Path,
        env: &[(OsString, Option<OsString>)],
    ) -> Self {
        let mut command = PtyCommand::new(program).args(args).current_dir(cwd);
        for (key, value) in env {
            command = match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        let process = command
            .spawn(PtySize {
                rows: ROWS,
                cols: COLS,
            })
            .expect("spawn a process on a pseudo-terminal");
        let writer = process.writer().expect("terminal writer");
        Self {
            name: name.to_owned(),
            process,
            writer,
            screen: TerminalGridStream::new(COLS, ROWS, GridLimits { scrollback_rows: 0 })
                .expect("screen model"),
            raw: Vec::new(),
            cursor: 0,
            exit: None,
        }
    }

    /// Takes output for at most `wait`; returns whether anything happened.
    fn pump(&mut self, wait: Duration) -> bool {
        if self.exit.is_some() {
            std::thread::sleep(wait.min(Duration::from_millis(20)));
            return false;
        }
        let Some(event) = self.process.next_event_timeout(wait) else {
            return false;
        };
        self.take(event);
        for event in self.process.ready_events(256) {
            self.take(event);
        }
        true
    }

    fn take(&mut self, event: PtyEvent) {
        match event {
            PtyEvent::Output(bytes) | PtyEvent::Stdout(bytes) => {
                self.answer_queries(&bytes);
                self.screen.process(&bytes);
                self.raw.extend_from_slice(&bytes);
            }
            PtyEvent::Exited(exit) => self.exit = Some(exit),
        }
    }

    /// Answers the two queries a terminal must answer for programs that wait on them:
    /// primary device attributes and the cursor position report.
    fn answer_queries(&self, bytes: &[u8]) {
        let windows = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        if windows(b"\x1b[c") || windows(b"\x1b[0c") {
            let _ = self.writer.write(b"\x1b[?62;22c".to_vec());
        }
        if windows(b"\x1b[6n") {
            let cursor = self.screen.grid().cursor();
            let _ = self
                .writer
                .write(format!("\x1b[{};{}R", cursor.row + 1, cursor.col + 1).into_bytes());
        }
    }

    /// Sends bytes as typed input.
    pub fn send(&mut self, bytes: &[u8]) {
        self.writer
            .write(bytes.to_vec())
            .unwrap_or_else(|_| panic!("{}: terminal closed; {}", self.name, self.diagnostic()));
        // Let a key sequence arrive as its own read, as from a person.
        self.pump(Duration::from_millis(50));
    }

    /// Types `printf` of `marker` spelled in octal, then Enter: the screen shows `marker`
    /// only if the shell ran the command, never from the echo of what was typed.
    pub fn type_marker(&mut self, marker: &str) {
        self.send(&printf_line(marker));
    }

    /// Waits for `pattern` in output written since the previous match; returns its groups.
    pub fn expect(&mut self, pattern: &str, timeout: Duration) -> Vec<String> {
        let regex = regex(pattern);
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(captures) = regex.captures(&self.raw[self.cursor..]) {
                let end = captures.get(0).expect("whole match").end();
                let groups = captures
                    .iter()
                    .map(|group| {
                        group.map_or_else(String::new, |group| {
                            String::from_utf8_lossy(group.as_bytes()).into_owned()
                        })
                    })
                    .collect();
                self.cursor += end;
                return groups;
            }
            assert!(
                Instant::now() < deadline,
                "{}: `{pattern}` not seen within {timeout:?}; {}",
                self.name,
                self.diagnostic()
            );
            self.pump(Duration::from_millis(50));
        }
    }

    /// The visible screen, one string per row.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        visible_text_lines(self.screen.grid(), 0, usize::from(ROWS))
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.lines().join("\n")
    }

    /// Waits until the visible screen satisfies `check`.
    pub fn wait_screen(&mut self, what: &str, timeout: Duration, check: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            if check(&self.text()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{}: {what} not on screen within {timeout:?}; {}",
                self.name,
                self.diagnostic()
            );
            self.pump(Duration::from_millis(50));
        }
    }

    pub fn wait_text(&mut self, text: &str, timeout: Duration) {
        self.wait_screen(&format!("`{text}`"), timeout, |screen| {
            screen.contains(text)
        });
    }

    /// Watches for `duration` and fails if `text` ever shows.
    pub fn assert_never_shows(&mut self, text: &str, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            assert!(
                !self.text().contains(text),
                "{}: `{text}` appeared; {}",
                self.name,
                self.diagnostic()
            );
            self.pump(Duration::from_millis(50));
        }
        assert!(
            !self.text().contains(text),
            "{}: `{text}` appeared",
            self.name
        );
    }

    /// Waits for the attach to draw its first frame: the quiet shell's prompt.
    pub fn wait_attached(&mut self) {
        self.wait_text(PROMPT, STEP);
    }

    /// Waits for the process to exit.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<PtyExit> {
        let deadline = Instant::now() + timeout;
        while self.exit.is_none() && Instant::now() < deadline {
            self.pump(Duration::from_millis(20));
        }
        self.exit
    }

    #[must_use]
    pub fn has_exited(&mut self) -> bool {
        while self.pump(Duration::ZERO) {}
        self.exit.is_some()
    }

    /// Whether the process ever wrote `needle`.
    #[must_use]
    pub fn raw_contains(&self, needle: &[u8]) -> bool {
        self.raw
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[must_use]
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// Bounded, secret-redacted evidence for a failed step.
    fn diagnostic(&self) -> String {
        let tail = &self.raw[self.raw.len().saturating_sub(2400)..];
        let tail = redact(&String::from_utf8_lossy(tail));
        format!(
            "exit={:?}\n--- screen ---\n{}\n--- output tail ---\n{tail:?}",
            self.exit,
            redact(&self.text())
        )
    }
}

/// Pairing links are secrets: never print them in test failures.
fn redact(text: &str) -> String {
    let link = regex("bmux-pair:[^ \\t\\r\\n\\x1b]+");
    String::from_utf8_lossy(&link.replace_all(text.as_bytes(), &b"[pairing link redacted]"[..]))
        .into_owned()
}

/// `printf '\NNN...\n'` + Enter for `marker`.
#[must_use]
pub fn printf_line(marker: &str) -> Vec<u8> {
    let mut line = String::from("printf '");
    for byte in marker.bytes() {
        let _ = write!(line, "\\{byte:03o}");
    }
    line.push_str("\\n'\r");
    line.into_bytes()
}

/// A marker unique to this run, so nothing stale can satisfy a check.
#[must_use]
pub fn marker(label: &str) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{label}-{}", &id[..8]).to_ascii_uppercase()
}

/// A profile that can run the connecting side of pairing: a daemonless [`Client`], or a
/// [`Host`] node's own profile (cluster nodes pair with each other).
pub trait Connecting {
    fn terminal(&self, name: &str, args: &[&str]) -> Term;
}

impl Connecting for Client {
    fn terminal(&self, name: &str, args: &[&str]) -> Term {
        Self::terminal(self, name, args)
    }
}

impl Connecting for Host {
    fn terminal(&self, name: &str, args: &[&str]) -> Term {
        Self::terminal(self, name, args)
    }
}

/// The role a device is approved with.
#[derive(Clone, Copy, Debug)]
pub enum Role {
    Full,
    ViewOnly,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ViewOnly => "view-only",
        }
    }
}

/// Pairs `client` with `host` through the public ceremony and returns the client's terminal,
/// which attaches after `Paired. Saved host: <alias>`.
pub fn pair(host: &Host, client: &impl Connecting, alias: &str, device: &str, role: Role) -> Term {
    let mut owner = host.terminal(
        "host-pair",
        &["remote", "pair", "--role", role.as_str(), "--name", device],
    );
    let link = owner.expect("Pairing link: (bmux-pair:[^ \\t\\r\\n\\x1b]+)", STEP)[1].clone();
    let mut connecting = client.terminal(
        "client-pair",
        &[
            "connect",
            "--pair",
            "--name",
            alias,
            "--device-name",
            device,
        ],
    );
    connecting.expect("Pairing link \\(input hidden\\): ", STEP);
    connecting.send(format!("{link}\r").as_bytes());
    assert!(
        !connecting.raw_contains(link.as_bytes()),
        "the pairing link was echoed on the connecting terminal"
    );
    let host_phrase = owner.expect(
        &format!("Compare this phrase on the other device: {PHRASE}"),
        STEP,
    )[1]
    .clone();
    let client_phrase =
        connecting.expect(&format!("Verify on the host: {PHRASE}"), STEP)[1].clone();
    assert_eq!(
        host_phrase, client_phrase,
        "both sides must show one phrase"
    );
    owner.expect("\\[y/N\\]", STEP);
    owner.send(b"y\r");
    connecting.expect(&format!("Paired\\. Saved host: {alias}"), STEP);
    let exit = owner
        .wait_exit(STEP)
        .expect("`remote pair` exits after approval");
    assert!(exit.success(), "`remote pair` failed: {exit:?}");
    connecting
}

/// Pairs and closes the pairing terminal once the host is saved, leaving the device approved.
pub fn pair_only(host: &Host, client: &impl Connecting, alias: &str, device: &str, role: Role) {
    drop(pair(host, client, alias, device, role));
}

/// Lines of `screen` that carry pane content worth comparing between two viewers: those
/// containing any of `needles`.
#[must_use]
pub fn lines_with(screen: &[String], needles: &[&str]) -> Vec<String> {
    screen
        .iter()
        .filter(|line| needles.iter().any(|needle| line.contains(needle)))
        .map(|line| line.trim_end().to_owned())
        .collect()
}
