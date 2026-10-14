// Shared by build.rs and the first-use source verifier. Never invokes a compiler.
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub fn inputs(root: &Path) -> io::Result<Vec<PathBuf>> {
    confined_file(root, Path::new("Cargo.toml"))?;
    let workspace = toml::from_str::<toml::Value>(&fs::read_to_string(root.join("Cargo.toml"))?)
        .map_err(io::Error::other)?;
    let mut files = BTreeSet::new();
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "flake.nix",
        "flake.lock",
        "scripts/dev.sh",
        "scripts/presenter-adapter.js",
        ".cargo/config.toml",
    ] {
        files.insert(PathBuf::from(name));
    }
    let mut pending = VecDeque::from([
        PathBuf::from("packages/web-renderer"),
        PathBuf::from("packages/presenter"),
    ]);
    let mut visited = BTreeSet::new();
    while let Some(package) = pending.pop_front() {
        if !visited.insert(package.clone()) {
            continue;
        }
        let manifest = package.join("Cargo.toml");
        confined_file(root, &manifest)?;
        let value = toml::from_str::<toml::Value>(&fs::read_to_string(root.join(&manifest))?)
            .map_err(io::Error::other)?;
        files.insert(manifest);
        for name in ["src", "browser", "build.rs"] {
            collect(root, &package.join(name), &mut files)?;
        }
        dependencies(root, &value, &workspace, &package, &mut pending)?;
    }
    let mut selected = files
        .into_iter()
        .map(|path| Ok((portable_name(&path)?, path)))
        .collect::<io::Result<Vec<_>>>()?;
    selected.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(selected.into_iter().map(|(_, path)| path).collect())
}

fn dependencies(
    root: &Path,
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
                        let base = if inherited { Path::new("") } else { package };
                        pending.push_back(dependency_path(root, base, Path::new(path))?);
                    }
                }
            }
        } else if key != "dev-dependencies" {
            dependencies(root, value, workspace, package, pending)?;
        }
    }
    Ok(())
}

// Normalize Cargo's legitimate sibling paths without allowing escape or aliases
// through symlinks. Keep identity names relative and stable across checkout roots.
fn dependency_path(root: &Path, base: &Path, dependency: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    let mut relative = base.to_path_buf();
    portable_name(base)?;
    for part in dependency.components() {
        match part {
            Component::Normal(name) => {
                relative.push(name);
                confined_file(root, &relative)?;
            }
            Component::CurDir => {}
            Component::ParentDir if relative.pop() => {}
            _ => return Err(io::Error::other("renderer dependency escapes workspace")),
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(io::Error::other("renderer dependency is workspace root"));
    }
    confined_file(root, &relative)?;
    if !fs::symlink_metadata(root.join(&relative))?.is_dir() {
        return Err(io::Error::other("renderer dependency is not a directory"));
    }
    Ok(relative)
}

// Check every ancestor, not only the final file: symlinked crate directories must
// never redirect provenance reads outside the captured source root.
fn confined_file(root: &Path, relative: &Path) -> io::Result<()> {
    portable_name(relative)?;
    if relative.as_os_str().is_empty() {
        return Err(io::Error::other("empty renderer input path"));
    }
    let mut path = root.to_path_buf();
    for part in relative.components() {
        path.push(part);
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_symlink() {
            return Err(io::Error::other("renderer input path contains a symlink"));
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
        confined_file(root, file)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn graph() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace.dependencies]\nshared = {path = 'packages/shared'}\n",
        )
        .unwrap();
        for name in ["web-renderer", "presenter", "shared"] {
            let package = root.join("packages").join(name);
            fs::create_dir_all(package.join("src")).unwrap();
            fs::write(package.join("src/lib.rs"), name).unwrap();
            let manifest = match name {
                "presenter" => {
                    "[dependencies]\nrenderer = {path = '../web-renderer'}\n[target.'cfg(unix)'.dependencies]\nshared = {workspace = true}\n"
                }
                "web-renderer" => "[dependencies]\nshared = {path = '../shared'}\n",
                _ => "[package]\nname = 'shared'\n",
            };
            fs::write(package.join("Cargo.toml"), manifest).unwrap();
        }
        for name in [
            "Cargo.lock",
            "flake.nix",
            "flake.lock",
            "scripts/dev.sh",
            "scripts/presenter-adapter.js",
            ".cargo/config.toml",
        ] {
            let file = root.join(name);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, "input").unwrap();
        }
        directory
    }

    #[test]
    fn provenance_normalizes_shared_manifest_graph_and_hashes_transitive_resources() {
        let directory = graph();
        let root = directory.path();
        let resource = root.join("packages/shared/src/resource.bin");
        fs::write(&resource, [1, 2, 3]).unwrap();
        let files = inputs(root).unwrap();
        assert!(files.contains(&PathBuf::from("packages/shared/src/resource.bin")));
        assert_eq!(
            files
                .iter()
                .filter(|p| p.as_path() == Path::new("packages/web-renderer/Cargo.toml"))
                .count(),
            1
        );
        assert!(files.iter().all(|p| portable_name(p).is_ok()));
        let first = identity(root, &files).unwrap();
        fs::write(resource, [1, 2, 4]).unwrap();
        assert_ne!(first, identity(root, &files).unwrap());
        assert_eq!(
            dependency_path(
                root,
                Path::new("packages/presenter"),
                Path::new("./../shared")
            )
            .unwrap(),
            PathBuf::from("packages/shared")
        );
    }

    #[test]
    fn provenance_rejects_escape_absolute_missing_and_symlink_dependencies() {
        let directory = graph();
        let root = directory.path();
        for path in [
            "../../../outside",
            "/tmp",
            "../../../../packages/shared",
            "../missing",
            "../shared/Cargo.toml",
        ] {
            assert!(
                dependency_path(root, Path::new("packages/presenter"), Path::new(path)).is_err(),
                "{path}"
            );
        }
        symlink(root.join("packages/shared"), root.join("packages/alias")).unwrap();
        for path in ["../alias", "../alias/../shared"] {
            assert!(
                dependency_path(root, Path::new("packages/presenter"), Path::new(path)).is_err()
            );
        }
        assert!(identity(root, &[PathBuf::from("packages/alias/src/lib.rs")]).is_err());
        fs::write(
            root.join("packages/presenter/Cargo.toml"),
            "[dependencies]\nshared = {path = '../alias'}\n",
        )
        .unwrap();
        assert!(inputs(root).is_err());
    }

    #[test]
    fn provenance_rejects_symlink_source_resource_and_manifest() {
        let directory = graph();
        let root = directory.path();
        symlink(
            root.join("Cargo.lock"),
            root.join("packages/shared/src/resource.bin"),
        )
        .unwrap();
        assert!(inputs(root).is_err());
        assert!(identity(root, &[PathBuf::from("packages/shared/src/resource.bin")]).is_err());
        let other = tempfile::tempdir().unwrap();
        fs::write(other.path().join("Cargo.toml"), "[package]\nname='other'\n").unwrap();
        symlink(other.path(), root.join("packages/external")).unwrap();
        assert!(dependency_path(root, Path::new(""), Path::new("packages/external")).is_err());
        let manifest = root.join("packages/shared/Cargo.toml");
        fs::remove_file(&manifest).unwrap();
        symlink(other.path().join("Cargo.toml"), &manifest).unwrap();
        assert!(inputs(root).is_err());
        assert!(identity(root, &[PathBuf::from("packages/shared/Cargo.toml")]).is_err());
    }
}
