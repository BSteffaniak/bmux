#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Derive macros for flattening nested enums into a single serde variant space.
//!
//! # Problem
//!
//! Protocols outgrow a single enum. A message type with a hundred variants is hard to maintain and
//! forces every consumer to depend on every domain's payload types. The natural fix is to group
//! variants into per-domain enums:
//!
//! ```ignore
//! enum Request {
//!     Session(SessionRequest),
//!     Permission(PermissionRequest),
//! }
//! ```
//!
//! But that changes the wire format: the outer variant name is encoded alongside the inner one.
//! Serde offers no way out. `#[serde(untagged)]` serializes correctly but cannot deserialize from
//! self-describing formats that present enum input, and `#[serde(flatten)]` does not apply to enums.
//!
//! # Solution
//!
//! Two derives cooperate:
//!
//! * [`FlattenedVariants`] on each domain enum declares the variant names it owns and generates the
//!   `bmux_codec::FlattenedVariants` implementation used for routing.
//! * [`FlattenedEnum`] on the composed enum hoists every `#[flattened]` domain's variants into one
//!   flat space.
//!
//! ```ignore
//! #[derive(FlattenedVariants)]
//! #[serde(rename_all = "snake_case")]
//! enum PermissionRequest {
//!     ListPermissions,
//!     ResolvePermission { permission_id: String, approved: bool },
//! }
//!
//! #[derive(FlattenedEnum)]
//! enum Request {
//!     #[flattened]
//!     Permission(PermissionRequest),
//! }
//! ```
//!
//! The encoded form matches a hand-written flat enum containing the same variants, so a nested type
//! can replace a flat one without a protocol change. Because both directions are generated from one
//! declaration, serialization and deserialization cannot drift, and an unrouted variant is a
//! compile error rather than a runtime failure.
//!
//! # Requirements and limits
//!
//! * Every variant of a [`FlattenedEnum`] must be `#[flattened]` and hold exactly one enum that
//!   implements `bmux_codec::FlattenedVariants`. Mixing flattened and plain variants is rejected,
//!   because a plain variant's own name would silently occupy the same flat namespace.
//! * Variant names must be unique across the whole flattened space; duplicates cannot be
//!   disambiguated by a flat namespace.
//! * Flattening is single-level per declaration; nested domains compose by deriving at each level.
//! * **Name-based encodings only.** Positional encodings identify variants by index, and a domain
//!   enum numbers its own variants from zero, so flattened positional bytes cannot match a flat
//!   enum's. Use a name-based mode when replacing a flat enum with a flattened one.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitStr, Variant, parse_macro_input};

