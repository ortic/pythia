//! PL/SQL and SQL language support: lexing, statement splitting and an
//! error-tolerant parser for navigation, outline and completion.

pub mod ast;
mod format;
mod header;
mod keywords;
mod lexer;
mod parser;
mod splitter;

pub use format::{
    Case, CommaStyle, FormatOptions, Formatted, Wrap, describe as describe_format,
    equivalent as only_layout_differs, equivalent_in as only_layout_differs_in, format, format_in,
    format_range, format_range_in, infer as infer_format, kept_as_written, kept_as_written_in,
};
pub use header::{UnitHeader, trigger_body_line, unit_header};
pub use keywords::{is_keyword, is_reserved};
pub use lexer::{Token, TokenKind, leading_tokens, open_comments, tokenize, tokenize_in};
pub use parser::{normalize_ident, parse, parse_tokens};
pub use splitter::{
    Statement, StatementKind, Syntax, split_statements, split_statements_in, statement_at,
    statement_at_in, terminated,
};
