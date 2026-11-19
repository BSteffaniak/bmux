use arboard::{Clipboard, ImageData};

fn main() {
    // Linked only to the temporary, named-board-only Arboard copy built by interop.py.
    let mut board = Clipboard::new().unwrap();
    let text = "bmux 🦀\n東京";
    let pixels = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
    ];
    match std::env::args().nth(1).as_deref() {
        Some("write-text") => board.set_text(text).unwrap(),
        Some("read-text") => assert_eq!(board.get_text().unwrap(), text),
        Some("write-image") => board
            .set_image(ImageData {
                width: 2,
                height: 2,
                bytes: (&pixels[..]).into(),
            })
            .unwrap(),
        Some("read-image") => {
            let image = board.get_image().unwrap();
            assert_eq!((image.width, image.height), (2, 2));
            assert_eq!(&*image.bytes, pixels);
        }
        _ => panic!("unknown operation"),
    }
}
