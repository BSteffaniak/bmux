#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Feedback to Bcode about what is on screen.
//!
//! `give` (bound to `shift+b` in runtime mode) notes when the person started, gathers facts
//! about the focused pane and opens a draft in the server, which marks that moment in the
//! rolling recording. The person writes the feedback in a prompt; the server sends it to the
//! Bcode feedback store and its intake session through Bcode's session bridge
//! (`bcode_session_bridge_runtime`), and reports the outcome as a notification: saved, and
//! whether triage started, is queued for Bcode to retry on its own, or is blocked.
//!
//! While the intake agent triages, it may call two tools this plugin offers in the intake
//! session: `bmux.recording_timeline`, which says which span of the rolling recording is
//! still available around the feedback, and `bmux.capture_recording`, which cuts the span
//! it chooses (without typed input) and attaches it to the feedback item.
//!
//! The Bcode connection starts with the first submission, so bmux never starts Bcode unless
//! feedback is given, and works on without it. With the `embedded-bcode` feature, Bcode runs
//! inside the bmux server unless an installed one is named (`BCODE_BIN` or the `bcode`
//! setting) or selected (`runtime = "installed"`).

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::schema! {
        source: "bpdl/feedback-plugin.bpdl",
        manifest: "plugin.toml",
        imports: {
            clients: {
                schema: ::bmux_clients_plugin_api::export_schema,
                crate_path: ::bmux_clients_plugin_api,
            },
            contexts: {
                schema: ::bmux_contexts_plugin_api::export_schema,
                crate_path: ::bmux_contexts_plugin_api,
            },
            pane: {
                schema: ::bmux_pane_runtime_plugin_api::export_schema,
                crate_path: ::bmux_pane_runtime_plugin_api,
            },
            recording: {
                schema: ::bmux_recording_plugin_api::export_schema,
                crate_path: ::bmux_recording_plugin_api,
            },
            tabbar: {
                schema: ::bmux_tab_bar_plugin_api::export_schema,
                crate_path: ::bmux_tab_bar_plugin_api,
            },
        },
    }
}

mod facets;
mod server;
mod settings;
mod tools;

use std::sync::{Arc, Mutex, PoisonError};

use bmux_plugin::prompt::{
    PromptPolicy, PromptRequest, PromptResponse, PromptValidation, PromptValue,
};
use bmux_plugin_sdk::author::{self, Activation, Call, Deactivation, Declarations, Plugin};
use bmux_plugin_sdk::definition::ComponentRole;
use bmux_plugin_sdk::prelude::*;
use declaration::provides::feedback_commands;

/// The application id feedback from bmux carries in Bcode, and the namespace of its tools.
pub const APP_ID: &str = "bmux";

/// How long an embedded Bcode gets to stop when the server's plugin deactivates.
#[cfg(feature = "embedded-bcode")]
const EMBEDDED_STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The feedback plugin.
#[derive(Default)]
pub struct FeedbackPlugin {
    /// The server instance's drafts and submission queue.
    server: Mutex<Option<Arc<server::Server>>>,
}

impl FeedbackPlugin {
    fn server(&self) -> author::Result<Arc<server::Server>> {
        self.server
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| PluginCommandError::unavailable("feedback is served by the bmux server"))
    }

    /// In the server instance, keep drafts and start the worker that holds the Bcode
    /// connection; other instances only run commands.
    fn serve(&self, cx: &Activation) -> author::Result<()> {
        let scope = &cx.lifecycle().scope;
        if scope.role != ComponentRole::Backend || scope.cli_command.is_some() {
            return Ok(());
        }
        let settings =
            settings::Settings::parse(cx.settings()).map_err(PluginCommandError::failed)?;
        let (server, jobs) = server::Server::new();
        cx.spawn("feedback-bridge", server::run(cx.host()?, settings, jobs))?;
        *self.server.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(server));
        Ok(())
    }

    /// `give`: open a draft now, then ask for the feedback without blocking the attach loop.
    fn give(context: &NativeCommandContext) -> author::Result<i32> {
        let opened_epoch_ms = server::epoch_millis();
        let facets = facets::collect(context);
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
        let draft = bmux_plugin::block_on_typed_dispatch(feedback_commands::client::begin(
            &mut client,
            opened_epoch_ms,
            facets,
        ))
        .map_err(|error| {
            PluginCommandError::unavailable(format!("feedback is unavailable: {error}"))
        })?;
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            PluginCommandError::unavailable("feedback needs the attach client's runtime")
        })?;
        // The prompt answers after this command returns, when its host route is gone, so the
        // answer is sent as a command of its own.
        handle.spawn(ask(draft));
        Ok(EXIT_OK)
    }

    /// `submit --draft N --text T` and `discard --draft N`.
    fn finish(context: &NativeCommandContext) -> author::Result<i32> {
        let draft = option_value(&context.arguments, "draft")
            .and_then(|draft| draft.parse::<u64>().ok())
            .ok_or_else(|| PluginCommandError::invalid_arguments("--draft takes a draft id"))?;
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
        let known = if context.command == "submit" {
            let text = option_value(&context.arguments, "text")
                .ok_or_else(|| PluginCommandError::invalid_arguments("--text is required"))?;
            bmux_plugin::block_on_typed_dispatch(feedback_commands::client::submit(
                &mut client,
                draft,
                text,
            ))
        } else {
            bmux_plugin::block_on_typed_dispatch(feedback_commands::client::discard(
                &mut client,
                draft,
            ))
        }
        .map_err(|error| {
            PluginCommandError::unavailable(format!("feedback is unavailable: {error}"))
        })?;
        if !known {
            return Err(PluginCommandError::failed(format!(
                "feedback draft {draft} expired; give the feedback again"
            )));
        }
        Ok(EXIT_OK)
    }
}

