//! Author-facing contract surface consumed by BPDL-generated bindings.
//!
//! `schema!` generates, for every provided service interface, a module
//! `<interface>::v<N>` holding:
//!
//! - an async provider trait (`counter::v1::Counter`) whose methods take a
//!   [`Call`] and return [`Result`];
//! - a typed [`Client`](self) over any transport (`counter::v1::Client`);
//! - a dispatch adapter that serves a provider through today's
//!   `PluginInstance::invoke` path, a portable adapter for checked frames, and
//!   a typed-registry registration for the bundled direct-call fast path;
//! - marker impls of [`ProvidedInterface`], [`Provides`] and
//!   [`RequiredInterface`] that a declaration builder uses to type-check
//!   `provide::<dyn counter::v1::Counter>()` and
//!   `require::<counter::v1::Client>()`.
//!
//! Package-level commands and extension points generate [`CommandDecl`]
//! constants with typed [`CommandArgs`], and [`ExtensionPoint`] types with a
//! behavior trait and a wire [`Extends`] adapter.
//!
//! Everything here is host-agnostic: no product types, no globals, and no
//! runtime. See `docs/plugin-author-api.md` for how the `Plugin` trait,
//! `Declarations` and `plugin!` consume these items.

use std::marker::PhantomData;
use std::sync::Arc;

use crate::definition::{PluginCallContext, PluginHostServices};
use crate::{
    NativeServiceContext, PluginCommandError, PluginEventKind, PortableServiceProvider,
    ServiceCancellation, ServiceInterfaceDescriptor, TypedServiceRegistry,
};

/// Result of an author handler.
pub type Result<T> = std::result::Result<T, Error>;

/// Author handler failure: an exit code and a message carried to the caller.
pub type Error = PluginCommandError;

/// Boxed `Send` future used where generated bindings need type erasure.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Per-call context handed to every generated provider method.
///
/// It identifies the invocation and caller when the call arrived through the
/// host, and carries the provider's event emitter. A direct in-process call
/// with no host invocation uses [`Call::detached`].
#[derive(Clone, Default)]
pub struct Call {
    invocation: Option<PluginCallContext>,
    caller_plugin_id: Option<String>,
    caller_client_id: Option<uuid::Uuid>,
    events: Option<Arc<dyn EventEmitter>>,
}

impl std::fmt::Debug for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Call")
            .field(
                "invocation",
                &self.invocation.as_ref().map(|c| &c.invocation_id),
            )
            .field("caller_plugin_id", &self.caller_plugin_id)
            .field("caller_client_id", &self.caller_client_id)
            .field("events", &self.events.is_some())
            .finish()
    }
}

impl Call {
    /// A call with no host invocation (direct typed call or test).
    #[must_use]
    pub fn detached() -> Self {
        Self::default()
    }

    /// A call admitted by the host for one service invocation.
    #[must_use]
    pub fn from_service(call: PluginCallContext, context: &NativeServiceContext) -> Self {
        Self {
            invocation: Some(call),
            caller_plugin_id: Some(context.request.caller_plugin_id.clone())
                .filter(|id| !id.is_empty()),
            caller_client_id: context.caller_client_id,
            events: None,
        }
    }

    /// Attach the emitter that [`Call::emit`] publishes through.
    #[must_use]
    pub fn with_events(mut self, events: Arc<dyn EventEmitter>) -> Self {
        self.events = Some(events);
        self
    }

    /// Host invocation authority, when the call came through the host.
    #[must_use]
    pub const fn invocation(&self) -> Option<&PluginCallContext> {
        self.invocation.as_ref()
    }

    /// Plugin that issued the call, when known.
    #[must_use]
    pub fn caller_plugin_id(&self) -> Option<&str> {
        self.caller_plugin_id.as_deref()
    }

    /// Client on whose behalf the call runs, when known.
    #[must_use]
    pub const fn caller_client_id(&self) -> Option<uuid::Uuid> {
        self.caller_client_id
    }

    /// Cancellation of the host invocation, when the call came through the host.
    #[must_use]
    pub fn cancellation(&self) -> Option<&ServiceCancellation> {
        self.invocation.as_ref().map(|call| &call.cancellation)
    }

    /// Host services for onward calls under this invocation's authority.
    #[must_use]
    pub fn host(&self) -> Option<&PluginHostServices> {
        self.invocation
            .as_ref()
            .and_then(|call| call.host_services.as_ref())
    }

