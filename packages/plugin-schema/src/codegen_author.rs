//! Author bindings generated from the canonical IR.
//!
//! For every service interface with a capability this emits, inside the
//! interface's module, a `v<N>` module holding:
//!
//! - `pub trait <Interface>`: the async provider trait. Methods take
//!   `&bmux_plugin_sdk::author::Call` and return
//!   `bmux_plugin_sdk::author::Result<T>`. Every method is `where Self:
//!   Sized`, so authors implement it with plain `async fn` while
//!   `dyn <Interface>` stays a valid type for declarations
//!   (`provide::<dyn counter::v1::Counter>()`).
//! - `pub struct Client<T>`: a typed client that calls a bundled provider
//!   directly (no serialization) or any [`TypedDispatchClient`] transport
//!   through the existing checked codec path.
//! - `dispatch`: serves one `PluginInstance::invoke` call through a provider,
//!   so generated providers work with today's host.
//! - `Portable<P>` and `register`: the checked byte adapter and the typed
//!   registry registration used by the bundled fast path.
//! - Marker impls of `author::{ProvidedInterface, Provides, RequiredInterface}`
//!   and `author::ContractEvent` for the interface's event payload.
//!
//! Package-level plugin `command`s generate typed argument structs and
//! `CommandDecl` constants (`commands::HELLO`); extension points generate a
//! `Data` record, a `Behavior` trait, a wire `Request` enum and an
//! `author::ExtensionPoint`/`Extends` implementation.
//!
//! [`TypedDispatchClient`]: https://docs.rs/bmux_plugin_sdk

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::{ImportMap, OwnTypeMap, pascal_case, resolve_qualified, snake_case};
use crate::ast::Primitive;
use crate::ir::{
    Delivery, ExtensionPoint, Field, Function, Interface, Item, Package, RefScope, Type,
};

const SDK: &str = "::bmux_plugin_sdk";

/// Where a rendered type sits relative to the schema root.
#[derive(Clone, Copy)]
struct Scope<'a> {
    /// `super::` hops from the current module to the schema root.
    to_root: usize,
    /// Module of the enclosing interface, for `RefScope::Local`.
    interface: Option<&'a str>,
}

pub(super) struct Author<'a> {
    pub package: &'a Package,
    pub imports: &'a ImportMap,
    pub own_types: &'a OwnTypeMap,
    /// Event payload types used by more than one interface get no
    /// `ContractEvent` impl: the channel would be ambiguous.
    shared_payloads: BTreeSet<(RefScope, String)>,
}

impl<'a> Author<'a> {
    pub(super) fn new(
        package: &'a Package,
        imports: &'a ImportMap,
        own_types: &'a OwnTypeMap,
    ) -> Self {
        let mut seen = BTreeMap::<(RefScope, String), usize>::new();
        for iface in &package.interfaces {
            if let Some(key) = events(iface).and_then(|events| payload_key(iface, &events.ty)) {
                *seen.entry(key).or_default() += 1;
            }
        }
        Self {
            package,
            imports,
            own_types,
            shared_payloads: seen
                .into_iter()
                .filter(|(_, count)| *count > 1)
                .map(|(key, _)| key)
                .collect(),
        }
    }

    fn ty(&self, ty: &Type, scope: Scope<'_>) -> String {
        let root = "super::".repeat(scope.to_root);
        match ty {
            Type::Primitive(primitive) => primitive_type(*primitive).to_owned(),
            Type::Unit => "()".to_owned(),
            Type::Option(inner) => format!("Option<{}>", self.ty(inner, scope)),
            Type::List(inner) => format!("Vec<{}>", self.ty(inner, scope)),
            Type::Map(key, value) => format!(
                "::std::collections::BTreeMap<{}, {}>",
                self.ty(key, scope),
                self.ty(value, scope)
            ),
            Type::Result(ok, error) => format!(
                "::std::result::Result<{}, {}>",
                self.ty(ok, scope),
                self.ty(error, scope)
            ),
            Type::Tuple(items) => {
                let items = items.iter().map(|item| self.ty(item, scope)).collect::<Vec<_>>();
                if items.len() == 1 {
                    format!("({},)", items[0])
                } else {
                    format!("({})", items.join(", "))
                }
            }
            Type::Ref(reference) => match &reference.scope {
                RefScope::Local => scope.interface.map_or_else(
                    || compile_error("a package-level declaration cannot name an interface-local type without its interface"),
                    |module| format!("{root}{module}::{}", pascal_case(&reference.name)),
                ),
                RefScope::Interface(other) => {
                    format!("{root}{}::{}", snake_case(other), pascal_case(&reference.name))
                }
                RefScope::Import(alias) => {
                    let path = resolve_qualified(alias, &reference.name, self.imports, self.own_types);
                    // Unresolved own-type fallbacks are relative to an interface module.
                    path.strip_prefix("super::").map_or(path.clone(), |relative| {
                        format!("{}{relative}", "super::".repeat(scope.to_root))
                    })
                }
            },
            Type::Stream(_) | Type::Future(_) | Type::Own(_) | Type::Borrow(_) => {
                compile_error("BPDL handles, streams and futures have no Rust author binding yet")
            }
        }
    }

