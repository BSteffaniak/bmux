use super::*;
use bmux_plugin::{
    PluginCandidateInventory, PluginCandidatePreference, PluginRegistry, PluginSelectionRequest,
};
use bmux_plugin_sdk::{CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION};

fn metadata() -> PluginMetadata {
    PluginMetadata {
        plugin_id: "example.approved".into(), version: "1.0.0".into(),
        roles: vec![ComponentRole::Backend], required_capabilities: Vec::new(),
        contracts: Vec::new(), manifest_toml: Some(
            "id='example.approved'\nname='Approved'\nversion='1.0.0'\ncomponent_roles=['backend']\n".into()),
    }
}
fn selected(manifest: &str) -> SelectedPluginCandidate {
    let mut registry = PluginRegistry::new();
    registry.register_bundled_manifest(manifest).unwrap();
    let mut inventory = PluginCandidateInventory::new();
    inventory
        .add_registered_plugin(registry.get("example.approved").unwrap())
        .unwrap();
    inventory
        .select(&PluginSelectionRequest {
            plugin_id: "example.approved".into(),
            role: ComponentRole::Backend,
            preference: PluginCandidatePreference::Bundled,
            plugin_api: CURRENT_PLUGIN_API_VERSION,
            native_abi: CURRENT_PLUGIN_ABI_VERSION,
            contracts: Vec::new(),
        })
        .unwrap()
}

#[cfg(unix)]
fn native_store(
    plugin_id: &str,
    role: &str,
    policy: &str,
) -> (
    tempfile::TempDir,
    bmux_config::artifact_approval::ArtifactApprovalStore,
) {
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let paths = bmux_config::ConfigPaths::new(
        root.clone(),
        root.join("runtime"),
        root.join("data"),
        root.join("state"),
    );
    std::fs::write(root.join(bmux_config::artifact_approval::APPROVAL_FILE), format!(
        "schema_version=1\nruntime_dir={:?}\n[[approvals]]\nplugin_id={plugin_id:?}\nrole={role:?}\nrequest_policy={policy:?}\nartifact_sha256='{}'",
        paths.runtime_dir.to_str().unwrap(), "a".repeat(64),
    )).unwrap();
    let store = bmux_config::artifact_approval::ArtifactApprovalStore::load(&paths)
        .unwrap()
        .unwrap();
    (temporary, store)
}

#[cfg(unix)]
#[test]
fn native_store_rejects_unknown_policy_and_frontend_kernel_approval() {
    let (_root, store) = native_store("bmux.clients", "backend", "arbitrary-policy");
    assert!(NativeKernelApproval::validate_store(&store).is_err());
    let (_root, store) = native_store("bmux.clients", "frontend", "clients-kernel-v1");
    assert!(NativeKernelApproval::validate_store(&store).is_err());
    let (_root, store) = native_store("other.provider", "backend", "clients-kernel-v1");
    assert!(NativeKernelApproval::validate_store(&store).is_err());
}

