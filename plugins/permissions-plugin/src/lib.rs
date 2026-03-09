#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use bmux_ipc::{SessionPermissionSummary, SessionRole, SessionSelector};
use bmux_plugin::{
    DEFAULT_NATIVE_ACTIVATE_SYMBOL, DEFAULT_NATIVE_COMMAND_WITH_CONTEXT_SYMBOL,
    DEFAULT_NATIVE_DEACTIVATE_SYMBOL, DEFAULT_NATIVE_ENTRY_SYMBOL, DEFAULT_NATIVE_EVENT_SYMBOL,
};
use std::ffi::{CStr, c_char};

const DESCRIPTOR: &str = concat!(
    "id = \"bmux.permissions\"\n",
    "display_name = \"bmux Permissions\"\n",
    "plugin_version = \"0.0.1-alpha.0\"\n",
    "description = \"Shipped bmux permissions command plugin\"\n",
    "capabilities = [\"commands\"]\n\n",
    "[[commands]]\n",
    "name = \"permissions\"\n",
    "path = [\"permissions\"]\n",
    "summary = \"List explicit role assignments for a session\"\n",
    "execution = \"host_callback\"\n",
    "expose_in_cli = false\n\n",
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
        "permissions" => run_permissions_command(&context),
        _ => 64,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn bmux_plugin_activate_v1(context: *const c_char) -> i32 {
    debug_assert_eq!(DEFAULT_NATIVE_ACTIVATE_SYMBOL, "bmux_plugin_activate_v1");
    match c_str_to_string(context) {
        Ok(_) => 0,
        Err(_) => 2,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn bmux_plugin_deactivate_v1(context: *const c_char) -> i32 {
    debug_assert_eq!(
        DEFAULT_NATIVE_DEACTIVATE_SYMBOL,
        "bmux_plugin_deactivate_v1"
    );
    match c_str_to_string(context) {
        Ok(_) => 0,
        Err(_) => 2,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn bmux_plugin_handle_event_v1(event: *const c_char) -> i32 {
    debug_assert_eq!(DEFAULT_NATIVE_EVENT_SYMBOL, "bmux_plugin_handle_event_v1");
    match c_str_to_string(event) {
        Ok(_) => 0,
        Err(_) => 2,
    }
}

fn run_permissions_command(context: &bmux_plugin::NativeCommandContext) -> i32 {
    let mut session = None;
    let mut as_json = false;
    let mut watch = false;
    let mut iter = context.arguments.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--session" => session = iter.next().cloned(),
            "--json" => as_json = true,
            "--watch" => watch = true,
            _ => return 64,
        }
    }
    let Some(session) = session else {
        eprintln!("permissions requires --session <name-or-uuid>");
        return 64;
    };

    let paths = ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
    );

    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| {
            handle.block_on(async_permissions_command(&paths, &session, as_json, watch))
        }),
        Err(_) => match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => {
                runtime.block_on(async_permissions_command(&paths, &session, as_json, watch))
            }
            Err(_) => 70,
        },
    }
}

async fn async_permissions_command(
    paths: &ConfigPaths,
    session: &str,
    as_json: bool,
    watch: bool,
) -> i32 {
    let selector = parse_session_selector(session);
    let mut client = match BmuxClient::connect_with_paths(paths, "bmux-permissions-plugin").await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("failed connecting to bmux host: {error}");
            return 1;
        }
    };

    if watch {
        println!("watching permissions for session '{session}' (Ctrl-C to stop)");
        let mut last_permissions: Option<Vec<SessionPermissionSummary>> = None;
        loop {
            match client.list_permissions(selector.clone()).await {
                Ok(permissions) => {
                    if last_permissions.as_ref() != Some(&permissions) {
                        render_permissions(&permissions, false);
                        last_permissions = Some(permissions);
                    }
                }
                Err(error) => {
                    eprintln!("failed listing permissions: {error}");
                    return 1;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
    }

    match client.list_permissions(selector).await {
        Ok(permissions) => {
            render_permissions(&permissions, as_json);
            0
        }
        Err(error) => {
            eprintln!("failed listing permissions: {error}");
            1
        }
    }
}

fn render_permissions(permissions: &[SessionPermissionSummary], as_json: bool) {
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(permissions).expect("permissions json should encode")
        );
        return;
    }

    if permissions.is_empty() {
        println!("no explicit role assignments");
        return;
    }

    println!("CLIENT_ID                            ROLE");
    for permission in permissions {
        println!(
            "{:<36} {}",
            permission.client_id,
            role_label(permission.role)
        );
    }
}

fn role_label(role: SessionRole) -> &'static str {
    match role {
        SessionRole::Owner => "owner",
        SessionRole::Writer => "writer",
        SessionRole::Observer => "observer",
    }
}

fn parse_session_selector(value: &str) -> SessionSelector {
    match uuid::Uuid::parse_str(value) {
        Ok(id) => SessionSelector::ById(id),
        Err(_) => SessionSelector::ByName(value.to_string()),
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

#[cfg(test)]
mod tests {
    use super::{DESCRIPTOR, bmux_plugin_entry_v1};

    #[test]
    fn descriptor_parses() {
        let descriptor =
            bmux_plugin::NativeDescriptor::from_toml_str(DESCRIPTOR.trim_end_matches('\0'))
                .expect("descriptor should parse");
        assert_eq!(descriptor.id, "bmux.permissions");
        assert_eq!(descriptor.commands.len(), 1);
        assert!(!descriptor.commands[0].expose_in_cli);
    }

    #[test]
    fn entrypoint_returns_pointer() {
        assert!(!bmux_plugin_entry_v1().is_null());
    }
}
