use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::Command;

fn fingerprint(path: &Path, hash: &mut Sha256) {
    let mut files = fs::read_dir(path)
        .expect("read benchmark sources")
        .map(|entry| entry.expect("source entry").path())
        .collect::<Vec<_>>();
    files.sort();
    for file in files {
        if file.is_dir() {
            fingerprint(&file, hash);
        } else if file.extension().is_some_and(|extension| extension == "rs") {
            println!("cargo:rerun-if-changed={}", file.display());
            hash.update(file.to_string_lossy().as_bytes());
            hash.update(fs::read(&file).expect("read benchmark source"));
        }
    }
}

fn main() {
    let mut hash = Sha256::new();
    fingerprint(Path::new("src"), &mut hash);
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    hash.update(fs::read("Cargo.toml").expect("read benchmark manifest"));
    hash.update(fs::read("build.rs").expect("read build identity source"));
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!("cargo:rustc-env=BMUX_BENCH_SOURCE_SHA256={digest}");
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_else(|| "unavailable".into());
    println!("cargo:rustc-env=BMUX_BENCH_GIT_REV={}", revision.trim());
    // Source fingerprint above is authoritative for benchmark source identity;
    // the Git revision is contextual and may not include local changes.
}
