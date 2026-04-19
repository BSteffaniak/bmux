//! Identifier newtypes used across the plugin framework.
//!
//! Core bmux is domain-agnostic: it never names a concept like "pane" or
//! "session" itself. Instead, plugins declare the interfaces, operations,
//! capabilities, and event streams they expose through BPDL schemas, and
//! the generated code emits `const` identifier values that both the plugin
//! (as producer) and consumers (in other plugins, in core plumbing, or in
//! the CLI) import from the same plugin-api crate.
//!
//! This module provides the string-backed newtype wrappers used for those
//! identifiers:
//!
//! - [`PluginEventKind`] — the kind/name of a published event stream.
//! - [`InterfaceId`] — the canonical identifier for a BPDL interface.
//! - [`OperationId`] — the name of an operation (query or command) within
//!   an interface.
//! - [`CapabilityId`] — a granted capability string in a plugin manifest.
//!
//! All four are backed by `Cow<'static, str>` so that compile-time
//! constants are zero-allocation (they store a `&'static str` borrow),
//! while values decoded off the wire can own their string data. On the
//! wire and at rest they serialize as plain strings, so cross-language
//! plugins interoperate without understanding any Rust types.
//!
//! ## Compile-time safety without muddying the wire format
//!
//! At the call site, plugin-api crates emit one `const` per identifier:
//!
//! ```ignore
//! // Generated from BPDL for `plugin bmux.windows`:
//! pub mod windows_events {
//!     pub const INTERFACE_ID: bmux_plugin_sdk::InterfaceId =
//!         bmux_plugin_sdk::InterfaceId::from_static("windows-events");
//!     pub const EVENT_KIND: bmux_plugin_sdk::PluginEventKind =
//!         bmux_plugin_sdk::PluginEventKind::from_static("bmux.windows/windows-events");
//! }
//! ```
//!
//! Both producer and subscriber import from the same const. Cross-crate
//! typo-checking happens at compile time; the actual wire format remains
//! a plain string so dynamic plugins or non-Rust clients can participate
//! without any type-aware bindings.

use std::borrow::Cow;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Kind/name of a [`PluginEvent`](crate::PluginEvent) stream.
///
/// Events in bmux are plugin-owned: plugins declare typed event streams
/// in their BPDL schema, and generated constants give both producer and
/// subscriber a compile-time-checked identifier without baking any
/// domain knowledge into core. The underlying wire representation is a
/// plain string of the form `"<namespace>/<stream-name>"`, for example
/// `"bmux.windows/pane-event"`.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PluginEventKind(Cow<'static, str>);

impl PluginEventKind {
    /// Construct from a `'static` string slice (typically a BPDL-generated
    /// constant). Cheap, const-friendly, zero-allocation.
    #[must_use]
    pub const fn from_static(value: &'static str) -> Self {
        Self(Cow::Borrowed(value))
    }

    /// Construct from an owned string (typically a wire-decoded value).
    #[must_use]
    pub const fn from_owned(value: String) -> Self {
        Self(Cow::Owned(value))
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl fmt::Debug for PluginEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PluginEventKind")
            .field(&self.as_str())
            .finish()
    }
}

impl fmt::Display for PluginEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for PluginEventKind {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for PluginEventKind {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for PluginEventKind {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<&'static str> for PluginEventKind {
    fn from(value: &'static str) -> Self {
        Self::from_static(value)
    }
}

impl From<String> for PluginEventKind {
    fn from(value: String) -> Self {
        Self::from_owned(value)
    }
}

impl From<PluginEventKind> for String {
    fn from(value: PluginEventKind) -> Self {
        value.0.into_owned()
    }
}

impl Serialize for PluginEventKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PluginEventKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(Self::from_owned(value))
    }
}

/// Canonical identifier for a BPDL interface.
///
/// Each BPDL-declared `interface <name>` block emits a generated
/// `pub const INTERFACE_ID: InterfaceId = InterfaceId::from_static("<name>")`
/// in its plugin-api crate. Consumers and providers both reference that
/// const when registering/resolving typed services.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct InterfaceId(Cow<'static, str>);

impl InterfaceId {
    #[must_use]
    pub const fn from_static(value: &'static str) -> Self {
        Self(Cow::Borrowed(value))
    }