    /// Publish a contract event on its generated channel.
    ///
    /// # Errors
    /// Fails when no emitter is attached, the payload cannot be encoded, or
    /// the emitter rejects it.
    // Taking the event by value mirrors publishing it: the caller hands it off.
    #[allow(clippy::needless_pass_by_value)]
    pub fn emit<E: ContractEvent>(&self, event: E) -> Result<()> {
        let events = self.events.as_ref().ok_or_else(|| {
            Error::unavailable(format!(
                "no event emitter is attached for '{}'",
                E::KIND.as_str()
            ))
        })?;
        let payload = crate::encode_service_message(&event)
            .map_err(|error| Error::failed(error.to_string()))?;
        events.emit(&E::KIND, E::DELIVERY, payload)
    }
}

/// How a contract event channel delivers values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDelivery {
    /// Every subscriber receives every value.
    Broadcast,
    /// Retained state: new subscribers receive the latest value first.
    State,
}

/// Publishes encoded contract events. The host or a test supplies it.
pub trait EventEmitter: Send + Sync {
    /// # Errors
    /// Returns an error when the event cannot be delivered.
    fn emit(&self, kind: &PluginEventKind, delivery: EventDelivery, payload: Vec<u8>)
    -> Result<()>;
}

/// An event payload bound to its BPDL interface's channel.
pub trait ContractEvent: serde::Serialize + Send + 'static {
    /// Namespaced event channel (`<plugin.id>/<interface>`).
    const KIND: PluginEventKind;
    /// Broadcast or retained state.
    const DELIVERY: EventDelivery;
}

/// A BPDL service interface a plugin can provide. Implemented by the
/// generated `dyn <Interface>` marker type.
pub trait ProvidedInterface {
    /// Generated service descriptor (capability, kind, interface id, contract).
    const DESCRIPTOR: ServiceInterfaceDescriptor;
}

/// `P` implements the provided interface `Self` (`dyn counter::v1::Counter`).
/// A declaration of `provide::<dyn I>()` for a plugin that does not implement
/// `I` fails to compile.
///
/// Calls that arrive without host invocation context (direct bundled calls
/// and portable frames) run with `base`: the provider-bound [`Call`] that
/// carries its event emitter.
pub trait Provides<P>: ProvidedInterface {
    /// Byte-level checked adapter for dylib and cross-process callers.
    fn portable(provider: Arc<P>, base: Call) -> Arc<dyn PortableServiceProvider>;

    /// Register `provider` for the bundled direct typed-call fast path,
    /// together with its portable adapter.
    ///
    /// # Errors
    /// Returns an error when the registry rejects the descriptor.
    fn register(
        provider: Arc<P>,
        base: Call,
        registry: &mut TypedServiceRegistry,
    ) -> crate::Result<()>;

    /// Serve one host invocation of this interface through `provider`.
    fn dispatch(
        provider: &P,
        call: Call,
        context: NativeServiceContext,
    ) -> BoxFuture<'_, Result<crate::ServiceResponse>>;
}

/// A typed client for a BPDL service interface a plugin consumes.
pub trait RequiredInterface {
    /// Generated service descriptor of the consumed interface.
    const DESCRIPTOR: ServiceInterfaceDescriptor;
}

/// Typed arguments of a BPDL plugin command, parsed from its CLI arguments.
pub trait CommandArgs: Sized {
    /// Parse `--name value` style arguments as declared in the manifest.
    ///
    /// # Errors
    /// Returns an invalid-arguments error naming the offending argument.
    fn parse(arguments: &[String]) -> Result<Self>;
}

/// A BPDL plugin command with typed arguments.
pub struct CommandDecl<A> {
    name: &'static str,
    args: PhantomData<fn() -> A>,
}

impl<A> CommandDecl<A> {
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            args: PhantomData,
        }
    }

    /// Command name as declared in BPDL and the manifest.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

impl<A: CommandArgs> CommandDecl<A> {
    /// Parse this command's arguments.
    ///
    /// # Errors
    /// Returns an invalid-arguments error.
    pub fn parse(&self, arguments: &[String]) -> Result<A> {
        A::parse(arguments)
    }
}

impl<A> Clone for CommandDecl<A> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<A> Copy for CommandDecl<A> {}

impl<A> std::fmt::Debug for CommandDecl<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CommandDecl").field(&self.name).finish()
    }
}

/// A typed extension point declared in BPDL (`extension-point p version N`).
pub trait ExtensionPoint {
    /// Stable id, `<package>.<name>/v<version>`.
    const ID: &'static str;
    /// Extension point version.
    const VERSION: u32;
    /// Declarative data every contribution supplies, readable before activation.
    type Data: serde::Serialize + serde::de::DeserializeOwned + Send + 'static;
}

