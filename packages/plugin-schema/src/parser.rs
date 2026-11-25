//! Recursive-descent parser that produces [`crate::ast`] nodes from tokens.
//!
//! Top-level grammar (`///` documentation may precede every declaration):
//!
//! ```text
//! schema      := header import* capability* top-item*
//! header      := 'plugin' dotted 'version' INT ';'
//! import      := 'import' NAME '=' dotted ';'
//! capability  := 'capability' NAME '=' dotted ';'
//! top-item    := annotation* ( interface | world | command | contribute | extension-point )
//! interface   := 'interface' NAME '{' (annotation* iface-item)* '}'
//! iface-item  := record | variant | enum | flags | resource | query | command | events
//! world       := 'world' NAME '{' world-item* '}'
//! world-item  := 'provide' iface-ref ['capability' dotted] ';'
//!              | 'provide' 'opaque' NAME ['@' INT] ':' kind 'capability' dotted ';'
//!              | 'provide' 'capability' dotted ';'
//!              | 'require' ['optional'] (iface-ref | 'capability' dotted) ';'
//! command     := 'command' NAME '(' params ')' ['title' STRING] ';'
//! contribute  := 'contribute' [NAME '.'] NAME '@' INT '{' (NAME ':' literal),* '}'
//! extension-point := 'extension-point' NAME 'version' INT
//!                    '{' 'data' '{' fields '}' ['interface' '{' operation* '}'] '}'
//! annotation  := '@' NAME ['(' (NAME | INT) ')']
//! ```

use crate::{
    Error, Span,
    ast::{
        Access, CapabilityDecl, Contribution, DeliveryMode, Docs, EnumCase, EnumDef, EventsDecl,
        ExtensionOperation, ExtensionPoint, ExtensionPointRef, Field, FlagDef, FlagsDef, Import,
        Interface, InterfaceItem, InterfaceRef, Literal, Operation, PluginCommand, PluginHeader,
        Primitive, RecordDef, ResourceDef, ResourceMethod, ResourceMethodKind, Schema,
        ServiceKindDecl, Stability, TypeRef, VariantCase, VariantDef, World, WorldItem,
    },
    lexer::{Token, TokenKind},
};

/// Parse a token stream into a [`Schema`] AST.
///
/// # Errors
///
/// Returns [`Error::Parse`] if the tokens don't form a valid BPDL schema
/// (missing keyword, unexpected token, malformed operation signature,
/// etc.).
pub fn parse(tokens: &[Token]) -> Result<Schema, Error> {
    let mut parser = Parser { tokens, index: 0 };
    parser.parse_schema()
}

struct Parser<'a> {
    tokens: &'a [Token],
    index: usize,
}

/// One parsed `@name` or `@name(arg)` annotation.
#[derive(Debug, Clone)]
struct Annotation {
    name: String,
    arg: Option<AnnotationArg>,
    span: Span,
}

#[derive(Debug, Clone)]
enum AnnotationArg {
    Name(String),
    Int(u64),
}

fn parse_error(span: Span, message: impl Into<String>) -> Error {
    Error::Parse {
        span,
        message: message.into(),
    }
}

impl Annotation {
    /// `@access(observe|input|manage|owner-local)`.
    fn access_arg(&self) -> Result<Access, Error> {
        match &self.arg {
            Some(AnnotationArg::Name(value)) => Access::parse(value).ok_or_else(|| {
                parse_error(
                    self.span,
                    format!(
                        "unknown access class `{value}` in `@access(...)`; expected `observe`, `input`, `manage`, or `owner-local`"
                    ),
                )
            }),
            _ => Err(parse_error(
                self.span,
                "expected access class in `@access(observe|input|manage|owner-local)`",
            )),
        }
    }

    /// The single `@access` among `annotations`, if any.
    fn single_access(annotations: &[Self]) -> Result<Option<Access>, Error> {
        let mut access = None;
        for annotation in annotations.iter().filter(|a| a.name == "access") {
            if access.replace(annotation.access_arg()?).is_some() {
                return Err(parse_error(
                    annotation.span,
                    "duplicate `@access` annotation",
                ));
            }
        }
        Ok(access)
    }

    fn int_arg(&self) -> Result<u32, Error> {
        match &self.arg {
            Some(AnnotationArg::Int(value)) => u32::try_from(*value).map_err(|_| {
                parse_error(self.span, format!("`@{}` value must fit in u32", self.name))
            }),
            _ => Err(parse_error(
                self.span,
                format!("`@{}` requires an integer argument", self.name),
            )),
        }
    }

    fn name_arg(&self) -> Result<String, Error> {
        match &self.arg {
            Some(AnnotationArg::Name(value)) => Ok(value.clone()),
            _ => Err(parse_error(
                self.span,
                format!("`@{}` requires a name argument", self.name),
            )),
        }
    }
}

/// Split stability annotations from the rest, rejecting duplicates.
fn take_stability(annotations: &mut Vec<Annotation>) -> Result<Stability, Error> {
    let mut stability = Stability::default();
    let mut rest = Vec::new();
    for annotation in annotations.drain(..) {
        let duplicate = match annotation.name.as_str() {
            "since" => stability.since.replace(annotation.int_arg()?).is_some(),
            "deprecated" => stability
                .deprecated
                .replace(annotation.int_arg()?)
                .is_some(),
            "unstable" => stability.unstable.replace(annotation.name_arg()?).is_some(),
            _ => {
                rest.push(annotation);
                continue;
            }
        };
        if duplicate {
            return Err(parse_error(
                annotation.span,
                format!("duplicate `@{}` annotation", annotation.name),
            ));
        }
    }
    *annotations = rest;
    Ok(stability)
}

fn reject_annotations(annotations: &[Annotation], target: &str) -> Result<(), Error> {
    if let Some(annotation) = annotations.first() {
        return Err(parse_error(
            annotation.span,
            format!(
                "unknown annotation `@{}` on {target}; only `@since`, `@unstable`, and `@deprecated` apply here",
                annotation.name
            ),
        ));
    }
    Ok(())
}

