use super::*;
use std::time::Duration;

fn scopes() -> (InstanceScope, ActivationScope, InvocationScope) {
    let instance = InstanceScope::new(PluginRuntimeScopeId::new());
    instance.set_label("example.holders").unwrap();
    let activation = instance
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let invocation = activation.begin_invocation().unwrap();
    invocation.set_label("probe-operation").unwrap();
    (instance, activation, invocation)
}

fn count(report: &OutstandingHolders, kind: HolderKind) -> usize {
    report
        .holders
        .iter()
        .filter(|holder| holder.kind == kind)
        .count()
}

fn sited_here(holder: &OutstandingHolder) -> bool {
    holder
        .site
        .is_some_and(|site| site.file().ends_with("holder_tests.rs"))
}

#[tokio::test]
async fn holders_are_attributed_to_creating_site_and_removed_on_drop() {
    let (instance, activation, invocation) = scopes();
    let plain = invocation.try_admit().unwrap();
    let plain_line = line!() - 1;
    let labeled = invocation.try_admit_labeled("render").unwrap();
    let resource = invocation.register_resource(Arc::new(5_u32)).unwrap();

    let report = activation.outstanding().unwrap();
    assert_eq!(report.instance_label.as_deref(), Some("example.holders"));
    assert_eq!(count(&report, HolderKind::Admission), 2);
    assert_eq!(count(&report, HolderKind::ChildScope), 1);
    assert_eq!(count(&report, HolderKind::Resource), 1);
    assert!(report.holders.iter().all(sited_here));
    assert!(report.holders.iter().any(|holder| {
        holder.kind == HolderKind::Admission
            && holder.label.is_none()
            && holder.site.map(Location::line) == Some(plain_line)
            && holder.scope_label.as_deref() == Some("probe-operation")
    }));
    assert!(report.holders.iter().any(|holder| {
        holder.kind == HolderKind::Admission && holder.label.as_deref() == Some("render")
    }));
    assert!(report.holders.iter().any(|holder| {
        holder.kind == HolderKind::Resource && holder.label.as_deref() == Some("u32")
    }));
    let text = report.to_string();
    assert!(text.contains("example.holders"), "{text}");
    assert!(text.contains("holder_tests.rs"), "{text}");
    assert!(text.contains("render"), "{text}");

    drop(plain);
    drop(labeled);
    let report = invocation.outstanding().unwrap();
    assert_eq!(count(&report, HolderKind::Admission), 0);
    assert_eq!(count(&report, HolderKind::Resource), 1);

    invocation.close().unwrap();
    let Err(ScopeError::Outstanding(report)) = invocation.finish() else {
        panic!("unreleased resource must block finish");
    };
    assert_eq!(count(&report, HolderKind::Resource), 1);
    drop(invocation.release_resource(resource).unwrap());
    invocation.drain().await.unwrap();
    invocation.finish().unwrap();
    activation.drain().await.unwrap();
    activation.finish().unwrap();
    instance.drain().await.unwrap();
    instance.finish().unwrap();
}

#[tokio::test]
async fn finish_lists_unfinished_child_scopes() {
    let (instance, activation, invocation) = scopes();
    let child = invocation.begin_child().unwrap();
    let child_line = line!() - 1;
    invocation.close().unwrap();
    let Err(ScopeError::Outstanding(report)) = invocation.finish() else {
        panic!("child must block finish");
    };
    let holder = report
        .holders
        .iter()
        .find(|holder| holder.kind == HolderKind::ChildScope)
        .expect("child listed");
    assert_eq!(holder.scope, ScopeKey::Invocation(child.id()));
    assert_eq!(holder.site.map(Location::line), Some(child_line));
    child.drain().await.unwrap();
    child.finish().unwrap();
    invocation.drain().await.unwrap();
    invocation.finish().unwrap();
    activation.drain().await.unwrap();
    activation.finish().unwrap();
    instance.drain().await.unwrap();
    instance.finish().unwrap();
}

