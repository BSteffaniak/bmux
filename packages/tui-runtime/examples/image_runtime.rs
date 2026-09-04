//! Minimal protocol-neutral image presenter example.
//!
//! Run with a host protocol feature, for example:
//! `cargo run -p bmux_tui_runtime --example image_runtime --features image-kitty`

use std::io;

use bmux_tui::geometry::Rect;
use bmux_tui::image::{
    ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
};
use bmux_tui_runtime::{
    ImageTerminalPresenter, Program, Runtime, RuntimeConfig, RuntimeEvent, Update,
};

struct ExampleProgram {
    presented: bool,
}

impl Program for ExampleProgram {
    type Message = ();
    type Error = std::convert::Infallible;

    fn presentation_committed(&mut self, _report: bmux_tui_runtime::PresentReport) -> Update<()> {
        self.presented = true;
        Update::exit()
    }

    fn update(&mut self, _event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
        Ok(Update::none())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let area = Rect::new(0, 0, 24, 6);
    let terminal = bmux_tui::terminal::Terminal::new(io::stdout(), area);
    let presenter = ImageTerminalPresenter::detect(
        terminal,
        |_: &mut ExampleProgram, cx: &mut bmux_tui::paint::PaintCx<'_, '_>| {
            cx.write_line(
                bmux_tui::paint::LocalRect::new(0, 0, 24, 1),
                &bmux_tui::text::Line::raw("BMUX protocol-neutral image"),
            );
            cx.push_image(ImageContribution::Present(ImagePlacement {
                key: ImageKey::new("example.checkerboard"),
                payload: ImagePayload::Pixels {
                    bytes: vec![
                        255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255, 255, 0, 0, 255,
                    ],
                    width: 2,
                    height: 2,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: Rect::new(0, 2, 4, 2),
                clip: cx.clip(),
                lifecycle: ImageLifecycle::Frame,
            }));
        },
        bmux_image::ImageConfig::default(),
    );
    let (runtime, handle) = Runtime::new(
        ExampleProgram { presented: false },
        presenter,
        RuntimeConfig::default(),
    );
    handle.request_redraw();
    let mut output = match runtime.run().await {
        Ok(output) => output,
        Err(_) => return Err("image runtime failed".into()),
    };
    output.presenter.cleanup_images()?;
    assert!(output.program.presented);
    Ok(())
}
