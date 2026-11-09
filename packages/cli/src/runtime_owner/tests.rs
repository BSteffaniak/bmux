use super::*;

#[test]
fn observer_loss_keeps_primary_reactor_running_until_actual_owner_release() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let driver = RuntimeDriver::new(runtime).unwrap();
    let owner = driver.owner();
    let weak = owner.downgrade();
    let (entered, started) = std::sync::mpsc::sync_channel(1);
    let (release, released) = tokio::sync::oneshot::channel();
    let (done, completed) = std::sync::mpsc::sync_channel(1);
    let retained = owner.clone();
    let observer = owner.handle().spawn(async move {
        let _owner = retained;
        entered.send(()).unwrap();
        released.await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        done.send(()).unwrap();
    });
    started.recv().unwrap();
    drop(observer); // Tokio observation detaches; actual work retains its owner.
    drop(owner);
    let (stopped, shutdown) = std::sync::mpsc::sync_channel(1);
    let join = std::thread::spawn(move || {
        driver.shutdown().unwrap();
        stopped.send(()).unwrap();
    });
    assert!(weak.upgrade().is_some());
    assert!(matches!(
        shutdown.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    release.send(()).unwrap();
    completed.recv().unwrap();
    shutdown.recv().unwrap();
    join.join().unwrap();
    assert!(weak.upgrade().is_none());
}

#[test]
fn owner_last_drop_inside_async_work_does_not_drop_runtime_on_that_worker() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let driver = RuntimeDriver::new(runtime).unwrap();
    let owner = driver.owner();
    let retained = owner.clone();
    let (done, completed) = std::sync::mpsc::sync_channel(1);
    owner.handle().spawn(async move {
        tokio::task::yield_now().await;
        drop(retained);
        done.send(()).unwrap();
    });
    drop(owner);
    completed.recv().unwrap();
    driver.shutdown().unwrap();
}

#[test]
fn shutdown_with_leaked_admission_is_bounded_and_names_the_holder() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let driver = RuntimeDriver::new(runtime).unwrap();
    let owner = driver.owner();
    let instance =
        bmux_plugin_runtime::InstanceScope::new(bmux_plugin_runtime::PluginRuntimeScopeId::new());
    let label = format!("example.leaky-{}", instance.id());
    instance.set_label(label.as_str()).unwrap();
    let activation = instance
        .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let leaked = activation.try_admit_labeled("never-settles").unwrap();
    let retained = owner.clone();
    let (entered, started) = std::sync::mpsc::sync_channel(1);
    // Deliberately leaked execution: it retains both a runtime owner pin and a
    // scope admission and never completes.
    owner.handle().spawn(async move {
        let _owner = retained;
        let _admission = leaked;
        entered.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    started.recv().unwrap();
    drop(owner);
    let begun = std::time::Instant::now();
    let error = driver
        .shutdown_within(std::time::Duration::from_millis(300))
        .unwrap_err();
    assert!(begun.elapsed() < std::time::Duration::from_secs(10));
    let RuntimeShutdownError::Abandoned {
        retained_pins,
        ref unfinished,
        ..
    } = error
    else {
        panic!("expected abandonment, got {error}");
    };
    assert!(retained_pins >= 1);
    assert!(unfinished.iter().any(|report| {
        report.instance == instance.id()
            && report.holders.iter().any(|holder| {
                holder.label.as_deref() == Some("never-settles")
                    && holder
                        .site
                        .is_some_and(|site| site.file().ends_with("runtime_owner/tests.rs"))
            })
    }));
    let text = error.to_string();
    assert!(text.contains(&label), "{text}");
    assert!(text.contains("never-settles"), "{text}");
}
