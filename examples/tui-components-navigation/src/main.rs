use anyhow::Result;
use bmux_tui_components_navigation::{render_navigation, rows};

fn main() -> Result<()> {
    for row in rows(&render_navigation()) {
        println!("{row}");
    }
    Ok(())
}
