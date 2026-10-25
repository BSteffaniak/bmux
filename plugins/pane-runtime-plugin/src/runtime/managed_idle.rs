//! Managed retirement and short-lived admission, serialized with the real manager.
use super::{SessionRuntimeManager, reservations, spawn_runtime_handle};
use bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::ManagedIdleStatus;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    policy: Option<u64>,
    idle_since: Option<Instant>,
    admissions: usize,
    fenced: bool,
    preparations: std::collections::BTreeMap<uuid::Uuid, Preparation>,
}

struct Preparation {
    owner: uuid::Uuid,
    expires: Instant,
}

const PREPARATION_TTL: Duration = Duration::from_mins(1);
const MAX_PREPARATIONS: usize = 128;

fn authority() -> &'static Arc<Mutex<State>> {
    static STATE: OnceLock<Arc<Mutex<State>>> = OnceLock::new();
    STATE.get_or_init(|| Arc::new(Mutex::new(State::default())))
}

/// Owned by actual preparation/cleanup, never by a connection or daemon lifetime.
pub(crate) struct Admission(Arc<Mutex<State>>);

impl Drop for Admission {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.lock() {
            state.admissions = state.admissions.saturating_sub(1);
            // Even work admitted and completed between watcher ticks resets grace.
            state.idle_since = None;
        }
    }
}

fn prune_preparations(state: &mut State, now: Instant) {
    let before = state.preparations.len();
    state.preparations.retain(|_, lease| lease.expires > now);
    if before != state.preparations.len() {
        // Expired preparation cannot donate its elapsed time to retirement grace.
        state.idle_since = None;
    }
}

pub(crate) fn preparation(
    instance: uuid::Uuid,
    owner: Option<uuid::Uuid>,
    lease_id: Option<uuid::Uuid>,
    release: bool,
) -> Result<
    Option<bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::ManagedPreparationLease>,
    String,
> {
    let owner = owner.ok_or("managed preparation requires a connected client")?;
    let readiness = crate::readiness::snapshot()?;
    if readiness.instance != instance {
        return Err("runtime instance changed; preparation has no effect authority".into());
    }
    if !readiness.helper_ready || readiness.fenced {
        return Err(
            "managed runtime unavailable or draining; preparation refused before effects".into(),
        );
    }
    super::managed_scope::selected()?.ok_or("runtime is not a managed companion")?;
    let mut state = authority()
        .lock()
        .map_err(|_| "idle admission lock poisoned")?;
    preparation_on(
        &mut state,
        instance,
        owner,
        lease_id,
        release,
        Instant::now(),
    )
}

fn preparation_on(
    state: &mut State,
    instance: uuid::Uuid,
    owner: uuid::Uuid,
    lease_id: Option<uuid::Uuid>,
    release: bool,
    now: Instant,
) -> Result<
    Option<bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::ManagedPreparationLease>,
    String,
> {
    use bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::ManagedPreparationLease;
    prune_preparations(state, now);
    if let Some(id) = lease_id {
        let lease = state
            .preparations
            .get_mut(&id)
            .ok_or("preparation lease expired or unavailable; no attach effects may be replayed")?;
        if lease.owner != owner {
            return Err("preparation lease belongs to another connection".into());
        }
        if release {
            state.preparations.remove(&id);
            state.idle_since = None;
            return Ok(None);
        }
        if state.fenced {
            return Err("managed runtime draining; preparation refused before effects".into());
        }
        lease.expires = now + PREPARATION_TTL;
        state.idle_since = None;
        return Ok(Some(ManagedPreparationLease {
            instance,
            lease_id: id,
            ttl_seconds: PREPARATION_TTL.as_secs(),
        }));
    }
    if release || state.fenced {
        return Err("managed runtime draining; preparation refused before effects".into());
    }
    if state.preparations.len() >= MAX_PREPARATIONS {
        return Err("managed preparation capacity exhausted before effects".into());
    }
    let id = uuid::Uuid::new_v4();
    state.preparations.insert(
        id,
        Preparation {
            owner,
            expires: now + PREPARATION_TTL,
        },
    );
    state.idle_since = None;
    Ok(Some(ManagedPreparationLease {
        instance,
        lease_id: id,
        ttl_seconds: PREPARATION_TTL.as_secs(),
    }))
}

pub(crate) fn is_fenced() -> Result<bool, String> {
    authority()
        .lock()
        .map(|state| state.fenced)
        .map_err(|_| "idle admission lock poisoned".into())
}

pub(crate) fn admit() -> anyhow::Result<Admission> {
    admit_on(authority())
}

fn admit_on(authority: &Arc<Mutex<State>>) -> anyhow::Result<Admission> {
    let mut state = authority
        .lock()
        .map_err(|_| anyhow::anyhow!("idle admission lock poisoned"))?;
    anyhow::ensure!(!state.fenced, "managed runtime fenced for idle shutdown");
    state.admissions = state
        .admissions
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("admission capacity exhausted"))?;
    state.idle_since = None;
    Ok(Admission(Arc::clone(authority)))
}

