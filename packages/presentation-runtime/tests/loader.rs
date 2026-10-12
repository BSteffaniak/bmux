use bmux_presentation_runtime::{HostError, Limits, loader::SourceLoader};
use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};
#[test]
fn bounded_markdown_loader_rejects_oversize_and_root_escape() {
    let root = std::env::temp_dir().join(format!(
        "bmux-presentation-loader-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let source = root.join("deck.md");
    fs::write(&source, "# A slide\n\nhello").unwrap();
    let mut loader = SourceLoader {
        root: root.clone(),
        limits: Limits::default(),
        mermaid: None,
    };
    assert!(loader.load_markdown(&source).is_ok());
    loader.limits.deck_bytes = 4;
    assert!(matches!(
        loader.load_markdown(&source),
        Err(HostError::Limit)
    ));
    assert!(matches!(
        loader.load_markdown(std::path::Path::new("/etc/hosts")),
        Err(HostError::Unauthorized)
    ));
    fs::remove_file(source).unwrap();
    fs::remove_dir(root).unwrap();
}