    /// Emit `v<N>` inside the interface module (indent: 4 spaces).
    pub(super) fn interface(&self, iface: &Interface, out: &mut String) {
        let module = super::interface_module(self.package, iface);
        let scope = Scope {
            to_root: 2,
            interface: Some(&module),
        };
        let functions = functions(iface);
        let routable = iface.capability.is_some() && !functions.is_empty();
        let event = events(iface);
        // Handles, streams and futures already fail the build through the
        // interface's own bindings; author bindings add no second error.
        if (!routable && event.is_none()) || iface.items.iter().any(item_unbindable) {
            return;
        }
        let version = iface.version.unwrap_or(1);
        let trait_name = provider_trait_name(iface);
        let reserved = [
            trait_name.as_str(),
            "Client",
            "Erased",
            "Portable",
            "dispatch",
            "register",
        ];
        let _ = writeln!(
            out,
            "    /// Author bindings for `{}`: provider trait, typed client and adapters.",
            iface.wire_id()
        );
        let _ = writeln!(out, "    pub mod v{version} {{");
        let types = iface
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Type(def) => Some(pascal_case(&def.name)),
                Item::Function(_) | Item::Events(_) => None,
            })
            .filter(|name| !reserved.contains(&name.as_str()))
            .collect::<Vec<_>>();
        if !types.is_empty() {
            let _ = writeln!(out, "        pub use super::{{{}}};", types.join(", "));
        }
        let _ = writeln!(
            out,
            "        pub use super::{{INTERFACE_ID, SERVICE_CONTRACT}};"
        );
        if let Some(events) = event {
            self.contract_event(iface, events, out);
            if !routable {
                self.event_client(iface, events, scope, out);
            }
        }
        if routable {
            self.provider_trait(iface, &trait_name, &functions, scope, out);
            self.erased(&trait_name, &functions, scope, out);
            self.bound(&trait_name, &functions, scope, out);
            self.client(&functions, scope, out);
            Self::dispatch(&trait_name, &functions, out);
            Self::dispatch_streaming(&trait_name, &functions, out);
            Self::portable(&trait_name, &functions, out);
            Self::markers(&trait_name, out);
        }
        out.push_str("    }\n\n");
    }

    fn params(&self, function: &Function, scope: Scope<'_>) -> String {
        let mut params = String::new();
        for field in &function.params {
            let _ = write!(
                params,
                ", {}: {}",
                field_name(field),
                self.ty(&field.ty, scope)
            );
        }
        if let Some(emits) = &function.emits {
            let _ = write!(
                params,
                ", events: {SDK}::typed_dispatch::TypedServiceEventSink<{}>, cancellation: {SDK}::ServiceCancellation",
                self.ty(emits, scope)
            );
        }
        params
    }

    fn args(function: &Function, prefix: &str) -> String {
        let mut args = String::new();
        for field in &function.params {
            let _ = write!(args, ", {prefix}{}", field_name(field));
        }
        if function.emits.is_some() {
            args.push_str(", events, cancellation");
        }
        args
    }

    fn provider_trait(
        &self,
        iface: &Interface,
        name: &str,
        functions: &[&Function],
        scope: Scope<'_>,
        out: &mut String,
    ) {
        docs(&iface.docs, "        ", out);
        let _ = writeln!(
            out,
            "        /// Provider of `{}`. Implement every method with `async fn`;\n        /// declare it with `provide::<dyn {name}>()`.",
            iface.wire_id()
        );
        let _ = writeln!(out, "        pub trait {name}: Send + Sync + 'static {{");
        for function in functions {
            docs(&function.docs, "            ", out);
            out.push_str(&wide(function, 2, "            "));
            let _ = writeln!(
                out,
                "            fn {}(&self, cx: &{SDK}::author::Call{}) -> impl ::std::future::Future<Output = {SDK}::author::Result<{}>> + Send where Self: Sized;",
                snake_case(&function.name),
                self.params(function, scope),
                self.ty(&function.result, scope)
            );
        }
        out.push_str("        }\n\n");
    }

    fn erased(
        &self,
        trait_name: &str,
        functions: &[&Function],
        scope: Scope<'_>,
        out: &mut String,
    ) {
        out.push_str(
            "        /// Object-safe form of the provider trait, used for direct bundled calls.\n",
        );
        out.push_str("        #[doc(hidden)]\n        pub trait Erased: Send + Sync {\n");
        for function in functions {
            out.push_str(&wide(function, 2, "            "));
            let _ = writeln!(
                out,
                "            fn {}<'a>(&'a self, cx: &'a {SDK}::author::Call{}) -> {SDK}::author::BoxFuture<'a, {SDK}::author::Result<{}>>;",
                snake_case(&function.name),
                self.params(function, scope),
                self.ty(&function.result, scope)
            );
        }
        out.push_str("        }\n\n");
        let _ = writeln!(out, "        impl<P: {trait_name}> Erased for P {{");
        for function in functions {
            let name = snake_case(&function.name);
            let _ = writeln!(
                out,
                "            fn {name}<'a>(&'a self, cx: &'a {SDK}::author::Call{}) -> {SDK}::author::BoxFuture<'a, {SDK}::author::Result<{}>> {{ Box::pin(<P as {trait_name}>::{name}(self, cx{})) }}",
                self.params(function, scope),
                self.ty(&function.result, scope),
                Self::args(function, "")
            );
        }
        out.push_str("        }\n\n");
    }

    fn bound(&self, trait_name: &str, functions: &[&Function], scope: Scope<'_>, out: &mut String) {
        out.push_str("        /// A registered provider: direct calls run with its provider-bound base call.\n");
        let _ = writeln!(
            out,
            "        struct Bound<P> {{ provider: ::std::sync::Arc<P>, base: {SDK}::author::Call }}\n"
        );
        let _ = writeln!(out, "        impl<P: {trait_name}> Erased for Bound<P> {{");
        for function in functions {
            let name = snake_case(&function.name);
            let _ = writeln!(
                out,
                "            fn {name}<'a>(&'a self, _cx: &'a {SDK}::author::Call{}) -> {SDK}::author::BoxFuture<'a, {SDK}::author::Result<{}>> {{ Box::pin(<P as {trait_name}>::{name}(&self.provider, &self.base{})) }}",
                self.params(function, scope),
                self.ty(&function.result, scope),
                Self::args(function, "")
            );
        }
        out.push_str("        }\n\n");
    }

    fn client(&self, functions: &[&Function], scope: Scope<'_>, out: &mut String) {
        out.push_str(
            "        /// Typed client. `direct` calls a bundled provider with no serialization;\n        /// `remote` uses any typed dispatch transport through the checked codec.\n",
        );
        let _ = writeln!(
            out,
            "        #[derive(Clone)]\n        pub struct Client<T = {SDK}::definition::PluginHostServices> {{ inner: ClientInner<T> }}\n"
        );
        out.push_str("        #[derive(Clone)]\n        enum ClientInner<T> { Direct(::std::sync::Arc<dyn Erased>), Remote(T) }\n\n");
        out.push_str("        impl<T> Client<T> {\n");
        out.push_str("            /// Call through a typed dispatch transport (checked codec).\n            #[must_use]\n            pub const fn remote(transport: T) -> Self { Self { inner: ClientInner::Remote(transport) } }\n");
        out.push_str("            /// Call a bundled provider directly.\n            #[must_use]\n            pub fn direct(provider: ::std::sync::Arc<dyn Erased>) -> Self { Self { inner: ClientInner::Direct(provider) } }\n");
        let _ = writeln!(
            out,
            "            /// Call the provider registered in a typed service handle directly.\n            ///\n            /// # Errors\n            /// Returns if the handle holds a different provider type.\n            pub fn from_handle(handle: &{SDK}::TypedServiceHandle) -> {SDK}::Result<Self> {{ Ok(Self::direct(handle.provider_as_trait::<dyn Erased>()?)) }}"
        );
        out.push_str("        }\n\n");
        let _ = writeln!(
            out,
            "        impl<T: {SDK}::TypedDispatchClient + Clone + Sync> Client<T> {{"
        );
        for function in functions {
            let name = snake_case(&function.name);
            docs(&function.docs, "            ", out);
            out.push_str("            /// # Errors\n            /// Returns the provider's failure or a transport failure.\n");
            out.push_str(&wide(function, 1, "            "));
            let _ = writeln!(
                out,
                "            pub async fn {name}(&self{}) -> {SDK}::author::Result<{}> {{",
                self.params(function, scope),
                self.ty(&function.result, scope)
            );
            let args = Self::args(function, "");
            let _ = writeln!(
                out,
                "                match &self.inner {{\n                    ClientInner::Direct(__direct) => __direct.{name}(&{SDK}::author::Call::detached(){args}).await,\n                    ClientInner::Remote(__transport) => {{\n                        let mut __transport = __transport.clone();\n                        super::client::{name}(&mut __transport{args}).await.map_err({SDK}::author::client_error)\n                    }}\n                }}\n            }}"
            );
        }
        out.push_str("        }\n\n");
    }

    fn dispatch(trait_name: &str, functions: &[&Function], out: &mut String) {
        out.push_str("        /// Serve one host invocation (`PluginInstance::invoke`) of this interface.\n        ///\n        /// Accepts checked and plain frames; provider failures become error responses.\n        ///\n        /// # Errors\n        /// Returns only when a checked frame does not match this contract.\n");
        let _ = writeln!(
            out,
            "        pub async fn dispatch<P: {trait_name}>(provider: &P, call: {SDK}::author::Call, context: {SDK}::NativeServiceContext) -> {SDK}::author::Result<{SDK}::ServiceResponse> {{"
        );
        let _ = writeln!(
            out,
            "            if context.request.service.interface_id != INTERFACE_ID.as_str() {{\n                return Ok({SDK}::ServiceResponse::error(\"unsupported_service_interface\", format!(\"interface '{{}}' is not '{{}}'\", context.request.service.interface_id, INTERFACE_ID.as_str())));\n            }}\n            let checked = {SDK}::service_contract::is_checked(&context.request.payload);\n            let body = if checked {{\n                {SDK}::service_contract::decode_for(INTERFACE_ID.as_str(), SERVICE_CONTRACT, &context.request.payload).map_err(|error| {SDK}::author::Error::failed(error.to_string()))?.to_vec()\n            }} else {{\n                context.request.payload.clone()\n            }};\n            let _ = &provider;\n            let _ = &call;\n            let mut response = match context.request.operation.as_str() {{"
        );
        for function in functions {
            let _ = writeln!(out, "                {:?} => {{", function.name);
            if function.emits.is_some() {
                let _ = writeln!(
                    out,
                    "                    {SDK}::ServiceResponse::error(\"streaming_required\", \"operation '{}' streams events; invoke it through a streaming transport\")",
                    function.name
                );
            } else {
                let request = request_type(function);
                let _ = writeln!(
                    out,
                    "                    match {SDK}::decode_service_message::<{request}>(&body) {{\n                        Err(error) => {SDK}::ServiceResponse::error(\"invalid_request\", error.to_string()),\n                        Ok(request) => {{\n                            let _ = &request;\n                            match provider.{}(&call{}).await {{\n                                Ok(value) => {SDK}::ServiceResponse::ok({SDK}::encode_service_message(&value).map_err(|error| {SDK}::author::Error::failed(error.to_string()))?),\n                                Err(error) => {SDK}::author::error_response(&error),\n                            }}\n                        }}\n                    }}",
                    snake_case(&function.name),
                    Self::args(function, "request.")
                );
            }
            out.push_str("                }\n");
        }
        let _ = writeln!(
            out,
            "                other => {SDK}::ServiceResponse::error(\"unsupported_service_operation\", format!(\"'{{}}' has no operation '{{other}}'\", INTERFACE_ID.as_str())),\n            }};\n            if checked && response.error.is_none() {{\n                response.payload = {SDK}::service_contract::encode_response(&response.payload);\n            }}\n            Ok(response)\n        }}\n"
        );
    }

    fn dispatch_streaming(trait_name: &str, functions: &[&Function], out: &mut String) {
        out.push_str("        /// Serve one streaming host invocation (`PluginInstance::invoke_streaming`).\n        ///\n        /// Event frames go to the invocation's sink in order before the single\n        /// terminal response; cancellation stops the provider's sink.\n        ///\n        /// # Errors\n        /// Returns when a checked frame does not match this contract or event\n        /// delivery failed.\n");
        let _ = writeln!(
            out,
            "        {}pub async fn dispatch_streaming<P: {trait_name}>(provider: &P, call: {SDK}::author::Call, context: {SDK}::NativeStreamingServiceContext) -> {SDK}::author::Result<{SDK}::ServiceResponse> {{\n            let {SDK}::NativeStreamingServiceContext {{ service: context, events }} = context;\n            if context.request.service.interface_id != INTERFACE_ID.as_str() {{\n                return Ok({SDK}::ServiceResponse::error(\"unsupported_service_interface\", format!(\"interface '{{}}' is not '{{}}'\", context.request.service.interface_id, INTERFACE_ID.as_str())));\n            }}\n            let checked = {SDK}::service_contract::is_checked(&context.request.payload);\n            let body = if checked {{\n                {SDK}::service_contract::decode_for(INTERFACE_ID.as_str(), SERVICE_CONTRACT, &context.request.payload).map_err(|error| {SDK}::author::Error::failed(error.to_string()))?.to_vec()\n            }} else {{\n                context.request.payload.clone()\n            }};\n            let stream = {SDK}::typed_dispatch::ServiceStreamContext {{\n                invocation_id: context.cancellation.invocation_id.clone(),\n                cancellation: call.cancellation().cloned().unwrap_or_default(),\n                events,\n            }};\n            let _ = (&provider, &call, &body, &stream);\n            let mut response = match context.request.operation.as_str() {{",
            // Interfaces without streaming operations keep the uniform async
            // signature `Provides` stores, with nothing to await.
            if functions.iter().any(|function| function.emits.is_some()) {
                ""
            } else {
                "#[allow(clippy::unused_async)]\n        "
            }
        );
        for function in functions.iter().filter(|function| function.emits.is_some()) {
            let request = request_type(function);
            let _ = writeln!(
                out,
                "                {:?} => {{\n                    let (stream, completion) = stream.scoped();\n                    if stream.cancellation.is_cancelled() {{\n                        {SDK}::ServiceResponse::error(\"cancelled\", \"service stream cancelled\")\n                    }} else {{\n                        match {SDK}::decode_service_message::<{request}>(&body) {{\n                            Err(error) => {SDK}::ServiceResponse::error(\"invalid_request\", error.to_string()),\n                            Ok(request) => {{\n                                let _ = &request;\n                                let events = stream.typed_sink();\n                                let cancellation = stream.cancellation.clone();\n                                let result = provider.{}(&call{}).await;\n                                completion.finish().map_err(|error| {SDK}::author::Error::failed(error.to_string()))?;\n                                match result {{\n                                    Ok(value) => {SDK}::ServiceResponse::ok({SDK}::encode_service_message(&value).map_err(|error| {SDK}::author::Error::failed(error.to_string()))?),\n                                    Err(error) => {SDK}::author::error_response(&error),\n                                }}\n                            }}\n                        }}\n                    }}\n                }}",
                function.name,
                snake_case(&function.name),
                Self::args(function, "request.")
            );
        }
        for function in functions.iter().filter(|function| function.emits.is_none()) {
            let _ = writeln!(
                out,
                "                {:?} => {SDK}::ServiceResponse::error(\"unary_operation\", \"operation '{}' is unary; invoke it without a stream\"),",
                function.name, function.name
            );
        }
        let _ = writeln!(
            out,
            "                other => {SDK}::ServiceResponse::error(\"unsupported_service_operation\", format!(\"'{{}}' has no operation '{{other}}'\", INTERFACE_ID.as_str())),\n            }};\n            if checked && response.error.is_none() {{\n                response.payload = {SDK}::service_contract::encode_response(&response.payload);\n            }}\n            Ok(response)\n        }}\n"
        );
    }

    fn portable(trait_name: &str, functions: &[&Function], out: &mut String) {
        out.push_str("        /// Checked byte adapter over a provider, for dylib and cross-process callers.\n        /// Calls run with the provider-bound base call.\n");
        let _ = writeln!(
            out,
            "        pub struct Portable<P> {{ provider: ::std::sync::Arc<P>, base: {SDK}::author::Call }}\n"
        );
        let _ = writeln!(
            out,
            "        impl<P> Portable<P> {{\n            #[must_use]\n            pub const fn new(provider: ::std::sync::Arc<P>, base: {SDK}::author::Call) -> Self {{ Self {{ provider, base }} }}\n        }}\n"
        );
        let _ = writeln!(
            out,
            "        impl<P: {trait_name}> {SDK}::PortableServiceProvider for Portable<P> {{\n            fn descriptor(&self) -> {SDK}::ServiceInterfaceDescriptor {{ super::SERVICE_DESCRIPTOR }}"
        );
        let streaming = functions
            .iter()
            .filter(|function| function.emits.is_some())
            .map(|function| format!("{:?}", function.name))
            .collect::<Vec<_>>();
        if !streaming.is_empty() {
            let _ = writeln!(
                out,
                "            fn supports_streaming(&self, operation: &str) -> bool {{ matches!(operation, {}) }}",
                streaming.join(" | ")
            );
        }
        let _ = writeln!(
            out,
            "            fn dispatch_checked<'a>(&'a self, operation: &'a str, payload: &'a [u8], stream: Option<{SDK}::typed_dispatch::ServiceStreamContext>) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = {SDK}::Result<Vec<u8>>> + Send + 'a>> {{\n                Box::pin(async move {{\n                    let payload = {SDK}::service_contract::decode_for(INTERFACE_ID.as_str(), SERVICE_CONTRACT, payload)?;\n                    let call = self.base.clone();\n                    let failed = |error: {SDK}::author::Error| {SDK}::PluginError::ServiceProtocol {{ details: error.message }};\n                    match operation {{"
        );
        for function in functions {
            let _ = writeln!(out, "                        {:?} => {{", function.name);
            if function.emits.is_some() {
                let _ = writeln!(
                    out,
                    "                            let stream = stream.ok_or_else(|| {SDK}::PluginError::ServiceProtocol {{ details: \"streaming operation requires an event sink\".to_string() }})?;\n                            let (stream, completion) = stream.scoped();\n                            if stream.cancellation.is_cancelled() {{ return Err({SDK}::PluginError::ServiceProtocol {{ details: \"service stream cancelled\".to_string() }}); }}\n                            let events = stream.typed_sink();\n                            let cancellation = stream.cancellation.clone();"
                );
            } else {
                let _ = writeln!(
                    out,
                    "                            if stream.is_some() {{ return Err({SDK}::PluginError::ServiceProtocol {{ details: \"unary operation does not accept streaming\".to_string() }}); }}"
                );
            }
            let request = request_type(function);
            let _ = writeln!(
                out,
                "                            let request: {request} = {SDK}::decode_service_message(payload)?;\n                            let _ = &request;\n                            let response = self.provider.{}(&call{}).await.map_err(failed)?;",
                snake_case(&function.name),
                Self::args(function, "request.")
            );
            if function.emits.is_some() {
                let _ = writeln!(
                    out,
                    "                            completion.finish()?;\n                            if stream.cancellation.is_cancelled() {{ return Err({SDK}::PluginError::ServiceProtocol {{ details: \"service stream cancelled\".to_string() }}); }}"
                );
            }
            let _ = writeln!(
                out,
                "                            Ok({SDK}::service_contract::encode_response(&{SDK}::encode_service_message(&response)?))\n                        }}"
            );
        }
        let _ = writeln!(
            out,
            "                        _ => {{ let _ = (&call, failed, stream); Err({SDK}::PluginError::ServiceProtocol {{ details: format!(\"unknown service operation: {{operation}}\") }}) }}\n                    }}\n                }})\n            }}\n        }}\n"
        );
        let _ = writeln!(
            out,
            "        /// Register `provider` for direct bundled calls together with its portable adapter.\n        ///\n        /// # Errors\n        /// Returns if the registry rejects the generated descriptor.\n        ///\n        /// Calls arriving without host context run with `base`, the provider-bound call.\n        pub fn register<P: {trait_name}>(provider: ::std::sync::Arc<P>, base: {SDK}::author::Call, registry: &mut {SDK}::TypedServiceRegistry) -> {SDK}::Result<()> {{\n            let direct: ::std::sync::Arc<dyn Erased> = ::std::sync::Arc::new(Bound {{ provider: provider.clone(), base: base.clone() }});\n            registry.insert_typed_provider(super::SERVICE_DESCRIPTOR, direct, ::std::sync::Arc::new(Portable::new(provider, base)))\n        }}\n"
        );
    }

    fn markers(trait_name: &str, out: &mut String) {
        let _ = writeln!(
            out,
            "        impl {SDK}::author::ProvidedInterface for dyn {trait_name} {{\n            const DESCRIPTOR: {SDK}::ServiceInterfaceDescriptor = super::SERVICE_DESCRIPTOR;\n        }}\n\n        impl<P: {trait_name}> {SDK}::author::Provides<P> for dyn {trait_name} {{\n            fn portable(provider: ::std::sync::Arc<P>, base: {SDK}::author::Call) -> ::std::sync::Arc<dyn {SDK}::PortableServiceProvider> {{ ::std::sync::Arc::new(Portable::new(provider, base)) }}\n            fn register(provider: ::std::sync::Arc<P>, base: {SDK}::author::Call, registry: &mut {SDK}::TypedServiceRegistry) -> {SDK}::Result<()> {{ register(provider, base, registry) }}\n            fn dispatch(provider: &P, call: {SDK}::author::Call, context: {SDK}::NativeServiceContext) -> {SDK}::author::BoxFuture<'_, {SDK}::author::Result<{SDK}::ServiceResponse>> {{ Box::pin(dispatch(provider, call, context)) }}\n            fn dispatch_streaming(provider: &P, call: {SDK}::author::Call, context: {SDK}::NativeStreamingServiceContext) -> {SDK}::author::BoxFuture<'_, {SDK}::author::Result<{SDK}::ServiceResponse>> {{ Box::pin(dispatch_streaming(provider, call, context)) }}\n        }}\n\n        impl<T> {SDK}::author::RequiredInterface for Client<T> {{\n            const DESCRIPTOR: {SDK}::ServiceInterfaceDescriptor = super::SERVICE_DESCRIPTOR;\n        }}\n\n        impl {SDK}::author::ServiceClient for Client<{SDK}::definition::PluginHostServices> {{\n            fn connect(host: {SDK}::definition::PluginHostServices) -> Self {{ Self::remote(host) }}\n            fn direct(handle: &{SDK}::TypedServiceHandle) -> {SDK}::Result<Self> {{ Self::from_handle(handle) }}\n        }}"
        );
    }

    fn contract_event(&self, iface: &Interface, events: &crate::ir::Events, out: &mut String) {
        let Some(key) = payload_key(iface, &events.ty) else {
            return;
        };
        if self.shared_payloads.contains(&key) {
            return;
        }
        let payload = self.ty(
            &events.ty,
            Scope {
                to_root: 2,
                interface: Some(&super::interface_module(self.package, iface)),
            },
        );
        let (kind, delivery) = match events.delivery {
            Delivery::Broadcast => ("EVENT_KIND", "Broadcast"),
            Delivery::State => ("STATE_KIND", "State"),
        };
        let _ = writeln!(
            out,
            "        impl {SDK}::author::ContractEvent for {payload} {{\n            const KIND: {SDK}::PluginEventKind = super::{kind};\n            const DELIVERY: {SDK}::author::EventDelivery = {SDK}::author::EventDelivery::{delivery};\n        }}\n"
        );
    }

    /// Typed subscription client of an events interface:
    /// `Client::subscribe_<event>()` over any `author::EventSource` (the
    /// activation's host services in both packaging modes).
    fn event_client(
        &self,
        iface: &Interface,
        events: &crate::ir::Events,
        scope: Scope<'_>,
        out: &mut String,
    ) {
        let payload = self.ty(&events.ty, scope);
        let method = match &events.ty {
            Type::Ref(reference) => format!("subscribe_{}", snake_case(&reference.name)),
            _ => "subscribe".to_owned(),
        };
        let (kind, delivery) = match events.delivery {
            Delivery::Broadcast => ("EVENT_KIND", "Broadcast"),
            Delivery::State => ("STATE_KIND", "State"),
        };
        let wire = iface.wire_id();
        let _ = writeln!(
            out,
            "        /// Typed subscriptions to `{wire}`. Obtain it with\n        /// `Activation::events::<Client>()` after `subscribe::<Client>()` in\n        /// `declare()`, or connect any `author::EventSource` with `remote`.\n        #[derive(Clone)]\n        pub struct Client<T = {SDK}::definition::PluginHostServices> {{ source: T }}\n\n        impl<T> Client<T> {{\n            /// Subscribe through `source`.\n            #[must_use]\n            pub const fn remote(source: T) -> Self {{ Self {{ source }} }}\n        }}\n\n        impl<T: {SDK}::author::EventSource + Sync> Client<T> {{\n            /// Stream `{wire}` events, owned by the activation the source belongs to.\n            ///\n            /// # Errors\n            /// The source cannot observe the channel (not registered, activation closed).\n            // Async keeps the signature transport neutral (a process transport awaits its handshake).\n            #[allow(clippy::unused_async)]\n            pub async fn {method}(&self) -> {SDK}::author::Result<{SDK}::author::Subscription<{payload}>> {{\n                {SDK}::author::subscribe(&self.source, &super::{kind}, {SDK}::author::EventDelivery::{delivery})\n            }}\n        }}\n\n        impl {SDK}::author::EventClient for Client<{SDK}::definition::PluginHostServices> {{\n            const KIND: {SDK}::PluginEventKind = super::{kind};\n            const DELIVERY: {SDK}::author::EventDelivery = {SDK}::author::EventDelivery::{delivery};\n            fn connect(host: {SDK}::definition::PluginHostServices) -> Self {{ Self::remote(host) }}\n        }}\n"
        );
    }

    /// Package-level `commands` module.
    pub(super) fn commands(&self, out: &mut String) {
        if self.package.commands.is_empty() {
            return;
        }
        let scope = Scope {
            to_root: 1,
            interface: None,
        };
        out.push_str(
            "/// Plugin commands declared in BPDL, with typed arguments.\npub mod commands {\n",
        );
        for command in &self.package.commands {
            let args = format!("{}Args", pascal_case(&command.name));
            docs(&command.docs, "    ", out);
            let _ = writeln!(out, "    /// Arguments of the `{}` command.", command.name);
            let _ = writeln!(
                out,
                "    #[derive(Debug, Clone, PartialEq)]\n    pub struct {args} {{"
            );
            for param in &command.params {
                docs(&param.docs, "        ", out);
                let _ = writeln!(
                    out,
                    "        pub {}: {},",
                    field_name(param),
                    self.ty(&param.ty, scope)
                );
            }
            out.push_str("    }\n\n");
            let booleans = command
                .params
                .iter()
                .filter(|param| matches!(param.ty, Type::Primitive(Primitive::Bool)))
                .map(|param| format!("{:?}", param.name))
                .collect::<Vec<_>>();
            let known = command
                .params
                .iter()
                .map(|param| format!("{:?}", param.name))
                .collect::<Vec<_>>();
            let _ = writeln!(
                out,
                "    impl {SDK}::author::CommandArgs for {args} {{\n        fn parse(arguments: &[String]) -> {SDK}::author::Result<Self> {{\n            let parser = {SDK}::author::CommandArgParser::new(arguments, &[{}])?;\n            parser.deny_unknown(&[{}])?;\n            let _ = &parser;\n            Ok(Self {{",
                booleans.join(", "),
                known.join(", ")
            );
            for param in &command.params {
                let _ = writeln!(
                    out,
                    "                {}: {},",
                    field_name(param),
                    command_argument(param)
                );
            }
            out.push_str("            })\n        }\n    }\n\n");
            let summary = command.title.as_deref().unwrap_or(&command.name);
            let _ = writeln!(
                out,
                "    /// `{}`: {summary}\n    pub const {}: {SDK}::author::CommandDecl<{args}> = {SDK}::author::CommandDecl::new({:?});\n",
                command.name,
                snake_case(&command.name).to_ascii_uppercase(),
                command.name
            );
        }
        out.push_str("}\n\n");
    }

    /// Package-level `extension_points` module.
    pub(super) fn extension_points(&self, out: &mut String) {
        if self.package.extension_points.is_empty() {
            return;
        }
        out.push_str(
            "/// Extension points declared by this package.\npub mod extension_points {\n",
        );
        for point in &self.package.extension_points {
            self.extension_point(point, out);
        }
        out.push_str("}\n\n");
    }

    fn extension_point(&self, point: &ExtensionPoint, out: &mut String) {
        let scope = Scope {
            to_root: 2,
            interface: None,
        };
        let id = point.id(&self.package.id);
        docs(&point.docs, "    ", out);
        let _ = writeln!(out, "    /// Extension point `{id}`.");
        let _ = writeln!(
            out,
            "    pub mod {}_v{} {{",
            snake_case(&point.name),
            point.version
        );
        out.push_str("        /// Declarative data every contribution supplies; readable before activation.\n");
        out.push_str("        #[derive(Debug, Clone, PartialEq, ::serde::Serialize, ::serde::Deserialize)]\n        pub struct Data {\n");
        for field in &point.data {
            docs(&field.docs, "            ", out);
            if let Some(adapter) = bytes_adapter(&field.ty) {
                let _ = writeln!(out, "            #[serde(with = \"{adapter}\")]");
            }
            let _ = writeln!(
                out,
                "            pub {}: {},",
                field_name(field),
                self.ty(&field.ty, scope)
            );
        }
        out.push_str("        }\n\n");
        out.push_str("        /// Behavior a contribution implements. Implement every method with `async fn`.\n        pub trait Behavior: Send + Sync + 'static {\n");
        for function in &point.functions {
            docs(&function.docs, "            ", out);
            out.push_str(&wide(function, 2, "            "));
            let _ = writeln!(
                out,
                "            fn {}(&self, cx: &{SDK}::author::Call{}) -> impl ::std::future::Future<Output = {SDK}::author::Result<{}>> + Send where Self: Sized;",
                snake_case(&function.name),
                self.params(function, scope),
                self.ty(&function.result, scope)
            );
        }
        out.push_str("        }\n\n");
        out.push_str("        /// One behavior call on the wire.\n        #[derive(Debug, Clone, PartialEq, ::serde::Serialize, ::serde::Deserialize)]\n        #[serde(rename_all = \"snake_case\")]\n        pub enum Request {\n");
        for function in &point.functions {
            let _ = writeln!(out, "            {} {{", pascal_case(&function.name));
            for field in &function.params {
                if let Some(adapter) = bytes_adapter(&field.ty) {
                    let _ = writeln!(out, "                #[serde(with = \"{adapter}\")]");
                }
                let _ = writeln!(
                    out,
                    "                {}: {},",
                    field_name(field),
                    self.ty(&field.ty, scope)
                );
            }
            out.push_str("            },\n");
        }
        out.push_str("        }\n\n");
        let _ = writeln!(
            out,
            "        /// Type-level handle for `{id}`.\n        pub struct Point;\n\n        impl {SDK}::author::ExtensionPoint for Point {{\n            const ID: &'static str = {id:?};\n            const VERSION: u32 = {};\n            type Data = Data;\n        }}\n",
            point.version
        );
        let _ = writeln!(
            out,
            "        impl<B: Behavior> {SDK}::author::Extends<B> for Point {{\n            fn dispatch<'a>(behavior: &'a B, call: &'a {SDK}::author::Call, payload: &'a [u8]) -> {SDK}::author::BoxFuture<'a, {SDK}::author::Result<Vec<u8>>> {{\n                Box::pin(async move {{\n                    let request: Request = {SDK}::decode_service_message(payload).map_err(|error| {SDK}::author::Error::invalid_arguments(error.to_string()))?;\n                    let _ = (&behavior, &call);\n                    match request {{"
        );
        for function in &point.functions {
            let fields = function
                .params
                .iter()
                .map(field_name)
                .collect::<Vec<_>>()
                .join(", ");
            let mut args = String::new();
            for field in &function.params {
                let _ = write!(args, ", {}", field_name(field));
            }
            let _ = writeln!(
                out,
                "                        Request::{} {{ {fields} }} => {{\n                            let value = behavior.{}(call{args}).await?;\n                            {SDK}::encode_service_message(&value).map_err(|error| {SDK}::author::Error::failed(error.to_string()))\n                        }}",
                pascal_case(&function.name),
                snake_case(&function.name)
            );
        }
        out.push_str(
            "                    }\n                })\n            }\n        }\n    }\n\n",
        );
    }
}

