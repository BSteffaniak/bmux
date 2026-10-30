//! Bounded, validated artifacts published as one no-replace directory operation.
use crate::analysis::{report, summarize, validate};
use crate::model::{BenchResult, RunRecord, RunSummary, SCHEMA_VERSION, Sample};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_FILE: u64 = 32 * 1024 * 1024;
const MAX_MANIFEST: u64 = 16 * 1024;
const FILES: [&str; 4] = ["run.json", "samples.jsonl", "summary.json", "report.md"];
static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    files: BTreeMap<String, Entry>,
}

fn digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let hash = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(hash.len() * 2);
    for byte in hash {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn json<T: Serialize>(value: &T) -> BenchResult<Vec<u8>> {
    serde_json::to_vec_pretty(value)
        .map_err(|error| format!("artifact serialization failed: {error}"))
}

fn raw_samples(run: &RunRecord) -> BenchResult<Vec<u8>> {
    let mut bytes = Vec::new();
    for outcome in &run.outcomes {
        for sample in &outcome.samples {
            serde_json::to_writer(&mut bytes, sample).map_err(|error| error.to_string())?;
            bytes.push(b'\n');
            if bytes.len() as u64 > MAX_FILE {
                return Err("samples.jsonl exceeds 32 MiB".into());
            }
        }
    }
    Ok(bytes)
}

fn sync_write(directory: &Path, name: &str, bytes: &[u8]) -> BenchResult<()> {
    if bytes.len() as u64 > MAX_FILE {
        return Err(format!("{name} exceeds 32 MiB"));
    }
    let path = directory.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("persist {}: {error}", path.display()))
}

fn sync_directory(path: &Path) -> BenchResult<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync directory {}: {error}", path.display()))
}

fn stage(parent: &Path) -> BenchResult<PathBuf> {
    for _ in 0..128 {
        let serial = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".bmux-bench-stage-{}-{serial}", std::process::id()));
        let result = {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&path)
        };
        match result {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create staging directory: {error}")),
        }
    }
    Err("could not reserve a fresh artifact staging directory".into())
}

// fs::rename can replace an existing empty directory. These native primitives
// enforce no-replace atomically, including a destination created after preflight.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn publish(source: &Path, destination: &Path) -> BenchResult<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let from = CString::new(source.as_os_str().as_bytes()).map_err(|_| "NUL in staging path")?;
    let to =
        CString::new(destination.as_os_str().as_bytes()).map_err(|_| "NUL in artifact path")?;
    #[cfg(target_os = "macos")]
    // SAFETY: both C strings live through the call; RENAME_EXCL prohibits replacement.
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    // SAFETY: valid C strings and AT_FDCWD descriptors; no pointers are retained.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "atomic no-replace publication failed: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn publish(_source: &Path, _destination: &Path) -> BenchResult<()> {
    Err("atomic no-replace directory publication unsupported on this platform".into())
}

/// Never replaces an existing result. On any pre-publication error, only a
/// hidden sibling staging directory can remain, never a healthy-looking result.
pub fn write_bundle(path: &Path, run: &RunRecord) -> BenchResult<()> {
    validate(run)?;
    if path.file_name().is_none() {
        return Err("artifact path must name a new result directory".into());
    }
    match fs::symlink_metadata(path) {
        Ok(_) => return Err(format!("refusing to overwrite artifact {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect artifact destination: {error}")),
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !parent.is_dir() {
        return Err("artifact parent directory must already exist".into());
    }
    let summary = summarize(run)?;
    let files = [
        json(run)?,
        raw_samples(run)?,
        json(&summary)?,
        report(run, &summary).into_bytes(),
    ];
    if files.iter().any(|bytes| bytes.len() as u64 > MAX_FILE) {
        return Err("artifact file exceeds 32 MiB".into());
    }
    let staging = stage(parent)?;
    let result = (|| {
        let mut manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            files: BTreeMap::new(),
        };
        for (name, bytes) in FILES.iter().zip(&files) {
            sync_write(&staging, name, bytes)?;
            manifest.files.insert(
                (*name).into(),
                Entry {
                    bytes: bytes.len() as u64,
                    sha256: digest(bytes),
                },
            );
        }
        sync_write(&staging, "checksums.json", &json(&manifest)?)?;
        sync_directory(&staging)?;
        publish(&staging, path)?;
        sync_directory(parent).map_err(|error| format!(
            "bundle published at {} but parent durability confirmation failed; do not retry as a new run: {error}", path.display()))
    })();
    if let Err(error) = result {
        // Only remove our reserved directory, never the publication destination.
        let cleanup = if staging.exists() {
            fs::remove_dir_all(&staging)
        } else {
            Ok(())
        };
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => format!(
                "{error}; incomplete staging retained at {}: {cleanup}",
                staging.display()
            ),
        });
    }
    Ok(())
}