/// `B` implements the behavior of extension point `Self`.
pub trait Extends<B>: ExtensionPoint {
    /// Decode one wire request, run the behavior and encode its response.
    fn dispatch<'a>(
        behavior: &'a B,
        call: &'a Call,
        payload: &'a [u8],
    ) -> BoxFuture<'a, Result<Vec<u8>>>;
}

/// Plugin metadata derived from a generated manifest.
///
/// Id, version, component roles, required capabilities and contracts all
/// come from the manifest, so a definition can never disagree with its
/// declaration.
///
/// `services` are the generated descriptors of the provided interfaces
/// (`declaration::SERVICE_CONTRACTS`). Their contract identities are embedded
/// as `modeled_service_contracts`, the evidence a host needs to route checked
/// frames to a dylib. Descriptors without evidence (unresolved imported wire
/// types) keep routing but are never advertised.
///
/// # Errors
/// Rejects a manifest that is not valid TOML or lacks id, version or roles.
pub fn metadata(
    manifest: &str,
    services: &[ServiceInterfaceDescriptor],
) -> Result<crate::definition::PluginMetadata> {
    let mut value: toml::Table =
        toml::from_str(manifest).map_err(|error| Error::failed(error.to_string()))?;
    let modeled = services
        .iter()
        .filter(|descriptor| !descriptor.contract.is_empty())
        .map(|descriptor| crate::definition::DeclaredServiceContract {
            capability: descriptor.capability.as_str().to_owned(),
            kind: descriptor.kind,
            interface_id: descriptor.interface_id.as_str().to_owned(),
            contract: descriptor.contract.to_vec(),
        })
        .collect::<Vec<_>>();
    for contract in &modeled {
        contract.validate()?;
    }
    if !modeled.is_empty() {
        value.insert(
            "modeled_service_contracts".into(),
            toml::Value::try_from(&modeled).map_err(|error| Error::failed(error.to_string()))?,
        );
    }
    let manifest = toml::to_string(&value).map_err(|error| Error::failed(error.to_string()))?;
    identity_metadata(&manifest)
}

fn identity_metadata(manifest: &str) -> Result<crate::definition::PluginMetadata> {
    #[derive(serde::Deserialize)]
    struct Identity {
        id: String,
        version: String,
        #[serde(default)]
        component_roles: Vec<crate::definition::ComponentRole>,
        #[serde(default)]
        required_capabilities: Vec<String>,
        #[serde(default)]
        contracts: Vec<crate::definition::ContractDescriptor>,
    }
    let identity: Identity =
        toml::from_str(manifest).map_err(|error| Error::failed(error.to_string()))?;
    if identity.component_roles.is_empty() {
        return Err(Error::failed(
            "manifest must declare component_roles explicitly",
        ));
    }
    Ok(crate::definition::PluginMetadata {
        plugin_id: identity.id,
        version: identity.version,
        roles: identity.component_roles,
        required_capabilities: identity.required_capabilities,
        contracts: identity.contracts,
        manifest_toml: Some(manifest.to_owned()),
    })
}

/// Map a transport failure from a generated client call to an author error.
#[must_use]
pub fn client_error(error: crate::TypedServiceClientError) -> Error {
    match error {
        crate::TypedServiceClientError::Dispatch(crate::TypedDispatchClientError::Server {
            details,
            ..
        }) => Error::failed(details),
        other => Error::failed(other.to_string()),
    }
}

/// Map an author error to the service response the host returns to callers.
#[must_use]
pub fn error_response(error: &Error) -> crate::ServiceResponse {
    crate::ServiceResponse::error("provider_failed", error.message.clone())
}

/// Parser for generated command arguments (`--name value`, `--name=value`,
/// bare `--flag` for booleans, repeated names for lists).
#[derive(Debug)]
pub struct CommandArgParser<'a> {
    values: Vec<(&'a str, Option<&'a str>)>,
}

impl<'a> CommandArgParser<'a> {
    /// Split `arguments` into named values.
    ///
    /// # Errors
    /// Rejects positional arguments and empty names.
    pub fn new(arguments: &'a [String], booleans: &[&str]) -> Result<Self> {
        let mut values = Vec::new();
        let mut iter = arguments.iter().map(String::as_str);
        while let Some(token) = iter.next() {
            let Some(name) = token.strip_prefix("--").filter(|name| !name.is_empty()) else {
                return Err(Error::invalid_arguments(format!(
                    "unexpected argument '{token}'; arguments are named (--name value)"
                )));
            };
            if let Some((name, value)) = name.split_once('=') {
                values.push((name, Some(value)));
            } else if booleans.contains(&name) {
                values.push((name, None));
            } else {
                let value = iter.next().ok_or_else(|| {
                    Error::invalid_arguments(format!("argument '--{name}' requires a value"))
                })?;
                values.push((name, Some(value)));
            }
        }
        Ok(Self { values })
    }