impl Parser<'_> {
    fn parse_schema(&mut self) -> Result<Schema, Error> {
        let plugin = self.parse_plugin_header()?;
        let mut imports = Vec::new();
        while self.check_after_docs(&TokenKind::Import) {
            imports.push(self.parse_import()?);
        }
        let mut capabilities = Vec::new();
        while self.check_identifier_after_docs("capability") {
            capabilities.push(self.parse_capability()?);
        }
        let mut schema = Schema {
            plugin,
            imports,
            capabilities,
            interfaces: Vec::new(),
            worlds: Vec::new(),
            commands: Vec::new(),
            contributions: Vec::new(),
            extension_points: Vec::new(),
        };
        while self.peek().is_some() {
            self.parse_top_item(&mut schema)?;
        }
        Ok(schema)
    }

    fn parse_top_item(&mut self, schema: &mut Schema) -> Result<(), Error> {
        let mut docs = self.take_docs();
        let mut annotations = self.parse_annotations()?;
        docs.extend(self.take_docs());
        let token = self
            .peek()
            .cloned()
            .ok_or_else(|| parse_error(Span::new(0, 0), "unexpected end of input"))?;
        match &token.kind {
            TokenKind::Interface => {
                let interface = self.parse_interface(docs, annotations)?;
                schema.interfaces.push(interface);
            }
            TokenKind::Command => {
                let stability = take_stability(&mut annotations)?;
                reject_annotations(&annotations, "a plugin command")?;
                schema
                    .commands
                    .push(self.parse_plugin_command(docs, stability)?);
            }
            TokenKind::Identifier(name) if name == "world" => {
                let stability = take_stability(&mut annotations)?;
                reject_annotations(&annotations, "a world")?;
                schema.worlds.push(self.parse_world(docs, stability)?);
            }
            TokenKind::Identifier(name) if name == "extension-point" => {
                let stability = take_stability(&mut annotations)?;
                reject_annotations(&annotations, "an extension point")?;
                schema
                    .extension_points
                    .push(self.parse_extension_point(docs, stability)?);
            }
            TokenKind::Identifier(name) if name == "contribute" => {
                reject_annotations(&annotations, "a contribution")?;
                schema.contributions.push(self.parse_contribution(docs)?);
            }
            other => {
                return Err(parse_error(
                    token.span,
                    format!(
                        "expected `interface`, `world`, `command`, `contribute`, or `extension-point` (got {other:?})"
                    ),
                ));
            }
        }
        Ok(())
    }

    fn parse_plugin_header(&mut self) -> Result<PluginHeader, Error> {
        let docs = self.take_docs();
        let span = self.expect(&TokenKind::Plugin, "expected `plugin` keyword")?;
        let plugin_id = self.parse_dotted_ident("expected plugin id after `plugin`")?;
        self.expect(&TokenKind::Version, "expected `version` keyword")?;
        let version = self.expect_int("expected integer version literal")?;
        self.expect(&TokenKind::Semicolon, "expected `;` ending plugin header")?;
        let version = u32::try_from(version)
            .map_err(|_| parse_error(span, format!("plugin version {version} out of u32 range")))?;
        Ok(PluginHeader {
            docs,
            plugin_id,
            version,
            span,
        })
    }

    fn parse_import(&mut self) -> Result<Import, Error> {
        // Imports carry no documentation; consume any for uniformity.
        let _ = self.take_docs();
        let span = self.expect(&TokenKind::Import, "expected `import` keyword")?;
        let alias = self.expect_identifier("expected import alias")?;
        self.expect(&TokenKind::Equals, "expected `=` after import alias")?;
        let plugin_id = self.parse_dotted_ident("expected plugin id in import")?;
        self.expect(&TokenKind::Semicolon, "expected `;` ending import")?;
        Ok(Import {
            alias,
            plugin_id,
            span,
        })
    }

    fn parse_capability(&mut self) -> Result<CapabilityDecl, Error> {
        let docs = self.take_docs();
        let span = self.expect_contextual_keyword("capability", "expected `capability` keyword")?;
        let name = self.expect_identifier("expected capability constant name")?;
        self.expect(&TokenKind::Equals, "expected `=` after capability name")?;
        let id = self.parse_dotted_ident("expected capability id")?;
        self.expect(&TokenKind::Semicolon, "expected `;` ending capability")?;
        Ok(CapabilityDecl {
            docs,
            name,
            id,
            span,
        })
    }

    fn parse_interface(
        &mut self,
        docs: Docs,
        mut annotations: Vec<Annotation>,
    ) -> Result<Interface, Error> {
        let stability = take_stability(&mut annotations)?;
        let mut capability = None;
        let mut interface_version = None;
        let mut major = None;
        let access = Annotation::single_access(&annotations)?;
        for annotation in annotations.iter().filter(|a| a.name != "access") {
            match annotation.name.as_str() {
                "capability" => {
                    let value = match &annotation.arg {
                        Some(AnnotationArg::Name(value)) => value.clone(),
                        _ => {
                            return Err(parse_error(
                                annotation.span,
                                "expected capability constant name in `@capability(NAME)`",
                            ));
                        }
                    };
                    if capability.replace(value).is_some() {
                        return Err(parse_error(
                            annotation.span,
                            "duplicate `@capability` interface annotation",
                        ));
                    }
                }
                "interface-version" => {
                    let version = match &annotation.arg {
                        Some(AnnotationArg::Int(value)) => u32::try_from(*value).map_err(|_| {
                            parse_error(annotation.span, "interface version must fit in u32")
                        })?,
                        _ => {
                            return Err(parse_error(
                                annotation.span,
                                "expected interface version integer in `@interface-version(N)`",
                            ));
                        }
                    };
                    if version == 0 {
                        return Err(parse_error(
                            annotation.span,
                            "interface version must be greater than zero",
                        ));
                    }
                    if interface_version.replace(version).is_some() {
                        return Err(parse_error(
                            annotation.span,
                            "duplicate `@interface-version` annotation",
                        ));
                    }
                }
                "major" => {
                    let value = annotation.int_arg()?;
                    if value == 0 {
                        return Err(parse_error(
                            annotation.span,
                            "contract major must be greater than zero",
                        ));
                    }
                    if major.replace(value).is_some() {
                        return Err(parse_error(
                            annotation.span,
                            "duplicate `@major` annotation",
                        ));
                    }
                }
                other => {
                    return Err(parse_error(
                        annotation.span,
                        format!(
                            "unknown interface annotation `@{other}`; expected `@capability(NAME)`, `@access(CLASS)`, `@interface-version(N)`, `@major(N)`, `@since(N)`, `@unstable(feature)`, or `@deprecated(N)`"
                        ),
                    ));
                }
            }
        }
        let span = self.expect(&TokenKind::Interface, "expected `interface` keyword")?;
        let name = self.expect_identifier("expected interface name")?;
        self.expect(&TokenKind::LBrace, "expected `{` opening interface body")?;
        let mut items = Vec::new();
        while !self.check_after_docs(&TokenKind::RBrace) {
            items.push(self.parse_interface_item()?);
        }
        let _ = self.take_docs();
        self.expect(&TokenKind::RBrace, "expected `}` closing interface body")?;
        if major.is_some() && interface_version.is_some() {
            return Err(parse_error(
                span,
                "`@major` is only for interfaces without `@interface-version`, whose version is their major",
            ));
        }
        Ok(Interface {
            docs,
            stability,
            name,
            interface_version,
            major,
            capability,
            access,
            items,
            span,
        })
    }

    // One dispatch over every interface item kind and its legal
    // annotations; splitting it would scatter the annotation rules.
    #[allow(clippy::too_many_lines)]
    fn parse_interface_item(&mut self) -> Result<InterfaceItem, Error> {
        let mut docs = self.take_docs();
        let mut annotations = self.parse_annotations()?;
        docs.extend(self.take_docs());
        let stability = take_stability(&mut annotations)?;
        let tok = self.peek().cloned().ok_or_else(|| {
            parse_error(Span::new(0, 0), "unexpected end of input inside interface")
        })?;
        let reentrant = annotations.iter().filter(|a| a.name == "reentrant").count();
        let access = Annotation::single_access(&annotations)?;
        let state = annotations.iter().any(|a| a.name == "state");
        if reentrant > 1 {
            return Err(parse_error(
                tok.span,
                "`@reentrant` must directly precede a `query` operation",
            ));
        }
        let item_annotations = |allowed: &[&str], target: &str| -> Result<(), Error> {
            for annotation in &annotations {
                if !allowed.contains(&annotation.name.as_str()) {
                    return Err(parse_error(
                        annotation.span,
                        format!("unknown annotation `@{}` on {target}", annotation.name),
                    ));
                }
            }
            Ok(())
        };
        if reentrant == 1 && !matches!(tok.kind, TokenKind::Query) {
            return Err(match tok.kind {
                TokenKind::Command => parse_error(
                    tok.span,
                    "`@reentrant` is only allowed on `query` operations; commands may mutate instance state and are never re-entrant",
                ),
                other => parse_error(
                    tok.span,
                    format!("`@reentrant` must precede a `query` operation (got {other:?})"),
                ),
            });
        }
        match &tok.kind {
            TokenKind::Record => {
                item_annotations(&[], "a record")?;
                Ok(InterfaceItem::Record(self.parse_record(docs, stability)?))
            }
            TokenKind::Variant => {
                item_annotations(&[], "a variant")?;
                Ok(InterfaceItem::Variant(self.parse_variant(docs, stability)?))
            }
            TokenKind::Enum => {
                item_annotations(&[], "an enum")?;
                Ok(InterfaceItem::Enum(self.parse_enum(docs, stability)?))
            }
            TokenKind::Identifier(name) if name == "flags" => {
                item_annotations(&[], "a flags type")?;
                Ok(InterfaceItem::Flags(self.parse_flags(docs, stability)?))
            }
            TokenKind::Identifier(name) if name == "resource" => {
                item_annotations(&[], "a resource")?;
                Ok(InterfaceItem::Resource(
                    self.parse_resource(docs, stability)?,
                ))
            }
            TokenKind::Query => {
                item_annotations(&["reentrant", "access"], "a query")?;
                let mut operation = self.parse_operation(OpKind::Query, docs, stability)?;
                operation.access = access;
                if reentrant == 1 {
                    if operation.emits.is_some() {
                        return Err(parse_error(
                            operation.span,
                            format!(
                                "`@reentrant` is not allowed on streaming operation `{}`; operations with `emits` cannot be re-entered",
                                operation.name,
                            ),
                        ));
                    }
                    operation.reentrant = true;
                }
                Ok(InterfaceItem::Query(operation))
            }
            TokenKind::Command => {
                item_annotations(&["access"], "a command")?;
                let mut operation = self.parse_operation(OpKind::Command, docs, stability)?;
                operation.access = access;
                Ok(InterfaceItem::Command(operation))
            }
            TokenKind::Events => {
                for annotation in &annotations {
                    if annotation.name != "state" {
                        return Err(parse_error(
                            annotation.span,
                            format!(
                                "unknown annotation `@{}`; only `@state` is recognised as a delivery hint on `events` declarations",
                                annotation.name,
                            ),
                        ));
                    }
                }
                let delivery = if state {
                    DeliveryMode::State
                } else {
                    DeliveryMode::Broadcast
                };
                Ok(InterfaceItem::Events(
                    self.parse_events(delivery, docs, stability)?,
                ))
            }
            _ => {
                if let Some(annotation) = annotations.first() {
                    return Err(parse_error(
                        annotation.span,
                        format!(
                            "unknown annotation `@{}` before {:?}",
                            annotation.name, tok.kind
                        ),
                    ));
                }
                Err(parse_error(
                    tok.span,
                    format!("unexpected token in interface body: {:?}", tok.kind),
                ))
            }
        }
    }

    fn parse_record(&mut self, docs: Docs, stability: Stability) -> Result<RecordDef, Error> {
        let span = self.expect(&TokenKind::Record, "expected `record`")?;
        let name = self.expect_identifier("expected record name")?;
        self.expect(&TokenKind::LBrace, "expected `{` opening record fields")?;
        let fields = self.parse_fields()?;
        self.expect(&TokenKind::RBrace, "expected `}` closing record fields")?;
        Ok(RecordDef {
            docs,
            stability,
            name,
            fields,
            span,
        })
    }

    fn parse_variant(&mut self, docs: Docs, stability: Stability) -> Result<VariantDef, Error> {
        let span = self.expect(&TokenKind::Variant, "expected `variant`")?;
        let name = self.expect_identifier("expected variant name")?;
        self.expect(&TokenKind::LBrace, "expected `{` opening variant cases")?;
        let mut cases = Vec::new();
        while !self.check_after_docs(&TokenKind::RBrace) {
            let case_docs = self.take_docs();
            let is_default = self.consume_default_annotation();
            let case_span = self.peek().map_or(Span::new(0, 0), |t| t.span);
            let case_name = self.expect_identifier("expected variant case name")?;
            let payload = if self.check(&TokenKind::LBrace) {
                self.advance();
                let fields = self.parse_fields()?;
                self.expect(&TokenKind::RBrace, "expected `}` closing variant payload")?;
                fields
            } else {
                Vec::new()
            };
            if is_default && !payload.is_empty() {
                return Err(parse_error(
                    case_span,
                    format!(
                        "@default is only allowed on unit cases; variant case `{case_name}` carries payload",
                    ),
                ));
            }
            cases.push(VariantCase {
                docs: case_docs,
                name: case_name,
                payload,
                is_default,
                span: case_span,
            });
            if self.check(&TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let _ = self.take_docs();
        self.expect(&TokenKind::RBrace, "expected `}` closing variant cases")?;
        Ok(VariantDef {
            docs,
            stability,
            name,
            cases,
            span,
        })
    }

    fn parse_enum(&mut self, docs: Docs, stability: Stability) -> Result<EnumDef, Error> {
        let span = self.expect(&TokenKind::Enum, "expected `enum`")?;
        let name = self.expect_identifier("expected enum name")?;
        self.expect(&TokenKind::LBrace, "expected `{` opening enum cases")?;
        let mut cases = Vec::new();
        while !self.check_after_docs(&TokenKind::RBrace) {
            let case_docs = self.take_docs();
            let is_default = self.consume_default_annotation();
            let case_span = self.peek().map_or(Span::new(0, 0), |t| t.span);
            let case_name = self.expect_identifier("expected enum case name")?;
            cases.push(EnumCase {
                docs: case_docs,
                name: case_name,
                is_default,
                span: case_span,
            });
            if self.check(&TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let _ = self.take_docs();
        self.expect(&TokenKind::RBrace, "expected `}` closing enum cases")?;
        Ok(EnumDef {
            docs,
            stability,
            name,
            cases,
            span,
        })
    }

    fn parse_flags(&mut self, docs: Docs, stability: Stability) -> Result<FlagsDef, Error> {
        let span = self.expect_contextual_keyword("flags", "expected `flags`")?;
        let name = self.expect_identifier("expected flags name")?;
        self.expect(&TokenKind::LBrace, "expected `{` opening flags")?;
        let mut flags = Vec::new();
        while !self.check_after_docs(&TokenKind::RBrace) {
            let flag_docs = self.take_docs();
            let flag_span = self.peek().map_or(Span::new(0, 0), |t| t.span);
            let flag_name = self.expect_identifier("expected flag name")?;
            flags.push(FlagDef {
                docs: flag_docs,
                name: flag_name,
                span: flag_span,
            });
            if self.check(&TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let _ = self.take_docs();
        self.expect(&TokenKind::RBrace, "expected `}` closing flags")?;
        Ok(FlagsDef {
            docs,
            stability,
            name,
            flags,
            span,
        })
    }

    fn parse_resource(&mut self, docs: Docs, stability: Stability) -> Result<ResourceDef, Error> {
        let span = self.expect_contextual_keyword("resource", "expected `resource`")?;
        let name = self.expect_identifier("expected resource name")?;
        self.expect(&TokenKind::LBrace, "expected `{` opening resource methods")?;
        let mut methods = Vec::new();
        while !self.check_after_docs(&TokenKind::RBrace) {
            let mut method_docs = self.take_docs();
            let mut annotations = self.parse_annotations()?;
            method_docs.extend(self.take_docs());
            let method_stability = take_stability(&mut annotations)?;
            reject_annotations(&annotations, "a resource method")?;
            let method_span = self.peek().map_or(Span::new(0, 0), |t| t.span);
            if self.check_identifier("constructor") {
                self.advance();
                let params = self.parse_params()?;
                self.expect(&TokenKind::Semicolon, "expected `;` ending constructor")?;
                methods.push(ResourceMethod {
                    docs: method_docs,
                    stability: method_stability,
                    kind: ResourceMethodKind::Constructor,
                    name: "constructor".to_string(),
                    params,
                    returns: None,
                    span: method_span,
                });
                continue;
            }
            let is_static = self.check_identifier("static");
            if is_static {
                self.advance();
            }
            let kind = match self.peek().map(|token| &token.kind) {
                Some(TokenKind::Query) if is_static => ResourceMethodKind::StaticQuery,
                Some(TokenKind::Command) if is_static => ResourceMethodKind::StaticCommand,
                Some(TokenKind::Query) => ResourceMethodKind::Query,
                Some(TokenKind::Command) => ResourceMethodKind::Command,
                other => {
                    return Err(parse_error(
                        method_span,
                        format!(
                            "expected `constructor`, `query`, `command`, or `static` in resource body (got {other:?})"
                        ),
                    ));
                }
            };
            self.advance();
            let method_name = self.expect_contextual_name("expected resource method name")?;
            let params = self.parse_params()?;
            self.expect(&TokenKind::Arrow, "expected `->` before return type")?;
            let returns = self.parse_type()?;
            self.expect(&TokenKind::Semicolon, "expected `;` ending resource method")?;
            methods.push(ResourceMethod {
                docs: method_docs,
                stability: method_stability,
                kind,
                name: method_name,
                params,
                returns: Some(returns),
                span: method_span,
            });
        }
        let _ = self.take_docs();
        self.expect(&TokenKind::RBrace, "expected `}` closing resource")?;
        Ok(ResourceDef {
            docs,
            stability,
            name,
            methods,
            span,
        })
    }

    fn parse_operation(
        &mut self,
        kind: OpKind,
        docs: Docs,
        stability: Stability,
    ) -> Result<Operation, Error> {
        let span = self.expect(
            match kind {
                OpKind::Query => &TokenKind::Query,
                OpKind::Command => &TokenKind::Command,
            },
            "expected operation keyword",
        )?;
        let name = self.expect_contextual_name("expected operation name")?;
        let params = self.parse_params()?;
        self.expect(&TokenKind::Arrow, "expected `->` before return type")?;
        let returns = self.parse_type()?;
        let emits = if self.check(&TokenKind::Emits) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(&TokenKind::Semicolon, "expected `;` ending operation")?;
        Ok(Operation {
            docs,
            stability,
            name,
            params,
            returns,
            emits,
            reentrant: false,
            access: None,
            span,
        })
    }

    /// `( [docs] [@secret] name: type, ... )`
    fn parse_params(&mut self) -> Result<Vec<Field>, Error> {
        self.expect(&TokenKind::LParen, "expected `(` opening params")?;
        let mut params = Vec::new();
        while !self.check_after_docs(&TokenKind::RParen) {
            let docs = self.take_docs();
            let param_span = self.peek().map_or(Span::new(0, 0), |t| t.span);
            let secret = self.parse_field_secret()?;
            let param_name = self.expect_identifier("expected parameter name")?;
            self.expect(&TokenKind::Colon, "expected `:` after parameter name")?;
            let ty = self.parse_type()?;
            params.push(Field {
                docs,
                secret,
                name: param_name,
                ty,
                span: param_span,
            });
            if self.check(&TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let _ = self.take_docs();
        self.expect(&TokenKind::RParen, "expected `)` closing params")?;
        Ok(params)
    }

    fn parse_events(
        &mut self,
        delivery: DeliveryMode,
        docs: Docs,
        stability: Stability,
    ) -> Result<EventsDecl, Error> {
        let span = self.expect(&TokenKind::Events, "expected `events`")?;
        let ty = self.parse_type()?;
        self.expect(
            &TokenKind::Semicolon,
            "expected `;` ending events declaration",
        )?;
        Ok(EventsDecl {
            docs,
            stability,
            ty,
            delivery,
            span,
        })
    }

    fn parse_world(&mut self, docs: Docs, stability: Stability) -> Result<World, Error> {
        let span = self.expect_contextual_keyword("world", "expected `world`")?;
        let name = self.expect_identifier("expected world name")?;
        self.expect(&TokenKind::LBrace, "expected `{` opening world")?;
        let mut items = Vec::new();
        while !self.check_after_docs(&TokenKind::RBrace) {
            items.push(self.parse_world_item()?);
        }
        let _ = self.take_docs();
        self.expect(&TokenKind::RBrace, "expected `}` closing world")?;
        Ok(World {
            docs,
            stability,
            name,
            items,
            span,
        })
    }

    /// Is the token after the cursor an identifier (contextual keyword use)?
    fn next_is_identifier(&self) -> bool {
        matches!(
            self.tokens.get(self.index + 1).map(|token| &token.kind),
            Some(TokenKind::Identifier(_))
        )
    }

    fn parse_world_item(&mut self) -> Result<WorldItem, Error> {
        let docs = self.take_docs();
        let span = self.peek().map_or(Span::new(0, 0), |t| t.span);
        let item = if self.check_identifier("provide") {
            self.advance();
            if self.check_identifier("opaque") && self.next_is_identifier() {
                self.advance();
                let name = self.expect_identifier("expected opaque service interface name")?;
                let version = self.parse_optional_version()?;
                self.expect(&TokenKind::Colon, "expected `:` before opaque service kind")?;
                let kind = match self.advance().map(|token| token.kind.clone()) {
                    Some(TokenKind::Query) => ServiceKindDecl::Query,
                    Some(TokenKind::Command) => ServiceKindDecl::Command,
                    Some(TokenKind::Events) => ServiceKindDecl::Events,
                    other => {
                        return Err(parse_error(
                            span,
                            format!(
                                "expected `query`, `command`, or `events` service kind (got {other:?})"
                            ),
                        ));
                    }
                };
                self.expect_contextual_keyword(
                    "capability",
                    "opaque services must name their `capability <id>`",
                )?;
                let capability = self.parse_dotted_ident("expected capability id")?;
                WorldItem::ProvideOpaque {
                    docs,
                    name,
                    version,
                    kind,
                    capability,
                    span,
                }
            } else if self.check_identifier("capability") && self.next_is_identifier() {
                self.advance();
                let capability = self.parse_dotted_ident("expected capability id")?;
                WorldItem::ProvideCapability {
                    docs,
                    capability,
                    span,
                }
            } else {
                let interface = self.parse_interface_ref()?;
                let capability = if self.check_identifier("capability") {
                    self.advance();
                    Some(self.parse_dotted_ident("expected capability id")?)
                } else {
                    None
                };
                WorldItem::Provide {
                    docs,
                    interface,
                    capability,
                    span,
                }
            }
        } else if self.check_identifier("require") {
            self.advance();
            let optional = self.check_identifier("optional") && self.next_is_identifier();
            if optional {
                self.advance();
            }
            if self.check_identifier("capability") && self.next_is_identifier() {
                self.advance();
                let capability = self.parse_dotted_ident("expected capability id")?;
                WorldItem::RequireCapability {
                    docs,
                    optional,
                    capability,
                    span,
                }
            } else {
                WorldItem::Require {
                    docs,
                    optional,
                    interface: self.parse_interface_ref()?,
                    span,
                }
            }
        } else {
            return Err(parse_error(
                span,
                format!(
                    "expected `provide` or `require` in world body (got {:?})",
                    self.peek().map(|token| &token.kind)
                ),
            ));
        };
        self.expect(&TokenKind::Semicolon, "expected `;` ending world item")?;
        Ok(item)
    }

    /// `name`, `alias.name`, either with an optional `@N` wire version.
    fn parse_interface_ref(&mut self) -> Result<InterfaceRef, Error> {
        let span = self.peek().map_or(Span::new(0, 0), |t| t.span);
        let first = self.expect_identifier("expected interface name")?;
        let (alias, name) = if self.check(&TokenKind::Dot) {
            self.advance();
            (
                Some(first),
                self.expect_identifier("expected interface name after `.`")?,
            )
        } else {
            (None, first)
        };
        let version = self.parse_optional_version()?;
        Ok(InterfaceRef {
            alias,
            name,
            version,
            span,
        })
    }

    fn parse_optional_version(&mut self) -> Result<Option<u32>, Error> {
        if !self.check(&TokenKind::At) {
            return Ok(None);
        }
        let span = self.expect(&TokenKind::At, "expected `@`")?;
        let version = self.expect_int("expected wire version after `@`")?;
        let version = u32::try_from(version)
            .ok()
            .filter(|version| *version > 0)
            .ok_or_else(|| parse_error(span, "wire version must be a positive u32"))?;
        Ok(Some(version))
    }

    fn parse_plugin_command(
        &mut self,
        docs: Docs,
        stability: Stability,
    ) -> Result<PluginCommand, Error> {
        let span = self.expect(&TokenKind::Command, "expected `command`")?;
        let name = self.expect_contextual_name("expected command name")?;
        let params = self.parse_params()?;
        let title = if self.check_identifier("title") {
            self.advance();
            Some(self.expect_string("expected string after `title`")?)
        } else {
            None
        };
        self.expect(&TokenKind::Semicolon, "expected `;` ending command")?;
        Ok(PluginCommand {
            docs,
            stability,
            name,
            params,
            title,
            span,
        })
    }

    fn parse_contribution(&mut self, docs: Docs) -> Result<Contribution, Error> {
        let span = self.expect_contextual_keyword("contribute", "expected `contribute`")?;
        let ref_span = self.peek().map_or(Span::new(0, 0), |t| t.span);
        let first = self.expect_identifier("expected extension point name")?;
        let (alias, name) = if self.check(&TokenKind::Dot) {
            self.advance();
            (
                Some(first),
                self.expect_identifier("expected extension point name after `.`")?,
            )
        } else {
            (None, first)
        };
        let version = self
            .parse_optional_version()?
            .ok_or_else(|| parse_error(ref_span, "contributions must name `extension-point@N`"))?;
        self.expect(&TokenKind::LBrace, "expected `{` opening contribution data")?;
        let mut values = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let field = self.expect_contextual_name("expected contribution field name")?;
            self.expect(&TokenKind::Colon, "expected `:` after contribution field")?;
            values.push((field, self.parse_literal()?));
            if self.check(&TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&TokenKind::RBrace, "expected `}` closing contribution data")?;
        Ok(Contribution {
            docs,
            extension_point: ExtensionPointRef {
                alias,
                name,
                version,
                span: ref_span,
            },
            values,
            span,
        })
    }

    fn parse_literal(&mut self) -> Result<Literal, Error> {
        let token = self.advance().cloned().ok_or_else(|| {
            parse_error(
                Span::new(0, 0),
                "expected literal (unexpected end of input)",
            )
        })?;
        match token.kind {
            TokenKind::StringLiteral(value) => Ok(Literal::String(value)),
            TokenKind::IntLiteral(value) => Ok(Literal::Int(value)),
            TokenKind::Identifier(value) if value == "true" => Ok(Literal::Bool(true)),
            TokenKind::Identifier(value) if value == "false" => Ok(Literal::Bool(false)),
            TokenKind::LBracket => {
                let mut items = Vec::new();
                while !self.check(&TokenKind::RBracket) {
                    items.push(self.parse_literal()?);
                    if self.check(&TokenKind::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.expect(&TokenKind::RBracket, "expected `]` closing list literal")?;
                Ok(Literal::List(items))
            }
            other => Err(parse_error(
                token.span,
                format!("expected string, integer, boolean, or list literal (got {other:?})"),
            )),
        }
    }

    fn parse_extension_point(
        &mut self,
        docs: Docs,
        stability: Stability,
    ) -> Result<ExtensionPoint, Error> {
        let span =
            self.expect_contextual_keyword("extension-point", "expected `extension-point`")?;
        let name = self.expect_identifier("expected extension point name")?;
        self.expect(
            &TokenKind::Version,
            "expected `version` after extension point name",
        )?;
        let version = self.expect_int("expected extension point version")?;
        let version = u32::try_from(version)
            .ok()
            .filter(|version| *version > 0)
            .ok_or_else(|| parse_error(span, "extension point version must be a positive u32"))?;
        self.expect(&TokenKind::LBrace, "expected `{` opening extension point")?;
        self.expect_contextual_keyword("data", "extension points start with `data { ... }`")?;
        self.expect(
            &TokenKind::LBrace,
            "expected `{` opening extension point data",
        )?;
        let data = self.parse_fields()?;
        self.expect(
            &TokenKind::RBrace,
            "expected `}` closing extension point data",
        )?;
        let mut operations = Vec::new();
        if self.check(&TokenKind::Interface) {
            self.advance();
            self.expect(
                &TokenKind::LBrace,
                "expected `{` opening extension point interface",
            )?;
            while !self.check_after_docs(&TokenKind::RBrace) {
                let mut op_docs = self.take_docs();
                let mut annotations = self.parse_annotations()?;
                op_docs.extend(self.take_docs());
                let op_stability = take_stability(&mut annotations)?;
                reject_annotations(&annotations, "an extension point operation")?;
                let operation = match self.peek().map(|token| &token.kind) {
                    Some(TokenKind::Query) => ExtensionOperation::Query(self.parse_operation(
                        OpKind::Query,
                        op_docs,
                        op_stability,
                    )?),
                    Some(TokenKind::Command) => ExtensionOperation::Command(self.parse_operation(
                        OpKind::Command,
                        op_docs,
                        op_stability,
                    )?),
                    other => {
                        return Err(parse_error(
                            span,
                            format!(
                                "expected `query` or `command` in extension point interface (got {other:?})"
                            ),
                        ));
                    }
                };
                operations.push(operation);
            }
            let _ = self.take_docs();
            self.expect(
                &TokenKind::RBrace,
                "expected `}` closing extension point interface",
            )?;
        }
        self.expect(&TokenKind::RBrace, "expected `}` closing extension point")?;
        Ok(ExtensionPoint {
            docs,
            stability,
            name,
            version,
            data,
            operations,
            span,
        })
    }

    /// Parse zero or more `@name` / `@name(arg)` annotations.
    fn parse_annotations(&mut self) -> Result<Vec<Annotation>, Error> {
        let mut annotations = Vec::new();
        while self.check(&TokenKind::At) {
            let at_span = self.expect(&TokenKind::At, "expected `@`")?;
            let (name, span) = match self.advance().cloned() {
                Some(Token {
                    kind: TokenKind::Identifier(name),
                    span,
                }) => (name, span),
                Some(other) => {
                    return Err(parse_error(
                        other.span,
                        format!("expected identifier after `@`; got {:?}", other.kind),
                    ));
                }
                None => {
                    return Err(parse_error(
                        at_span,
                        "expected annotation identifier after `@`",
                    ));
                }
            };
            let arg = if self.check(&TokenKind::LParen) {
                self.advance();
                let arg = match self.advance().cloned() {
                    Some(Token {
                        kind: TokenKind::Identifier(value),
                        ..
                    }) => AnnotationArg::Name(value),
                    Some(Token {
                        kind: TokenKind::IntLiteral(value),
                        ..
                    }) => AnnotationArg::Int(value),
                    other => {
                        return Err(parse_error(
                            span,
                            format!(
                                "expected name or integer argument in `@{name}(...)` (got {:?})",
                                other.map(|token| token.kind)
                            ),
                        ));
                    }
                };
                self.expect(
                    &TokenKind::RParen,
                    "expected `)` closing annotation argument",
                )?;
                Some(arg)
            } else {
                None
            };
            annotations.push(Annotation { name, arg, span });
        }
        Ok(annotations)
    }

    fn parse_field_secret(&mut self) -> Result<bool, Error> {
        if !self.check(&TokenKind::At) {
            return Ok(false);
        }
        let span = self.expect(&TokenKind::At, "expected `@`")?;
        let name = self.expect_identifier("expected field annotation")?;
        if name != "secret" || self.check(&TokenKind::At) {
            return Err(parse_error(
                span,
                "only one `@secret` annotation is allowed on a field or parameter",
            ));
        }
        Ok(true)
    }

    fn parse_fields(&mut self) -> Result<Vec<Field>, Error> {
        let mut fields = Vec::new();
        while !self.check_after_docs(&TokenKind::RBrace) {
            let docs = self.take_docs();
            let span = self.peek().map_or(Span::new(0, 0), |t| t.span);
            let secret = self.parse_field_secret()?;
            let name = self.expect_contextual_name("expected field name")?;
            self.expect(&TokenKind::Colon, "expected `:` after field name")?;
            let ty = self.parse_type()?;
            fields.push(Field {
                docs,
                secret,
                name,
                ty,
                span,
            });
            if self.check(&TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let _ = self.take_docs();
        Ok(fields)
    }

    fn parse_single_type_param(&mut self, keyword: &str) -> Result<TypeRef, Error> {
        self.expect(
            &TokenKind::LAngle,
            &format!("expected `<` after `{keyword}`"),
        )?;
        let inner = self.parse_type()?;
        self.expect(
            &TokenKind::RAngle,
            &format!("expected `>` closing `{keyword}<...>`"),
        )?;
        Ok(inner)
    }

    fn parse_type(&mut self) -> Result<TypeRef, Error> {
        let tok = self
            .peek()
            .cloned()
            .ok_or_else(|| parse_error(Span::new(0, 0), "unexpected end of input in type"))?;
        let next_is_angle = matches!(
            self.tokens.get(self.index + 1).map(|token| &token.kind),
            Some(TokenKind::LAngle)
        );
        let ty = match tok.kind {
            TokenKind::List => {
                self.advance();
                TypeRef::List(Box::new(self.parse_single_type_param("list")?))
            }
            TokenKind::Map => {
                self.advance();
                self.expect(&TokenKind::LAngle, "expected `<` after `map`")?;
                let key = self.parse_type()?;
                self.expect(&TokenKind::Comma, "expected `,` between map types")?;
                let value = self.parse_type()?;
                self.expect(&TokenKind::RAngle, "expected `>` closing `map<...>`")?;
                TypeRef::Map(Box::new(key), Box::new(value))
            }
            TokenKind::Result => {
                self.advance();
                self.expect(&TokenKind::LAngle, "expected `<` after `result`")?;
                let ok = self.parse_type()?;
                self.expect(&TokenKind::Comma, "expected `,` between result types")?;
                let err = self.parse_type()?;
                self.expect(&TokenKind::RAngle, "expected `>` closing `result<...>`")?;
                TypeRef::Result(Box::new(ok), Box::new(err))
            }
            TokenKind::Unit => {
                self.advance();
                TypeRef::Unit
            }
            // Contextual generic type constructors. Without a following `<`
            // these words remain ordinary type names.
            TokenKind::Identifier(ref name) if next_is_angle && name == "tuple" => {
                self.advance();
                self.expect(&TokenKind::LAngle, "expected `<` after `tuple`")?;
                let mut items = vec![self.parse_type()?];
                while self.check(&TokenKind::Comma) {
                    self.advance();
                    items.push(self.parse_type()?);
                }
                self.expect(&TokenKind::RAngle, "expected `>` closing `tuple<...>`")?;
                TypeRef::Tuple(items)
            }
            TokenKind::Identifier(ref name) if next_is_angle && name == "stream" => {
                self.advance();
                TypeRef::Stream(Box::new(self.parse_single_type_param("stream")?))
            }
            TokenKind::Identifier(ref name) if next_is_angle && name == "future" => {
                self.advance();
                TypeRef::Future(Box::new(self.parse_single_type_param("future")?))
            }
            TokenKind::Identifier(ref name) if next_is_angle && name == "own" => {
                self.advance();
                TypeRef::Own(Box::new(self.parse_single_type_param("own")?))
            }
            TokenKind::Identifier(ref name) if next_is_angle && name == "borrow" => {
                self.advance();
                TypeRef::Borrow(Box::new(self.parse_single_type_param("borrow")?))
            }
            TokenKind::Identifier(ref name) => {
                self.advance();
                // Qualified reference: `alias.type-name`.
                if self.check(&TokenKind::Dot) {
                    self.advance();
                    let type_name = self.expect_identifier(
                        "expected type name after `.` in qualified type reference",
                    )?;
                    TypeRef::Qualified {
                        alias: name.clone(),
                        name: type_name,
                    }
                } else if let Some(prim) = Primitive::from_keyword(name) {
                    TypeRef::Primitive(prim)
                } else {
                    TypeRef::Named(name.clone())
                }
            }
            _ => {
                return Err(parse_error(
                    tok.span,
                    format!("expected type, got {:?}", tok.kind),
                ));
            }
        };

        // Trailing `?` makes the type nullable.
        if self.check(&TokenKind::Question) {
            self.advance();
            Ok(TypeRef::Option(Box::new(ty)))
        } else {
            Ok(ty)
        }
    }

    /// Parse a dotted identifier sequence (`bmux.tabs`, `plugin.name`).
    /// Requires at least one identifier; subsequent `.<ident>` segments
    /// are joined with `.` in the returned string.
    fn parse_dotted_ident(&mut self, message: &str) -> Result<String, Error> {
        let mut out = self.expect_identifier(message)?;
        while self.check(&TokenKind::Dot) {
            self.advance();
            // Id segments may spell keywords (`bmux.events.subscribe`).
            let seg = match self.peek().and_then(|token| token.kind.keyword()) {
                Some(keyword) => {
                    self.advance();
                    keyword.to_string()
                }
                None => self.expect_identifier("expected identifier after `.`")?,
            };
            out.push('.');
            out.push_str(&seg);
        }
        Ok(out)
    }

    /// Consume `@default` if present. Returns true if consumed.
    fn consume_default_annotation(&mut self) -> bool {
        if !self.check(&TokenKind::At) {
            return false;
        }
        // Peek one past `@` for an identifier `default`.
        if let Some(Token {
            kind: TokenKind::Identifier(name),
            ..
        }) = self.tokens.get(self.index + 1)
            && name == "default"
        {
            self.advance(); // @
            self.advance(); // default
            return true;
        }
        false
    }

    /// Consume consecutive `///` documentation lines.
    fn take_docs(&mut self) -> Docs {
        let mut docs = Vec::new();
        while let Some(Token {
            kind: TokenKind::DocComment(line),
            ..
        }) = self.peek()
        {
            docs.push(line.clone());
            self.index += 1;
        }
        docs
    }

    /// Like [`Self::check`], looking past documentation lines.
    fn check_after_docs(&self, kind: &TokenKind) -> bool {
        self.tokens[self.index.min(self.tokens.len())..]
            .iter()
            .find(|token| !matches!(token.kind, TokenKind::DocComment(_)))
            .is_some_and(|token| &token.kind == kind)
    }

    fn check_identifier_after_docs(&self, value: &str) -> bool {
        self.tokens[self.index.min(self.tokens.len())..]
            .iter()
            .find(|token| !matches!(token.kind, TokenKind::DocComment(_)))
            .is_some_and(
                |token| matches!(&token.kind, TokenKind::Identifier(ident) if ident == value),
            )
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn advance(&mut self) -> Option<&Token> {
        let t = self.tokens.get(self.index);
        if t.is_some() {
            self.index += 1;
        }
        t
    }

    fn check(&self, kind: &TokenKind) -> bool {
        self.peek().is_some_and(|t| &t.kind == kind)
    }

    fn check_identifier(&self, value: &str) -> bool {
        matches!(self.peek().map(|t| &t.kind), Some(TokenKind::Identifier(ident)) if ident == value)
    }

    fn expect(&mut self, kind: &TokenKind, message: &str) -> Result<Span, Error> {
        let tok = self.peek().ok_or_else(|| {
            parse_error(
                Span::new(0, 0),
                format!("{message} (unexpected end of input)"),
            )
        })?;
        if &tok.kind == kind {
            let span = tok.span;
            self.advance();
            Ok(span)
        } else {
            Err(parse_error(
                tok.span,
                format!("{message} (got {:?})", tok.kind),
            ))
        }
    }

    fn expect_contextual_keyword(&mut self, value: &str, message: &str) -> Result<Span, Error> {
        let tok = self.peek().cloned().ok_or_else(|| {
            parse_error(
                Span::new(0, 0),
                format!("{message} (unexpected end of input)"),
            )
        })?;
        if let TokenKind::Identifier(name) = &tok.kind
            && name == value
        {
            self.advance();
            Ok(tok.span)
        } else {
            Err(parse_error(
                tok.span,
                format!("{message} (got {:?})", tok.kind),
            ))
        }
    }

    fn expect_contextual_name(&mut self, message: &str) -> Result<String, Error> {
        let tok = self.peek().cloned().ok_or_else(|| {
            parse_error(
                Span::new(0, 0),
                format!("{message} (unexpected end of input)"),
            )
        })?;
        let name = match tok.kind {
            TokenKind::Identifier(name) => name,
            TokenKind::List => "list".to_string(),
            TokenKind::Map => "map".to_string(),
            TokenKind::Result => "result".to_string(),
            TokenKind::Unit => "unit".to_string(),
            TokenKind::Events => "events".to_string(),
            other => {
                return Err(parse_error(tok.span, format!("{message} (got {other:?})")));
            }
        };
        self.advance();
        Ok(name)
    }

    fn expect_identifier(&mut self, message: &str) -> Result<String, Error> {
        let tok = self.peek().cloned().ok_or_else(|| {
            parse_error(
                Span::new(0, 0),
                format!("{message} (unexpected end of input)"),
            )
        })?;
        if let TokenKind::Identifier(name) = tok.kind {
            self.advance();
            Ok(name)
        } else {
            Err(parse_error(
                tok.span,
                format!("{message} (got {:?})", tok.kind),
            ))
        }
    }

    fn expect_string(&mut self, message: &str) -> Result<String, Error> {
        let tok = self.peek().cloned().ok_or_else(|| {
            parse_error(
                Span::new(0, 0),
                format!("{message} (unexpected end of input)"),
            )
        })?;
        if let TokenKind::StringLiteral(value) = tok.kind {
            self.advance();
            Ok(value)
        } else {
            Err(parse_error(
                tok.span,
                format!("{message} (got {:?})", tok.kind),
            ))
        }
    }

    fn expect_int(&mut self, message: &str) -> Result<u64, Error> {
        let tok = self.peek().cloned().ok_or_else(|| {
            parse_error(
                Span::new(0, 0),
                format!("{message} (unexpected end of input)"),
            )
        })?;
        if let TokenKind::IntLiteral(n) = tok.kind {
            self.advance();
            Ok(n)
        } else {
            Err(parse_error(
                tok.span,
                format!("{message} (got {:?})", tok.kind),
            ))
        }
    }
}

#[derive(Clone, Copy)]
enum OpKind {
    Query,
    Command,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn must_parse(source: &str) -> Schema {
        let tokens = tokenize(source).expect("lex");
        parse(&tokens).expect("parse")
    }

    #[test]
    fn parses_plugin_header_only() {
        let schema = must_parse("plugin bmux.tabs version 1;");
        assert_eq!(schema.plugin.plugin_id, "bmux.tabs");
        assert_eq!(schema.plugin.version, 1);
        assert_eq!(schema.interfaces, [] as [crate::ast::Interface; 0]);
        assert_eq!(schema.imports, [] as [crate::ast::Import; 0]);
    }

    #[test]
    fn parses_record_with_primitive_and_option() {
        let schema = must_parse(
            "plugin p version 1;\n\
             interface i {\n\
               record r { id: uuid, name: string?, count: u32 }\n\
             }",
        );
        let InterfaceItem::Record(rec) = &schema.interfaces[0].items[0] else {
            panic!("expected record");
        };
        assert_eq!(rec.name, "r");
        assert_eq!(rec.fields.len(), 3);
    }

    #[test]
    fn parses_variant_with_payload() {
        let schema = must_parse(
            "plugin p version 1;\n\
             interface i {\n\
               variant v {\n\
                 on,\n\
                 off { reason: string },\n\
               }\n\
             }",
        );
        let InterfaceItem::Variant(var) = &schema.interfaces[0].items[0] else {
            panic!("expected variant");
        };
        assert_eq!(var.cases.len(), 2);
        assert_eq!(var.cases[0].payload.len(), 0);
        assert_eq!(var.cases[1].payload.len(), 1);
    }

    #[test]
    fn parses_type_keywords_as_contextual_operation_and_field_names() {
        let schema = must_parse(
            "plugin p version 1;\n\
             interface state {\n\
               record event-list { events: list<string> }\n\
               query list() -> event-list;\n\
             }",
        );
        let InterfaceItem::Record(record) = &schema.interfaces[0].items[0] else {
            panic!("expected record");
        };
        assert_eq!(record.fields[0].name, "events");
        let InterfaceItem::Query(operation) = &schema.interfaces[0].items[1] else {
            panic!("expected query");
        };
        assert_eq!(operation.name, "list");
    }

    #[test]
    fn parses_versioned_interface_annotations_in_either_order() {
        let schema = must_parse(
            "plugin p version 1;\n\
             capability READ = p.read;\n\
             @interface-version(2)\n\
             @capability(READ)\n\
             interface state { query get() -> bool; }",
        );
        let interface = &schema.interfaces[0];
        assert_eq!(interface.name, "state");
        assert_eq!(interface.interface_version, Some(2));
        assert_eq!(interface.capability.as_deref(), Some("READ"));
    }

    #[test]
    fn rejects_zero_interface_version() {
        let tokens = tokenize(
            "plugin p version 1;\n\
             @interface-version(0) interface state {}",
        )
        .expect("lex");
        let error = parse(&tokens).expect_err("zero interface version should fail");
        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn rejects_duplicate_interface_version() {
        let tokens = tokenize(
            "plugin p version 1;\n\
             @interface-version(1) @interface-version(2) interface state {}",
        )
        .expect("lex");
        let error = parse(&tokens).expect_err("duplicate interface version should fail");
        assert!(error.to_string().contains("duplicate `@interface-version`"));
    }

    #[test]
    fn parses_query_command_events() {
        let schema = must_parse(
            "plugin p version 1;\n\
             interface i {\n\
               record e { kind: u32 }\n\
               query q(id: uuid) -> bool;\n\
               command c(id: uuid) -> result<unit, string>;\n\
               events e;\n\
             }",
        );
        let items = &schema.interfaces[0].items;
        assert!(matches!(items[1], InterfaceItem::Query(_)));
        assert!(matches!(items[2], InterfaceItem::Command(_)));
        let InterfaceItem::Events(decl) = &items[3] else {
            panic!("expected events");
        };
        assert_eq!(decl.delivery, DeliveryMode::Broadcast);
    }

    #[test]
    fn parses_state_annotated_events() {
        let schema = must_parse(
            "plugin p version 1;\n\
             interface focus {\n\
               record focus-state { pane_id: uuid }\n\
               @state events focus-state;\n\
             }",
        );
        let InterfaceItem::Events(decl) = &schema.interfaces[0].items[1] else {
            panic!("expected events declaration");
        };
        assert_eq!(decl.delivery, DeliveryMode::State);
    }

    #[test]
    fn rejects_unknown_events_annotation() {
        let source = "plugin p version 1;\n\
                      interface focus {\n\
                        record r { pane_id: uuid }\n\
                        @durable events r;\n\
                      }";
        let tokens = tokenize(source).expect("lex");
        let err = parse(&tokens).expect_err("unknown annotation must fail");
        match err {
            Error::Parse { message, .. } => {
                assert!(
                    message.contains("@durable"),
                    "parse error should mention the unknown annotation name; got: {message}",
                );
            }
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    fn parse_error(source: &str) -> String {
        let tokens = tokenize(source).expect("lex");
        match parse(&tokens).expect_err("must be rejected") {
            Error::Parse { message, .. } => message,
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    #[test]
    fn parses_reentrant_query_marker() {
        let schema = must_parse(
            "plugin p version 1;\n\
             interface i {\n\
               @reentrant query provenance() -> u32;\n\
               query plain() -> u32;\n\
             }",
        );
        let InterfaceItem::Query(marked) = &schema.interfaces[0].items[0] else {
            panic!("expected query");
        };
        assert_eq!(marked.name, "provenance");
        assert!(marked.reentrant);
        let InterfaceItem::Query(plain) = &schema.interfaces[0].items[1] else {
            panic!("expected query");
        };
        assert!(!plain.reentrant);
    }

    #[test]
    fn rejects_reentrant_on_commands_streams_and_other_items() {
        let command = parse_error(
            "plugin p version 1; interface i { @reentrant command set(n: u32) -> unit; }",
        );
        assert!(
            command.contains("only allowed on `query`"),
            "command rejection must explain the restriction; got: {command}"
        );
        let streaming = parse_error(
            "plugin p version 1; interface i { record e { n: u32 } @reentrant query watch() -> unit emits e; }",
        );
        assert!(
            streaming.contains("streaming operation `watch`"),
            "streaming rejection must name the operation; got: {streaming}"
        );
        let events = parse_error(
            "plugin p version 1; interface i { record e { n: u32 } @reentrant events e; }",
        );
        assert!(events.contains("must precede a `query`"), "got: {events}");
        let doubled = parse_error(
            "plugin p version 1; interface i { @reentrant @reentrant query get() -> u32; }",
        );
        assert!(doubled.contains("directly precede"), "got: {doubled}");
    }

    #[test]
    fn parses_interface_access_and_operation_overrides() {
        let schema = must_parse(
            "plugin p version 1;\n\
             @access(observe) @capability(READ)\n\
             interface i {\n\
               query get() -> u32;\n\
               @access(owner-local) query secret() -> u32;\n\
               @reentrant @access(input) query poke() -> u32;\n\
             }\n\
             @capability(WRITE) interface c { @access(manage) command set(n: u32) -> unit; }",
        );
        let iface = &schema.interfaces[0];
        assert_eq!(iface.access, Some(Access::Observe));
        let access = |index: usize| match &iface.items[index] {
            InterfaceItem::Query(op) => op.access,
            other => panic!("expected query, got {other:?}"),
        };
        assert_eq!(access(0), None);
        assert_eq!(access(1), Some(Access::OwnerLocal));
        assert_eq!(access(2), Some(Access::Input));
        assert_eq!(schema.interfaces[1].access, None);
        let InterfaceItem::Command(set) = &schema.interfaces[1].items[0] else {
            panic!("expected command");
        };
        assert_eq!(set.access, Some(Access::Manage));
    }

    #[test]
    fn rejects_bad_access_annotations() {
        let unknown =
            parse_error("plugin p version 1; @access(admin) interface i { query get() -> u32; }");
        assert!(
            unknown.contains("unknown access class `admin`"),
            "got: {unknown}"
        );
        let missing =
            parse_error("plugin p version 1; @access interface i { query get() -> u32; }");
        assert!(missing.contains("expected access class"), "got: {missing}");
        let doubled = parse_error(
            "plugin p version 1; interface i { @access(input) @access(observe) query get() -> u32; }",
        );
        assert!(doubled.contains("duplicate `@access`"), "got: {doubled}");
        let doubled_iface = parse_error(
            "plugin p version 1; @access(input) @access(observe) interface i { query get() -> u32; }",
        );
        assert!(
            doubled_iface.contains("duplicate `@access`"),
            "got: {doubled_iface}"
        );
        let on_events = parse_error(
            "plugin p version 1; interface i { record e { n: u32 } @access(observe) events e; }",
        );
        assert!(on_events.contains("@access"), "got: {on_events}");
        let on_record =
            parse_error("plugin p version 1; interface i { @access(observe) record e { n: u32 } }");
        assert!(on_record.contains("@access"), "got: {on_record}");
    }

    #[test]
    fn parses_import_directive() {
        let schema = must_parse(
            "plugin p version 1;\n\
             import windows = bmux.tabs;\n\
             interface i { record r { id: uuid } }",
        );
        assert_eq!(schema.imports.len(), 1);
        assert_eq!(schema.imports[0].alias, "windows");
        assert_eq!(schema.imports[0].plugin_id, "bmux.tabs");
    }

    #[test]
    fn parses_capability_directive() {
        let schema = must_parse(
            "plugin p version 1;\n\
              capability FOO_READ = bmux.foo.read;\n\
              interface i { query q() -> unit; }",
        );
        assert_eq!(schema.capabilities.len(), 1);
        assert_eq!(schema.capabilities[0].name, "FOO_READ");
        assert_eq!(schema.capabilities[0].id, "bmux.foo.read");
    }

    #[test]
    fn parses_map_type() {
        let schema = must_parse(
            "plugin p version 1;\n\
             interface i {\n\
               record r { labels: map<string, u32> }\n\
             }",
        );
        let InterfaceItem::Record(rec) = &schema.interfaces[0].items[0] else {
            panic!("expected record");
        };
        let TypeRef::Map(k, v) = &rec.fields[0].ty else {
            panic!("expected map type");
        };
        assert!(matches!(**k, TypeRef::Primitive(Primitive::String)));
        assert!(matches!(**v, TypeRef::Primitive(Primitive::U32)));
    }

    #[test]
    fn parses_qualified_type_reference() {
        let schema = must_parse(
            "plugin p version 1;\n\
             import windows = bmux.tabs;\n\
             interface i {\n\
               query q(id: uuid) -> windows.pane-state;\n\
             }",
        );
        let InterfaceItem::Query(op) = &schema.interfaces[0].items[0] else {
            panic!("expected query");
        };
        let TypeRef::Qualified { alias, name } = &op.returns else {
            panic!("expected qualified type ref");
        };
        assert_eq!(alias, "windows");
        assert_eq!(name, "pane-state");
    }

    #[test]
    fn parses_default_on_enum_case() {
        let schema = must_parse(
            "plugin p version 1;\n\
             interface i {\n\
               enum e { a, @default b, c }\n\
             }",
        );
        let InterfaceItem::Enum(en) = &schema.interfaces[0].items[0] else {
            panic!("expected enum");
        };
        assert!(!en.cases[0].is_default);
        assert!(en.cases[1].is_default);
        assert!(!en.cases[2].is_default);
    }

    #[test]
    fn parses_request_scoped_streaming_command() {
        let schema = must_parse(
            "plugin p version 1;\n\
             interface i {\n\
               record start { prompt: string }\n\
               record finish { text: string }\n\
               variant turn-event { delta { text: string }, done }\n\
               command start-turn(request: start) -> finish emits turn-event;\n\
             }",
        );
        let InterfaceItem::Command(op) = &schema.interfaces[0].items[3] else {
            panic!("expected command");
        };
        assert_eq!(op.name, "start-turn");
        assert!(matches!(op.emits, Some(TypeRef::Named(ref name)) if name == "turn-event"));
    }

    #[test]
    fn rejects_unknown_streaming_event_type() {
        let tokens = tokenize(
            "plugin p version 1;\n\
             interface i {\n\
               record start { prompt: string }\n\
               record finish { text: string }\n\
               command start-turn(request: start) -> finish emits missing-event;\n\
             }",
        )
        .expect("lex");
        let schema = parse(&tokens).expect("parse");
        let err = crate::validator::validate(&schema).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn rejects_default_on_variant_case_with_payload() {
        let tokens = tokenize(
            "plugin p version 1;\n\
             interface i {\n\
               variant v { @default on { reason: string }, off }\n\
             }",
        )
        .expect("lex");
        let err = parse(&tokens).unwrap_err();
        assert!(matches!(err, Error::Parse { .. }));
    }
}
