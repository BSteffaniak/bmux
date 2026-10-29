//! The build worker can be checked independently of the expensive guest build.

#[path = "../build_worker.rs"]
mod build_worker;

#[test]
fn compilation_runs_on_named_worker_and_can_borrow_inputs() {
    let input = String::from("guest");
    let parent = std::thread::current().id();
    let result = build_worker::run(|| {
        assert_ne!(std::thread::current().id(), parent);
        assert_eq!(std::thread::current().name(), Some("mermaid-aot-build"));
        input.as_str()
    })
    .unwrap();
    assert_eq!(result, "guest");
}

#[test]
fn compilation_errors_are_not_replaced_by_fallback_artifacts() {
    let result = build_worker::run(|| Err::<(), _>("compiler failed")).unwrap();
    assert_eq!(result, Err("compiler failed"));
}

#[test]
fn compiler_panics_propagate_to_build_caller() {
    let result = std::panic::catch_unwind(|| {
        let _ = build_worker::run(|| panic!("compiler panic fixture"));
    });
    let payload = result.expect_err("build must fail on compiler panic");
    assert_eq!(
        payload.downcast_ref::<&str>(),
        Some(&"compiler panic fixture")
    );
}
