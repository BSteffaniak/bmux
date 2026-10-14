//! First-use setup only; neither native compilation nor terminal startup invokes this module.
#[path = "provenance.rs"]
mod provenance;

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const RENDERER_SHA256: &str = env!("BMUX_WEB_RENDERER_SHA256");
pub const WASM_BINDGEN_VERSION: &str = env!("BMUX_WEB_WASM_BINDGEN_VERSION");
pub const RELEASE_TAG: &str = env!("BMUX_WEB_RELEASE_TAG");
const FILES: [&str; 8] = [
    "index.html",
    "bootstrap.js",
    "canvas-viewer.js",
    "live-viewer.js",
    "live-page.js",
    "style.css",
    "bmux_web_renderer.js",
    "bmux_web_renderer_bg.wasm",
];
const MANIFEST_LIMIT: u64 = 16 * 1024;
const FILE_LIMIT: u64 = 8 * 1024 * 1024;
const TOTAL_LIMIT: u64 = 24 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    version: String,
    renderer_sha256: String,
    wasm_bindgen_version: String,
    files: Vec<Entry>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    size: u64,
    sha256: String,
}

/// Resolve assets once web presentation is selected.
///
/// Overrides bypass setup; the host still validates its exact asset allowlist before
/// binding. Cached bundles work offline. Corrupt existing caches are errors: an
/// operator must explicitly move them aside.
pub async fn resolve_assets(override_directory: Option<PathBuf>) -> io::Result<PathBuf> {
    if let Some(path) = override_directory {
        return Ok(path);
    }
    let cache = cache_root()?;
    resolve_assets_with_cache(None, cache).await
}

/// Resolve assets beneath an explicit caller-selected cache root.
///
/// Normally this is selected `state_dir/cache`. Bundles and private tools live beneath
/// `cache_root/web-assets`. No environment mutation occurs. The caller applies
/// `BMUX_CACHE_DIR` precedence before invoking.
pub async fn resolve_assets_with_cache(
    override_directory: Option<PathBuf>,
    cache_root: PathBuf,
) -> io::Result<PathBuf> {
    if let Some(path) = override_directory {
        return Ok(path);
    }
    let cache = absolute(cache_root)?;
    tokio::task::spawn_blocking(move || resolve(&cache))
        .await
        .map_err(io::Error::other)?
}

fn cache_root() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("BMUX_CACHE_DIR") {
        return absolute(PathBuf::from(path));
    }
    let isolated = ["BMUX_SLOT_NAME", "BMUX_STATE_DIR", "BMUX_RUNTIME_DIR"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
        || std::env::var("BMUX_RUNTIME_NAME")
            .is_ok_and(|name| !name.is_empty() && name != "default")
        || std::env::var("BMUX_TARGET").is_ok_and(|target| !target.is_empty() && target != "local");
    let slot_executable = std::env::args_os()
        .next()
        .and_then(|name| {
            PathBuf::from(name)
                .file_name()
                .map(|name| name.to_string_lossy().starts_with("bmux-"))
        })
        .unwrap_or(false);
    if isolated || slot_executable {
        return Err(io::Error::other(
            "selected runtime requires explicit isolated BMUX_CACHE_DIR for web assets",
        ));
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return absolute(PathBuf::from(path)).map(|path| path.join("bmux"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        io::Error::other("web cache requires BMUX_CACHE_DIR, XDG_CACHE_HOME or HOME")
    })?;
    absolute(PathBuf::from(home)).map(|path| path.join(".cache/bmux"))
}
fn absolute(path: PathBuf) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::other("web cache root must be absolute"));
    }
    Ok(path)
}

struct Lock(File);
impl Lock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("web setup lock is not a regular file"));
        }
        // SAFETY: file owns a valid descriptor for the entire lock lifetime.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(file))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: the owned descriptor remains valid until after drop returns.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn cache_key() -> String {
    let release = if RELEASE_TAG.is_empty() {
        "source"
    } else {
        RELEASE_TAG
    };
    format!("{}-{release}-{RENDERER_SHA256}", env!("CARGO_PKG_VERSION"))
}

