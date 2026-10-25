//! Subprocess checks for ownership surviving launcher loss. Fixtures use only
//! explicit fresh roots and release files; no installed runtime is contacted.

use super::*;

#[test]
fn launcher_exit_helper() {
    let Some(root) = std::env::var_os("BTERM_LAUNCHER_EXIT_FIXTURE") else {
        return;
    };
    let root = Root(PathBuf::from(root));
    let spec = shell_spec(
        &root,
        ": > inherited; while [ ! -f release ]; do /bin/sleep 0.01; done; : > done",
    );
    let ownership = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&spec.ownership_file)
        .unwrap();
    ownership.try_lock().unwrap();
    let child = spawn_detached(&spec, &ownership).unwrap();
    std::fs::write(root.0.join("child-pid"), child.id().to_string()).unwrap();
    // Abrupt launcher exit intentionally skips Rust drop and the parent reaper.
    // The companion's inherited stdin must be sufficient fencing by itself.
    std::process::exit(0);
}

#[tokio::test]
async fn launcher_exit_does_not_release_the_child_startup_claim() {
    let root = Root::new();
    let mut launcher = Command::new(std::env::current_exe().unwrap())
        .args(["launcher_exit_helper", "--nocapture"])
        .env("BTERM_LAUNCHER_EXIT_FIXTURE", &root.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    await_file(&root, "inherited").await;
    assert!(launcher.wait().unwrap().success());
    let claim = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.0.join("startup.lock"))
        .unwrap();
    assert!(matches!(
        claim.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    std::fs::write(root.0.join("release"), b"").unwrap();
    await_file(&root, "done").await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while claim.try_lock().is_err() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn inherited_claim_is_a_regular_file_and_child_is_detached() {
    use std::os::unix::fs::MetadataExt as _;
    let root = Root::new();
    let spec = shell_spec(
        &root,
        "printf '%s\\n%s\\n' \"$BMUX_COMPANION_STARTUP_LOCK_PROTOCOL\" \"$BMUX_COMPANION_STARTUP_LOCK\" > handoff; : > ready; while [ ! -f release ]; do /bin/sleep 0.01; done",
    );
    let owner = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&spec.ownership_file)
        .unwrap();
    owner.try_lock().unwrap();
    let mut child = spawn_detached(&spec, &owner).unwrap();
    await_file(&root, "ready").await;
    assert_eq!(
        std::fs::read_to_string(root.0.join("handoff")).unwrap(),
        format!("v1\n{}\n", spec.ownership_file.display())
    );
    let named = std::fs::metadata(&spec.ownership_file).unwrap();
    assert!(named.is_file());
    assert_eq!(named.ino(), owner.metadata().unwrap().ino());
    let pid = i32::try_from(child.id()).unwrap();
    // SAFETY: read-only OS identity queries for this test's own child.
    assert_eq!(unsafe { libc::getsid(pid) }, pid);
    // SAFETY: read-only OS identity query for this test's own child.
    assert_eq!(unsafe { libc::getpgid(pid) }, pid);
    drop(owner);
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&spec.ownership_file)
        .unwrap();
    assert!(matches!(
        contender.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    std::fs::write(root.0.join("release"), b"").unwrap();
    assert!(child.wait().unwrap().success());
    contender.try_lock().unwrap();
}
