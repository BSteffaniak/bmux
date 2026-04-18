//! Tokenizer for BPDL.
//!
//! The grammar uses a small set of tokens: keywords (`plugin`, `version`,
//! `interface`, `import`, `record`, `variant`, `enum`, `query`, `command`,
//! `events`, `list`, `map`, `result`, `unit`), identifiers, integer
//! literals, and punctuation (`{ } ( ) , ; : ? < > . = @ ->`).

use crate::{Error, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    // Keywords
    Plugin,
    Version,
    Interface,
    Import,
    Record,
    Variant,
    Enum,
    Query,
    Command,
    Events,
    List,
    Map,
    Result,
    Unit,

    Identifier(String),
    IntLiteral(u64),

    // Punctuation
    LBrace,
    RBrace,
    LParen,
    RParen,
    LAngle,
    RAngle,
    Comma,
    Semicolon,
    Colon,
    Question,
    Arrow,
    Equals,
    At,
    Dot,
}

/// Tokenize a BPDL source string into a list of [`Token`]s.
///
/// # Errors
///
/// Returns [`Error::Lex`] if the source contains a character that is not
/// valid in BPDL (e.g., a stray `-` not followed by `>`).
#[allow(clippy::too_many_lines)]
pub fn tokenize(source: &str) -> Result<Vec<Token>, Error> {
    let mut tokens = Vec::new();
    let mut line: u32 = 1;
    let mut column: u32 = 1;
    let mut chars = source.chars().peekable();

    while let Some(&ch) = chars.peek() {
        let start = Span::new(line, column);
        match ch {
            ' ' | '\t' | '\r' => {
                chars.next();
                column += 1;
            }
            '\n' => {
                chars.next();
                line += 1;
                column = 1;
            }
            '/' => {
                chars.next();
                column += 1;
                if chars.peek() == Some(&'/') {
                    // Line comment: consume until newline.
                    chars.next();
                    column += 1;
                    for c in chars.by_ref() {
                        if c == '\n' {
                            line += 1;
                            column = 1;
                            break;
                        }
                        column += 1;
                    }
                } else {
                    return Err(Error::Lex {
                        span: start,
                        message: format!("unexpected character '{ch}'"),
                    });
                }
            }
            '{' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::LBrace,
                    span: start,
                });
            }
            '}' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::RBrace,
                    span: start,
                });
            }
            '(' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::LParen,
                    span: start,
                });
            }
            ')' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::RParen,
                    span: start,
                });
            }
            '<' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::LAngle,
                    span: start,
                });
            }
            '>' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::RAngle,
                    span: start,
                });
            }
            ',' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::Comma,
                    span: start,
                });
            }
            ';' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::Semicolon,
                    span: start,
                });
            }
            ':' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::Colon,
                    span: start,
                });
            }
            '?' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::Question,
                    span: start,
                });
            }
            '=' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::Equals,
                    span: start,
                });
            }
            '@' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::At,
                    span: start,
                });
            }
            '.' => {
                chars.next();
                column += 1;
                tokens.push(Token {
                    kind: TokenKind::Dot,
                    span: start,
                });
            }
            '-' => {
                chars.next();
                column += 1;
                if chars.peek() == Some(&'>') {
                    chars.next();
                    column += 1;
                    tokens.push(Token {
                        kind: TokenKind::Arrow,
                        span: start,
                    });
                } else {
                    return Err(Error::Lex {
                        span: start,
                        message: "expected '>' after '-' (arrow token)".to_string(),
                    });
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let (ident, consumed) = consume_identifier(chars.clone());
                for _ in 0..consumed {
                    chars.next();
                }
                column += u32::try_from(consumed).unwrap_or(u32::MAX);
                let kind = match ident.as_str() {
                    "plugin" => TokenKind::Plugin,
                    "version" => TokenKind::Version,
                    "interface" => TokenKind::Interface,
                    "import" => TokenKind::Import,
                    "record" => TokenKind::Record,
                    "variant" => TokenKind::Variant,
                    "enum" => TokenKind::Enum,
                    "query" => TokenKind::Query,
                    "command" => TokenKind::Command,
                    "events" => TokenKind::Events,
                    "list" => TokenKind::List,
                    "map" => TokenKind::Map,
                    "result" => TokenKind::Result,
                    "unit" => TokenKind::Unit,
                    _ => TokenKind::Identifier(ident),
                };
                tokens.push(Token { kind, span: start });
            }
            c if c.is_ascii_digit() => {
                let (digits, consumed) = consume_digits(chars.clone());
                for _ in 0..consumed {
                    chars.next();
                }
                column += u32::try_from(consumed).unwrap_or(u32::MAX);
                let value: u64 = digits.parse().map_err(|_| Error::Lex {
                    span: start,
                    message: format!("invalid integer literal '{digits}'"),
                })?;
                tokens.push(Token {
                    kind: TokenKind::IntLiteral(value),
                    span: start,
                });
            }
            _ => {
                return Err(Error::Lex {
                    span: start,
                    message: format!("unexpected character '{ch}'"),
                });
            }
        }
    }

    Ok(tokens)
}