fn resolve(cache: &Path) -> io::Result<PathBuf> {
    for path in cache.ancestors() {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::other(
                    "web cache ancestor is not a regular directory",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let parent = cache.join("web-assets");
    private_directory(&parent)?;
    let _lock = Lock::acquire(&parent.join(format!("{RENDERER_SHA256}.lock")))?;
    let destination = parent.join(cache_key());
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            verify_bundle(&destination)?;
            sync_directory(&destination)?;
            sync_directory(&parent)?;
            sync_directory(cache)?;
            return Ok(destination);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let stage = tempfile::Builder::new()
        .prefix(".setup-")
        .tempdir_in(&parent)?;
    let source = Path::new(env!("BMUX_WEB_SOURCE_ROOT"));
    if RELEASE_TAG.is_empty() {
        verify_source(source)?; // Never execute source merely because a root exists.
        eprintln!("Preparing BMUX web renderer assets (first use)…");
        build_source(source, stage.path(), &parent)?;
        write_manifest(stage.path())?;
    } else {
        eprintln!("Downloading verified BMUX web renderer assets (first use)…");
        download_bundle(stage.path())?;
    }
    publish_bundle(stage.path(), &destination, cache)?;
    Ok(destination)
}

fn publish_bundle(stage: &Path, destination: &Path, cache: &Path) -> io::Result<()> {
    verify_bundle(stage)?;
    sync_directory(stage)?;
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("invalid web bundle destination"))?;
    // Same-filesystem rename, under an interprocess lock; never overwrite existing state.
    match fs::symlink_metadata(destination) {
        Ok(_) => return Err(io::Error::other("web cache appeared during setup")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(stage, destination)?;
    sync_directory(parent)?;
    sync_directory(cache)
}

fn private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::other(
                "web cache path is not a regular directory",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("invalid web cache root"))?;
    private_directory(parent)?;
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(path)?.is_dir() {
                return Err(io::Error::other(
                    "web cache path is not a regular directory",
                ));
            }
        }
        Err(error) => return Err(error),
    }
    sync_directory(path)?;
    sync_directory(parent)
}

fn verify_source(root: &Path) -> io::Result<()> {
    let files = provenance::inputs(root)?;
    if provenance::identity(root, &files)? != RENDERER_SHA256 {
        return Err(io::Error::other(
            "renderer source differs from compiled provenance; rebuild the native binary or provide explicit assets",
        ));
    }
    Ok(())
}
fn read_regular(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > limit {
        return Err(io::Error::other(
            "web bundle file exceeds bounds or is not regular",
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("web bundle file grew beyond limit"));
    }
    Ok(bytes)
}
fn parse_manifest(bytes: &[u8]) -> io::Result<Manifest> {
    if bytes.len() as u64 > MANIFEST_LIMIT {
        return Err(io::Error::other("web manifest too large"));
    }
    let manifest: Manifest = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    if manifest.schema != 1
        || manifest.version != env!("CARGO_PKG_VERSION")
        || manifest.renderer_sha256 != RENDERER_SHA256
        || manifest.wasm_bindgen_version != WASM_BINDGEN_VERSION
        || manifest.files.len() != FILES.len()
    {
        return Err(io::Error::other("incompatible web bundle manifest"));
    }
    let mut names = BTreeSet::new();
    let mut total = 0_u64;
    for entry in &manifest.files {
        if !FILES.contains(&entry.name.as_str())
            || !names.insert(&entry.name)
            || entry.size == 0
            || entry.size > FILE_LIMIT
            || entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(io::Error::other("invalid web bundle entry"));
        }
        total += entry.size;
    }
    if total > TOTAL_LIMIT {
        return Err(io::Error::other("web bundle exceeds total budget"));
    }
    Ok(manifest)
}
fn verify_bundle(directory: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(directory)?.is_dir() {
        return Err(io::Error::other("web cache is not a regular directory"));
    }
    let manifest_path = directory.join("manifest.json");
    let manifest = parse_manifest(&read_regular(&manifest_path, MANIFEST_LIMIT)?)?;
    sync_regular(&manifest_path)?;
    for entry in &manifest.files {
        let path = directory.join(&entry.name);
        let bytes = read_regular(&path, FILE_LIMIT)?;
        verify_entry(entry, &bytes)?;
        sync_regular(&path)?;
    }
    Ok(())
}
fn verify_entry(entry: &Entry, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() as u64 != entry.size
        || provenance::hex(&Sha256::digest(bytes)) != entry.sha256
        || (entry.name.ends_with(".wasm") && !bytes.starts_with(b"\0asm\x01\0\0\0"))
    {
        return Err(io::Error::other(
            "corrupt web bundle; explicitly move the cache aside to repair",
        ));
    }
    Ok(())
}
fn durable_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
fn sync_regular(path: &Path) -> io::Result<()> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?
        .sync_all()
}
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
fn write_manifest(directory: &Path) -> io::Result<()> {
    let mut entries = Vec::new();
    let mut total = 0_u64;
    for name in FILES {
        let bytes = read_regular(&directory.join(name), FILE_LIMIT)?;
        total += bytes.len() as u64;
        if total > TOTAL_LIMIT {
            return Err(io::Error::other("built web assets exceed total budget"));
        }
        File::open(directory.join(name))?.sync_all()?;
        entries.push(Entry {
            name: name.into(),
            size: bytes.len() as u64,
            sha256: provenance::hex(&Sha256::digest(&bytes)),
        });
    }
    let manifest = Manifest {
        schema: 1,
        version: env!("CARGO_PKG_VERSION").into(),
        renderer_sha256: RENDERER_SHA256.into(),
        wasm_bindgen_version: WASM_BINDGEN_VERSION.into(),
        files: entries,
    };
    durable_file(
        &directory.join("manifest.json"),
        &serde_json::to_vec(&manifest).map_err(io::Error::other)?,
    )
}