#[tokio::test]
async fn drain_deadline_reports_leaked_admission_and_never_succeeds() {
    let (_instance, activation, invocation) = scopes();
    let leaked = invocation.try_admit_labeled("leaked-call").unwrap();
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_millis(100);
    let Err(ScopeError::DeadlineExceeded(report)) = activation.drain_until(deadline).await else {
        panic!("leaked admission must expire the deadline");
    };
    assert!(started.elapsed() < Duration::from_secs(5));
    let holder = report
        .holders
        .iter()
        .find(|holder| holder.kind == HolderKind::Admission)
        .expect("admission listed");
    assert_eq!(holder.label.as_deref(), Some("leaked-call"));
    assert!(sited_here(holder));
    assert_eq!(holder.scope, ScopeKey::Invocation(invocation.id()));
    assert_eq!(activation.state().unwrap(), LifecycleState::CleanupRequired);
    // Late settlement cannot convert an abandoned teardown into success.
    drop(leaked);
    assert!(matches!(
        activation.drain().await,
        Err(ScopeError::Cleanup(failure)) if failure.contains("leaked-call")
    ));
    assert!(activation.finish().is_err());
}

#[tokio::test]
async fn drain_deadline_aborts_and_names_stuck_task() {
    let (_instance, activation, invocation) = scopes();
    invocation
        .spawn("stuck-worker", |_cancel| async {
            std::future::pending::<()>().await;
            Ok(())
        })
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
    let Err(ScopeError::DeadlineExceeded(report)) = invocation.drain_until(deadline).await else {
        panic!("stuck task must expire the deadline");
    };
    assert!(report.holders.iter().any(|holder| {
        holder.kind == HolderKind::Task && holder.label.as_deref() == Some("stuck-worker")
    }));
    assert!(report.holders.iter().any(|holder| {
        holder.kind == HolderKind::Admission
            && holder.label.as_deref() == Some("stuck-worker")
            && sited_here(holder)
    }));
    // The abort released the task's admission without unloading anything.
    tokio::time::timeout(Duration::from_secs(5), async {
        while invocation
            .outstanding()
            .is_ok_and(|report| count(&report, HolderKind::Admission) > 0)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(invocation.drain().await.is_err());
    assert!(activation.finish().is_err());
}

#[test]
fn concurrent_admit_drop_and_queries_do_not_deadlock() {
    let (instance, activation, invocation) = scopes();
    let (done, finished) = std::sync::mpsc::channel();
    let mut workers = Vec::new();
    for worker in 0..8 {
        let invocation = invocation.clone();
        let activation = activation.clone();
        let done = done.clone();
        workers.push(std::thread::spawn(move || {
            for round in 0..2_000 {
                let guard = if worker % 2 == 0 {
                    invocation.try_admit().unwrap()
                } else {
                    invocation.try_admit_labeled("worker").unwrap()
                };
                if round % 50 == 0 {
                    let _ = activation.outstanding().unwrap();
                    let _ = unfinished_instances();
                }
                drop(guard);
            }
            done.send(()).unwrap();
        }));
    }
    drop(done);
    for _ in 0..8 {
        finished
            .recv_timeout(Duration::from_secs(60))
            .expect("admit/drop/query must not deadlock");
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(
        count(&invocation.outstanding().unwrap(), HolderKind::Admission),
        0
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
        instance.drain().await.unwrap();
        instance.finish().unwrap();
    });
}

#[tokio::test]
async fn unfinished_instances_names_leaks_and_omits_finished() {
    let leaked = InstanceScope::new(PluginRuntimeScopeId::new());
    let label = format!("example.leaked-{}", leaked.id());
    leaked.set_label(label.as_str()).unwrap();
    let activation = leaked
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    let _held = activation.try_admit_labeled("held-forever").unwrap();
    let finished = InstanceScope::new(PluginRuntimeScopeId::new());
    let finished_id = finished.id();
    finished.drain().await.unwrap();
    finished.finish().unwrap();

    let reports = unfinished_instances();
    let report = reports
        .iter()
        .find(|report| report.instance == leaked.id())
        .expect("leaked instance reported");
    assert_eq!(report.instance_label.as_deref(), Some(label.as_str()));
    assert!(report.holders.iter().any(|holder| {
        holder.kind == HolderKind::Admission && holder.label.as_deref() == Some("held-forever")
    }));
    assert!(reports.iter().all(|report| report.instance != finished_id));
}
