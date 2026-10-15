//! A relocated executable must render with only embedded artifacts.
use bmux_mermaid_render::{MermaidCancellationToken, MermaidRenderRequest, render_mermaid};

#[test]
fn embedded_fixture() {
    let Some(output) = std::env::var_os("BMUX_EMBEDDED_RENDER_TEST_OUTPUT") else {
        return;
    };
    let request = MermaidRenderRequest::svg("flowchart LR\nA[Hello] --> B[World]", 800, 600);
    let rendered = render_mermaid(&request, &MermaidCancellationToken::default()).unwrap();
    let bmux_mermaid_render::MermaidRenderedOutput::Svg(bytes) = rendered.output;
    std::fs::write(output, bytes).unwrap();
}

#[test]
fn relocated_executable_needs_no_worker_guest_file_or_toolchain() {
    let installation = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let executable = installation
        .path()
        .join(format!("render-check{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let output = installation.path().join("diagram.svg");
    let result = std::process::Command::new(&executable)
        .args(["--exact", "embedded_fixture", "--nocapture"])
        .current_dir(installation.path())
        .env("BMUX_EMBEDDED_RENDER_TEST_OUTPUT", &output)
        .env("PATH", "")
        .env("TMPDIR", scratch.path())
        .env("TMP", scratch.path())
        .env("TEMP", scratch.path())
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    let svg = std::fs::read_to_string(output).unwrap();
    assert!(svg.contains("<svg") && svg.contains("Hello") && svg.contains("World"));
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(installation.path()).unwrap().count(), 2);
}
