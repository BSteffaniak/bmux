#[path = "src/provenance.rs"]
mod provenance;

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let inputs = provenance::inputs(&root).expect("renderer source provenance");
    let identity = provenance::identity(&root, &inputs).expect("renderer input identity");
    for file in &inputs {
        println!("cargo:rerun-if-changed={}", root.join(file).display());
    }
    // Watch directories too: adding/removing an input must invalidate the identity.
    for file in &inputs {
        if let Some(parent) = file.parent() {
            println!("cargo:rerun-if-changed={}", root.join(parent).display());
        }
    }
    println!("cargo:rerun-if-env-changed=BMUX_WEB_RELEASE_TAG");
    let tag = std::env::var("BMUX_WEB_RELEASE_TAG").unwrap_or_default();
    assert!(
        tag.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid release tag"
    );
    assert!(tag != "latest", "release tag must be immutable and exact");
    println!("cargo:rustc-env=BMUX_WEB_RELEASE_TAG={tag}");
    println!("cargo:rustc-env=BMUX_WEB_SOURCE_ROOT={}", root.display());
    println!("cargo:rustc-env=BMUX_WEB_RENDERER_SHA256={identity}");
    let renderer = toml::from_str::<toml::Value>(
        &std::fs::read_to_string(root.join("packages/web-renderer/Cargo.toml")).unwrap(),
    )
    .unwrap();
    let bindgen = renderer["target"]["cfg(target_arch = \"wasm32\")"]["dependencies"]
        ["wasm-bindgen"]["version"].as_str().expect("pinned wasm-bindgen version");
    let lock =
        toml::from_str::<toml::Value>(&std::fs::read_to_string(root.join("Cargo.lock")).unwrap())
            .unwrap();
    let locked_bindgen = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"].as_str() == Some("wasm-bindgen"))
        .expect("wasm-bindgen in workspace lock")["version"]
        .as_str()
        .unwrap();
    assert_eq!(
        bindgen.trim_start_matches('='),
        locked_bindgen,
        "renderer manifest and lock must pin the same binding generator"
    );
    println!("cargo:rustc-env=BMUX_WEB_WASM_BINDGEN_VERSION={locked_bindgen}");
}
