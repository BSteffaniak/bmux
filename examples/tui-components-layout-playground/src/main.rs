use anyhow::Result;
use bmux_tui_components_layout_playground::{render_layout_playground, rows};

fn main() -> Result<()> {
    for row in rows(&render_layout_playground()) {
        println!("{row}");
    }
    Ok(())
}
