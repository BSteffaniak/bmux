//! Source-only guard for the served bootstrap; no extra asset routes required.
//! Runtime browser polling behavior still requires coordinator browser validation.
#[test]
fn viewport_requires_an_explicit_leased_controller_and_bounded_latest_queue() {
    let source = include_str!("../browser/bootstrap.js");
    assert!(!source.contains("/view-info"));
    assert!(source.contains("request('/view', controller, { method: 'POST'"));
    assert!(source.contains("if (!info?.viewport_allowed) return;"));
    assert!(source.contains("'X-Bmux-Controller': secret"));
    assert!(source.contains("requestId = 0n"));
    assert!(source.contains("pendingViewport ??= value"));
    assert!(source.contains("response.status === 429"));
    assert!(source.contains("response.status === 409"));
    assert!(source.contains("method: 'DELETE', keepalive: true"));
    assert!(source.contains("if (stopped || secret !== controllerSecret || value.generation !== info?.generation) return;"));
    assert!(source.contains("nextGeneration !== generation) await refreshInfo(controller)"));
    assert!(!source.contains("setInterval"));
    assert!(!source.contains("new CanvasRenderer('bmux', 10, 20)"));
    assert!(source.contains("observer.disconnect()"));
    assert!(source.contains("renderer.free()"));
}

#[test]
fn transport_cursor_is_independent_from_native_presentation_revision() {
    let source = include_str!("../browser/bootstrap.js");
    assert!(source.contains("let pollCursor = null;"));
    assert!(source.contains("const requestCursor = pollCursor;"));
    assert!(source.contains("encodeURIComponent(requestCursor)"));
    assert!(source.contains("const parsed = BigInt(value);"));
    assert!(source.contains("parsed > 18446744073709551615n"));
    assert!(source.contains("/^[1-9][0-9]{0,19}$/"));
    assert!(source.contains("transport < transportHighWater"));
    assert!(source.contains("transport === transportHighWater && requestCursor !== null"));
    assert!(
        source.contains(
            "generation === nextGeneration && renderer.revision() === inspected.revision"
        )
    );
    assert!(source.contains("await renderer.repair_json(json)"));
    assert!(!source.contains("nextGeneration !== generation) renderer.reset()"));
    assert!(!source.contains("inspected.revision !== announced"));
    assert!(!source.contains("renderer.revision() === announced"));
    assert!(!source.contains("encodeURIComponent(renderer.revision())"));
    let commit = source
        .find("await renderer.present_json(json);")
        .expect("backend commit");
    let advance = source[commit..]
        .find("pollCursor = announced;")
        .expect("advance after commit");
    assert!(advance > 0);
    assert!(source.contains(
        "Snapshot failed; reconnecting with a full snapshot.';\n        pollCursor = null;"
    ));
}
