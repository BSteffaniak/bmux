//! Tabs-owned configuration for fresh-shell directory selection.
//!
//! Parsing does not touch the filesystem: paths belong to the execution host.
//! Runtime resolution and explicit command overrides are separate operations.

use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::{self, InitialShellOptions, SpawnError};
use bmux_plugin::ServiceCaller;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CwdSettings {
    #[serde(default)]
    pub new_pane: DirectoryPolicy,
    #[serde(default)]
    pub new_tab: DirectoryPolicy,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DirectoryPolicy {
    #[default]
    RuntimeDefault,
    Home,
    Path {
        path: String,
    },
    SourcePane {
        fallback: DirectoryFallback,
    },
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DirectoryFallback {
    Home,
    RuntimeDefault,
}

impl CwdSettings {
    pub fn parse(settings: Option<&toml::Value>) -> Result<Self, String> {
        let Some(value) = settings.and_then(|settings| settings.get("cwd")) else {
            return Ok(Self::default());
        };
        for operation in ["new_pane", "new_tab"] {
            if let Some(policy) = value.get(operation).and_then(toml::Value::as_table) {
                let source = policy.get("source").and_then(toml::Value::as_str);
                for key in policy.keys() {
                    let allowed = key == "source"
                        || (key == "path" && source == Some("path"))
                        || (key == "fallback" && source == Some("source-pane"));
                    if !allowed {
                        return Err(format!("invalid tabs cwd.{operation} field: {key}"));
                    }
                }
            }
        }
        let settings: Self = value
            .clone()
            .try_into()
            .map_err(|error| format!("invalid tabs cwd settings: {error}"))?;
        settings.new_pane.validate("new_pane")?;
        settings.new_tab.validate("new_tab")?;
        Ok(settings)
    }
}

/// Resolve only the explicitly captured source; never consult runtime-global focus.
pub fn resolve_split_options(
    caller: &(impl ServiceCaller + Sync),
    policy: &DirectoryPolicy,
    session_id: uuid::Uuid,
    target: Option<uuid::Uuid>,
) -> Result<InitialShellOptions, String> {
    let cwd = match policy {
        DirectoryPolicy::RuntimeDefault => None,
        DirectoryPolicy::Path { path } => Some(path.clone()),
        DirectoryPolicy::Home => Some(resolve_home(caller, session_id)?),
        DirectoryPolicy::SourcePane { fallback } => {
            let observation = target
                .map(|pane_id| {
                    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller);
                    let source = bmux_plugin::block_on_typed_dispatch(
                        pane_runtime_spawn_v1::client::capture_source(
                            &mut client,
                            session_id,
                            pane_id,
                        ),
                    )
                    .map_err(|error| format!("capture source service failed: {error}"))?;
                    match source {
                        Ok(source) => bmux_plugin::block_on_typed_dispatch(
                            pane_runtime_spawn_v1::client::observe_directory(&mut client, source),
                        )
                        .map_err(|error| format!("observe directory service failed: {error}")),
                        Err(error) => Ok(Err(error)),
                    }
                })
                .transpose()?;
            match observation {
                Some(Ok(observation)) => Some(observation.path),
                None
                | Some(Err(
                    SpawnError::SourceUnavailable
                    | SpawnError::StaleExecution
                    | SpawnError::ObservationUnavailable
                    | SpawnError::ObservationTimeout,
                )) => match fallback {
                    DirectoryFallback::RuntimeDefault => None,
                    DirectoryFallback::Home => Some(resolve_home(caller, session_id)?),
                },
                Some(Err(error)) => return Err(format!("source directory failed: {error:?}")),
            }
        }
    };
    Ok(InitialShellOptions { cwd })
}

/// Resolve policies when no attachment-local source was captured. Only home
/// resolution needs a session authorization context; fixed paths and the runtime
/// default must remain usable without an existing execution.
pub fn resolve_without_source(
    caller: &(impl ServiceCaller + Sync),
    policy: &DirectoryPolicy,
    session: impl FnOnce() -> Result<uuid::Uuid, String>,
) -> Result<InitialShellOptions, String> {
    let cwd = match policy {
        DirectoryPolicy::Path { path } => Some(path.clone()),
        DirectoryPolicy::RuntimeDefault
        | DirectoryPolicy::SourcePane {
            fallback: DirectoryFallback::RuntimeDefault,
        } => None,
        DirectoryPolicy::Home
        | DirectoryPolicy::SourcePane {
            fallback: DirectoryFallback::Home,
        } => Some(resolve_home(caller, session()?)?),
    };
    Ok(InitialShellOptions { cwd })
}

/// Explain through the same read-only resolver used by creation. This does not
/// claim that a resolved path has passed execution-side startup validation.
pub fn explain(
    caller: &(impl ServiceCaller + Sync),
    operation: &str,
    policy: &DirectoryPolicy,
    source: Option<(uuid::Uuid, uuid::Uuid)>,
    session: impl FnOnce() -> Result<uuid::Uuid, String>,
) -> serde_json::Value {
    let result = if let Some((session_id, pane_id)) = source {
        resolve_split_options(caller, policy, session_id, Some(pane_id))
    } else {
        resolve_without_source(caller, policy, session)
    };
    serde_json::json!({
        "operation": operation,
        "policy": policy,
        "source": source.map(|(session, pane)| serde_json::json!({"session_id": session, "pane_id": pane})),
        "cwd": result.as_ref().ok().and_then(|options| options.cwd.as_deref()),
        "error": result.err(),
        "creation_validated": false,
    })
}

fn resolve_home(
    caller: &(impl ServiceCaller + Sync),
    session_id: uuid::Uuid,
) -> Result<String, String> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller);
    bmux_plugin::block_on_typed_dispatch(pane_runtime_spawn_v1::client::home_directory(
        &mut client,
        session_id,
    ))
    .map_err(|error| format!("execution home service failed: {error}"))?
    .map_err(|error| format!("execution home unavailable: {error:?}"))
}