fn download_bundle(directory: &Path) -> io::Result<()> {
    // Explicit provider installation avoids depending on another entry point's TLS setup.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::limited(5))
        .https_only(true)
        .build()
        .map_err(io::Error::other)?;
    let base = format!(
        "https://github.com/BSteffaniak/bmux/releases/download/{RELEASE_TAG}/bmux-web-assets-{RENDERER_SHA256}"
    );
    let bytes = download(&client, &format!("{base}.json"), MANIFEST_LIMIT)?;
    let manifest = parse_manifest(&bytes)?;
    durable_file(&directory.join("manifest.json"), &bytes)?;
    for entry in &manifest.files {
        let bytes = download(&client, &format!("{base}-{}", entry.name), entry.size)?;
        verify_entry(entry, &bytes)?;
        durable_file(&directory.join(&entry.name), &bytes)?;
    }
    Ok(())
}
fn download(client: &reqwest::blocking::Client, url: &str, limit: u64) -> io::Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(io::Error::other)?;
    if response.content_length().is_some_and(|size| size > limit) {
        return Err(io::Error::other("web download exceeds budget"));
    }
    let mut bytes = Vec::new();
    response.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() as u64 > limit {
        return Err(io::Error::other("web download exceeds budget or is empty"));
    }
    Ok(bytes)
}

