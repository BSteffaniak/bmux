//! The smallest plugin: one BPDL command, one implementation for bundled and
//! dynamic packaging.

/// Plugin declaration generated from the BPDL package and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::schema! {
        source: "bpdl/hello-plugin.bpdl",
        manifest: "plugin.toml",
    }
}

use bmux_plugin_sdk::author::{self, Call, Declarations, Plugin};
use declaration::commands::{HELLO, HelloArgs};

#[derive(Default)]
pub struct Hello;

async fn hello(_cx: &Call, args: HelloArgs) -> author::Result<()> {
    println!("Hello, {}!", args.name.as_deref().unwrap_or("world"));
    Ok(())
}

impl Plugin for Hello {
    fn declare(d: &mut Declarations<Self>) {
        d.command(HELLO, hello);
    }
}

bmux_plugin_sdk::plugin!(Hello);

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::ComponentRole;

    #[test]
    fn definition_is_inert_and_backend_only() {
        let definition = plugin_definition();
        assert_eq!(definition.metadata.plugin_id, "example.hello");
        assert_eq!(definition.metadata.roles, vec![ComponentRole::Backend]);
        let manifest = definition.metadata.manifest_toml.unwrap();
        assert!(manifest.contains("name = \"hello\""));
        assert!(manifest.contains("owns_paths = [[\"hello\"]]"));
    }
}
