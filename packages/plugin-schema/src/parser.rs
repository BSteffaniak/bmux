//! Recursive-descent parser that produces [`crate::ast`] nodes from tokens.

use crate::{
    Error, Span,
    ast::{
        CapabilityDecl, DeliveryMode, EnumCase, EnumDef, EventsDecl, Field, Import, Interface,
        InterfaceItem, Operation, PluginHeader, Primitive, RecordDef, Schema, TypeRef, VariantCase,
        VariantDef,
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

impl Parser<'_> {
    fn parse_schema(&mut self) -> Result<Schema, Error> {
        let plugin = self.parse_plugin_header()?;
        let mut imports = Vec::new();
        while self.check(&TokenKind::Import) {
            imports.push(self.parse_import()?);
        }
        let mut capabilities = Vec::new();
        while self.check_identifier("capability") {
            capabilities.push(self.parse_capability()?);
        }
        let mut interfaces = Vec::new();
        while self.peek().is_some() {
            interfaces.push(self.parse_interface()?);
        }
        Ok(Schema {
            plugin,
            imports,
            capabilities,
            interfaces,
        })
    }

    fn parse_plugin_header(&mut self) -> Result<PluginHeader, Error> {
        let span = self.expect(&TokenKind::Plugin, "expected `plugin` keyword")?;
        let plugin_id = self.parse_dotted_ident("expected plugin id after `plugin`")?;
        self.expect(&TokenKind::Version, "expected `version` keyword")?;
        let version = self.expect_int("expected integer version literal")?;
        self.expect(&TokenKind::Semicolon, "expected `;` ending plugin header")?;
        let version = u32::try_from(version).map_err(|_| Error::Parse {
            span,
            message: format!("plugin version {version} out of u32 range"),
        })?;
        Ok(PluginHeader {
            plugin_id,
            version,
            span,
        })
    }

    fn parse_import(&mut self) -> Result<Import, Error> {
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
        let span = self.expect_contextual_keyword("capability", "expected `capability` keyword")?;
        let name = self.expect_identifier("expected capability constant name")?;
        self.expect(&TokenKind::Equals, "expected `=` after capability name")?;
        let id = self.parse_dotted_ident("expected capability id")?;
        self.expect(&TokenKind::Semicolon, "expected `;` ending capability")?;
        Ok(CapabilityDecl { name, id, span })
    }

    fn parse_interface(&mut self) -> Result<Interface, Error> {
        let mut capability = None;
        let mut interface_version = None;
        while self.check(&TokenKind::At) {
            match self.consume_interface_annotation()? {
                InterfaceAnnotation::Capability(value) => {
                    if capability.replace(value).is_some() {
                        return Err(Error::Parse {
                            span: self.peek().map_or(Span::new(0, 0), |token| token.span),
                            message: "duplicate `@capability` interface annotation".to_string(),
                        });
                    }
                }
                InterfaceAnnotation::Version(value) => {
                    if interface_version.replace(value).is_some() {
                        return Err(Error::Parse {
                            span: self.peek().map_or(Span::new(0, 0), |token| token.span),
                            message: "duplicate `@interface-version` annotation".to_string(),
                        });
                    }
                }
            }
        }
        let span = self.expect(&TokenKind::Interface, "expected `interface` keyword")?;
        let name = self.expect_identifier("expected interface name")?;
        self.expect(&TokenKind::LBrace, "expected `{` opening interface body")?;
        let mut items = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            items.push(self.parse_interface_item()?);
        }
        self.expect(&TokenKind::RBrace, "expected `}` closing interface body")?;
        Ok(Interface {
            name,
            interface_version,
            capability,
            items,
            span,
        })
    }

    fn parse_interface_item(&mut self) -> Result<InterfaceItem, Error> {
        let tok = self.peek().cloned().ok_or_else(|| Error::Parse {
            span: Span::new(0, 0),
            message: "unexpected end of input inside interface".to_string(),
        })?;
        match tok.kind {
            TokenKind::Record => Ok(InterfaceItem::Record(self.parse_record()?)),
            TokenKind::Variant => Ok(InterfaceItem::Variant(self.parse_variant()?)),
            TokenKind::Enum => Ok(InterfaceItem::Enum(self.parse_enum()?)),
            TokenKind::Query => Ok(InterfaceItem::Query(self.parse_operation(OpKind::Query)?)),
            TokenKind::Command => Ok(InterfaceItem::Command(
                self.parse_operation(OpKind::Command)?,
            )),
            TokenKind::Events => Ok(InterfaceItem::Events(
                self.parse_events(DeliveryMode::Broadcast)?,
            )),
            TokenKind::At => {
                // Annotation prefix. Currently only `@state events T;` is
                // recognised; future annotations (e.g. `@durable`)
                // extend this arm. The annotation may only precede an
                // `events` declaration.
                let delivery = self.consume_events_delivery_annotation()?;
                Ok(InterfaceItem::Events(self.parse_events(delivery)?))
            }
            _ => Err(Error::Parse {
                span: tok.span,
                message: format!("unexpected token in interface body: {:?}", tok.kind),
            }),
        }
    }

    fn parse_record(&mut self) -> Result<RecordDef, Error> {
        let span = self.expect(&TokenKind::Record, "expected `record`")?;
        let name = self.expect_identifier("expected record name")?;
        self.expect(&TokenKind::LBrace, "expected `{` opening record fields")?;
        let fields = self.parse_fields()?;
        self.expect(&TokenKind::RBrace, "expected `}` closing record fields")?;
        Ok(RecordDef { name, fields, span })
    }

    fn parse_variant(&mut self) -> Result<VariantDef, Error> {
        let span = self.expect(&TokenKind::Variant, "expected `variant`")?;
        let name = self.expect_identifier("expected variant name")?;
        self.expect(&TokenKind::LBrace, "expected `{` opening variant cases")?;
        let mut cases = Vec::new();
        while !self.check(&TokenKind::RBrace) {
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
                return Err(Error::Parse {
                    span: case_span,
                    message: format!(
                        "@default is only allowed on unit cases; variant case `{case_name}` carries payload",
                    ),
                });
            }
            cases.push(VariantCase {
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
        self.expect(&TokenKind::RBrace, "expected `}` closing variant cases")?;
        Ok(VariantDef { name, cases, span })
    }

    fn parse_enum(&mut self) -> Result<EnumDef, Error> {
        let span = self.expect(&TokenKind::Enum, "expected `enum`")?;
        let name = self.expect_identifier("expected enum name")?;
        self.expect(&TokenKind::LBrace, "expected `{` opening enum cases")?;
        let mut cases = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let is_default = self.consume_default_annotation();
            let case_span = self.peek().map_or(Span::new(0, 0), |t| t.span);
            let case_name = self.expect_identifier("expected enum case name")?;
            cases.push(EnumCase {
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
        self.expect(&TokenKind::RBrace, "expected `}` closing enum cases")?;
        Ok(EnumDef { name, cases, span })
    }

    fn parse_operation(&mut self, kind: OpKind) -> Result<Operation, Error> {
        let span = self.expect(
            match kind {
                OpKind::Query => &TokenKind::Query,
                OpKind::Command => &TokenKind::Command,
            },
            "expected operation keyword",
        )?;
        let name = self.expect_contextual_name("expected operation name")?;
        self.expect(&TokenKind::LParen, "expected `(` opening params")?;
        let mut params = Vec::new();
        while !self.check(&TokenKind::RParen) {
            let param_span = self.peek().map_or(Span::new(0, 0), |t| t.span);
            let param_name = self.expect_identifier("expected parameter name")?;
            self.expect(&TokenKind::Colon, "expected `:` after parameter name")?;
            let ty = self.parse_type()?;
            params.push(Field {
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
        self.expect(&TokenKind::RParen, "expected `)` closing params")?;
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
            name,
            params,
            returns,
            emits,
            span,
        })
    }

    fn parse_events(&mut self, delivery: DeliveryMode) -> Result<EventsDecl, Error> {
        let span = self.expect(&TokenKind::Events, "expected `events`")?;
        let ty = self.parse_type()?;
        self.expect(
            &TokenKind::Semicolon,
            "expected `;` ending events declaration",
        )?;
        Ok(EventsDecl { ty, delivery, span })
    }

    /// Consume an `@state` prefix annotation if present and return the
    /// resulting delivery mode. Only `@state` is currently recognised;
    /// any other `@<ident>` sequence before an `events` declaration is
    /// rejected with a parse error pointing at the annotation site.
    fn consume_events_delivery_annotation(&mut self) -> Result<DeliveryMode, Error> {
        let at_span = self.expect(&TokenKind::At, "expected `@`")?;
        let (name, span) = match self.advance().cloned() {
            Some(Token {
                kind: TokenKind::Identifier(name),
                span,
            }) => (name, span),
            Some(other) => {
                return Err(Error::Parse {
                    span: other.span,
                    message: format!("expected identifier after `@`; got {:?}", other.kind),
                });
            }
            None => {
                return Err(Error::Parse {
                    span: at_span,
                    message: "expected annotation identifier after `@`".to_string(),
                });
            }
        };
        match name.as_str() {
            "state" => Ok(DeliveryMode::State),
            other => Err(Error::Parse {
                span,
                message: format!(
                    "unknown annotation `@{other}`; only `@state` is recognised as a delivery hint on `events` declarations",
                ),
            }),
        }
    }

    /// Consume one interface-level annotation.
    fn consume_interface_annotation(&mut self) -> Result<InterfaceAnnotation, Error> {
        let at_span = self.expect(&TokenKind::At, "expected `@`")?;
        let (name, span) = match self.advance().cloned() {
            Some(Token {
                kind: TokenKind::Identifier(name),
                span,
            }) => (name, span),
            Some(other) => {
                return Err(Error::Parse {
                    span: other.span,
                    message: format!("expected identifier after `@`; got {:?}", other.kind),
                });
            }
            None => {
                return Err(Error::Parse {
                    span: at_span,
                    message: "expected annotation identifier after `@`".to_string(),
                });
            }
        };
        match name.as_str() {
            "capability" => {
                self.expect(&TokenKind::LParen, "expected `(` after `@capability`")?;
                let capability = self.expect_identifier("expected capability constant name")?;
                self.expect(&TokenKind::RParen, "expected `)` after capability name")?;
                Ok(InterfaceAnnotation::Capability(capability))
            }
            "interface-version" => {
                self.expect(
                    &TokenKind::LParen,
                    "expected `(` after `@interface-version`",
                )?;
                let version = self.expect_int("expected interface version integer")?;
                self.expect(&TokenKind::RParen, "expected `)` after interface version")?;
                let version = u32::try_from(version).map_err(|_| Error::Parse {
                    span,
                    message: "interface version must fit in u32".to_string(),
                })?;
                if version == 0 {
                    return Err(Error::Parse {
                        span,
                        message: "interface version must be greater than zero".to_string(),
                    });
                }
                Ok(InterfaceAnnotation::Version(version))
            }
            other => Err(Error::Parse {
                span,
                message: format!(
                    "unknown interface annotation `@{other}`; expected `@capability(NAME)` or `@interface-version(N)`"
                ),
            }),
        }
    }

    fn parse_fields(&mut self) -> Result<Vec<Field>, Error> {
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let span = self.peek().map_or(Span::new(0, 0), |t| t.span);
            let name = self.expect_contextual_name("expected field name")?;
            self.expect(&TokenKind::Colon, "expected `:` after field name")?;
            let ty = self.parse_type()?;
            fields.push(Field { name, ty, span });
            if self.check(&TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(fields)
    }

    fn parse_type(&mut self) -> Result<TypeRef, Error> {
        let tok = self.peek().cloned().ok_or_else(|| Error::Parse {
            span: Span::new(0, 0),
            message: "unexpected end of input in type".to_string(),
        })?;
        let ty = match tok.kind {
            TokenKind::List => {
                self.advance();
                self.expect(&TokenKind::LAngle, "expected `<` after `list`")?;
                let inner = self.parse_type()?;
                self.expect(&TokenKind::RAngle, "expected `>` closing `list<...>`")?;
                TypeRef::List(Box::new(inner))
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
                return Err(Error::Parse {
                    span: tok.span,
                    message: format!("expected type, got {:?}", tok.kind),
                });
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

    /// Parse a dotted identifier sequence (`bmux.windows`, `plugin.name`).
    /// Requires at least one identifier; subsequent `.<ident>` segments
    /// are joined with `.` in the returned string.
    fn parse_dotted_ident(&mut self, message: &str) -> Result<String, Error> {
        let mut out = self.expect_identifier(message)?;
        while self.check(&TokenKind::Dot) {
            self.advance();
            let seg = self.expect_identifier("expected identifier after `.`")?;
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
        let tok = self.peek().ok_or_else(|| Error::Parse {
            span: Span::new(0, 0),
            message: format!("{message} (unexpected end of input)"),
        })?;
        if &tok.kind == kind {
            let span = tok.span;
            self.advance();
            Ok(span)
        } else {
            Err(Error::Parse {
                span: tok.span,
                message: format!("{message} (got {:?})", tok.kind),
            })
        }
    }

    fn expect_contextual_keyword(&mut self, value: &str, message: &str) -> Result<Span, Error> {
        let tok = self.peek().cloned().ok_or_else(|| Error::Parse {
            span: Span::new(0, 0),
            message: format!("{message} (unexpected end of input)"),
        })?;
        if let TokenKind::Identifier(name) = &tok.kind
            && name == value
        {
            self.advance();
            Ok(tok.span)
        } else {
            Err(Error::Parse {
                span: tok.span,
                message: format!("{message} (got {:?})", tok.kind),
            })
        }
    }

    fn expect_contextual_name(&mut self, message: &str) -> Result<String, Error> {
        let tok = self.peek().cloned().ok_or_else(|| Error::Parse {
            span: Span::new(0, 0),
            message: format!("{message} (unexpected end of input)"),
        })?;
        let name = match tok.kind {
            TokenKind::Identifier(name) => name,
            TokenKind::List => "list".to_string(),
            TokenKind::Map => "map".to_string(),
            TokenKind::Result => "result".to_string(),
            TokenKind::Unit => "unit".to_string(),
            TokenKind::Events => "events".to_string(),
            other => {
                return Err(Error::Parse {
                    span: tok.span,
                    message: format!("{message} (got {other:?})"),
                });
            }
        };
        self.advance();
        Ok(name)
    }

    fn expect_identifier(&mut self, message: &str) -> Result<String, Error> {
        let tok = self.peek().cloned().ok_or_else(|| Error::Parse {
            span: Span::new(0, 0),
            message: format!("{message} (unexpected end of input)"),
        })?;
        if let TokenKind::Identifier(name) = tok.kind {
            self.advance();
            Ok(name)
        } else {
            Err(Error::Parse {
                span: tok.span,
                message: format!("{message} (got {:?})", tok.kind),
            })
        }
    }

    fn expect_int(&mut self, message: &str) -> Result<u64, Error> {
        let tok = self.peek().cloned().ok_or_else(|| Error::Parse {
            span: Span::new(0, 0),
            message: format!("{message} (unexpected end of input)"),
        })?;
        if let TokenKind::IntLiteral(n) = tok.kind {
            self.advance();
            Ok(n)
        } else {
            Err(Error::Parse {
                span: tok.span,
                message: format!("{message} (got {:?})", tok.kind),
            })
        }
    }
}

enum InterfaceAnnotation {
    Capability(String),
    Version(u32),
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
        let schema = must_parse("plugin bmux.windows version 1;");
        assert_eq!(schema.plugin.plugin_id, "bmux.windows");
        assert_eq!(schema.plugin.version, 1);
        assert!(schema.interfaces.is_empty());
        assert!(schema.imports.is_empty());
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

    #[test]
    fn parses_import_directive() {
        let schema = must_parse(
            "plugin p version 1;\n\
             import windows = bmux.windows;\n\
             interface i { record r { id: uuid } }",
        );
        assert_eq!(schema.imports.len(), 1);
        assert_eq!(schema.imports[0].alias, "windows");
        assert_eq!(schema.imports[0].plugin_id, "bmux.windows");
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
             import windows = bmux.windows;\n\
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