    /// Reject names the command does not declare.
    ///
    /// # Errors
    /// Names the first unknown argument.
    pub fn deny_unknown(&self, known: &[&str]) -> Result<()> {
        self.values
            .iter()
            .find(|(name, _)| !known.contains(name))
            .map_or(Ok(()), |(name, _)| {
                Err(Error::invalid_arguments(format!(
                    "unknown argument '--{name}'"
                )))
            })
    }

    /// The last value of an optional argument.
    ///
    /// # Errors
    /// Fails when the value cannot be parsed.
    pub fn optional<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>> {
        self.values
            .iter()
            .rev()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, value)| parse_value(name, value.unwrap_or("true")))
            .transpose()
    }

    /// A required argument.
    ///
    /// # Errors
    /// Fails when it is missing or cannot be parsed.
    pub fn required<T: std::str::FromStr>(&self, name: &str) -> Result<T> {
        self.optional(name)?.ok_or_else(|| {
            Error::invalid_arguments(format!("missing required argument '--{name}'"))
        })
    }

    /// Every value of a repeated argument.
    ///
    /// # Errors
    /// Fails when a value cannot be parsed.
    pub fn many<T: std::str::FromStr>(&self, name: &str) -> Result<Vec<T>> {
        self.values
            .iter()
            .filter(|(candidate, _)| *candidate == name)
            .map(|(_, value)| parse_value(name, value.unwrap_or("true")))
            .collect()
    }

    /// Whether a boolean flag is set.
    ///
    /// # Errors
    /// Fails when an explicit value is not a boolean.
    pub fn flag(&self, name: &str) -> Result<bool> {
        Ok(self.optional::<bool>(name)?.unwrap_or(false))
    }
}

fn parse_value<T: std::str::FromStr>(name: &str, value: &str) -> Result<T> {
    value
        .parse()
        .map_err(|_| Error::invalid_arguments(format!("invalid value '{value}' for '--{name}'")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct Changed {
        value: u64,
    }
    impl ContractEvent for Changed {
        const KIND: PluginEventKind = PluginEventKind::from_static("acme.counter/counter");
        const DELIVERY: EventDelivery = EventDelivery::State;
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(String, EventDelivery, Vec<u8>)>>);
    impl EventEmitter for Recorder {
        fn emit(
            &self,
            kind: &PluginEventKind,
            delivery: EventDelivery,
            payload: Vec<u8>,
        ) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push((kind.as_str().to_owned(), delivery, payload));
            Ok(())
        }
    }

    #[test]
    fn emit_encodes_on_the_generated_channel_and_requires_an_emitter() {
        assert!(Call::detached().emit(Changed { value: 1 }).is_err());
        let recorder = Arc::new(Recorder::default());
        let call = Call::detached().with_events(recorder.clone());
        call.emit(Changed { value: 7 }).unwrap();
        let (kind, delivery, payload) = recorder.0.lock().unwrap().remove(0);
        assert_eq!(kind, "acme.counter/counter");
        assert_eq!(delivery, EventDelivery::State);
        let decoded: Changed = crate::decode_service_message(&payload).unwrap();
        assert_eq!(decoded, Changed { value: 7 });
    }

    #[test]
    fn command_arguments_are_named_typed_and_strict() {
        let args = [
            "--name",
            "ada",
            "--count=3",
            "--loud",
            "--tag",
            "a",
            "--tag",
            "b",
        ]
        .map(String::from);
        let parser = CommandArgParser::new(&args, &["loud"]).unwrap();
        parser
            .deny_unknown(&["name", "count", "loud", "tag"])
            .unwrap();
        assert_eq!(parser.required::<String>("name").unwrap(), "ada");
        assert_eq!(parser.optional::<u32>("count").unwrap(), Some(3));
        assert!(parser.flag("loud").unwrap());
        assert_eq!(parser.many::<String>("tag").unwrap(), ["a", "b"]);
        assert!(parser.deny_unknown(&["name"]).is_err());
        assert!(parser.required::<String>("missing").is_err());
        assert!(CommandArgParser::new(&["positional".to_owned()], &[]).is_err());
        assert!(CommandArgParser::new(&["--name".to_owned()], &[]).is_err());
    }
}
