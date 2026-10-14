// Shared by build.rs and the first-use source verifier. Never invokes a compiler.
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub fn inputs(root: &Path) -> io::Result<Vec<PathBuf>> {
    let workspace = toml::from_str::<toml::Value>(&fs::read_to_string(root.join("Cargo.toml"))?)
        .map_err(io::Error::other)?;
    let mut files = BTreeSet::new();
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "flake.nix",
        "flake.lock",
        "scripts/dev.sh",
        ".cargo/config.toml",
    ] {
        files.insert(PathBuf::from(name));
    }
    let mut pending = VecDeque::from([PathBuf::from("packages/web-renderer")]);
    let mut visited = BTreeSet::new();
    while let Some(package) = pending.pop_front() {
        if !visited.insert(package.clone()) {
            continue;
        }
        let manifest = package.join("Cargo.toml");
        let value = toml::from_str::<toml::Value>(&fs::read_to_string(root.join(&manifest))?)
            .map_err(io::Error::other)?;
        files.insert(manifest);
        for name in ["src", "browser", "build.rs"] {
            collect(root, &package.join(name), &mut files)?;
        }
        dependencies(&value, &workspace, &package, &mut pending)?;
    }
    let mut selected = files
        .into_iter()
        .map(|path| Ok((portable_name(&path)?, path)))
        .collect::<io::Result<Vec<_>>>()?;
    selected.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(selected.into_iter().map(|(_, path)| path).collect())
}

fn dependencies(
    value: &toml::Value,
    workspace: &toml::Value,
    package: &Path,
    pending: &mut VecDeque<PathBuf>,
) -> io::Result<()> {
    let Some(table) = value.as_table() else {
        return Ok(());
    };
    for (key, value) in table {
        if key == "dependencies" || key == "build-dependencies" {
            if let Some(deps) = value.as_table() {
                for (name, spec) in deps {
                    let inherited =
                        spec.get("workspace").and_then(toml::Value::as_bool) == Some(true);
                    let spec = if inherited {
                        &workspace["workspace"]["dependencies"][name]
                    } else {
                        spec
                    };
                    if let Some(path) = spec.get("path").and_then(toml::Value::as_str) {
                        let path = if inherited {
                            PathBuf::from(path)
                        } else {
                            package.join(path)
                        };
                        // Canonicalize later relative to root; reject traversal rather than guessing.
                        if path
                            .components()
                            .any(|part| !matches!(part, std::path::Component::Normal(_)))
                        {
                            return Err(io::Error::other("unsupported renderer dependency path"));
                        }
                        pending.push_back(path);
                    }
                }
            }
        } else if key != "dev-dependencies" {
            dependencies(value, workspace, package, pending)?;
        }
    }
    Ok(())
}

fn collect(root: &Path, relative: &Path, files: &mut BTreeSet<PathBuf>) -> io::Result<()> {
    let path = root.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_symlink() {
        return Err(io::Error::other("renderer input is a symlink"));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            collect(root, &relative.join(entry?.file_name()), files)?;
        }
    } else if metadata.is_file() {
        files.insert(relative.to_path_buf());
    } else {
        return Err(io::Error::other("renderer input is not a regular file"));
    }
    Ok(())
}

pub fn portable_name(path: &Path) -> io::Result<String> {
    path.components()
        .map(|part| match part {
            std::path::Component::Normal(name) => name
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| io::Error::other("non-UTF8 renderer path")),
            _ => Err(io::Error::other("renderer input path must be relative")),
        })
        .collect::<io::Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

pub fn identity(root: &Path, files: &[PathBuf]) -> io::Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"bmux-web-renderer-inputs-v1\0");
    for file in files {
        let name = portable_name(file)?;
        let path = root.join(file);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
            return Err(io::Error::other(
                "renderer provenance input must be a bounded regular file",
            ));
        }
        let bytes = fs::read(path)?;
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    Ok(hex(&hash.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}
