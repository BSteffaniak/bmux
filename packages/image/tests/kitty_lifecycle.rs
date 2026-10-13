#![cfg(feature = "kitty")]

use bmux_image::codec::kitty::{encode_place, encode_transmit, parse_command};
use bmux_image::model::{ImageEvent, ImagePosition, KittyFormat};
use bmux_image::registry::ImageRegistry;

fn command(registry: &mut ImageRegistry, body: &[u8]) {
    registry.handle_event(
        ImageEvent::KittyCommand {
            command: parse_command(body, ImagePosition { row: 0, col: 0 })
                .expect("valid Kitty command"),
            filtered_byte_offset: 0,
        },
        8,
        16,
    );
}

fn upload(registry: &mut ImageRegistry) {
    command(
        registry,
        &encode_transmit(7, KittyFormat::Rgba, &[255, 0, 0, 255], 1, 1),
    );
}

fn place(registry: &mut ImageRegistry, id: u32) {
    command(registry, &encode_place(7, id, 0, 0));
}

#[test]
fn lowercase_deletion_retains_upload_for_replacement() {
    for delete in ["Ga=d,d=i,i=7", "Ga=d,d=i,i=7,p=1", "Ga=d,d=a"] {
        let mut registry = ImageRegistry::new(8, 1024);
        upload(&mut registry);
        place(&mut registry, 1);
        let pixels = registry.images()[0].payload.pixels.clone();
        command(&mut registry, delete.as_bytes());
        assert!(registry.images().is_empty());
        place(&mut registry, 2);
        assert_eq!(registry.images().len(), 1, "{delete}");
        assert_eq!(registry.images()[0].payload.pixels, pixels);
    }
}

#[test]
fn uppercase_deletion_releases_upload() {
    for delete in ["Ga=d,d=I,i=7", "Ga=d,d=I,i=7,p=1", "Ga=d,d=A"] {
        let mut registry = ImageRegistry::new(8, 1024);
        upload(&mut registry);
        place(&mut registry, 1);
        command(&mut registry, delete.as_bytes());
        place(&mut registry, 2);
        assert!(registry.images().is_empty(), "{delete}");
        upload(&mut registry);
        place(&mut registry, 2);
        assert_eq!(registry.images().len(), 1);
    }
}

#[test]
fn hidden_uploads_obey_aggregate_byte_and_count_limits() {
    let mut registry = ImageRegistry::new(2, 1024);
    for id in 1..=10 {
        command(
            &mut registry,
            &encode_transmit(id, KittyFormat::Rgba, &[255, 0, 0, 255], 1, 1),
        );
    }
    for id in 1..=10 {
        command(&mut registry, &encode_place(id, id, 0, 0));
    }
    assert_eq!(registry.images().len(), 2);
}

#[test]
fn unsupported_delete_selector_does_not_delete_unrelated_images() {
    assert!(parse_command(b"Ga=d,d=p,x=1,y=1", ImagePosition { row: 0, col: 0 }).is_none());
}

#[test]
fn releasing_one_placement_preserves_other_users_of_content() {
    let mut registry = ImageRegistry::new(8, 1024);
    upload(&mut registry);
    place(&mut registry, 1);
    place(&mut registry, 2);
    command(&mut registry, b"Ga=d,d=I,i=7,p=1");
    assert_eq!(registry.images().len(), 1);
    place(&mut registry, 3);
    assert_eq!(registry.images().len(), 2);
    assert_eq!(
        registry.images()[0].position,
        ImagePosition { row: 0, col: 0 }
    );
}
