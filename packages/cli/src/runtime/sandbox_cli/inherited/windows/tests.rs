use super::*;
use std::process::Command;

fn acl(path: &Path) -> Vec<u8> {
    let output = Command::new("icacls.exe").arg(path).output().unwrap();
    assert!(output.status.success());
    output.stdout
}

#[test]
fn broad_parent_stays_broad_but_owned_tree_and_snapshots_are_private() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("broad");
    std::fs::create_dir(&parent).unwrap();
    // Deliberately broad only inside this test's disposable directory. Production
    // never adjusts existing ACLs. Numeric SID avoids localized account names.
    let result = Command::new("icacls.exe")
        .arg(&parent)
        .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
        .status()
        .unwrap();
    assert!(result.success());
    let before = acl(&parent);
    assert!(Tree::existing(&parent).is_err());
    let root = parent.join("owned");
    let mut tree = Tree::create(&root).unwrap();
    assert_eq!(acl(&parent), before);
    tree.directory(&root.join("config")).unwrap();
    tree.directory(&root.join("config/bmux")).unwrap();
    tree.directory(&root.join("runtime")).unwrap();
    tree.write(
        &root.join("config/bmux/config.toml"),
        b"credential = 'secret'",
    )
    .unwrap();
    tree.write(&root.join("development.json"), b"{}").unwrap();
    assert!(
        tree.write(&root.join("development.json"), b"overwrite")
            .is_err()
    );
    for path in [
        &root,
        &root.join("config"),
        &root.join("config/bmux"),
        &root.join("runtime"),
    ] {
        drop(Tree::existing(path).unwrap());
    }
    let parents = tree.open_parent(&root.join("config/bmux")).unwrap();
    let file = child(
        parents.last().unwrap(),
        OsStr::new("config.toml"),
        false,
        false,
        &tree.security,
    )
    .unwrap();
    tree.security.check_file(file.as_raw_handle()).unwrap();
    assert_eq!(acl(&parent), before);
    assert!(Tree::create(&parent).is_err());
    assert_eq!(acl(&parent), before);
    drop(file);
    drop(parents);
    drop(tree);
}

#[test]
fn rejects_junction_ancestor_without_repairing_target() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let before = acl(&target);
    let junction = temp.path().join("junction");
    let output = Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&target)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(Tree::create(&junction.join("owned")).is_err());
    assert!(!target.join("owned").exists());
    assert_eq!(acl(&target), before);
    std::fs::remove_dir(junction).unwrap();
}

#[test]
fn rejects_existing_broad_owned_subdirectory_without_repair() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("owned");
    let tree = Tree::create(&root).unwrap();
    let broad = root.join("broad");
    std::fs::create_dir(&broad).unwrap();
    assert!(
        Command::new("icacls.exe")
            .arg(&broad)
            .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
            .status()
            .unwrap()
            .success()
    );
    let before = acl(&broad);
    assert!(tree.write(&broad.join("secret"), b"secret").is_err());
    assert!(!broad.join("secret").exists());
    assert_eq!(acl(&broad), before);
}
