bmux_plugin_schema_macros::schema_inline!(
    r#"
plugin p version 1;

capability I_READ = p.i.read;
capability I_WRITE = p.i.write;

@capability(I_READ)
interface i {
    record r {
        id: uuid,
        name: string?,
    }

    query get(id: uuid) -> r?;
    query mirror(value: r) -> r;
}

@capability(I_WRITE)
interface i-commands {
    command rename(id: uuid, name: string) -> result<unit, string>;
    command mirror(value: i.r) -> i.r;
}
"#
);

fn main() {
    use bmux_plugin_sdk::{PluginContract, TypedServiceEndpoint};
    let local = i::R {
        id: uuid::Uuid::nil(),
        name: None,
    };
    let _: <i::client::MirrorEndpoint as TypedServiceEndpoint>::Request =
        i::client::MirrorRequest {
            value: local.clone(),
        };
    let _: <i_commands::client::MirrorEndpoint as TypedServiceEndpoint>::Request =
        i_commands::client::MirrorRequest { value: local };
    assert_eq!(i::SERVICE_CONTRACT.len(), 32);
    assert_eq!(i_commands::SERVICE_CONTRACT.len(), 32);
    assert_ne!(i::SERVICE_CONTRACT, i_commands::SERVICE_CONTRACT);
    assert_eq!(
        Contract::service_contract_for("i"),
        Some(i::SERVICE_CONTRACT)
    );
    assert_eq!(
        Contract::service_contract_for("i-commands"),
        Some(i_commands::SERVICE_CONTRACT)
    );
    assert_eq!(Contract::service_contract_for("unknown"), None);
    assert_eq!(i::SERVICE_DESCRIPTOR.contract, i::SERVICE_CONTRACT);
}