/// `#[allow(clippy::too_many_arguments)]` when `self`, `cx`, the BPDL
/// parameters and any stream parameters exceed clippy's limit of 7.
fn wide(function: &Function, fixed: usize, indent: &str) -> String {
    let streams = if function.emits.is_some() { 2 } else { 0 };
    if function.params.len() + fixed + streams > 7 {
        format!("{indent}#[allow(clippy::too_many_arguments)]\n")
    } else {
        String::new()
    }
}

fn unbindable(ty: &Type) -> bool {
    match ty {
        Type::Stream(_) | Type::Future(_) | Type::Own(_) | Type::Borrow(_) => true,
        Type::Option(inner) | Type::List(inner) => unbindable(inner),
        Type::Map(key, value) | Type::Result(key, value) => unbindable(key) || unbindable(value),
        Type::Tuple(items) => items.iter().any(unbindable),
        Type::Primitive(_) | Type::Unit | Type::Ref(_) => false,
    }
}

fn item_unbindable(item: &Item) -> bool {
    match item {
        Item::Function(function) => {
            function.params.iter().any(|field| unbindable(&field.ty))
                || unbindable(&function.result)
                || function.emits.as_ref().is_some_and(unbindable)
        }
        Item::Events(events) => unbindable(&events.ty),
        Item::Type(_) => false,
    }
}

