use bmux_presenter_runtime::{HostError, Limits, loader::SourceLoader};
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

#[test]
fn loader_prepares_inline_images_and_rejects_unknown_themes_and_oversized_assets() {
    use bmux_presenter::{Block, BlockKind, DeckBuilder, SlideBuilder, TextFormat};
    let root = std::env::temp_dir().join(format!("bmux-inline-images-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let image_path = root.join("picture.png");
    image::RgbaImage::new(1, 1).save(&image_path).unwrap();
    let mut deck = DeckBuilder::new("inline")
        .slide(SlideBuilder::new("a").build())
        .build()
        .unwrap();
    deck.slides[0].blocks.push(Block {
        id: "rich".into(),
        reveal_step: 0,
        source: None,
        kind: BlockKind::Text {
            content: "**before** ![*alt*][image] after\n\n[image]: picture.png\n".into(),
            format: TextFormat::Markdown,
        },
    });
    let mut loader = SourceLoader {
        root: root.clone(),
        limits: Limits::default(),
        mermaid: None,
    };
    let prepared = loader.prepare(deck.clone(), &root).unwrap();
    assert_eq!(prepared.assets.len(), 1);
    let BlockKind::Text { content, .. } = &prepared.deck.slides[0].blocks[0].kind else {
        panic!("text")
    };
    assert!(content.contains("**before**"));
    assert!(content.contains(&format!("![*alt*]({})", prepared.assets[0].id)));
    let mut invalid = deck.clone();
    invalid.metadata.theme = Some("unknown".into());
    assert!(matches!(
        loader.prepare(invalid, &root),
        Err(HostError::Invalid)
    ));
    loader.limits.asset_bytes = 1;
    assert!(matches!(loader.prepare(deck, &root), Err(HostError::Limit)));
    fs::remove_file(image_path).unwrap();
    fs::remove_dir(root).unwrap();
}
