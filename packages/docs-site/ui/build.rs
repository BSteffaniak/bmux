fn main() {
    let marker = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../public/live-assets/current-version");
    // The marker is generated (and ignored) and usually absent; Cargo treats a
    // missing rerun-if-changed path as changed on every build. Watch the
    // nearest existing ancestor instead: creating the marker changes it.
    let watched = marker
        .ancestors()
        .find(|path| path.exists())
        .unwrap_or(&marker);
    println!("cargo:rerun-if-changed={}", watched.display());
    let version = std::fs::read_to_string(&marker).ok();
    let path = version
        .as_deref()
        .map(str::trim)
        .filter(|version| {
            version.len() == 64 && version.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .map_or_else(
            || "/live-assets/live-page.js".to_owned(),
            |version| format!("/live-assets/{version}/live-page.js"),
        );
    println!("cargo:rustc-env=BMUX_LIVE_PAGE_MODULE={path}");
}
