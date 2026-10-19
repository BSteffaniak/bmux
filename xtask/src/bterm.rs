//! Immutable Bterm product bundles. Staging never installs into a user runtime.
//! Hashes detect incomplete/corrupt bundles; they are not signatures or provenance.
use crate::artifacts::{Result, error};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

const MANIFEST: &str = "bterm-bundle.json";
const COMPATIBILITY: &str = "bterm-bmux-v1";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    compatibility_id: String,
    target: String,
    artifacts: Vec<Artifact>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    role: String,
    path: PathBuf,
    sha256: String,
}

fn digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let mut output = String::with_capacity(64);
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}")?;
    }
    Ok(output)
}

fn regular(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(error(format!(
            "bundle requires regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn copy(source: &Path, root: &Path, path: &Path, role: &str) -> Result<Artifact> {
    regular(source)?;
    let destination = root.join(path);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, &destination)?;
    Ok(Artifact {
        role: role.into(),
        path: path.into(),
        sha256: digest(&destination)?,
    })
}

/// Stage an immutable, explicit companion bundle. Destination must not exist.
/// macOS produces a real .app hierarchy; Linux and Windows produce relocatable roots.
/// No command runs either executable and no service or live configuration is touched.
pub fn stage(workspace: &Path, source: &Path, destination: &Path, target: &str) -> Result<()> {
    let unix = crate::artifacts::unix_target(target)?;
    if destination.exists() {
        return Err(error("bundle destination already exists"));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| error("destination requires a parent"))?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::tempdir_in(parent)?;
    let payload = staging.path().join("payload");
    let mac = target.contains("apple-darwin");
    let root = if mac {
        payload.join("Contents/MacOS")
    } else {
        payload.clone()
    };
    // Use the same manifest-driven companion payload as the normal BMUX product.
    // Helpers must remain beside the companion for owner-local plugin discovery.
    crate::artifacts::stage(
        source,
        &root,
        target,
        &crate::artifacts::declarations(workspace)?,
    )?;
    let inventory = crate::artifacts::verify(&root)?;
    let mut artifacts = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if entry.file_name() != "bmux" && entry.file_name() != "bmux.exe" {
            artifacts.push(Artifact {
                role: "runtime-payload".into(),
                path: entry.file_name().into(),
                sha256: digest(&entry.path())?,
            });
        }
    }
    if inventory.target != target {
        return Err(error("companion inventory target mismatch"));
    }
    for (name, role) in [("bterm", "frontend"), ("bmux", "companion")] {
        let name = if unix {
            name.into()
        } else {
            format!("{name}.exe")
        };
        artifacts.push(copy(&source.join(&name), &root, Path::new(&name), role)?);
        #[cfg(unix)]
        if unix {
            use std::os::unix::fs::PermissionsExt as _;
            if fs::metadata(source.join(&name))?.permissions().mode() & 0o111 == 0 {
                return Err(error("bundled executable is not executable"));
            }
        }
    }
    let fonts = workspace.join("packages/fonts");
    for name in ["Regular", "Bold", "Italic", "BoldItalic"] {
        let name = format!("JetBrainsMonoNerdFont-{name}.ttf");
        artifacts.push(copy(
            &fonts.join("assets/nerd-fonts").join(&name),
            &root,
            &Path::new("resources/fonts").join(name),
            "font",
        )?);
    }
    for (source, name) in [
        (workspace.join("LICENSE"), "BMUX-LICENSE"),
        (
            fonts.join("licenses/NERD_FONTS_LICENSE"),
            "NERD_FONTS_LICENSE",
        ),
        (
            fonts.join("licenses/JETBRAINS_MONO_OFL.txt"),
            "JETBRAINS_MONO_OFL.txt",
        ),
        (fonts.join("THIRD_PARTY_FONTS.md"), "THIRD_PARTY_FONTS.md"),
    ] {
        artifacts.push(copy(
            &source,
            &root,
            &Path::new("notices").join(name),
            "notice",
        )?);
    }
    let manifest = Manifest {
        schema_version: 1,
        compatibility_id: COMPATIBILITY.into(),
        target: target.into(),
        artifacts,
    };
    fs::write(root.join(MANIFEST), serde_json::to_vec_pretty(&manifest)?)?;
    if mac {
        fs::write(
            payload.join("Contents/Info.plist"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>CFBundleExecutable</key><string>bterm</string><key>CFBundleIdentifier</key><string>dev.bmux.bterm</string><key>CFBundleName</key><string>Bterm</string><key>CFBundlePackageType</key><string>APPL</string><key>NSHighResolutionCapable</key><true/></dict></plist>",
        )?;
    } else if unix {
        fs::create_dir_all(payload.join("share/applications"))?;
        fs::write(
            payload.join("share/applications/bterm.desktop"),
            "[Desktop Entry]\nType=Application\nName=Bterm\nComment=BMUX native terminal\nExec=bterm\nTryExec=bterm\nTerminal=false\nCategories=System;TerminalEmulator;\n",
        )?;
    }
    verify(&root, target)?;
    fs::rename(payload, destination)?;
    Ok(())
}

/// Verify installed files without executing them or contacting any runtime.
/// `root` is the executable directory (Contents/MacOS on macOS).
pub fn verify(root: &Path, expected_target: &str) -> Result<()> {
    crate::artifacts::unix_target(expected_target)?;
    regular(&root.join(MANIFEST))?;
    let manifest: Manifest = serde_json::from_reader(fs::File::open(root.join(MANIFEST))?)?;
    if manifest.schema_version != 1
        || manifest.compatibility_id != COMPATIBILITY
        || manifest.target != expected_target
    {
        return Err(error(
            "unsupported bundle schema, companion compatibility, or target",
        ));
    }
    let mut paths = BTreeSet::new();
    let mut roles = BTreeSet::new();
    for artifact in &manifest.artifacts {
        if artifact.path.as_os_str().is_empty()
            || artifact
                .path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(error("bundle path must be relative and traversal-free"));
        }
        // Also reject Windows separators on Unix so a bundle has one interpretation.
        if artifact.path.to_string_lossy().contains('\\')
            || !paths.insert(artifact.path.to_string_lossy().to_ascii_lowercase())
        {
            return Err(error("ambiguous or duplicate bundle path"));
        }
        let mut path = root.to_path_buf();
        for component in artifact.path.components() {
            path.push(component);
            if fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err(error("bundle symlinks are unsupported"));
            }
        }
        regular(&path)?;
        if digest(&path)? != artifact.sha256 {
            return Err(error("bundle artifact hash mismatch"));
        }
        if matches!(artifact.role.as_str(), "frontend" | "companion")
            && !roles.insert(&artifact.role)
        {
            return Err(error("duplicate required bundle role"));
        }
    }
    if roles.len() != 2 {
        return Err(error("bundle requires frontend and companion"));
    }
    if !paths.contains("artifacts.json") {
        return Err(error("bundle requires companion artifact inventory"));
    }
    let inventory = crate::artifacts::verify_with_extra_members(
        root,
        &[
            "bterm",
            "bterm.exe",
            "bterm-bundle.json",
            "resources",
            "notices",
            "share",
        ],
    )?;
    if inventory.target != expected_target
        || inventory
            .artifacts
            .iter()
            .any(|item| !paths.contains(&item.relative_path.to_ascii_lowercase()))
    {
        return Err(error("companion inventory is not covered by the bundle"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stages_all_platform_layouts_and_detects_corruption() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::create_dir(&source).unwrap();
        for name in [
            "bmux",
            "bterm",
            "bmux.exe",
            "bterm.exe",
            "xtask",
            "xtask.exe",
            "bmux-pane-execution-supervisor",
        ] {
            fs::write(source.join(name), name).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(source.join(name), fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        for target in [
            "x86_64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
        ] {
            let destination = dir.path().join(target);
            stage(&workspace, &source, &destination, target).unwrap();
            let root = if target.contains("darwin") {
                destination.join("Contents/MacOS")
            } else {
                destination.clone()
            };
            verify(&root, target).unwrap();
            if !target.contains("windows") {
                let helper = root.join("bmux-pane-execution-supervisor");
                assert!(helper.is_file());
                let bytes = fs::read(&helper).unwrap();
                fs::write(&helper, "corrupt").unwrap();
                assert!(verify(&root, target).is_err());
                fs::remove_file(&helper).unwrap();
                assert!(verify(&root, target).is_err());
                fs::write(&helper, bytes).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
                }
                verify(&root, target).unwrap();
            }
            assert!(verify(&root, "wrong-target").is_err());
            assert!(stage(&workspace, &source, &destination, target).is_err());
            fs::write(
                root.join(if target.contains("windows") {
                    "bmux.exe"
                } else {
                    "bmux"
                }),
                "changed",
            )
            .unwrap();
            assert!(verify(&root, target).is_err());
        }
    }
    #[test]
    fn rejects_traversal_and_duplicate_roles() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("binary"), "fixture").unwrap();
        let make = |path: &str| Artifact {
            role: "frontend".into(),
            path: path.into(),
            sha256: digest(&dir.path().join("binary")).unwrap(),
        };
        let mut manifest = Manifest {
            schema_version: 1,
            compatibility_id: COMPATIBILITY.into(),
            target: "x86_64-unknown-linux-gnu".into(),
            artifacts: vec![make("../binary")],
        };
        let check = |manifest: &Manifest| {
            fs::write(
                dir.path().join(MANIFEST),
                serde_json::to_vec(manifest).unwrap(),
            )
            .unwrap();
            assert!(verify(dir.path(), &manifest.target).is_err());
        };
        check(&manifest);
        manifest.artifacts = vec![make("binary"), make("binary")];
        check(&manifest);
        manifest.artifacts = vec![make("binary")];
        check(&manifest);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_bundle_artifact() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("outside"), "fixture").unwrap();
        symlink("outside", dir.path().join("binary")).unwrap();
        let manifest = Manifest {
            schema_version: 1,
            compatibility_id: COMPATIBILITY.into(),
            target: "x86_64-unknown-linux-gnu".into(),
            artifacts: vec![Artifact {
                role: "frontend".into(),
                path: "binary".into(),
                sha256: digest(&dir.path().join("outside")).unwrap(),
            }],
        };
        fs::write(
            dir.path().join(MANIFEST),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(verify(dir.path(), &manifest.target).is_err());
    }

    #[test]
    fn missing_companion_does_not_publish() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("bundle");
        assert!(
            stage(
                dir.path(),
                dir.path(),
                &destination,
                "x86_64-unknown-linux-gnu"
            )
            .is_err()
        );
        assert!(!destination.exists());
    }
}