#[cfg(all(unix, feature = "bundled-plugin-clients"))]
#[test]
fn native_approval_matches_explicit_component_but_issues_no_grant() {
    let (_root, store) = native_store("bmux.clients", "backend", "clients-kernel-v1");
    let manifest = store.runtime_dir().parent().unwrap().join("provider.toml");
    std::fs::write(
        &manifest,
        "id='bmux.clients'\nname='Clients'\nversion='1.0.0'\nentry='provider.dylib'\n",
    )
    .unwrap();
    let mut inventory = PluginCandidateInventory::new();
    inventory
        .add_dynamic_manifest_path_for_plugin("bmux.clients", &manifest)
        .unwrap();
    let selected = inventory
        .select(&PluginSelectionRequest {
            plugin_id: "bmux.clients".into(),
            role: ComponentRole::Backend,
            preference: PluginCandidatePreference::Dynamic {
                manifest_path: manifest,
            },
            plugin_api: CURRENT_PLUGIN_API_VERSION,
            native_abi: CURRENT_PLUGIN_ABI_VERSION,
            contracts: Vec::new(),
        })
        .unwrap();
    assert!(
        NativeKernelApproval::for_selected(&store, &selected, store.runtime_dir(), false).is_err()
    );
    assert!(
        NativeKernelApproval::for_selected(
            &store,
            &selected,
            std::path::Path::new("/other-runtime"),
            true
        )
        .is_err()
    );
    let mut pinned = selected.candidate().plugin.clone();
    pinned.manifest.artifact_sha256 = Some("b".repeat(64));
    let mut pinned_inventory = PluginCandidateInventory::new();
    pinned_inventory.add_registered_plugin(&pinned).unwrap();
    let pinned_selection = pinned_inventory
        .select(&PluginSelectionRequest {
            plugin_id: "bmux.clients".into(),
            role: ComponentRole::Backend,
            preference: PluginCandidatePreference::Default,
            plugin_api: CURRENT_PLUGIN_API_VERSION,
            native_abi: CURRENT_PLUGIN_ABI_VERSION,
            contracts: Vec::new(),
        })
        .unwrap();
    assert!(
        NativeKernelApproval::for_selected(&store, &pinned_selection, store.runtime_dir(), true)
            .is_err()
    );
    let intent = NativeKernelApproval::for_selected(&store, &selected, store.runtime_dir(), true)
        .unwrap()
        .unwrap();
    assert!(intent.matches_observation("bmux.clients", ComponentRole::Backend, &"a".repeat(64)));
    assert!(!intent.matches_observation("bmux.clients", ComponentRole::Backend, &"b".repeat(64)));
    assert!(!intent.matches_observation("bmux.clients", ComponentRole::Frontend, &"a".repeat(64)));
    assert!(!intent.matches_observation(
        "another.provider",
        ComponentRole::Backend,
        &"a".repeat(64)
    ));
    let request = bmux_ipc::encode(&bmux_ipc::Request::CurrentStreamProvenance).unwrap();
    assert!((intent.authorize)(&request).is_ok());
    let denied = bmux_ipc::encode(&bmux_ipc::Request::Ping).unwrap();
    assert!((intent.authorize)(&denied).is_err());
    let bundled = self::selected(metadata().manifest_toml.as_deref().unwrap());
    // Different logical ID carries no approval, even if it is bundled.
    assert!(
        NativeKernelApproval::for_selected(&store, &bundled, store.runtime_dir(), true)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn identical_metadata_without_selected_loader_receipt_cannot_bind_native_grant() {
    struct Inert;
    impl bmux_plugin_sdk::definition::PluginInstance for Inert {}
    let metadata = metadata();
    let selected = selected(metadata.manifest_toml.as_deref().unwrap());
    let approval = TrustedKernelPolicy::bundled(&metadata, |_| Ok(()))
        .unwrap()
        .approve(&selected)
        .unwrap();
    let artifact = bmux_plugin::DefinitionArtifact::from_definition(
        bmux_plugin_sdk::definition::PluginDefinition::new(metadata, |_| {
            Ok(Arc::new(Inert) as Arc<dyn bmux_plugin_sdk::definition::PluginInstance>)
        }),
    );
    let execution = artifact
        .create(PluginRuntimeScopeId::new(), ComponentRole::Backend, &[])
        .unwrap();
    assert!(execution.verified_artifact().is_none());
    assert!(checked_receipt(&approval.selected, &execution).is_err());
    execution.destroy().await.unwrap();
}

#[test]
fn trusted_policy_rejects_changed_selected_declaration() {
    let metadata = metadata();
    let canonical = metadata.manifest_toml.as_deref().unwrap();
    assert!(
        TrustedKernelPolicy::bundled(&metadata, |_| Ok(()))
            .unwrap()
            .approve(&selected(canonical))
            .is_ok()
    );
    let changed = canonical.replace("version='1.0.0'", "version='1.0.1'");
    assert!(
        TrustedKernelPolicy::bundled(&metadata, |_| Ok(()))
            .unwrap()
            .approve(&selected(&changed))
            .is_err()
    );
}

#[test]
fn approved_artifact_retains_plugin_owned_request_policy() {
    fn deny_request(_: &[u8]) -> bmux_plugin_sdk::Result<()> {
        Err(PluginError::ServiceProtocol {
            details: "plugin-owned policy denied".into(),
        })
    }
    let metadata = metadata();
    let approved = TrustedKernelPolicy::bundled(&metadata, deny_request)
        .unwrap()
        .approve(&selected(metadata.manifest_toml.as_deref().unwrap()))
        .unwrap();
    assert!(
        (approved.authorize)(&[])
            .unwrap_err()
            .to_string()
            .contains("plugin-owned policy denied")
    );
}

#[test]
fn issuer_rejects_inconsistent_composition_identity() {
    let mut metadata = metadata();
    metadata.plugin_id = "example.not-the-canonical-artifact".into();
    assert!(TrustedKernelPolicy::bundled(&metadata, |_| Ok(())).is_err());
}