fn functions(iface: &Interface) -> Vec<&Function> {
    iface
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Function(function) => Some(function),
            Item::Type(_) | Item::Events(_) => None,
        })
        .collect()
}

fn events(iface: &Interface) -> Option<&crate::ir::Events> {
    iface.items.iter().find_map(|item| match item {
        Item::Events(events) => Some(events),
        Item::Type(_) | Item::Function(_) => None,
    })
}

/// Event payloads defined in this package (impls on imported types would
/// violate coherence).
fn payload_key(iface: &Interface, ty: &Type) -> Option<(RefScope, String)> {
    let Type::Ref(reference) = ty else {
        return None;
    };
    match &reference.scope {
        RefScope::Local => Some((
            RefScope::Interface(iface.name.clone()),
            reference.name.clone(),
        )),
        RefScope::Interface(_) => Some((reference.scope.clone(), reference.name.clone())),
        RefScope::Import(_) => None,
    }
}

/// The provider trait is the interface name in `PascalCase`, unless one of
/// the interface's own types already uses that name.
fn provider_trait_name(iface: &Interface) -> String {
    let name = pascal_case(&iface.name);
    let clashes = iface
        .items
        .iter()
        .any(|item| matches!(item, Item::Type(def) if pascal_case(&def.name) == name));
    if clashes {
        format!("{name}Provider")
    } else {
        name
    }
}

