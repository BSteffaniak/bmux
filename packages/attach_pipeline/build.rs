//! `image_protocols`: set when any terminal image protocol feature is enabled, so code that
//! writes images is gated by one cfg instead of the list of protocol features.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(image_protocols)");
    let enabled = ["IMAGE_KITTY", "IMAGE_SIXEL", "IMAGE_ITERM2"]
        .iter()
        .any(|feature| std::env::var_os(format!("CARGO_FEATURE_{feature}")).is_some());
    if enabled {
        println!("cargo::rustc-cfg=image_protocols");
    }
}