/// Ask for the feedback of `draft`, then submit or discard it.
async fn ask(draft: u64) {
    let request = PromptRequest::text_input("Feedback for Bcode")
        .message("What should Bcode know about what is on screen?")
        .submit_label("Send")
        .owner_plugin_id("bmux.feedback")
        .modal_id("feedback")
        .policy(PromptPolicy::Enqueue)
        .input_required(true)
        .input_validation(PromptValidation::NonEmpty);
    let action = match bmux_plugin::prompt::request(request).await {
        Ok(PromptResponse::Submitted(PromptValue::Text(text))) => submit_action(draft, &text),
        Ok(_) => format!("plugin:bmux.feedback:discard --draft {draft}"),
        Err(error) => {
            tracing::warn!(%error, "bmux.feedback: the feedback prompt did not open");
            format!("plugin:bmux.feedback:discard --draft {draft}")
        }
    };
    if let Err(error) = bmux_plugin::action_dispatch::dispatch(action) {
        tracing::warn!(%error, "bmux.feedback: could not finish the feedback draft");
    }
}

/// The keymap action submitting `text` for `draft`, quoted for the action parser.
fn submit_action(draft: u64, text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for ch in text.chars() {
        if matches!(ch, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    format!("plugin:bmux.feedback:submit --draft {draft} --text {quoted}")
}

/// The value of `--name VALUE` or `--name=VALUE`.
fn option_value(arguments: &[String], name: &str) -> Option<String> {
    let long = format!("--{name}");
    let prefix = format!("--{name}=");
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        if let Some(value) = argument.strip_prefix(&prefix) {
            return Some(value.to_owned());
        }
        if *argument == long {
            return arguments.next().cloned();
        }
    }
    None
}

impl feedback_commands::v1::FeedbackCommands for FeedbackPlugin {
    async fn begin(
        &self,
        cx: &Call,
        opened_epoch_ms: u64,
        facets: Vec<feedback_commands::Facet>,
    ) -> author::Result<u64> {
        let server = self.server()?;
        let draft = server.open(opened_epoch_ms, facets);
        if let Some(host) = cx.host() {
            server::mark_opened(host, draft, opened_epoch_ms).await;
        }
        Ok(draft)
    }

    fn submit(
        &self,
        _cx: &Call,
        draft: u64,
        text: String,
    ) -> impl std::future::Future<Output = author::Result<bool>> + Send {
        std::future::ready(self.server().map(|server| server.submit(draft, text)))
    }

    fn discard(
        &self,
        _cx: &Call,
        draft: u64,
    ) -> impl std::future::Future<Output = author::Result<bool>> + Send {
        std::future::ready(self.server().map(|server| server.discard(draft)))
    }
}

impl Plugin for FeedbackPlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.provide::<dyn feedback_commands::v1::FeedbackCommands>();
        d.native_command(
            "give",
            |_: &Self, _cx: &Call, context: NativeCommandContext| {
                std::future::ready(Self::give(&context))
            },
        );
        for command in ["submit", "discard"] {
            d.native_command(
                command,
                |_: &Self, _cx: &Call, context: NativeCommandContext| {
                    std::future::ready(Self::finish(&context))
                },
            );
        }
    }

    fn activate(
        &self,
        cx: &mut Activation,
    ) -> impl std::future::Future<Output = author::Result<()>> + Send {
        std::future::ready(self.serve(cx))
    }

    fn deactivate(
        &self,
        _cx: &mut Deactivation,
    ) -> impl std::future::Future<Output = author::Result<()>> + Send {
        let served = self
            .server
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        async move {
            // An embedded Bcode releases its sessions and plugins before the server exits.
            #[cfg(feature = "embedded-bcode")]
            if served.is_some() {
                bcode_session_bridge_embedded::shutdown(EMBEDDED_STOP_WAIT).await;
            }
            drop(served);
            Ok(())
        }
    }
}

bmux_plugin_sdk::plugin!(FeedbackPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submissions_quote_the_text_for_the_action_parser() {
        assert_eq!(
            submit_action(7, r#"the "pane" \ froze"#),
            r#"plugin:bmux.feedback:submit --draft 7 --text "the \"pane\" \\ froze""#
        );
    }

    #[test]
    fn options_are_read_in_both_spellings() {
        let arguments = ["--draft", "3", "--text=hi there"].map(String::from);
        assert_eq!(option_value(&arguments, "draft").as_deref(), Some("3"));
        assert_eq!(
            option_value(&arguments, "text").as_deref(),
            Some("hi there")
        );
        assert_eq!(option_value(&arguments, "missing"), None);
    }

    #[test]
    fn the_definition_matches_its_declaration() {
        let definition = plugin_definition();
        assert_eq!(definition.metadata.plugin_id, "bmux.feedback");
        definition
            .validate()
            .expect("embedded declaration must match metadata");
    }
}
