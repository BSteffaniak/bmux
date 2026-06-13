use anyhow::Result;
use bmux_tui_components_gallery::{render_gallery, rows};

fn main() -> Result<()> {
    for row in rows(&render_gallery()) {
        println!("{row}");
    }
    Ok(())
}
