use anyhow::Result;
use bmux_tui_components_inputs::{render_inputs, rows};

fn main() -> Result<()> {
    for row in rows(&render_inputs()) {
        println!("{row}");
    }
    Ok(())
}
