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