pub(crate) fn configure(
    instance: uuid::Uuid,
    grace_seconds: u64,
) -> Result<ManagedIdleStatus, String> {
    let readiness = crate::readiness::snapshot()?;
    if readiness.instance != instance {
        return Err("runtime instance changed".into());
    }
    if readiness.fenced || !readiness.helper_ready {
        return Err("managed idle policy requires a ready, unfenced execution authority".into());
    }
    if readiness
        .managed_target_identity
        .as_deref()
        .is_none_or(str::is_empty)
    {
        return Err("runtime was not explicitly launched as a managed companion".into());
    }
    super::managed_scope::selected()?.ok_or("managed execution target scope unavailable")?;
    {
        let mut state = authority()
            .lock()
            .map_err(|_| "idle admission lock poisoned")?;
        if state.fenced {
            return Err("runtime already fenced".into());
        }
        match state.policy {
            Some(previous) if previous != grace_seconds => {
                return Err("managed idle policy already configured differently".into());
            }
            Some(_) => {}
            None => {
                state.policy = Some(grace_seconds);
                state.idle_since = None;
            }
        }
    }
    inspect(Some(instance), false)
}

pub(crate) fn inspect(
    instance: Option<uuid::Uuid>,
    fence: bool,
) -> Result<ManagedIdleStatus, String> {
    let readiness = crate::readiness::snapshot()?;
    if instance.is_some_and(|instance| instance != readiness.instance) {
        return Err("runtime instance changed".into());
    }
    let handle =
        spawn_runtime_handle().ok_or("execution manager unavailable; idle safety unknown")?;
    // Durable checks happen before manager locking: existing recovery diagnostics
    // may read manager state while holding the store. A concurrent mutation is
    // covered by its admission owner and resets grace even if it finishes here.
    let retained_blocker = reservations::idle_blocker().or_else(super::supervisor::idle_blocker);
    let retained_authority = reservations::retained_authority();
    let manager = handle
        .0
        .lock()
        .map_err(|_| "execution manager lock poisoned")?;
    let mut state = authority()
        .lock()
        .map_err(|_| "idle admission lock poisoned")?;
    let now = Instant::now();
    if fence {
        prune_preparations(&mut state, now);
    }
    let preparation_count = state
        .preparations
        .values()
        .filter(|lease| lease.expires > now)
        .count();
    let blocker = runtime_blocker(&manager)
        .or_else(|| {
            (preparation_count != 0).then(|| {
                format!("{preparation_count} client connections preparing native admission")
            })
        })
        .or_else(|| {
            (state.admissions != 0).then(|| {
                format!(
                    "{} pending execution admissions or cleanup owners",
                    state.admissions
                )
            })
        })
        .or(retained_blocker)
        .or_else(|| match super::pane_shutdown_owners().lock() {
            Ok(owners) if owners.values().any(|owner| !owner.is_finished()) => {
                Some("pane cleanup owner is still settling".into())
            }
            Ok(_) => None,
            Err(_) => Some("pane cleanup ownership unavailable".into()),
        });
    // Diagnostic inspection never starts/resets a timer or fences a runtime.
    if fence {
        evaluate(&manager, &mut state, blocker.as_deref(), now);
    }
    Ok(ManagedIdleStatus {
        instance: readiness.instance,
        managed: state.policy.is_some(),
        grace_seconds: state.policy.unwrap_or(0),
        idle_for_seconds: state
            .idle_since
            .map_or(0, |since| now.saturating_duration_since(since).as_secs()),
        blocker,
        retained_authority,
        fenced: state.fenced,
    })
}

fn runtime_blocker(manager: &SessionRuntimeManager) -> Option<String> {
    if !manager.runtimes.is_empty() {
        // A quiet detached shell remains real work. Never infer quiescence from
        // output silence, a prompt, absent windows, or absent foreground commands.
        return Some(format!(
            "{} retained session runtimes (including detached shells/jobs)",
            manager.runtimes.len()
        ));
    }
    None
}

fn evaluate(
    manager: &SessionRuntimeManager,
    state: &mut State,
    blocker: Option<&str>,
    now: Instant,
) {
    let runtime_blocker = runtime_blocker(manager);
    let blocker = runtime_blocker.as_deref().or(blocker);
    if state.admissions != 0 || state.preparations.values().any(|lease| lease.expires > now) {
        update(state, Some("pending admission or cleanup"), now);
    } else {
        update(state, blocker, now);
    }
}

fn update(state: &mut State, blocker: Option<&str>, now: Instant) {
    if state.fenced {
        return;
    }
    let Some(seconds) = state.policy.filter(|seconds| *seconds != 0) else {
        state.idle_since = None;
        return;
    };
    if blocker.is_some() {
        state.idle_since = None;
        return;
    }
    let since = *state.idle_since.get_or_insert(now);
    if now.saturating_duration_since(since) >= Duration::from_secs(seconds) {
        state.fenced = true;
    }
}

