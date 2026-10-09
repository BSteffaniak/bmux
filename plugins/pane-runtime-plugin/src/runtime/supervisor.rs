//! Owner-private control client for the installed supervisor artifact.
use crate::supervisor_protocol::{self as protocol, Bootstrap, Operation, Request, Response};
use sha2::{Digest, Sha256};
use std::fmt::Write as FmtWrite;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;

static INSTALLATION: OnceLock<Mutex<Option<Installation>>> = OnceLock::new();
struct Installation {
    path: PathBuf,
    hash: String,
    root: PathBuf,
}

fn short_endpoint_directory() -> anyhow::Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("bmux-exec-")
        .tempdir_in("/tmp")?;
    anyhow::ensure!(
        directory.path().join("control.sock").as_os_str().len() < 104,
        "supervisor control endpoint exceeds platform Unix socket path limit"
    );
    Ok(directory)
}

pub(crate) fn activate(context: &bmux_plugin_sdk::NativeLifecycleContext) {
    let resolved = context.auxiliary_executables.get("execution-supervisor");
    let installation = resolved
        .filter(|artifact| artifact.protocol_version == protocol::VERSION)
        .map(|artifact| Installation {
            path: PathBuf::from(&artifact.path),
            hash: artifact.artifact_sha256.clone(),
            root: PathBuf::from(&context.connection.runtime_dir),
        });
    *INSTALLATION
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("supervisor installation lock") = installation;
}

#[cfg(test)]
pub(super) fn install_test_artifact() -> anyhow::Result<()> {
    static ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
    let path = PathBuf::from(std::env::var("BMUX_TEST_SUPERVISOR_PATH")?);
    let hash = std::env::var("BMUX_TEST_SUPERVISOR_SHA256")?;
    anyhow::ensure!(path.is_absolute(), "test supervisor path must be absolute");
    let mut digest = String::with_capacity(64);
    for byte in Sha256::digest(std::fs::read(&path)?) {
        write!(&mut digest, "{byte:02x}")?;
    }
    anyhow::ensure!(hash == digest, "test supervisor artifact hash mismatch");
    if ROOT.get().is_none() {
        let root = tempfile::tempdir()?;
        let _ = ROOT.set(root);
    }
    *INSTALLATION
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow::anyhow!("supervisor installation lock poisoned"))? =
        Some(Installation {
            path,
            hash,
            root: ROOT
                .get()
                .expect("test artifact root initialized")
                .path()
                .to_path_buf(),
        });
    Ok(())
}

pub(super) struct SupervisorControl {
    directory: PathBuf,
    token: String,
    authentication: String,
}
impl SupervisorControl {
    pub(super) fn prepare(
        token: uuid::Uuid,
    ) -> anyhow::Result<(Self, portable_pty::CommandBuilder)> {
        let installation = INSTALLATION
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| anyhow::anyhow!("supervisor installation lock poisoned"))?;
        let installation = installation.as_ref().ok_or_else(|| {
            anyhow::anyhow!("execution supervisor helper artifact is unavailable")
        })?;
        anyhow::ensure!(
            installation.path.is_absolute(),
            "supervisor helper path must be absolute"
        );
        let mut digest = String::with_capacity(64);
        for byte in Sha256::digest(std::fs::read(&installation.path)?) {
            write!(&mut digest, "{byte:02x}")?;
        }
        anyhow::ensure!(
            digest == installation.hash,
            "supervisor helper artifact hash mismatch"
        );
        // Match the existing resurrection host's short, owner-private endpoint
        // mechanism. Persist the generated locator; never reconstruct by hashing.
        let endpoint_directory = short_endpoint_directory()?;
        let directory = endpoint_directory.path().to_path_buf();
        anyhow::ensure!(
            directory.join("control.sock").as_os_str().len() < 104,
            "supervisor control endpoint exceeds platform Unix socket path limit"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let authentication = uuid::Uuid::new_v4().to_string();
        let bootstrap = Bootstrap {
            version: protocol::VERSION,
            execution_token: token.to_string(),
            authentication: authentication.clone(),
        };
        let path = directory.join("bootstrap.json");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(&serde_json::to_vec(&bootstrap)?)?;
        file.sync_all()?;
        std::fs::File::open(&directory)?.sync_all()?;
        let locator = installation.root.join(format!("supervisor-{token}.json"));
        crate::atomic_state::commit(
            &locator,
            &serde_json::json!({
                "version": protocol::VERSION, "execution_token": token,
                "endpoint": directory.join("control.sock"), "bootstrap": path,
                "artifact_sha256": installation.hash,
            }),
        )?;
        // Execution-owned endpoint survives client/daemon disconnect; explicit
        // retirement, not this stack frame, owns eventual removal.
        let _retained_directory = endpoint_directory.keep();
        let mut command = portable_pty::CommandBuilder::new(&installation.path);
        command.arg("--serve");
        command.arg(path);
        command.arg(directory.join("control.sock"));
        Ok((
            Self {
                directory,
                token: token.to_string(),
                authentication,
            },
            command,
        ))
    }

    #[cfg(unix)]
    pub(super) fn request(&self, operation: Operation) -> anyhow::Result<Response> {
        let mut socket =
            std::os::unix::net::UnixStream::connect(self.directory.join("control.sock"))?;
        let timeout = if matches!(&operation, Operation::Hello | Operation::Status) {
            std::time::Duration::from_millis(500)
        } else {
            std::time::Duration::from_secs(5)
        };
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;
        protocol::write_frame(
            &mut socket,
            &Request {
                version: protocol::VERSION,
                execution_token: self.token.clone(),
                authentication: self.authentication.clone(),
                operation,
            },
        )?;
        let response: Response = protocol::read_frame(&mut socket)?;
        anyhow::ensure!(
            response.version == protocol::VERSION && response.execution_token == self.token,
            "supervisor response identity mismatch"
        );
        anyhow::ensure!(
            response.error.is_none(),
            "supervisor operation failed: {:?}",
            response.error
        );
        Ok(response)
    }

    #[cfg(not(unix))]
    pub(super) fn request(&self, _operation: Operation) -> anyhow::Result<Response> {
        anyhow::bail!("supervisor requires Unix")
    }

    pub(super) async fn start(
        &self,
        launch: &super::PaneLaunchSpec,
        authorize: impl Fn() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut last_error;
        loop {
            match self.request(Operation::Hello) {
                Ok(_) => break,
                Err(error) => last_error = error,
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "supervisor handshake failed before startup deadline: {last_error}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        authorize()?;
        self.request(Operation::Start {
            mutation_id: self.token.clone(),
            program: launch.program.clone(),
            args: launch.args.clone(),
            cwd: launch.cwd.clone(),
            env: launch.env.clone(),
        })?;
        Ok(())
    }
}
