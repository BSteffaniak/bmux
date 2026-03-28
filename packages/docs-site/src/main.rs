//! Entry point for the bmux documentation website.

use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let runtime = switchy::unsync::runtime::Builder::new().build()?;
    let runtime = Arc::new(runtime);

    let app = bmux_docs_site::init()
        .with_viewport(bmux_docs_site::VIEWPORT.clone())
        .with_router(bmux_docs_site::ROUTER.clone())
        .with_runtime_handle(runtime.handle());

    bmux_docs_site::build_app(app)?.run()?;

    Ok(())
}
