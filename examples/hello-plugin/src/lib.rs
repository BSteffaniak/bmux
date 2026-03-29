use bmux_plugin_sdk::prelude::*;

#[derive(Default)]
pub struct HelloPlugin;

impl RustPlugin for HelloPlugin {
    fn run_command(&mut self, ctx: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(ctx, {
            "hello" => {
                let name = ctx.arguments.first().map_or("world", String::as_str);
                println!("Hello, {name}!");
                Ok(EXIT_OK)
            },
        })
    }
}

bmux_plugin_sdk::export_plugin!(HelloPlugin, include_str!("../plugin.toml"));
