//! Facts about where feedback was given, gathered in the attach client when it starts.

use bmux_plugin_sdk::NativeCommandContext;

use crate::declaration::provides::feedback_commands::Facet;
use crate::declaration::requires::{clients_state, contexts_state, pane_runtime_state};

/// The bmux version, the selected session and tab, and the focused pane. Each fact is best
/// effort: one that cannot be read is left out.
pub fn collect(context: &NativeCommandContext) -> Vec<Facet> {
    let mut facets = vec![facet("bmux.version", &context.host.product_version)];
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
    if let Ok(Some(tab)) =
        bmux_plugin::block_on_typed_dispatch(contexts_state::client::current_context(&mut client))
    {
        facets.push(facet("bmux.tab", tab.name.as_deref().unwrap_or("")));
        facets.push(facet("bmux.tab_id", &tab.id.to_string()));
    }
    let session =
        bmux_plugin::block_on_typed_dispatch(clients_state::client::current_client(&mut client))
            .ok()
            .and_then(Result::ok)
            .and_then(|client| client.selected_session_id);
    let Some(session) = session else {
        return facets;
    };
    facets.push(facet("bmux.session_id", &session.to_string()));
    let panes = bmux_plugin::block_on_typed_dispatch(pane_runtime_state::client::list_panes(
        &mut client,
        Some(session),
    ))
    .ok()
    .and_then(Result::ok);
    if let Some(pane) = panes.and_then(|list| list.panes.into_iter().find(|pane| pane.focused)) {
        facets.push(facet("bmux.pane_id", &pane.id.to_string()));
        if let Some(name) = pane.name {
            facets.push(facet("bmux.pane_name", &name));
        }
        facets.extend(command_facets(
            pane.active_command.as_deref().unwrap_or(&pane.shell),
        ));
    }
    facets
}

/// The program the focused pane runs, and its whole command line as a sensitive fact: an
/// argument may be a secret.
fn command_facets(command: &str) -> Vec<Facet> {
    let program = command.split_whitespace().next().unwrap_or_default();
    let program = program.rsplit('/').next().unwrap_or(program);
    if program.is_empty() {
        return Vec::new();
    }
    vec![
        facet("bmux.pane_program", program),
        Facet {
            key: "bmux.pane_command".to_owned(),
            value: command.to_owned(),
            sensitive: true,
        },
    ]
}

fn facet(key: &str, value: &str) -> Facet {
    Facet {
        key: key.to_owned(),
        value: value.to_owned(),
        sensitive: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_is_sensitive_and_the_program_is_not() {
        let facets = command_facets("/usr/bin/psql -h db --password=hunter2");
        assert_eq!(facets[0].key, "bmux.pane_program");
        assert_eq!(facets[0].value, "psql");
        assert!(!facets[0].sensitive);
        assert_eq!(facets[1].key, "bmux.pane_command");
        assert!(facets[1].sensitive);
        assert_eq!(command_facets("  "), Vec::<Facet>::new());
    }
}
