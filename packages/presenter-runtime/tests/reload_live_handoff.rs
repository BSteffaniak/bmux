use bmux_presenter::{Deck, DeckBuilder, SlideBuilder};
use bmux_presenter_runtime::{
    Asset, Host, HostConfig, HostError, Owner, OwnerContext, OwnerUpdate, SurfaceInput,
};
use std::sync::{Arc, Mutex};

struct Resource {
    generation: u64,
}
struct LiveOwner {
    resource: Option<Resource>,
    fail_scene: bool,
    reject_commit: bool,
    hooks: Arc<Mutex<Vec<u64>>>,
}
impl Owner for LiveOwner {
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
    fn prepare_reload(&self, _: &Deck, _: &[Asset]) -> Result<Box<dyn Owner>, HostError> {
        Ok(Box::new(Self {
            resource: None,
            fail_scene: self.fail_scene,
            reject_commit: self.reject_commit,
            hooks: self.hooks.clone(),
        }))
    }
    fn commit_reload(&mut self, candidate: &mut dyn Owner) -> Result<(), HostError> {
        let next = candidate
            .as_any_mut()
            .and_then(|value| value.downcast_mut::<Self>())
            .ok_or(HostError::Invalid)?;
        let generation = self.resource.as_ref().ok_or(HostError::Invalid)?.generation;
        if next.reject_commit {
            return Err(HostError::EffectUncertain);
        }
        self.hooks.lock().unwrap().push(generation);
        next.resource = self.resource.take();
        Ok(())
    }
    fn reconcile_reload(
        &mut self,
        _: OwnerContext<'_>,
    ) -> Result<Vec<bmux_presenter_runtime::mirror::MirrorFrame>, String> {
        if self.fail_scene {
            Err("detached projection failed".into())
        } else {
            Ok(vec![])
        }
    }
    fn demo_input(
        &mut self,
        _: OwnerContext<'_>,
        _: &SurfaceInput,
    ) -> Result<OwnerUpdate, HostError> {
        Err(HostError::Unauthorized)
    }
    fn effect(&mut self, _: OwnerContext<'_>, _: &str, _: &str) -> Result<OwnerUpdate, HostError> {
        Err(HostError::Unauthorized)
    }
}
fn deck() -> Deck {
    DeckBuilder::new("handoff")
        .slide(SlideBuilder::new("s").build())
        .build()
        .unwrap()
}
fn host(fail_scene: bool, reject_commit: bool) -> (Host, Arc<Mutex<Vec<u64>>>) {
    let hooks = Arc::new(Mutex::new(vec![]));
    let mut host = Host::new(deck(), vec![], HostConfig::default()).unwrap();
    host.set_owner(Box::new(LiveOwner {
        resource: Some(Resource { generation: 73 }),
        fail_scene,
        reject_commit,
        hooks: hooks.clone(),
    }))
    .unwrap();
    (host, hooks)
}
#[test]
fn candidate_failure_never_calls_handoff_or_takes_original_resource() {
    let (mut host, hooks) = host(true, false);
    assert!(host.reload(deck(), vec![]).is_err());
    assert!(hooks.lock().unwrap().is_empty());
    // An independently prepared valid candidate can still take the original.
    host.reload_with_owner(
        deck(),
        vec![],
        Box::new(LiveOwner {
            resource: None,
            fail_scene: false,
            reject_commit: false,
            hooks: hooks.clone(),
        }),
    )
    .unwrap();
    assert_eq!(*hooks.lock().unwrap(), vec![73]);
}
#[test]
fn compatible_reload_transfers_same_generation_only_at_commit() {
    let (mut host, hooks) = host(false, false);
    host.reload(deck(), vec![]).unwrap();
    host.reload(deck(), vec![]).unwrap();
    assert_eq!(*hooks.lock().unwrap(), vec![73, 73]);
}
#[test]
fn precommit_rejection_keeps_original_for_later_success() {
    let (mut host, hooks) = host(false, true);
    assert_eq!(host.reload(deck(), vec![]), Err(HostError::EffectUncertain));
    assert!(hooks.lock().unwrap().is_empty());
    host.reload_with_owner(
        deck(),
        vec![],
        Box::new(LiveOwner {
            resource: None,
            fail_scene: false,
            reject_commit: false,
            hooks: hooks.clone(),
        }),
    )
    .unwrap();
    assert_eq!(*hooks.lock().unwrap(), vec![73]);
}