fn request_type(function: &Function) -> String {
    if function.params.is_empty() {
        "()".to_owned()
    } else {
        format!("super::client::{}Request", pascal_case(&function.name))
    }
}

fn field_name(field: &Field) -> String {
    let name = snake_case(&field.name);
    if matches!(
        name.as_str(),
        "type"
            | "match"
            | "ref"
            | "mod"
            | "fn"
            | "impl"
            | "loop"
            | "move"
            | "self"
            | "use"
            | "where"
            | "async"
            | "await"
            | "dyn"
    ) {
        format!("r#{name}")
    } else {
        name
    }
}

/// Parse expression for one command argument, matching the generated
/// manifest: `option<T>` optional, `list<T>` repeated, `bool` a flag,
/// anything else required.
fn command_argument(param: &Field) -> String {
    let name = format!("{:?}", param.name);
    let parsable = |ty: &Type| matches!(ty, Type::Primitive(primitive) if !matches!(primitive, Primitive::Bytes));
    match &param.ty {
        Type::Option(inner) if parsable(inner) => format!("parser.optional({name})?"),
        Type::List(inner) if parsable(inner) => format!("parser.many({name})?"),
        Type::Primitive(Primitive::Bool) => format!("parser.flag({name})?"),
        ty if parsable(ty) => format!("parser.required({name})?"),
        _ => compile_error(&format!(
            "command argument `{}` must be a primitive, option<primitive> or list<primitive>",
            param.name
        )),
    }
}