/// Consume an identifier body. Identifiers use `[a-zA-Z_]` followed by
/// `[a-zA-Z0-9_-]`. Note: `.` is NOT part of an identifier — it's a
/// standalone [`TokenKind::Dot`] used for plugin ids (`bmux.windows`)
/// and qualified type refs (`alias.type-name`). The parser re-joins
/// identifier sequences across dots where the grammar requires it.
fn consume_identifier(mut iter: std::iter::Peekable<std::str::Chars<'_>>) -> (String, usize) {
    let mut s = String::new();
    let mut consumed = 0;
    while let Some(&c) = iter.peek() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            s.push(c);
            iter.next();
            consumed += 1;
        } else {
            break;
        }
    }
    (s, consumed)
}

fn consume_digits(mut iter: std::iter::Peekable<std::str::Chars<'_>>) -> (String, usize) {
    let mut s = String::new();
    let mut consumed = 0;
    while let Some(&c) = iter.peek() {
        if c.is_ascii_digit() {
            s.push(c);
            iter.next();
            consumed += 1;
        } else {
            break;
        }
    }
    (s, consumed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_plugin_header() {
        let toks = tokenize("plugin bmux.windows version 1;").expect("lex");
        let kinds: Vec<_> = toks.into_iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Plugin,
                TokenKind::Identifier("bmux".to_string()),
                TokenKind::Dot,
                TokenKind::Identifier("windows".to_string()),
                TokenKind::Version,
                TokenKind::IntLiteral(1),
                TokenKind::Semicolon,
            ]
        );
    }

    #[test]
    fn tokenizes_arrow_and_question() {
        let toks = tokenize("-> ? <").expect("lex");
        let kinds: Vec<_> = toks.into_iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![TokenKind::Arrow, TokenKind::Question, TokenKind::LAngle]
        );
    }

    #[test]
    fn skips_line_comments() {
        let toks = tokenize("plugin a version 1; // tail comment\n").expect("lex");
        assert_eq!(toks.len(), 5);
    }

    #[test]
    fn rejects_stray_dash() {
        let err = tokenize("plugin a version 1; foo - bar").unwrap_err();
        assert!(matches!(err, Error::Lex { .. }));
    }

    #[test]
    fn tokenizes_import_directive() {
        let toks = tokenize("import windows = bmux.windows;").expect("lex");
        let kinds: Vec<_> = toks.into_iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Import,
                TokenKind::Identifier("windows".to_string()),
                TokenKind::Equals,
                TokenKind::Identifier("bmux".to_string()),
                TokenKind::Dot,
                TokenKind::Identifier("windows".to_string()),
                TokenKind::Semicolon,
            ]
        );
    }

    #[test]
    fn tokenizes_map_keyword() {
        let toks = tokenize("map<string, u32>").expect("lex");
        let kinds: Vec<_> = toks.into_iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Map,
                TokenKind::LAngle,
                TokenKind::Identifier("string".to_string()),
                TokenKind::Comma,
                TokenKind::Identifier("u32".to_string()),
                TokenKind::RAngle,
            ]
        );
    }

    #[test]
    fn tokenizes_at_and_equals() {
        let toks = tokenize("@default = ascii").expect("lex");
        let kinds: Vec<_> = toks.into_iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::At,
                TokenKind::Identifier("default".to_string()),
                TokenKind::Equals,
                TokenKind::Identifier("ascii".to_string()),
            ]
        );
    }
}
