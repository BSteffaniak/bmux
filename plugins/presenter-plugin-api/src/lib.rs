//! Stable BPDL-generated presentation services. No runtime state or IO lives here.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/presenter-plugin.bpdl",
}

#[cfg(test)]
mod tests {
    use bmux_plugin_sdk::PluginContract;

    #[test]
    fn separates_viewer_and_management_authority() {
        let declarations = super::Contract::service_declarations().unwrap();
        assert_eq!(declarations.len(), 3);
        let debug = format!("{declarations:?}");
        assert!(debug.contains("bmux.presenter.view.v2"));
        assert!(debug.contains("bmux.presenter.manage.v1"));
        assert!(debug.contains("bmux.presenter.manage.v2"));
    }
}