const fn primitive_type(primitive: Primitive) -> &'static str {
    match primitive {
        Primitive::Bool => "bool",
        Primitive::U8 => "u8",
        Primitive::U16 => "u16",
        Primitive::U32 => "u32",
        Primitive::U64 => "u64",
        Primitive::I8 => "i8",
        Primitive::I16 => "i16",
        Primitive::I32 => "i32",
        Primitive::I64 => "i64",
        Primitive::F32 => "f32",
        Primitive::F64 => "f64",
        Primitive::String => "String",
        Primitive::Bytes => "Vec<u8>",
        Primitive::Uuid => "::uuid::Uuid",
        Primitive::Char => "char",
    }
}

fn bytes_adapter(ty: &Type) -> Option<&'static str> {
    match ty {
        Type::Primitive(Primitive::Bytes) => Some("bmux_codec::serde_bytes_vec"),
        Type::List(inner) if matches!(inner.as_ref(), Type::Primitive(Primitive::U8)) => {
            Some("bmux_codec::serde_bytes_vec")
        }
        Type::Option(inner) if bytes_adapter(inner).is_some() => {
            Some("bmux_codec::serde_bytes_vec::option")
        }
        _ => None,
    }
}

fn compile_error(message: &str) -> String {
    format!("::core::compile_error!({message:?})")
}

fn docs(lines: &[String], indent: &str, out: &mut String) {
    for line in lines {
        let _ = writeln!(
            out,
            "{indent}///{}{line}",
            if line.is_empty() { "" } else { " " }
        );
    }
}
