//! BMUX Plugin Definition Language (BPDL).
//!
//! BPDL is a small, purpose-built IDL for declaring the typed contracts
//! between BMUX plugins. A `.bpdl` file defines one *plugin interface* —
//! the plugin's version and the set of records, variants, enums, queries,
//! commands, and events it exposes. Plugins ship their schema alongside
//! their manifest; consumers import those schemas and get typed bindings.
//!
//! This crate contains:
//!
//! - The [`ast`] module: typed AST produced by the parser.
//! - The [`lexer`] module: tokenizer.
//! - The [`parser`] module: recursive-descent parser producing [`ast`] nodes.
//! - The [`validator`] module: semantic checks (duplicate names, unknown
//!   type references, cycles in records/variants, map-key validity,
//!   `@default` uniqueness, import-alias resolution).
//! - The [`codegen_rust`] module: emits Rust source that defines the
//!   records, variants, enums, `Default` impls (for `@default` cases),
//!   and `<Iface>Service` traits for an interface. Qualified type
//!   references are resolved through a caller-provided import table.
//! - The [`registry`] module: runtime schema registry used by the
//!   plugin host to check consumer/provider compatibility.
//!
//! # Example schema
//!
//! ```bpdl
//! plugin bmux.windows version 1;
//!
//! interface windows-state {
//!     record pane-state {
//!         id: uuid,
//!         focused: bool,
//!         name: string?,
//!     }
//!
//!     variant pane-event {
//!         focused { pane-id: uuid },
//!         closed { pane-id: uuid },
//!     }
//!
//!     query pane-state(id: uuid) -> pane-state?;
//!     command focus-pane(id: uuid) -> result<unit, string>;
//!     events pane-event;
//! }
//! ```
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod ast;
pub mod codegen_rust;
pub mod lexer;
pub mod parser;
pub mod registry;
pub mod validator;

use std::collections::BTreeMap;
use std::fmt;

/// Location in a source file. Line and column are 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: u32,
    pub column: u32,
}

impl Span {
    #[must_use]
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

/// Top-level error type returned from [`compile`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("lex error at {span}: {message}")]
    Lex { span: Span, message: String },

    #[error("parse error at {span}: {message}")]
    Parse { span: Span, message: String },

    #[error("validation error: {message}")]
    Validate { message: String },
}

/// Parse a BPDL source string into an AST and validate it in isolation.
///
/// Qualified type references are tolerated as long as the alias is
/// declared via `import`, but the imported type is not resolved.
///
/// # Errors
///
/// Returns [`Error::Lex`] if the source contains invalid characters,
/// [`Error::Parse`] if the token stream doesn't form a valid BPDL schema,
/// or [`Error::Validate`] if the schema has semantic issues (duplicate
/// type names, unresolved type references, etc.).
pub fn compile(source: &str) -> Result<ast::Schema, Error> {
    let tokens = lexer::tokenize(source)?;
    let schema = parser::parse(&tokens)?;
    validator::validate(&schema)?;
    Ok(schema)
}

/// Parse and validate a schema with imports fully resolved.
///
/// `imports` maps each `import <alias>` declared in this schema to the
/// imported plugin's already-parsed [`ast::Schema`]. Qualified type
/// references (`alias.type`) are resolved against this table during
/// validation; unknown aliases or unknown types cause
/// [`Error::Validate`].
///
/// # Errors
///
/// As [`compile`], plus additional validation errors when imports
/// cannot be resolved.
pub fn compile_with_imports(
    source: &str,
    imports: &BTreeMap<String, ast::Schema>,
) -> Result<ast::Schema, Error> {
    let tokens = lexer::tokenize(source)?;
    let schema = parser::parse(&tokens)?;
    validator::validate_with_imports(&schema, imports)?;
    Ok(schema)
}
