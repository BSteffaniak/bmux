//! Recursive-descent parser that produces [`crate::ast`] nodes from tokens.

use crate::{
    Error, Span,
    ast::{
        EnumCase, EnumDef, Field, Import, Interface, InterfaceItem, Operation, PluginHeader,
        Primitive, RecordDef, Schema, TypeRef, VariantCase, VariantDef,
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
        let mut interfaces = Vec::new();
        while self.peek().is_some() {
            interfaces.push(self.parse_interface()?);
        }
        Ok(Schema {
            plugin,
            imports,
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

    fn parse_interface(&mut self) -> Result<Interface, Error> {
        let span = self.expect(&TokenKind::Interface, "expected `interface` keyword")?;
        let name = self.expect_identifier("expected interface name")?;
        self.expect(&TokenKind::LBrace, "expected `{` opening interface body")?;
        let mut items = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            items.push(self.parse_interface_item()?);
        }
        self.expect(&TokenKind::RBrace, "expected `}` closing interface body")?;
        Ok(Interface { name, items, span })
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
            TokenKind::Events => Ok(InterfaceItem::Events(self.parse_events()?)),
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
        let name = self.expect_identifier("expected operation name")?;
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
        self.expect(&TokenKind::Semicolon, "expected `;` ending operation")?;
        Ok(Operation {
            name,
            params,
            returns,
            span,
        })
    }

    fn parse_events(&mut self) -> Result<TypeRef, Error> {
        self.expect(&TokenKind::Events, "expected `events`")?;
        let ty = self.parse_type()?;
        self.expect(
            &TokenKind::Semicolon,
            "expected `;` ending events declaration",
        )?;
        Ok(ty)
    }

    fn parse_fields(&mut self) -> Result<Vec<Field>, Error> {
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let span = self.peek().map_or(Span::new(0, 0), |t| t.span);
            let name = self.expect_identifier("expected field name")?;
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
        assert!(matches!(items[3], InterfaceItem::Events(_)));
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
