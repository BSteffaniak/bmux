//! A real private launch image for the managed-startup integration test.
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};

pub(super) struct Image {
    path: PathBuf,
    pub(super) identity: String,
    pub(super) sha256: String,
}

fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut result, "{byte:02x}").unwrap();
    }
    result
}

impl Image {
    pub(super) fn new(root: &Path) -> Self {
        use std::os::fd::AsRawFd as _;
        let directory = root.join("launch-image");
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("bmux");
        std::fs::copy(super::support::bmux_binary(), &path).unwrap();
        let sha256 = digest(&std::fs::read(&path).unwrap());
        // The frontend is never executed in this server-side handshake test.
        std::fs::write(directory.join("frontend"), b"test frontend").unwrap();
        // Match startup_ownership's canonical struct field order for identity hashing.
        #[derive(serde::Serialize)]
        struct Artifact<'a> {
            role: &'a str,
            path: &'a str,
            sha256: String,
        }
        #[derive(serde::Serialize)]
        struct Manifest<'a> {
            schema_version: u32,
            compatibility_id: &'a str,
            target: String,
            artifacts: Vec<Artifact<'a>>,
        }
        let manifest = Manifest {
            schema_version: 1,
            compatibility_id: "bterm-bmux-v1",
            target: format!("{}-apple-darwin", std::env::consts::ARCH),
            artifacts: vec![
                Artifact {
                    role: "companion",
                    path: "bmux",
                    sha256: sha256.clone(),
                },
                Artifact {
                    role: "frontend",
                    path: "frontend",
                    sha256: digest(b"test frontend"),
                },
            ],
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        std::fs::write(directory.join("bterm-bundle.json"), &bytes).unwrap();
        let identity = serde_json::json!({
            "companion_sha256": sha256,
            "artifact_set_sha256": digest(&bytes),
        })
        .to_string();
        let file = std::fs::File::open(&path).unwrap();
        file.sync_all().unwrap();
        // SAFETY: pins only the new fixture-owned copy, never a build artifact.
        assert_eq!(
            unsafe { libc::fchflags(file.as_raw_fd(), libc::UF_IMMUTABLE) },
            0
        );
        Self {
            path,
            identity,
            sha256,
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd as _;
        let file = std::fs::File::open(&self.path).unwrap();
        // SAFETY: only unpins this fixture after its child has been killed and reaped.
        assert_eq!(unsafe { libc::fchflags(file.as_raw_fd(), 0) }, 0);
    }
}
