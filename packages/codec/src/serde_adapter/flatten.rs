//! Runtime support for flattening nested enums into one serde variant space.
//!
//! These items are the runtime half of the `FlattenedEnum` derive in `bmux_codec_derive`. A
//! proc-macro crate can only export macros, so the shared trait and helper live here.
//!
//! # Why a routing table is required
//!
//! Serde's [`EnumAccess`](serde::de::EnumAccess) yields a [`VariantAccess`](serde::de::VariantAccess)
//! that is consumed by the first read of the variant payload. A flattened deserializer therefore
//! cannot try each domain in turn and rewind on failure: it must know which domain owns a variant
//! name *before* touching the payload.
//!
//! [`FlattenedVariants`] provides exactly that: each domain enum declares the variant names it owns
//! and how to deserialize one of them, so a composed enum can route in a single pass.

use core::fmt;
use serde::de::{self, EnumAccess, VariantAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// A domain enum that participates in a flattened variant space.
///
/// Implemented by `#[derive(FlattenedEnum)]`. Implement it manually only when a domain enum needs
/// custom variant handling.
pub trait FlattenedVariants: Sized {
    /// Every variant name this enum owns, as encoded on the wire.
    ///
    /// Names must be unique across all domains sharing one flattened space; a flat namespace cannot
    /// disambiguate duplicates.
    const OWNED_VARIANTS: &'static [&'static str];

    /// Deserialize the variant named `variant` from `access`.
    ///
    /// `variant` is guaranteed to be one of [`Self::OWNED_VARIANTS`]. Implementations consume
    /// `access` exactly once.
    ///
    /// # Errors
    ///
    /// Returns an error when `variant` is not recognized or the payload does not match the
    /// variant's shape.
    fn deserialize_variant<'de, A: VariantAccess<'de>>(
        variant: &str,
        access: A,
    ) -> Result<Self, A::Error>;
}

/// Boxing a domain enum keeps its variants in the same flattened space.
///
/// Large domains are often boxed so they do not set the size of the composed enum. The wire form is
/// unchanged, because `Box` serializes transparently.
impl<T: FlattenedVariants> FlattenedVariants for Box<T> {
    const OWNED_VARIANTS: &'static [&'static str] = T::OWNED_VARIANTS;

    fn deserialize_variant<'de, A: VariantAccess<'de>>(
        variant: &str,
        access: A,
    ) -> Result<Self, A::Error> {
        T::deserialize_variant(variant, access).map(Box::new)
    }
}

/// An owned enum variant name read through serde's identifier path.
///
/// Reading a variant name as a [`String`] asks the format for a *string value*, which fails on
/// formats that encode variant names untagged. Deserializing through
/// [`Deserializer::deserialize_identifier`] reads the name as an identifier instead.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FlattenedVariantName(String);

impl FlattenedVariantName {
    /// Borrow the variant name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume this wrapper and return the owned variant name.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for FlattenedVariantName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FlattenedVariantName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for FlattenedVariantName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NameVisitor;

        impl Visitor<'_> for NameVisitor {
            type Value = FlattenedVariantName;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an enum variant name")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(FlattenedVariantName(value.to_owned()))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                // Positional encodings identify variants by index.
                Ok(FlattenedVariantName(value.to_string()))
            }
        }

        deserializer.deserialize_identifier(NameVisitor)
    }
}

/// Read the variant name from `data` without consuming its payload.
///
/// Returns the name and the [`VariantAccess`] to hand to
/// [`FlattenedVariants::deserialize_variant`].
///
/// # Errors
///
/// Returns an error when the variant name cannot be read.
pub fn split_variant<'de, A: EnumAccess<'de>>(
    data: A,
) -> Result<(FlattenedVariantName, A::Variant), A::Error> {
    data.variant::<FlattenedVariantName>()
}
