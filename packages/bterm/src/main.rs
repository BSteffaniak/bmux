//! Native Bterm executable. Diagnostics remain visible on stderr; no failure is
//! converted into a demo window or an attachment to another runtime.

use std::process::ExitCode;

fn main() -> ExitCode {
    match bmux_bterm::app::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bterm: {error:#}");
            ExitCode::FAILURE
        }
    }
}
