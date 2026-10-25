//! Behavioral coverage for explicit policy relaxations; strict transport tests remain unchanged.
use super::*;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

struct Directory(std::path::PathBuf);
impl Directory {
    fn new() -> Self {
        let path =
            std::path::PathBuf::from("/tmp").join(format!("ipc-policy-{}", uuid::Uuid::new_v4()));
        UnixSecurityPolicy::strict()
            .prepare_parent(&path.join("socket"))
            .unwrap();
        Self(path)
    }
    fn endpoint(&self) -> IpcEndpoint {
        IpcEndpoint::unix_socket(self.0.join("socket"))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[tokio::test]
async fn modes_and_owner_are_explicit_and_existing_directories_are_not_repaired() {
    let dir = Directory::new();
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o750)).unwrap();
    assert!(LocalIpcListener::bind(&dir.endpoint()).is_err());
    let policy = UnixSecurityPolicy {
        parent_mode: Some(0o750),
        parent_owner: IdentityPolicy::User(std::fs::metadata(&dir.0).unwrap().uid()),
        socket_mode: 0o660,
        ..UnixSecurityPolicy::strict()
    };
    let listener = LocalIpcListener::bind_with_policy(&dir.endpoint(), policy).unwrap();
    assert_eq!(
        std::fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777,
        0o750
    );
    assert_eq!(
        std::fs::metadata(dir.0.join("socket"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o660
    );
    let client = LocalIpcStream::connect(&dir.endpoint()).await.unwrap();
    let accepted = listener.accept().await.unwrap();
    drop((client, accepted, listener));
}

#[test]
fn ownership_and_mode_validation_can_be_disabled_independently() {
    let dir = Directory::new();
    let uid = std::fs::metadata(&dir.0).unwrap().uid();
    let policy = UnixSecurityPolicy {
        parent_owner: IdentityPolicy::User(uid.wrapping_add(1)),
        ..UnixSecurityPolicy::strict()
    };
    assert!(policy.prepare_parent(&dir.0.join("socket")).is_err());
    let policy = UnixSecurityPolicy {
        parent_owner: IdentityPolicy::Unrestricted,
        parent_mode: None,
        ..policy
    };
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    policy.prepare_parent(&dir.0.join("socket")).unwrap();
}

#[test]
fn parent_creation_and_invalid_modes_are_checked_before_mutation() {
    let dir = Directory::new();
    let socket = dir.0.join("missing/socket");
    let policy = UnixSecurityPolicy {
        create_parents: false,
        ..UnixSecurityPolicy::strict()
    };
    assert!(policy.prepare_parent(&socket).is_err());
    assert!(!socket.parent().unwrap().exists());
    for policy in [
        UnixSecurityPolicy {
            socket_mode: 0o1600,
            ..UnixSecurityPolicy::strict()
        },
        UnixSecurityPolicy {
            directory_mode: 0o1700,
            ..UnixSecurityPolicy::strict()
        },
        UnixSecurityPolicy {
            parent_mode: Some(0o1700),
            ..UnixSecurityPolicy::strict()
        },
    ] {
        assert!(
            LocalIpcListener::bind_with_policy(&IpcEndpoint::unix_socket(&socket), policy).is_err()
        );
        assert!(!socket.parent().unwrap().exists());
    }
    UnixSecurityPolicy::strict()
        .prepare_parent(&socket)
        .unwrap();
    policy.prepare_parent(&socket).unwrap();
}

#[tokio::test]
async fn peer_policy_applies_to_both_connect_and_accept() {
    let dir = Directory::new();
    let uid = std::fs::metadata(&dir.0).unwrap().uid();
    let listener = LocalIpcListener::bind_with_policy(
        &dir.endpoint(),
        UnixSecurityPolicy {
            peer: IdentityPolicy::User(uid.wrapping_add(1)),
            ..UnixSecurityPolicy::strict()
        },
    )
    .unwrap();
    let client = LocalIpcStream::connect(&dir.endpoint()).await.unwrap();
    assert!(listener.accept().await.is_err());
    drop(client);
    assert!(
        LocalIpcStream::connect_with_policy(
            &dir.endpoint(),
            IdentityPolicy::User(uid.wrapping_add(1))
        )
        .await
        .is_err()
    );
    // Drain the rejected outgoing connection; the listener's own policy still rejects it.
    assert!(listener.accept().await.is_err());
    let client = LocalIpcStream::connect_with_policy(&dir.endpoint(), IdentityPolicy::Unrestricted)
        .await
        .unwrap();
    assert!(listener.accept().await.is_err());
    drop((client, listener));
}

#[tokio::test]
async fn unrestricted_peers_and_explicit_matching_identities_connect() {
    for peer in [IdentityPolicy::Unrestricted, IdentityPolicy::EffectiveUser] {
        let dir = Directory::new();
        let uid = std::fs::metadata(&dir.0).unwrap().uid();
        let listener = LocalIpcListener::bind_with_policy(
            &dir.endpoint(),
            UnixSecurityPolicy {
                peer,
                ..UnixSecurityPolicy::strict()
            },
        )
        .unwrap();
        let client =
            LocalIpcStream::connect_with_policy(&dir.endpoint(), IdentityPolicy::User(uid))
                .await
                .unwrap();
        let accepted = listener.accept().await.unwrap();
        drop((client, accepted, listener));
    }
}

#[test]
fn symlink_policy_distinguishes_parent_from_ancestors() {
    let dir = Directory::new();
    let target = dir.0.join("target");
    UnixSecurityPolicy::strict()
        .prepare_parent(&target.join("nested/socket"))
        .unwrap();
    let alias = dir.0.join("alias");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    for suffix in ["socket", "nested/socket"] {
        let path = alias.join(suffix);
        assert!(UnixSecurityPolicy::strict().prepare_parent(&path).is_err());
        assert!(
            UnixSecurityPolicy {
                symlinks: SymlinkPolicy::Reject,
                ..UnixSecurityPolicy::strict()
            }
            .prepare_parent(&path)
            .is_err()
        );
        UnixSecurityPolicy {
            symlinks: SymlinkPolicy::Follow,
            ..UnixSecurityPolicy::strict()
        }
        .prepare_parent(&path)
        .unwrap();
    }
    assert!(
        std::fs::symlink_metadata(alias)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn ancestor_checks_can_be_explicitly_relaxed() {
    let dir = Directory::new();
    let ancestor = dir.0.join("shared");
    UnixSecurityPolicy::strict()
        .prepare_parent(&ancestor.join("socket"))
        .unwrap();
    std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o777)).unwrap();
    let path = ancestor.join("private/socket");
    assert!(UnixSecurityPolicy::strict().prepare_parent(&path).is_err());
    assert!(!ancestor.join("private").exists());
    UnixSecurityPolicy {
        trusted_ancestors: false,
        ..UnixSecurityPolicy::strict()
    }
    .prepare_parent(&path)
    .unwrap();
}
