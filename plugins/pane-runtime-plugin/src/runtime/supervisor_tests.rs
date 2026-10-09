#[cfg(unix)]
#[test]
fn long_runtime_parent_does_not_lengthen_control_endpoint() {
    let parent = tempfile::tempdir().unwrap();
    let long = parent.path().join("runtime-".repeat(32));
    assert!(long.as_os_str().len() > 104);
    let endpoint = super::short_endpoint_directory().unwrap();
    let listener =
        std::os::unix::net::UnixListener::bind(endpoint.path().join("control.sock")).unwrap();
    assert!(
        listener
            .local_addr()
            .unwrap()
            .as_pathname()
            .unwrap()
            .as_os_str()
            .len()
            < 104
    );
}
