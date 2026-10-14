fn main() {
    let marker = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../public/live-assets/current-version");
    println!("cargo:rerun-if-changed={}", marker.display());
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