    #[must_use]
    pub const fn from_owned(value: String) -> Self {
        Self(Cow::Owned(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl fmt::Debug for InterfaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("InterfaceId").field(&self.as_str()).finish()
    }
}

impl fmt::Display for InterfaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for InterfaceId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for InterfaceId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for InterfaceId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<&'static str> for InterfaceId {
    fn from(value: &'static str) -> Self {
        Self::from_static(value)
    }
}

impl From<String> for InterfaceId {
    fn from(value: String) -> Self {
        Self::from_owned(value)
    }
}

impl From<InterfaceId> for String {
    fn from(value: InterfaceId) -> Self {
        value.0.into_owned()
    }
}

impl Serialize for InterfaceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for InterfaceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(Self::from_owned(value))
    }
}

/// Name of an operation (query or command) within an interface.
///
/// BPDL codegen does not currently emit one constant per operation —
/// operations are dispatched through the generated service trait's
/// method names. [`OperationId`] is provided for lower-level byte
/// routers (and for places like plugin-manifest bindings) that must
/// compare operation strings dynamically.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct OperationId(Cow<'static, str>);

impl OperationId {
    #[must_use]
    pub const fn from_static(value: &'static str) -> Self {
        Self(Cow::Borrowed(value))
    }

    #[must_use]
    pub const fn from_owned(value: String) -> Self {
        Self(Cow::Owned(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl fmt::Debug for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OperationId").field(&self.as_str()).finish()
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for OperationId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for OperationId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for OperationId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<&'static str> for OperationId {
    fn from(value: &'static str) -> Self {
        Self::from_static(value)
    }
}

impl From<String> for OperationId {
    fn from(value: String) -> Self {
        Self::from_owned(value)
    }
}

impl From<OperationId> for String {
    fn from(value: OperationId) -> Self {
        value.0.into_owned()
    }
}

impl Serialize for OperationId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for OperationId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(Self::from_owned(value))
    }
}

/// Capability identifier granted to a plugin through its manifest.
///
/// Capabilities gate access to host primitives (for example,
/// `bmux.storage.read`) and to other plugins' typed services (for
/// example, `bmux.windows.write`, which an unrelated plugin needs in
/// order to invoke any mutating operation on the windows plugin's
/// services). Plugin-api crates emit `CapabilityId` constants so
/// consumers don't hand-type capability strings.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CapabilityId(Cow<'static, str>);

impl CapabilityId {
    #[must_use]
    pub const fn from_static(value: &'static str) -> Self {
        Self(Cow::Borrowed(value))
    }

    #[must_use]
    pub const fn from_owned(value: String) -> Self {
        Self(Cow::Owned(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl fmt::Debug for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CapabilityId").field(&self.as_str()).finish()
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for CapabilityId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for CapabilityId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for CapabilityId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<&'static str> for CapabilityId {
    fn from(value: &'static str) -> Self {
        Self::from_static(value)
    }
}

impl From<String> for CapabilityId {
    fn from(value: String) -> Self {
        Self::from_owned(value)
    }
}

impl From<CapabilityId> for String {
    fn from(value: CapabilityId) -> Self {
        value.0.into_owned()
    }
}

impl Serialize for CapabilityId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CapabilityId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(Self::from_owned(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_event_kind_const_is_borrowed() {
        const KIND: PluginEventKind = PluginEventKind::from_static("test.fixture/event");
        assert_eq!(KIND.as_str(), "test.fixture/event");
        assert_eq!(KIND, "test.fixture/event");
    }

    #[test]
    fn interface_id_roundtrips_through_json() {
        const ID: InterfaceId = InterfaceId::from_static("windows-events");
        let json = serde_json::to_string(&ID).expect("serialize");
        assert_eq!(json, "\"windows-events\"");
        let decoded: InterfaceId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, ID);
        assert_eq!(decoded.as_str(), "windows-events");
    }

    #[test]
    fn owned_and_static_compare_equal() {
        let owned = InterfaceId::from_owned("foo".to_string());
        let stat = InterfaceId::from_static("foo");
        assert_eq!(owned, stat);
    }

    #[test]
    fn all_newtypes_implement_display() {
        let e = PluginEventKind::from_static("ev");
        let i = InterfaceId::from_static("if");
        let o = OperationId::from_static("op");
        let c = CapabilityId::from_static("cap");
        assert_eq!(format!("{e}"), "ev");
        assert_eq!(format!("{i}"), "if");
        assert_eq!(format!("{o}"), "op");
        assert_eq!(format!("{c}"), "cap");
    }
}
