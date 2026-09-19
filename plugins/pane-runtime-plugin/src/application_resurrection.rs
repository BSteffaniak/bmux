//! Invocation-specific client of the optional application-resurrection plugin.
use bmux_application_resurrection_plugin_api::resurrection_launch_v1::client;
use bmux_plugin::{ServiceCallerDispatchClient, TypedServiceCaller, block_on_typed_dispatch};
use std::sync::{Arc, RwLock};

static CALLER: RwLock<Option<Arc<TypedServiceCaller>>> = RwLock::new(None);

pub fn activate(context: &bmux_plugin_sdk::NativeLifecycleContext) {
    if let Ok(mut caller) = CALLER.write() {
        *caller = context
            .enabled_plugins
            .iter()
            .any(|id| id == "bmux.application_resurrection")
            .then(|| Arc::new(TypedServiceCaller::from_lifecycle_context(context)));
    }
}

#[derive(Clone)]
pub struct Launch {
    caller: Arc<TypedServiceCaller>,
    key: String,
    pub endpoint: String,
    pub invocation: String,
}

pub fn prepare(
    key: String,
    restored: bool,
    shell: bool,
    replay: bool,
    expected_invocation: Option<String>,
) -> Result<Option<Launch>, String> {
    let Some(caller) = CALLER.read().map_err(|e| e.to_string())?.clone() else {
        return Ok(None);
    };
    let mut dispatch = ServiceCallerDispatchClient::new(caller.as_ref());
    let prepared = block_on_typed_dispatch(
        bmux_application_resurrection_plugin_api::resurrection_launch_v2::client::prepare(
            &mut dispatch,
            key.clone(),
            restored,
            shell,
            replay,
            expected_invocation,
        ),
    )
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:?}"))?;
    Ok(Some(Launch {
        caller,
        key,
        endpoint: prepared.endpoint,
        invocation: prepared.invocation,
    }))
}

impl Launch {
    pub fn bind(&self, pid: u32) -> Result<(), String> {
        let mut dispatch = ServiceCallerDispatchClient::new(self.caller.as_ref());
        block_on_typed_dispatch(client::bind(
            &mut dispatch,
            self.key.clone(),
            self.endpoint.clone(),
            pid,
        ))
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:?}"))?;
        Ok(())
    }
    pub fn boundary(&self, prompt: bool) -> Result<String, String> {
        let mut dispatch = ServiceCallerDispatchClient::new(self.caller.as_ref());
        block_on_typed_dispatch(
            bmux_application_resurrection_plugin_api::resurrection_launch_v2::client::boundary(
                &mut dispatch,
                self.key.clone(),
                self.endpoint.clone(),
                prompt,
            ),
        )
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:?}"))
    }
    pub fn close(&self) {
        let mut dispatch = ServiceCallerDispatchClient::new(self.caller.as_ref());
        if let Err(error) = block_on_typed_dispatch(client::close(
            &mut dispatch,
            self.key.clone(),
            self.endpoint.clone(),
        )) {
            tracing::warn!(%error, "failed closing resurrection endpoint");
        }
    }
}