impl DirectoryPolicy {
    pub fn explicit_path(path: String) -> Result<Self, String> {
        let policy = Self::Path { path };
        policy.validate("explicit")?;
        Ok(policy)
    }

    fn validate(&self, operation: &str) -> Result<(), String> {
        if let Self::Path { path } = self
            && (!Path::new(path).is_absolute() || path.contains('\0'))
        {
            return Err(format!(
                "tabs cwd.{operation}.path must be an absolute path without NUL bytes"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explanation_reuses_headless_resolution_without_side_effects() {
        let report = explain(
            &NoCalls,
            "pane",
            &DirectoryPolicy::SourcePane {
                fallback: DirectoryFallback::RuntimeDefault,
            },
            None,
            || panic!("headless runtime fallback needs no selection query"),
        );
        assert!(report["cwd"].is_null());
        assert!(report["error"].is_null());
        assert_eq!(report["creation_validated"], false);
        assert_eq!(report["policy"]["source"], "source-pane");
        assert_eq!(report["policy"]["fallback"], "runtime-default");
    }

    #[test]
    fn source_independent_policies_do_not_require_an_existing_session() {
        for (policy, expected) in [
            (
                DirectoryPolicy::Path {
                    path: "/execution/project".into(),
                },
                Some("/execution/project"),
            ),
            (DirectoryPolicy::RuntimeDefault, None),
            (
                DirectoryPolicy::SourcePane {
                    fallback: DirectoryFallback::RuntimeDefault,
                },
                None,
            ),
        ] {
            let options = resolve_without_source(&NoCalls, &policy, || {
                panic!("must not resolve an unrelated session")
            })
            .unwrap();
            assert_eq!(options.cwd.as_deref(), expected);
        }
    }

    #[test]
    fn explicit_directory_rejects_invalid_paths_without_local_existence_check() {
        assert!(DirectoryPolicy::explicit_path("relative".into()).is_err());
        assert!(DirectoryPolicy::explicit_path("/invalid\0path".into()).is_err());
        assert_eq!(
            DirectoryPolicy::explicit_path("/execution-only/path".into()).unwrap(),
            DirectoryPolicy::Path {
                path: "/execution-only/path".into()
            }
        );
    }

    struct NoCalls;

    impl ServiceCaller for NoCalls {
        fn execute_kernel_request(
            &self,
            _request: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            panic!("directory policy must not bypass generated services")
        }

        fn call_service_raw(
            &self,
            _capability: &str,
            _kind: bmux_plugin_sdk::ServiceKind,
            _interface: &str,
            _operation: &str,
            _payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            panic!("no source must not query global focus or mutate resources")
        }
    }

    #[test]
    fn headless_source_falls_back_without_querying_global_focus() {
        let policy = DirectoryPolicy::SourcePane {
            fallback: DirectoryFallback::RuntimeDefault,
        };
        assert_eq!(
            resolve_split_options(&NoCalls, &policy, uuid::Uuid::nil(), None)
                .unwrap()
                .cwd,
            None
        );
    }

    #[test]
    fn fixed_directory_is_forwarded_without_local_filesystem_lookup() {
        let path = "/execution-host-only/project".to_string();
        let policy = DirectoryPolicy::Path { path: path.clone() };
        assert_eq!(
            resolve_split_options(&NoCalls, &policy, uuid::Uuid::nil(), None)
                .unwrap()
                .cwd,
            Some(path)
        );
    }

    struct HomeAuthority;
    impl ServiceCaller for HomeAuthority {
        fn execute_kernel_request(
            &self,
            _: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            panic!("home must use generated transport")
        }
        fn call_service_raw(
            &self,
            capability: &str,
            _: bmux_plugin_sdk::ServiceKind,
            interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            assert_eq!(capability, "bmux.pane_runtime.spawn.v1");
            assert_eq!(interface, "pane-runtime-spawn-v1");
            assert_eq!(operation, "home-directory");
            let request: pane_runtime_spawn_v1::client::HomeDirectoryRequest =
                bmux_plugin_sdk::decode_service_message(&payload)?;
            assert_eq!(request.session_id, uuid::Uuid::nil());
            bmux_plugin_sdk::encode_service_message(&Ok::<String, SpawnError>(
                "/execution-authority/home".into(),
            ))
        }
    }

    #[test]
    fn home_and_headless_home_fallback_use_execution_authority() {
        for policy in [
            DirectoryPolicy::Home,
            DirectoryPolicy::SourcePane {
                fallback: DirectoryFallback::Home,
            },
        ] {
            assert_eq!(
                resolve_split_options(&HomeAuthority, &policy, uuid::Uuid::nil(), None)
                    .unwrap()
                    .cwd
                    .as_deref(),
                Some("/execution-authority/home")
            );
        }
    }

    fn parse(value: &str) -> Result<CwdSettings, String> {
        CwdSettings::parse(Some(&toml::from_str::<toml::Value>(value).unwrap()))
    }

    #[test]
    fn absent_settings_preserve_runtime_default_independently() {
        assert_eq!(CwdSettings::parse(None).unwrap(), CwdSettings::default());
        let settings = parse("[cwd.new_pane]\nsource = 'home'").unwrap();
        assert_eq!(settings.new_pane, DirectoryPolicy::Home);
        assert_eq!(settings.new_tab, DirectoryPolicy::RuntimeDefault);
    }

    #[test]
    fn pane_and_tab_policies_are_independent() {
        let settings = parse(
            "[cwd.new_pane]\nsource = 'source-pane'\nfallback = 'home'\n\
             [cwd.new_tab]\nsource = 'path'\npath = '/work/project'",
        )
        .unwrap();
        assert_eq!(
            settings.new_pane,
            DirectoryPolicy::SourcePane {
                fallback: DirectoryFallback::Home
            }
        );
        assert_eq!(
            settings.new_tab,
            DirectoryPolicy::Path {
                path: "/work/project".into()
            }
        );
    }

    #[test]
    fn inherited_directory_requires_an_explicit_nonrecursive_fallback() {
        for value in [
            "source = 'source-pane'",
            "source = 'source-pane'\nfallback = 'source-pane'",
            "source = 'source-pane'\nfallback = 'path'",
        ] {
            assert!(parse(&format!("[cwd.new_pane]\n{value}")).is_err());
        }
        assert!(
            parse("[cwd.new_pane]\nsource = 'source-pane'\nfallback = 'runtime-default'").is_ok()
        );
    }

    #[test]
    fn invalid_or_ambiguous_settings_are_not_silently_ignored() {
        for value in [
            "source = 'unknown'",
            "source = 'path'",
            "source = 'path'\npath = 'relative'",
            "source = 'path'\npath = '~/project'",
            "source = 'path'\npath = '$HOME/project'",
            "source = 'home'\npath = '/ignored'",
            "source = 'home'\nfallback = 'runtime-default'",
            "source = 'runtime-default'\nextra = true",
        ] {
            assert!(
                parse(&format!("[cwd.new_tab]\n{value}")).is_err(),
                "{value}"
            );
        }
        assert!(parse("[cwd.new_workspace]\nsource = 'home'").is_err());
    }

    #[test]
    fn path_existence_is_not_checked_on_the_config_reading_host() {
        assert!(parse("[cwd.new_tab]\nsource = 'path'\npath = '/execution-host-only'").is_ok());
    }
}