/// Derive `Serialize`, `Deserialize`, and `FlattenedVariants` for one domain enum.
///
/// Supports unit, newtype, and struct variants. Honors `#[serde(rename_all = "...")]` and
/// per-variant `#[serde(rename = "...")]` so wire names match an existing flat enum.
#[proc_macro_derive(FlattenedVariants, attributes(serde))]
pub fn flattened_variants(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_variants(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Derive `Serialize`/`Deserialize` that flatten `#[flattened]` domains into one variant space.
#[proc_macro_derive(FlattenedEnum, attributes(flattened))]
pub fn flattened_enum(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_composed(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Rename styles supported for wire variant names.
#[derive(Clone, Copy)]
enum RenameAll {
    None,
    SnakeCase,
    CamelCase,
    KebabCase,
}

impl RenameAll {
    fn apply(self, ident: &syn::Ident) -> String {
        let raw = ident.to_string();
        match self {
            Self::None => raw,
            Self::SnakeCase => to_delimited(&raw, '_'),
            Self::KebabCase => to_delimited(&raw, '-'),
            Self::CamelCase => {
                let mut chars = raw.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_lowercase().collect::<String>() + chars.as_str()
                })
            }
        }
    }
}

fn to_delimited(value: &str, delimiter: char) -> String {
    let mut out = String::with_capacity(value.len() + 4);
    for (index, ch) in value.char_indices() {
        if ch.is_uppercase() {
            if index != 0 {
                out.push(delimiter);
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn parse_rename_all(input: &DeriveInput) -> syn::Result<RenameAll> {
    let mut style = RenameAll::None;
    for attr in &input.attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        // Other serde attributes are not this macro's concern, so unrelated parse failures are
        // ignored; an unsupported `rename_all` value is still reported.
        let mut unsupported = None;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all") {
                let value: LitStr = meta.value()?.parse()?;
                match value.value().as_str() {
                    "snake_case" => style = RenameAll::SnakeCase,
                    "camelCase" => style = RenameAll::CamelCase,
                    "kebab-case" => style = RenameAll::KebabCase,
                    other => {
                        unsupported = Some(syn::Error::new_spanned(
                            &value,
                            format!("unsupported rename_all style '{other}'"),
                        ));
                    }
                }
            }
            Ok(())
        });
        if let Some(error) = unsupported {
            return Err(error);
        }
    }
    Ok(style)
}

fn wire_name(variant: &Variant, style: RenameAll) -> String {
    for attr in &variant.attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let mut renamed = None;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let value: LitStr = meta.value()?.parse()?;
                renamed = Some(value.value());
            }
            Ok(())
        });
        if let Some(renamed) = renamed {
            return renamed;
        }
    }
    style.apply(&variant.ident)
}