fn build_source(root: &Path, output: &Path, cache: &Path) -> io::Result<()> {
    verify_source(root)?;
    let tools_lock =
        Lock::acquire(&cache.join(format!("wasm-bindgen-{WASM_BINDGEN_VERSION}.lock")))?;
    let tools = cache.join(format!("wasm-bindgen-{WASM_BINDGEN_VERSION}"));
    let bindgen = tools.join("bin/wasm-bindgen");
    let tools_exist = match fs::symlink_metadata(&tools) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if tools_exist {
        check_bindgen(&bindgen)?;
        sync_regular(&bindgen)?;
        sync_regular(&tools.join("sha256"))?;
        sync_directory(&tools.join("bin"))?;
        sync_directory(&tools)?;
        sync_directory(cache)?;
    } else {
        let stage = tempfile::Builder::new()
            .prefix(".tools-")
            .tempdir_in(cache)?;
        let exact_version = format!("={WASM_BINDGEN_VERSION}");
        run_dev(
            root,
            &[
                "cargo",
                "install",
                "wasm-bindgen-cli",
                "--version",
                &exact_version,
                "--locked",
                "--root",
                utf8(stage.path())?,
            ],
        )?;
        let private_binary = stage.path().join("bin/wasm-bindgen");
        check_bindgen_version(&private_binary)?;
        let digest = provenance::hex(&Sha256::digest(read_regular(
            &private_binary,
            128 * 1024 * 1024,
        )?));
        durable_file(&stage.path().join("sha256"), digest.as_bytes())?;
        File::open(&private_binary)?.sync_all()?;
        sync_directory(&stage.path().join("bin"))?;
        sync_directory(stage.path())?;
        fs::rename(stage.path(), &tools)?;
        sync_directory(cache)?;
    }
    drop(tools_lock);
    let _source_lock = Lock::acquire(&cache.join("source-build.lock"))?;
    verify_source(root)?;
    // scripts/dev.sh owns compiler and target selection. The shell reads the Nix-selected
    // target directory; never override CARGO_TARGET_DIR or call a PATH compiler directly.
    let script = "set -eu; test -n \"$BMUX_NIX_TOOLCHAIN\"; test -n \"$CARGO_TARGET_DIR\"; cargo build --locked -p bmux_web_renderer --features wasm --target wasm32-unknown-unknown; \"$1\" \"$CARGO_TARGET_DIR/wasm32-unknown-unknown/debug/bmux_web_renderer.wasm\" --target web --out-dir \"$2\"";
    run_dev(
        root,
        &[
            "bash",
            "-c",
            script,
            "bmux-web-setup",
            utf8(&bindgen)?,
            utf8(output)?,
        ],
    )?;
    verify_source(root)?;
    for name in [
        "index.html",
        "bootstrap.js",
        "style.css",
        "canvas-viewer.js",
        "live-viewer.js",
        "live-page.js",
    ] {
        let bytes = read_regular(
            &root.join("packages/web-renderer/browser").join(name),
            FILE_LIMIT,
        )?;
        durable_file(&output.join(name), &bytes)?;
    }
    Ok(())
}
fn utf8(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::other("web setup path must be UTF-8"))
}
fn run_dev(root: &Path, args: &[&str]) -> io::Result<()> {
    verify_source(root)?;
    let status = Command::new("bash")
        .arg(root.join("scripts/dev.sh"))
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(
            "pinned web setup failed; retry after resolving toolchain or network error",
        ));
    }
    Ok(())
}
fn check_bindgen(path: &Path) -> io::Result<()> {
    let tools = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("invalid private tool path"))?;
    if !fs::symlink_metadata(tools)?.is_dir() || !fs::symlink_metadata(tools.join("bin"))?.is_dir()
    {
        return Err(io::Error::other("private tool cache must not be a symlink"));
    }
    let expected = read_regular(&tools.join("sha256"), 64)?;
    let actual = provenance::hex(&Sha256::digest(read_regular(path, 128 * 1024 * 1024)?));
    if actual.as_bytes() != expected {
        return Err(io::Error::other(
            "corrupt private wasm-bindgen; explicitly repair the tool cache",
        ));
    }
    check_bindgen_version(path)
}
fn check_bindgen_version(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "private wasm-bindgen is not a regular file",
        ));
    }
    let output = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success()
        || output.stdout != format!("wasm-bindgen {WASM_BINDGEN_VERSION}\n").as_bytes()
    {
        return Err(io::Error::other(
            "private wasm-bindgen version mismatch; explicitly repair the tool cache",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encoder_preserves_leading_zeroes_and_known_sha256() {
        assert_eq!(provenance::hex(&[0, 1, 15, 16, 255]), "00010f10ff");
        assert_eq!(
            provenance::hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn source_mismatch_never_provisions_tools_or_executes_setup() {
        let source = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        assert!(build_source(source.path(), output.path(), cache.path()).is_err());
        assert_eq!(fs::read_dir(cache.path()).unwrap().count(), 0);
        assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
    }

    fn fixture(directory: &Path) {
        for name in FILES {
            durable_file(
                &directory.join(name),
                if name.ends_with(".wasm") {
                    b"\0asm\x01\0\0\0"
                } else {
                    b"asset"
                },
            )
            .unwrap();
        }
        write_manifest(directory).unwrap();
    }
    #[tokio::test]
    async fn override_has_no_setup_side_effects() {
        let path = PathBuf::from("not-even-required-to-exist");
        assert_eq!(resolve_assets(Some(path.clone())).await.unwrap(), path);
    }
    #[test]
    fn cache_reuse_is_offline_and_corruption_is_not_repaired() {
        let cache = tempfile::tempdir().unwrap();
        let directory = cache.path().join("web-assets").join(cache_key());
        fs::create_dir_all(&directory).unwrap();
        fixture(&directory);
        assert_eq!(resolve(cache.path()).unwrap(), directory);
        fs::write(directory.join("bootstrap.js"), b"corrupt").unwrap();
        assert!(resolve(cache.path()).is_err());
        assert_eq!(
            fs::read(directory.join("bootstrap.js")).unwrap(),
            b"corrupt"
        );
    }
    #[test]
    fn manifest_rejects_schema_identity_traversal_and_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        fixture(directory.path());
        let bytes = fs::read(directory.path().join("manifest.json")).unwrap();
        for mutation in ["schema", "identity", "path", "duplicate", "size"] {
            let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            match mutation {
                "schema" => value["schema"] = 2.into(),
                "identity" => value["renderer_sha256"] = "other".into(),
                "path" => value["files"][0]["name"] = "../index.html".into(),
                "duplicate" => value["files"][1] = value["files"][0].clone(),
                _ => value["files"][0]["size"] = (FILE_LIMIT + 1).into(),
            }
            assert!(parse_manifest(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        assert!(parse_manifest(&vec![b' '; MANIFEST_LIMIT as usize + 1]).is_err());
    }
    #[test]
    fn symlink_assets_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        fixture(directory.path());
        fs::rename(
            directory.path().join("style.css"),
            directory.path().join("real.css"),
        )
        .unwrap();
        std::os::unix::fs::symlink("real.css", directory.path().join("style.css")).unwrap();
        assert!(verify_bundle(directory.path()).is_err());
    }
    #[test]
    fn source_missing_or_mismatched_is_rejected_before_execution() {
        let directory = tempfile::tempdir().unwrap();
        assert!(verify_source(directory.path()).is_err());
    }
    #[test]
    fn atomic_publication_rejects_incomplete_and_never_overwrites() {
        let cache = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir_in(cache.path()).unwrap();
        let destination = cache.path().join("bundle");
        durable_file(&stage.path().join("index.html"), b"partial").unwrap();
        assert!(publish_bundle(stage.path(), &destination, cache.path()).is_err());
        assert!(!destination.exists());
        let valid = tempfile::tempdir_in(cache.path()).unwrap();
        fixture(valid.path());
        publish_bundle(valid.path(), &destination, cache.path()).unwrap();
        verify_bundle(&destination).unwrap();
        let another = tempfile::tempdir_in(cache.path()).unwrap();
        fixture(another.path());
        assert!(publish_bundle(another.path(), &destination, cache.path()).is_err());
        verify_bundle(&destination).unwrap();
    }
    #[test]
    fn bounded_http_download_and_hash_tamper() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // Construct before starting a scoped server: a client setup panic must not
        // leave scope cleanup waiting for a connection that will never arrive.
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        for (body, limit, success) in [
            (b"hello".as_slice(), 5, true),
            (b"oversized".as_slice(), 5, false),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            std::thread::scope(|scope| {
                scope.spawn(move || {
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    let mut socket = loop {
                        match listener.accept() {
                            Ok((socket, _)) => break socket,
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                assert!(
                                    std::time::Instant::now() < deadline,
                                    "HTTP fixture accept timed out"
                                );
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            Err(error) => panic!("HTTP fixture accept failed: {error}"),
                        }
                    };
                    socket
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    socket
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut request = [0; 2048];
                    let _ = socket.read(&mut request).unwrap();
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    // Over-budget readers may deliberately close before the body completes.
                    let _ = socket.write_all(body);
                });
                let result = download(&client, &format!("http://{address}/asset"), limit);
                assert_eq!(result.is_ok(), success);
                if let Ok(bytes) = result {
                    let entry = Entry {
                        name: "index.html".into(),
                        size: 5,
                        sha256: provenance::hex(&Sha256::digest(b"hello")),
                    };
                    verify_entry(&entry, &bytes).unwrap();
                    assert!(verify_entry(&entry, b"jello").is_err());
                }
            });
        }
    }
    #[test]
    fn source_fingerprint_changes_with_source_bytes() {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "Cargo.lock",
            "flake.nix",
            "flake.lock",
            "scripts/dev.sh",
            ".cargo/config.toml",
        ] {
            let path = directory.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"input").unwrap();
        }
        fs::write(
            directory.path().join("Cargo.toml"),
            "[workspace.dependencies]\n",
        )
        .unwrap();
        let renderer = directory.path().join("packages/web-renderer");
        fs::create_dir_all(renderer.join("src")).unwrap();
        fs::write(renderer.join("Cargo.toml"), "[dependencies]\n").unwrap();
        fs::write(renderer.join("src/lib.rs"), b"original").unwrap();
        let inputs = provenance::inputs(directory.path()).unwrap();
        let original = provenance::identity(directory.path(), &inputs).unwrap();
        fs::write(renderer.join("src/lib.rs"), b"changed").unwrap();
        assert_ne!(
            original,
            provenance::identity(directory.path(), &inputs).unwrap()
        );
        assert!(verify_source(directory.path()).is_err());
    }
    #[test]
    fn relative_cache_root_is_rejected_without_creation() {
        assert!(absolute(PathBuf::from("relative-cache")).is_err());
    }
    #[test]
    fn corrupt_private_tool_is_rejected_without_executing_it() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("bin")).unwrap();
        fs::write(
            directory.path().join("bin/wasm-bindgen"),
            b"not an executable",
        )
        .unwrap();
        fs::write(directory.path().join("sha256"), "0".repeat(64)).unwrap();
        let error = check_bindgen(&directory.path().join("bin/wasm-bindgen")).unwrap_err();
        assert!(error.to_string().contains("corrupt private"));
    }
    #[test]
    fn concurrent_readers_reuse_one_valid_cache() {
        let cache = tempfile::tempdir().unwrap();
        let directory = cache.path().join("web-assets").join(cache_key());
        fs::create_dir_all(&directory).unwrap();
        fixture(&directory);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| assert_eq!(resolve(cache.path()).unwrap(), directory));
            }
        });
    }
}