fn bounded_read(path: &Path, maximum: u64) -> BenchResult<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(format!(
            "{} is not a bounded regular artifact file",
            path.display()
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let opened = file.metadata().map_err(|error| error.to_string())?;
    if !opened.is_file() || opened.len() > maximum {
        return Err("artifact changed to nonregular or oversized file".into());
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > maximum {
        return Err(format!("{} grew past artifact size limit", path.display()));
    }
    Ok(bytes)
}

// JSON decoders may differ by a few ulps without serde_json's optional
// float_roundtrip feature. Accept only finite rounding-scale differences, never
// altered counts, identities, methods, warning text, or uncertainty structure.
fn same_number(a: f64, b: f64) -> bool {
    a.is_finite()
        && b.is_finite()
        && (a == b || (a - b).abs() <= 8.0 * f64::EPSILON * a.abs().max(b.abs()))
}

fn same_summary(stored: &RunSummary, expected: &RunSummary) -> bool {
    stored.schema_version == expected.schema_version
        && stored.method == expected.method
        && stored.metrics.len() == expected.metrics.len()
        && stored.metrics.iter().zip(&expected.metrics).all(|(a, b)| {
            a.case_id == b.case_id
                && a.metric == b.metric
                && a.unit == b.unit
                && a.count == b.count
                && a.outliers == b.outliers
                && a.warnings == b.warnings
                && [
                    (a.median, b.median),
                    (a.min, b.min),
                    (a.max, b.max),
                    (a.mad, b.mad),
                    (a.ci_low, b.ci_low),
                    (a.ci_high, b.ci_high),
                ]
                .iter()
                .all(|(left, right)| same_number(*left, *right))
        })
}

/// Checksums detect accidental damage, not authenticity. All redundant views are
/// checked against the validated canonical run; a recomputed checksum is not trust.
pub fn read_bundle(path: &Path) -> BenchResult<RunRecord> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("inspect bundle: {error}"))?;
    if !metadata.is_dir() {
        return Err("bundle must be a real directory, not a symlink".into());
    }
    let manifest: Manifest =
        serde_json::from_slice(&bounded_read(&path.join("checksums.json"), MAX_MANIFEST)?)
            .map_err(|error| format!("invalid checksum manifest: {error}"))?;
    if manifest.schema_version != SCHEMA_VERSION
        || manifest.files.len() != FILES.len()
        || FILES.iter().any(|name| !manifest.files.contains_key(*name))
    {
        return Err("unsupported manifest version or unexpected artifact file set".into());
    }
    let mut files = BTreeMap::new();
    for name in FILES {
        let entry = &manifest.files[name];
        if entry.bytes > MAX_FILE
            || entry.sha256.len() != 64
            || !entry.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(format!("invalid checksum entry for {name}"));
        }
        let bytes = bounded_read(&path.join(name), MAX_FILE)?;
        if bytes.len() as u64 != entry.bytes || digest(&bytes) != entry.sha256 {
            return Err(format!("checksum/size mismatch for {name}"));
        }
        files.insert(name, bytes);
    }
    let run: RunRecord = serde_json::from_slice(&files["run.json"])
        .map_err(|error| format!("invalid run.json: {error}"))?;
    validate(&run)?;
    // Parse every line, rejecting malformed, duplicate, reordered, or extra data.
    let raw = &files["samples.jsonl"];
    if !raw.is_empty() && !raw.ends_with(b"\n") {
        return Err("samples.jsonl has an incomplete final line".into());
    }
    let raw_text =
        std::str::from_utf8(raw).map_err(|error| format!("invalid samples UTF-8: {error}"))?;
    let mut lines = raw_text.split_terminator('\n');
    for outcome in &run.outcomes {
        for expected in &outcome.samples {
            let line = lines
                .next()
                .ok_or("samples.jsonl is missing observations")?;
            let sample: Sample = serde_json::from_str(line)
                .map_err(|error| format!("invalid samples.jsonl: {error}"))?;
            if serde_json::to_value(sample).map_err(|error| error.to_string())?
                != serde_json::to_value(expected).map_err(|error| error.to_string())?
            {
                return Err("samples.jsonl disagrees with run.json".into());
            }
        }
    }
    if lines.next().is_some() {
        return Err("samples.jsonl contains extra observations".into());
    }
    let stored: RunSummary = serde_json::from_slice(&files["summary.json"])
        .map_err(|error| format!("invalid summary.json: {error}"))?;
    let expected = summarize(&run)?;
    if stored.schema_version != SCHEMA_VERSION || !same_summary(&stored, &expected) {
        return Err("summary.json disagrees with deterministic raw-data analysis".into());
    }
    if files["report.md"] != report(&run, &expected).as_bytes()
        && files["report.md"] != report(&run, &stored).as_bytes()
    {
        return Err("report.md disagrees with validated run and summary".into());
    }
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::tests::fixture;
    use crate::model::Status;

    #[test]
    fn sha256_hex_matches_standard_vectors() {
        assert_eq!(
            digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            Self(stage(&std::env::temp_dir()).unwrap())
        }
        fn result(&self) -> PathBuf {
            self.0.join("result")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn rewrite_with_checksum(path: &Path, name: &str, bytes: &[u8]) {
        fs::write(path.join(name), bytes).unwrap();
        let manifest_path = path.join("checksums.json");
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.files.insert(
            name.into(),
            Entry {
                bytes: bytes.len() as u64,
                sha256: digest(bytes),
            },
        );
        fs::write(manifest_path, json(&manifest).unwrap()).unwrap();
    }

    #[test]
    fn complete_bundle_round_trip_and_all_files_exist() {
        let scratch = Scratch::new();
        let path = scratch.result();
        let run = fixture(&[10.0; 9]);
        write_bundle(&path, &run).unwrap();
        for name in FILES {
            assert!(path.join(name).is_file());
        }
        let decoded = read_bundle(&path).unwrap();
        assert_eq!(
            serde_json::to_value(run).unwrap(),
            serde_json::to_value(decoded).unwrap()
        );
    }

    #[test]
    fn decimal_observations_round_trip_with_rounding_bounded_summary_checks() {
        let scratch = Scratch::new();
        let run = fixture(&[
            0.1,
            1.234_567_890_123_456,
            100.123_456_789,
            1.0 / 3.0,
            0.000_001,
            999.99,
            2.001,
            7.321,
            1.25,
        ]);
        write_bundle(&scratch.result(), &run).unwrap();
        let decoded = read_bundle(&scratch.result()).unwrap();
        assert!(same_summary(
            &summarize(&run).unwrap(),
            &summarize(&decoded).unwrap()
        ));
        assert!(!same_number(10.0, 10.000_001));
        assert!(!same_number(f64::NAN, f64::NAN));
    }

    #[test]
    fn noncomplete_status_is_preserved_not_promoted() {
        for status in [
            Status::Failed,
            Status::Partial,
            Status::Cancelled,
            Status::Unsupported,
        ] {
            let scratch = Scratch::new();
            let mut run = fixture(&[10.0; 9]);
            run.status = status;
            run.outcomes[0].status = status;
            run.outcomes[0].reason = Some("explicit test failure".into());
            run.outcomes[0].samples.clear();
            if status == Status::Failed {
                run.outcomes[0].payload_hash.clear();
            }
            write_bundle(&scratch.result(), &run).unwrap();
            let read = read_bundle(&scratch.result()).unwrap();
            assert_eq!(read.status, status);
            assert_eq!(read.outcomes[0].status, status);
        }
    }

    #[test]
    fn refuses_existing_directory_or_file_without_changes() {
        let scratch = Scratch::new();
        let path = scratch.result();
        let run = fixture(&[10.0; 9]);
        fs::create_dir(&path).unwrap();
        assert!(write_bundle(&path, &run).is_err());
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        let file = scratch.0.join("file");
        fs::write(&file, "keep").unwrap();
        assert!(write_bundle(&file, &run).is_err());
        assert_eq!(fs::read_to_string(file).unwrap(), "keep");
    }

    #[test]
    fn invalid_input_never_publishes_directory() {
        let scratch = Scratch::new();
        let mut run = fixture(&[10.0; 9]);
        run.outcomes[0].samples[0].value = f64::INFINITY;
        assert!(write_bundle(&scratch.result(), &run).is_err());
        assert!(!scratch.result().exists());
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 0);
    }

    #[test]
    fn checksum_tampering_and_missing_files_are_explicit() {
        let scratch = Scratch::new();
        let path = scratch.result();
        write_bundle(&path, &fixture(&[10.0; 9])).unwrap();
        fs::write(path.join("report.md"), "truncated").unwrap();
        assert!(read_bundle(&path).unwrap_err().contains("checksum/size"));
        fs::remove_file(path.join("report.md")).unwrap();
        assert!(read_bundle(&path).is_err());
    }

    #[test]
    fn recomputed_checksums_do_not_hide_semantic_tampering() {
        for file in ["run.json", "samples.jsonl", "summary.json", "report.md"] {
            let scratch = Scratch::new();
            let path = scratch.result();
            write_bundle(&path, &fixture(&[10.0; 9])).unwrap();
            let bytes = fs::read(path.join(file)).unwrap();
            let text = String::from_utf8(bytes).unwrap();
            let changed = match file {
                "run.json" => text.replacen("\"schema_version\": 1", "\"schema_version\": 99", 1),
                "samples.jsonl" => text.replacen("\"value\":10.0", "\"value\":11.0", 1),
                "summary.json" => text.replacen("\"median\": 10.0", "\"median\": 11.0", 1),
                _ => "forged report".into(),
            };
            assert_ne!(changed, text);
            rewrite_with_checksum(&path, file, changed.as_bytes());
            assert!(read_bundle(&path).is_err(), "tampered {file}");
        }
    }

    #[test]
    fn incomplete_or_extra_jsonl_and_unknown_fields_are_rejected() {
        for mode in 0..4 {
            let scratch = Scratch::new();
            let path = scratch.result();
            write_bundle(&path, &fixture(&[10.0; 9])).unwrap();
            if mode < 3 {
                let mut bytes = fs::read(path.join("samples.jsonl")).unwrap();
                match mode {
                    0 => {
                        bytes.pop();
                    }
                    1 => {
                        bytes.push(b'\n');
                    }
                    _ => {
                        bytes.clear();
                    }
                }
                rewrite_with_checksum(&path, "samples.jsonl", &bytes);
            } else {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(path.join("run.json")).unwrap()).unwrap();
                value["unrecognized"] = true.into();
                rewrite_with_checksum(&path, "run.json", &json(&value).unwrap());
            }
            assert!(read_bundle(&path).is_err(), "mode {mode}");
        }
    }

    #[test]
    fn oversized_files_are_rejected_before_read() {
        let scratch = Scratch::new();
        let path = scratch.0.join("large");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_FILE + 1).unwrap();
        assert!(bounded_read(&path, MAX_FILE).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_files_and_bundle_directories_are_rejected() {
        use std::os::unix::fs::symlink;
        let scratch = Scratch::new();
        let path = scratch.result();
        write_bundle(&path, &fixture(&[10.0; 9])).unwrap();
        let link = scratch.0.join("bundle-link");
        symlink(&path, &link).unwrap();
        assert!(read_bundle(&link).is_err());
        assert!(write_bundle(&link, &fixture(&[10.0; 9])).is_err());
        let report = path.join("report.md");
        let saved = scratch.0.join("saved-report");
        fs::rename(&report, &saved).unwrap();
        symlink(&saved, &report).unwrap();
        assert!(read_bundle(&path).is_err());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn atomic_publication_does_not_replace_racing_empty_destination() {
        let scratch = Scratch::new();
        let source = stage(&scratch.0).unwrap();
        let destination = scratch.result();
        fs::create_dir(&destination).unwrap();
        assert!(publish(&source, &destination).is_err());
        assert!(source.is_dir());
        assert!(destination.is_dir());
    }
}