#[allow(clippy::too_many_lines)]
fn expand_variants(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            input,
            "FlattenedVariants can only be derived for enums",
        ));
    };

    let name = &input.ident;
    let style = parse_rename_all(input)?;
    let enum_name_lit = LitStr::new(&name.to_string(), name.span());

    let mut names = Vec::new();
    let mut serialize_arms = Vec::new();
    let mut deserialize_arms = Vec::new();

    for (index, variant) in data.variants.iter().enumerate() {
        let ident = &variant.ident;
        let wire = wire_name(variant, style);
        let wire_lit = LitStr::new(&wire, ident.span());
        let variant_index = u32::try_from(index).map_err(|_| {
            syn::Error::new_spanned(variant, "enum has more variants than serde supports")
        })?;
        names.push(wire_lit.clone());

        match &variant.fields {
            Fields::Unit => {
                serialize_arms.push(quote! {
                    Self::#ident => serializer.serialize_unit_variant(
                        #enum_name_lit, #variant_index, #wire_lit,
                    )
                });
                deserialize_arms.push(quote! {
                    #wire_lit => {
                        ::serde::de::VariantAccess::unit_variant(access)?;
                        ::core::result::Result::Ok(Self::#ident)
                    }
                });
            }
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                serialize_arms.push(quote! {
                    Self::#ident(value) => serializer.serialize_newtype_variant(
                        #enum_name_lit, #variant_index, #wire_lit, value,
                    )
                });
                deserialize_arms.push(quote! {
                    #wire_lit => ::core::result::Result::Ok(Self::#ident(
                        ::serde::de::VariantAccess::newtype_variant(access)?,
                    ))
                });
            }
            Fields::Unnamed(_) => {
                return Err(syn::Error::new_spanned(
                    variant,
                    "tuple variants with more than one field are not supported yet",
                ));
            }
            Fields::Named(fields) => {
                let field_idents: Vec<_> = fields
                    .named
                    .iter()
                    .map(|field| field.ident.clone().expect("named field"))
                    .collect();
                let field_types: Vec<_> = fields.named.iter().map(|field| &field.ty).collect();
                let field_lits: Vec<_> = field_idents
                    .iter()
                    .map(|ident| LitStr::new(&ident.to_string(), ident.span()))
                    .collect();
                let field_count = field_idents.len();

                serialize_arms.push(quote! {
                    Self::#ident { #(#field_idents,)* } => {
                        use ::serde::ser::SerializeStructVariant as _;
                        let mut variant = serializer.serialize_struct_variant(
                            #enum_name_lit, #variant_index, #wire_lit, #field_count,
                        )?;
                        #(variant.serialize_field(#field_lits, #field_idents)?;)*
                        variant.end()
                    }
                });

                // Reuse serde's derived struct deserializer for this variant's field set so field
                // names and types are never hand-copied.
                deserialize_arms.push(quote! {
                    #wire_lit => {
                        #[derive(::serde::Deserialize)]
                        struct Fields { #(#field_idents: #field_types,)* }

                        struct Bridge;
                        impl<'de> ::serde::de::Visitor<'de> for Bridge {
                            type Value = Fields;
                            fn expecting(
                                &self,
                                formatter: &mut ::core::fmt::Formatter<'_>,
                            ) -> ::core::fmt::Result {
                                formatter.write_str(concat!(#wire_lit, " fields"))
                            }
                            fn visit_map<A: ::serde::de::MapAccess<'de>>(
                                self,
                                map: A,
                            ) -> ::core::result::Result<Self::Value, A::Error> {
                                ::serde::Deserialize::deserialize(
                                    ::serde::de::value::MapAccessDeserializer::new(map),
                                )
                            }
                            fn visit_seq<A: ::serde::de::SeqAccess<'de>>(
                                self,
                                seq: A,
                            ) -> ::core::result::Result<Self::Value, A::Error> {
                                ::serde::Deserialize::deserialize(
                                    ::serde::de::value::SeqAccessDeserializer::new(seq),
                                )
                            }
                        }

                        let fields = ::serde::de::VariantAccess::struct_variant(
                            access, &[#(#field_lits,)*], Bridge,
                        )?;
                        ::core::result::Result::Ok(Self::#ident {
                            #(#field_idents: fields.#field_idents,)*
                        })
                    }
                });
            }
        }
    }

    let all_names: Vec<_> = names.clone();

    Ok(quote! {
        impl ::bmux_codec::FlattenedVariants for #name {
            const OWNED_VARIANTS: &'static [&'static str] = &[#(#all_names,)*];

            fn deserialize_variant<'de, A: ::serde::de::VariantAccess<'de>>(
                variant: &str,
                access: A,
            ) -> ::core::result::Result<Self, A::Error> {
                match variant {
                    #(#deserialize_arms,)*
                    other => ::core::result::Result::Err(::serde::de::Error::unknown_variant(
                        other,
                        <Self as ::bmux_codec::FlattenedVariants>::OWNED_VARIANTS,
                    )),
                }
            }
        }

        impl ::serde::Serialize for #name {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::core::result::Result<S::Ok, S::Error> {
                match self {
                    #(#serialize_arms,)*
                }
            }
        }

        impl<'de> ::serde::Deserialize<'de> for #name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::core::result::Result<Self, D::Error> {
                struct DomainVisitor;

                impl<'de> ::serde::de::Visitor<'de> for DomainVisitor {
                    type Value = #name;

                    fn expecting(
                        &self,
                        formatter: &mut ::core::fmt::Formatter<'_>,
                    ) -> ::core::fmt::Result {
                        formatter.write_str(concat!("a ", #enum_name_lit, " variant"))
                    }

                    fn visit_enum<A: ::serde::de::EnumAccess<'de>>(
                        self,
                        data: A,
                    ) -> ::core::result::Result<Self::Value, A::Error> {
                        let (variant, access) = ::bmux_codec::split_variant(data)?;
                        <#name as ::bmux_codec::FlattenedVariants>::deserialize_variant(
                            variant.as_str(),
                            access,
                        )
                    }
                }

                deserializer.deserialize_enum(
                    #enum_name_lit,
                    <#name as ::bmux_codec::FlattenedVariants>::OWNED_VARIANTS,
                    DomainVisitor,
                )
            }
        }
    })
}

