#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use bmux_ipc::SessionSelector;
use bmux_plugin::{
    DEFAULT_NATIVE_COMMAND_SYMBOL, DEFAULT_NATIVE_COMMAND_WITH_CONTEXT_SYMBOL,
    DEFAULT_NATIVE_DEACTIVATE_SYMBOL,
};
use bmux_plugin::{DEFAULT_NATIVE_ENTRY_SYMBOL, DEFAULT_NATIVE_EVENT_SYMBOL};
use std::ffi::{CStr, c_char};

const DESCRIPTOR: &str = concat!(
    "id = \"example.native\"\n",
    "display_name = \"Example Native Plugin\"\n",
    "plugin_version = \"0.0.1-alpha.0\"\n",
    "description = \"Example in-repo native plugin for bmux\"\n",
    "capabilities = [\"commands\", \"event_subscription\"]\n\n",
    "[[commands]]\n",
    "name = \"hello\"\n",
    "summary = \"Print a hello message\"\n",
    "execution = \"host_callback\"\n\n",
    "[[commands]]\n",
    "name = \"permissions-list\"\n",
    "summary = \"List session permissions through bmux host IPC\"\n",
    "execution = \"host_callback\"\n\n",
    "[[event_subscriptions]]\n",
    "kinds = [\"system\", \"window\"]\n",
    "names = [\"server_started\", \"window_created\"]\n\n",
    "[plugin_api]\n",
    "minimum = \"1.0\"\n\n",
    "[native_abi]\n",
    "minimum = \"1.0\"\n",
    "\0"
);

#[unsafe(no_mangle)]
pub extern "C" fn bmux_plugin_entry_v1() -> *const c_char {
    debug_assert_eq!(DEFAULT_NATIVE_ENTRY_SYMBOL, "bmux_plugin_entry_v1");
    DESCRIPTOR.as_ptr().cast()
}

