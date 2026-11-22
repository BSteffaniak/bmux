//! Activation-bound frontend prompt/action inboxes owned by one attach host.
#[cfg(test)]
#[path = "frontend_bindings_tests.rs"]
mod tests;
use bmux_plugin::{
    action_dispatch::{ActionDispatchInbox, ActionDispatchPort, AdmittedAction},
    prompt::{AdmittedPrompt, PromptInbox, PromptPort},
};
use bmux_plugin_runtime::{ActivationScope, PluginInstanceId};
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

struct OwnerInboxes {
    scope: ActivationScope,
    prompts: PromptInbox,
    actions: ActionDispatchInbox,
}
/// Host-owned receiver; no process-global channels or alternate activation tree.
pub struct FrontendBindings {
    installs: mpsc::Receiver<OwnerInboxes>,
    owners: BTreeMap<PluginInstanceId, OwnerInboxes>,
}
/// Inject this registrar into the selected frontend's activation binder.
#[derive(Clone)]
pub struct FrontendBindingRegistrar {
    installs: mpsc::Sender<OwnerInboxes>,
}

pub(super) enum FrontendEvent {
    Prompt(Box<AdmittedPrompt>),
    Action(AdmittedAction),
    OwnerClosed(PluginInstanceId),
}
enum InboxWake {
    Installed(Option<OwnerInboxes>),
    Event(Option<FrontendEvent>),
}

impl FrontendBindings {
    /// Construct bounded attachment-local wiring before frontend activation.
    #[must_use]
    pub fn channel() -> (FrontendBindingRegistrar, Self) {
        let (sender, installs) = mpsc::channel(32);
        (
            FrontendBindingRegistrar { installs: sender },
            Self {
                installs,
                owners: BTreeMap::new(),
            },
        )
    }
    pub(super) async fn next(&mut self) -> anyhow::Result<FrontendEvent> {
        loop {
            let event = {
                let mut pending = FuturesUnordered::new();
                for (id, inbox) in &mut self.owners {
                    let id = *id;
                    pending.push(async move {
                        tokio::select! {
                            prompt = inbox.prompts.recv() => prompt.map_or(FrontendEvent::OwnerClosed(id), |prompt| FrontendEvent::Prompt(Box::new(prompt))),
                            action = inbox.actions.recv() => action.map_or(FrontendEvent::OwnerClosed(id), FrontendEvent::Action),
                        }
                    });
                }
                tokio::select! {
                    installed = self.installs.recv(), if !self.installs.is_closed() || !self.installs.is_empty() => InboxWake::Installed(installed),
                    event = pending.next(), if !pending.is_empty() => InboxWake::Event(event),
                    else => std::future::pending().await,
                }
            };
            match event {
                InboxWake::Installed(Some(installed)) => {
                    let id = installed.scope.instance_id();
                    anyhow::ensure!(
                        self.owners.len() < 32 && !self.owners.contains_key(&id),
                        "frontend inbox owner duplicate or capacity exhausted"
                    );
                    self.owners.insert(id, installed);
                }
                InboxWake::Event(Some(event)) => {
                    if let FrontendEvent::OwnerClosed(id) = &event {
                        self.owners.remove(id);
                    }
                    return Ok(event);
                }
                InboxWake::Installed(None) | InboxWake::Event(None) => {}
            }
        }
    }
}
impl FrontendBindingRegistrar {
    /// The loader activation binder this attachment registers on the frontend
    /// instances it owns. The loader calls it with each actual activation
    /// scope; the minted ports are offered to that activation only, and their
    /// inboxes close when it deactivates.
    #[must_use]
    pub fn activation_binder(self) -> std::sync::Arc<bmux_plugin::ActivationBinder> {
        std::sync::Arc::new(
            move |scope: &ActivationScope,
                  capabilities: &bmux_plugin_sdk::definition::ActivationCapabilities| {
                let failure =
                    |details: String| bmux_plugin_sdk::PluginError::ServiceProtocol { details };
                let (prompt, actions) = self
                    .bind(scope.clone(), scope.instance_id())
                    .map_err(|error| failure(format!("{error:#}")))?;
                capabilities
                    .insert(prompt)
                    .map_err(|error| failure(error.message))?;
                capabilities
                    .insert(actions)
                    .map_err(|error| failure(error.message))
            },
        )
    }

    /// The caller supplies the actual loader-owned activation, never decoded IDs.
    ///
    /// # Errors
    /// Rejects cross-instance binding, closed scopes and full/disconnected hosts.
    pub fn bind(
        &self,
        scope: ActivationScope,
        instance: PluginInstanceId,
    ) -> anyhow::Result<(PromptPort, ActionDispatchPort)> {
        anyhow::ensure!(
            scope.instance_id() == instance,
            "frontend activation instance mismatch"
        );
        let (prompt, prompts) = PromptPort::channel(scope.clone(), 16)?;
        let (action, actions) = ActionDispatchPort::channel(scope.clone(), 32)?;
        self.installs
            .try_send(OwnerInboxes {
                scope,
                prompts,
                actions,
            })
            .map_err(|_| anyhow::anyhow!("frontend inbox registration full or disconnected"))?;
        Ok((prompt, action))
    }
}