/// Parse and validate the `#[flattened]` domain variants of a composed enum.
fn parse_domains(input: &DeriveInput) -> syn::Result<Vec<(syn::Ident, syn::Type)>> {
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            input,
            "FlattenedEnum can only be derived for enums",
        ));
    };

    let mut domains = Vec::new();
    for variant in &data.variants {
        if !variant
            .attrs
            .iter()
            .any(|attr| attr.path().is_ident("flattened"))
        {
            return Err(syn::Error::new_spanned(
                variant,
                "every variant must be #[flattened]; a plain variant's own name would also occupy \
                 the flat namespace, which cannot be disambiguated",
            ));
        }
        let Fields::Unnamed(fields) = &variant.fields else {
            return Err(syn::Error::new_spanned(
                variant,
                "#[flattened] requires a newtype variant holding one enum, for example \
                 `Session(SessionRequest)`",
            ));
        };
        if fields.unnamed.len() != 1 {
            return Err(syn::Error::new_spanned(
                variant,
                "#[flattened] requires exactly one field",
            ));
        }
        domains.push((variant.ident.clone(), fields.unnamed[0].ty.clone()));
    }

    if domains.is_empty() {
        return Err(syn::Error::new_spanned(
            input,
            "FlattenedEnum requires at least one #[flattened] variant",
        ));
    }
    Ok(domains)
}

fn expand_composed(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let domains = parse_domains(input)?;
    let name = &input.ident;
    let enum_name_lit = LitStr::new(&name.to_string(), name.span());

    let serialize_arms = domains.iter().map(|(ident, _)| {
        quote! {
            // Delegate to the domain enum so the outer name is never written.
            Self::#ident(inner) => ::serde::Serialize::serialize(inner, serializer)
        }
    });

    let route_arms = domains.iter().map(|(ident, inner)| {
        quote! {
            if <#inner as ::bmux_codec::FlattenedVariants>::OWNED_VARIANTS
                .contains(&variant.as_str())
            {
                return ::core::result::Result::Ok(#name::#ident(
                    <#inner as ::bmux_codec::FlattenedVariants>::deserialize_variant(
                        variant.as_str(),
                        access,
                    )?,
                ));
            }
        }
    });

    let domain_types: Vec<_> = domains.iter().map(|(_, inner)| inner.clone()).collect();

    Ok(quote! {
        impl #name {
            /// Every variant name in this enum's flattened space.
            #[must_use]
            pub fn flattened_variants() -> ::std::vec::Vec<&'static str> {
                let mut names = ::std::vec::Vec::new();
                #(names.extend_from_slice(
                    <#domain_types as ::bmux_codec::FlattenedVariants>::OWNED_VARIANTS,
                );)*
                names
            }
        }

        impl ::serde::Serialize for #name {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::core::result::Result<S::Ok, S::Error> {
                match self {
                    #(#serialize_arms,)*
                }
            }
        }

        impl<'de> ::serde::Deserialize<'de> for #name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::core::result::Result<Self, D::Error> {
                struct FlattenedVisitor;

                impl<'de> ::serde::de::Visitor<'de> for FlattenedVisitor {
                    type Value = #name;

                    fn expecting(
                        &self,
                        formatter: &mut ::core::fmt::Formatter<'_>,
                    ) -> ::core::fmt::Result {
                        formatter.write_str(concat!("a ", #enum_name_lit, " variant"))
                    }

                    fn visit_enum<A: ::serde::de::EnumAccess<'de>>(
                        self,
                        data: A,
                    ) -> ::core::result::Result<Self::Value, A::Error> {
                        // Read the flat variant name once, then hand the payload to exactly one
                        // domain. `VariantAccess` is consumed by the first read, so routing cannot
                        // be retried.
                        let (variant, access) = ::bmux_codec::split_variant(data)?;
                        #(#route_arms)*
                        ::core::result::Result::Err(::serde::de::Error::custom(
                            ::std::format!("unknown variant `{}`", variant.as_str()),
                        ))
                    }
                }

                deserializer.deserialize_enum(#enum_name_lit, &[], FlattenedVisitor)
            }
        }
    })
}