#[unsafe(no_mangle)]
pub extern "C" fn bmux_plugin_run_command_with_context_v1(context: *const c_char) -> i32 {
    debug_assert_eq!(
        DEFAULT_NATIVE_COMMAND_WITH_CONTEXT_SYMBOL,
        "bmux_plugin_run_command_with_context_v1"
    );
    let Ok(payload) = c_str_to_string(context) else {
        return 2;
    };
    let Ok(context) = serde_json::from_str::<bmux_plugin::NativeCommandContext>(&payload) else {
        return 3;
    };

    match context.command.as_str() {
        "permissions-list" => run_permissions_list(&context),
        "hello" => {
            if context.arguments.is_empty() {
                println!("example.native: hello from bmux plugin");
            } else {
                println!("example.native: hello {}", context.arguments.join(" "));
            }
            0
        }
        _ => 64,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn bmux_plugin_run_command_v1(
    command: *const c_char,
    argc: usize,
    argv: *const *const c_char,
) -> i32 {
    debug_assert_eq!(DEFAULT_NATIVE_COMMAND_SYMBOL, "bmux_plugin_run_command_v1");
    let Ok(command) = c_str_to_string(command) else {
        return 2;
    };
    let Ok(arguments) = c_array_to_vec(argc, argv) else {
        return 2;
    };

    match command.as_str() {
        "hello" => {
            if arguments.is_empty() {
                println!("example.native: hello from bmux plugin");
            } else {
                println!("example.native: hello {}", arguments.join(" "));
            }
            0
        }
        _ => 64,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn bmux_plugin_activate_v1(context: *const c_char) -> i32 {
    let Ok(payload) = c_str_to_string(context) else {
        return 2;
    };
    match serde_json::from_str::<serde_json::Value>(&payload) {
        Ok(value) => {
            let plugin_id = value
                .get("plugin_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            println!("example.native: activated {plugin_id}");
            0
        }
        Err(_) => 3,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn bmux_plugin_deactivate_v1(context: *const c_char) -> i32 {
    debug_assert_eq!(
        DEFAULT_NATIVE_DEACTIVATE_SYMBOL,
        "bmux_plugin_deactivate_v1"
    );
    let Ok(payload) = c_str_to_string(context) else {
        return 2;
    };
    match serde_json::from_str::<serde_json::Value>(&payload) {
        Ok(value) => {
            let plugin_id = value
                .get("plugin_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            println!("example.native: deactivated {plugin_id}");
            0
        }
        Err(_) => 3,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn bmux_plugin_handle_event_v1(event: *const c_char) -> i32 {
    debug_assert_eq!(DEFAULT_NATIVE_EVENT_SYMBOL, "bmux_plugin_handle_event_v1");
    let Ok(payload) = c_str_to_string(event) else {
        return 2;
    };
    match serde_json::from_str::<serde_json::Value>(&payload) {
        Ok(value) => {
            let event_name = value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            println!("example.native: observed event {event_name}");
            0
        }
        Err(_) => 3,
    }
}

fn c_str_to_string(ptr: *const c_char) -> Result<String, ()> {
    if ptr.is_null() {
        return Err(());
    }

    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| ())
}

fn run_permissions_list(context: &bmux_plugin::NativeCommandContext) -> i32 {
    let Some(session) = context.arguments.first() else {
        eprintln!("example.native permissions-list requires a session name or UUID");
        return 64;
    };

    let paths = ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
    );

    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            tokio::task::block_in_place(|| handle.block_on(async_permissions_list(&paths, session)))
        }
        Err(_) => match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(async_permissions_list(&paths, session)),
            Err(_) => 70,
        },
    }
}

async fn async_permissions_list(paths: &ConfigPaths, session: &str) -> i32 {
    let selector = parse_session_selector(session);
    match BmuxClient::connect_with_paths(paths, "example-native-plugin").await {
        Ok(mut client) => match client.list_permissions(selector).await {
            Ok(permissions) => {
                if permissions.is_empty() {
                    println!("example.native: no explicit role assignments");
                } else {
                    println!("example.native permissions:");
                    for permission in permissions {
                        println!(
                            "{} {}",
                            permission.client_id,
                            session_role_name(permission.role)
                        );
                    }
                }
                0
            }
            Err(error) => {
                eprintln!("example.native: failed listing permissions: {error}");
                1
            }
        },
        Err(error) => {
            eprintln!("example.native: failed connecting to bmux host: {error}");
            1
        }
    }
}

fn parse_session_selector(value: &str) -> SessionSelector {
    match uuid::Uuid::parse_str(value) {
        Ok(id) => SessionSelector::ById(id),
        Err(_) => SessionSelector::ByName(value.to_string()),
    }
}

fn session_role_name(role: bmux_ipc::SessionRole) -> &'static str {
    match role {
        bmux_ipc::SessionRole::Owner => "owner",
        bmux_ipc::SessionRole::Writer => "writer",
        bmux_ipc::SessionRole::Observer => "observer",
    }
}

fn c_array_to_vec(argc: usize, argv: *const *const c_char) -> Result<Vec<String>, ()> {
    if argc == 0 {
        return Ok(Vec::new());
    }
    if argv.is_null() {
        return Err(());
    }

    let pointers = unsafe { std::slice::from_raw_parts(argv, argc) };
    pointers.iter().map(|ptr| c_str_to_string(*ptr)).collect()
}

#[cfg(test)]
mod tests {
    use super::{DESCRIPTOR, bmux_plugin_entry_v1};

    #[test]
    fn descriptor_parses_as_native_plugin() {
        let descriptor = DESCRIPTOR.trim_end_matches('\0');
        let parsed = bmux_plugin::NativeDescriptor::from_toml_str(descriptor)
            .expect("example descriptor should parse");
        assert_eq!(parsed.id, "example.native");
        assert_eq!(parsed.commands.len(), 2);
        assert_eq!(parsed.event_subscriptions.len(), 1);
    }

    #[test]
    fn entrypoint_returns_descriptor_pointer() {
        assert!(!bmux_plugin_entry_v1().is_null());
    }
}