#[cfg(test)]
pub(super) fn admissions_settled_for_test() -> bool {
    authority().lock().unwrap().admissions == 0
}

#[cfg(test)]
pub(super) fn preparation_for_test(
    owner: uuid::Uuid,
    lease_id: Option<uuid::Uuid>,
    release: bool,
) -> uuid::Uuid {
    let mut state = authority().lock().unwrap();
    preparation_on(
        &mut state,
        uuid::Uuid::nil(),
        owner,
        lease_id,
        release,
        Instant::now(),
    )
    .unwrap()
    .map_or(uuid::Uuid::nil(), |lease| lease.lease_id)
}

#[cfg(test)]
pub(super) fn fence_for_test(manager: &SessionRuntimeManager) -> bool {
    let mut state = authority().lock().unwrap();
    state.policy = Some(900);
    state.idle_since = Some(
        Instant::now()
            .checked_sub(Duration::from_secs(901))
            .unwrap(),
    );
    evaluate(manager, &mut state, None, Instant::now());
    state.fenced
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_preparation_blocks_shutdown_renewal_is_owner_scoped_and_expiry_resets_grace() {
        let now = Instant::now();
        let instance = uuid::Uuid::new_v4();
        let owner = uuid::Uuid::new_v4();
        let mut state = State {
            policy: Some(900),
            ..State::default()
        };
        let lease = preparation_on(&mut state, instance, owner, None, false, now)
            .unwrap()
            .unwrap();
        assert_eq!(lease.ttl_seconds, 60);
        assert!(
            preparation_on(
                &mut state,
                instance,
                uuid::Uuid::new_v4(),
                Some(lease.lease_id),
                false,
                now
            )
            .is_err()
        );
        assert!(
            preparation_on(
                &mut state,
                instance,
                uuid::Uuid::new_v4(),
                Some(lease.lease_id),
                true,
                now
            )
            .is_err()
        );
        let renewed = preparation_on(
            &mut state,
            instance,
            owner,
            Some(lease.lease_id),
            false,
            now + Duration::from_secs(30),
        )
        .unwrap()
        .unwrap();
        assert_eq!(renewed.lease_id, lease.lease_id);
        prune_preparations(&mut state, now + Duration::from_secs(61));
        assert_eq!(state.preparations.len(), 1);
        state.idle_since = Some(now);
        prune_preparations(&mut state, now + Duration::from_secs(91));
        assert!(state.idle_since.is_none());
        assert!(
            preparation_on(
                &mut state,
                instance,
                owner,
                Some(lease.lease_id),
                false,
                now + Duration::from_secs(91)
            )
            .is_err()
        );
        update(&mut state, None, now + Duration::from_secs(91));
        assert!(!state.fenced);
        update(&mut state, None, now + Duration::from_secs(991));
        assert!(state.fenced);
        assert!(
            preparation_on(
                &mut state,
                instance,
                owner,
                None,
                false,
                now + Duration::from_secs(991)
            )
            .is_err()
        );
    }

    #[test]
    fn preparation_capacity_and_release_are_bounded_without_keeping_a_daemon_alive() {
        let now = Instant::now();
        let instance = uuid::Uuid::new_v4();
        let owner = uuid::Uuid::new_v4();
        let mut state = State::default();
        let mut leases = Vec::new();
        for _ in 0..MAX_PREPARATIONS {
            leases.push(
                preparation_on(&mut state, instance, owner, None, false, now)
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(preparation_on(&mut state, instance, owner, None, false, now).is_err());
        let lease = leases.pop().unwrap();
        assert!(
            preparation_on(&mut state, instance, owner, Some(lease.lease_id), true, now)
                .unwrap()
                .is_none()
        );
        assert!(preparation_on(&mut state, instance, owner, None, false, now).is_ok());
        prune_preparations(&mut state, now + PREPARATION_TTL);
        assert!(state.preparations.is_empty());
    }

    #[test]
    fn diagnostics_and_disabled_policy_cannot_retire() {
        let mut state = State::default();
        let now = Instant::now();
        update(&mut state, None, now);
        assert!(!state.fenced);
        state.policy = Some(0);
        update(&mut state, None, now + Duration::from_mins(150));
        assert!(!state.fenced);
        assert!(state.idle_since.is_none());
    }

    #[test]
    fn grace_resets_for_admission_completed_between_ticks_and_fence_is_sticky() {
        let authority = Arc::new(Mutex::new(State {
            policy: Some(900),
            ..State::default()
        }));
        let now = Instant::now();
        update(&mut authority.lock().unwrap(), None, now);
        drop(admit_on(&authority).unwrap());
        update(
            &mut authority.lock().unwrap(),
            None,
            now + Duration::from_mins(15),
        );
        assert!(!authority.lock().unwrap().fenced);
        update(
            &mut authority.lock().unwrap(),
            None,
            now + Duration::from_mins(30),
        );
        assert!(admit_on(&authority).is_err());
    }
}
