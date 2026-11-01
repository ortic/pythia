//! Language intelligence for SQL and PL/SQL: completion, signature help,
//! hover, go to definition, find references, outline and diagnostics.
//!
//! Pure logic without UI or database access. Database metadata comes through
//! the [`Catalog`] trait, which never blocks: whatever isn't cached yet is
//! reported back as [`CatalogRequest`]s in the result's `missing` list, so the
//! app can fetch it in the background and ask again.
//!
//! Keep one [`Analysis`] per editor and call [`Analysis::update`] with the
//! current text before each request; it only re-parses when the text changed.

mod builtins;
mod catalog;
mod complete;
mod navigate;
mod outline;
mod sema;

use std::ops::Range;

pub use catalog::{
    Catalog, CatalogCache, CatalogColumn, CatalogMember, CatalogObject, CatalogParam,
    CatalogRequest, EmptyCatalog, Lookup, MemberKind, NameStyle, ParamMode,
};
pub use complete::{
    CaseStyle, Completion, CompletionItem, CompletionKind, CompletionOptions, match_rank,
};
pub use navigate::{Definition, DefinitionResult, Hover, Signature, SignatureHelp};
pub use outline::{OutlineItem, OutlineKind};
pub use pythia_plsql::ast::{Diagnostic, Severity};

use catalog::Tracker;
use pythia_plsql::ast::Ast;
use pythia_plsql::{Syntax, Token, TokenKind};
use sema::Sema;

/// Parsed state of one source text.
#[derive(Debug, Clone, Default)]
pub struct Analysis {
    src: String,
    syntax: Syntax,
    /// `&name` is a substitution variable (SQL*Plus's); otherwise `&` is
    /// an operator.
    substitution: bool,
    tokens: Vec<Token>,
    ast: Ast,
}

impl Analysis {
    pub fn new(src: &str) -> Self {
        Self::new_in(src, Syntax::Oracle)
    }

    /// [`Analysis::new`] of code for a database with `syntax`, lexed the way
    /// it reads it: SQL Server's and SQLite's `[name]` is a quoted name
    /// (#208). `&name` is a substitution variable on Oracle only, as a SQL
    /// window has it without `SET DEFINE` (#190).
    pub fn new_in(src: &str, syntax: Syntax) -> Self {
        Self::new_with(src, syntax, syntax == Syntax::Oracle)
    }

    /// [`Analysis::new_in`] with `&name` a substitution variable or not
    /// (`SET DEFINE ON|OFF`). Without, `1&c.` is `1 & c.`, a bitwise AND
    /// on MySQL, PostgreSQL, SQL Server and SQLite (#287).
    pub fn new_with(src: &str, syntax: Syntax, substitution: bool) -> Self {
        let mut tokens = pythia_plsql::tokenize_in(src, syntax);
        if !substitution {
            tokens = without_substitution(src, syntax, tokens);
        }
        let ast = pythia_plsql::parse_tokens(src, &tokens);
        Self {
            src: src.to_owned(),
            syntax,
            substitution,
            tokens,
            ast,
        }
    }

    /// Re-parses if `src` differs from the analyzed text. Returns whether it did.
    pub fn update(&mut self, src: &str) -> bool {
        self.update_with(src, self.syntax, self.substitution)
    }

    /// [`Analysis::update`] for code of a database with `syntax`.
    pub fn update_in(&mut self, src: &str, syntax: Syntax) -> bool {
        self.update_with(src, syntax, syntax == Syntax::Oracle)
    }

    /// [`Analysis::update_in`] with `&name` a substitution variable or not.
    pub fn update_with(&mut self, src: &str, syntax: Syntax, substitution: bool) -> bool {
        if self.src == src
            && self.syntax == syntax
            && self.substitution == substitution
            && !(src.is_empty() && self.ast.scopes.is_empty())
        {
            return false;
        }
        *self = Self::new_with(src, syntax, substitution);
        true
    }

    pub fn source(&self) -> &str {
        &self.src
    }

    pub fn ast(&self) -> &Ast {
        &self.ast
    }

    pub fn tokens(&self) -> &[Token] {
        &self.tokens
    }

    fn sema<'a>(&'a self, tracker: &'a Tracker<'a>) -> Sema<'a> {
        Sema {
            src: &self.src,
            tokens: &self.tokens,
            ast: &self.ast,
            cat: tracker,
        }
    }

    /// Completion at `offset` with default options.
    pub fn complete(&self, offset: usize, catalog: &dyn Catalog) -> Completion {
        self.complete_with(offset, catalog, &CompletionOptions::default())
    }

    pub fn complete_with(
        &self,
        offset: usize,
        catalog: &dyn Catalog,
        options: &CompletionOptions,
    ) -> Completion {
        // A quoted name being typed reads as closed at the caret, so the
        // rest of the statement stays what it is (#234).
        if let Some(close) = complete::unclosed_quote(&self.tokens, &self.src, offset) {
            let mut src = self.src.clone();
            src.insert(offset, close);
            return Self::new_with(&src, self.syntax, self.substitution)
                .complete_with(offset, catalog, options);
        }
        let tracker = Tracker::new(catalog);
        let mut completion = complete::complete(&self.sema(&tracker), offset, options);
        completion.missing = tracker.into_missing();
        completion
    }

    /// Parameter hints for the call around `offset`. `Some` with no
    /// signatures means catalog data is missing (see `missing`).
    pub fn signature_help(&self, offset: usize, catalog: &dyn Catalog) -> Option<SignatureHelp> {
        let tracker = Tracker::new(catalog);
        let mut help = navigate::signature_help(&self.sema(&tracker), offset);
        let missing = tracker.into_missing();
        match &mut help {
            Some(h) => h.missing = missing,
            None if !missing.is_empty() => {
                help = Some(SignatureHelp {
                    missing,
                    ..SignatureHelp::default()
                })
            }
            None => {}
        }
        help
    }

    /// Information about the name at `offset`.
    pub fn hover(&self, offset: usize, catalog: &dyn Catalog) -> Option<Hover> {
        let tracker = Tracker::new(catalog);
        let mut hover = navigate::hover(&self.sema(&tracker), offset);
        let missing = tracker.into_missing();
        match &mut hover {
            Some(h) => h.missing = missing,
            None if !missing.is_empty() => {
                // Unknown so far: report what to fetch so a retry can answer.
                let range = self.word_at(offset).unwrap_or(offset..offset);
                hover = Some(Hover {
                    range,
                    title: String::new(),
                    detail: String::new(),
                    missing,
                });
            }
            None => {}
        }
        hover
    }

    /// Where the name at `offset` is declared.
    pub fn definition(&self, offset: usize, catalog: &dyn Catalog) -> DefinitionResult {
        let tracker = Tracker::new(catalog);
        let target = navigate::definition(&self.sema(&tracker), offset);
        DefinitionResult {
            target,
            missing: tracker.into_missing(),
        }
    }

    /// Occurrences of the local symbol, alias or CTE at `offset` (including
    /// its declaration), in source order.
    pub fn references(&self, offset: usize) -> Vec<Range<usize>> {
        navigate::references(self, offset)
    }

    pub fn outline(&self) -> Vec<OutlineItem> {
        let empty = EmptyCatalog;
        let tracker = Tracker::new(&empty);
        outline::outline(&self.sema(&tracker))
    }

    /// Syntax problems found while parsing (conservative).
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.ast.diagnostics
    }

    fn word_at(&self, offset: usize) -> Option<Range<usize>> {
        let empty = EmptyCatalog;
        let tracker = Tracker::new(&empty);
        let sema = self.sema(&tracker);
        sema.token_at(offset).map(|i| self.tokens[i].span.clone())
    }
}

/// `tokens` with each `&name` the lexer found read as `&` operators and
/// what follows them.
fn without_substitution(src: &str, syntax: Syntax, tokens: Vec<Token>) -> Vec<Token> {
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        if token.kind != TokenKind::SubstVar {
            out.push(token);
            continue;
        }
        let start = token.span.start;
        let text = &src[token.span.clone()];
        let rest = start + text.len() - text.trim_start_matches('&').len();
        out.extend((start..rest).map(|at| Token {
            kind: TokenKind::Operator,
            span: at..at + 1,
        }));
        out.extend(
            pythia_plsql::tokenize_in(&src[rest..token.span.end], syntax)
                .into_iter()
                .map(|t| Token {
                    kind: t.kind,
                    span: rest + t.span.start..rest + t.span.end,
                }),
        );
    }
    out
}

/// One-shot completion (parses `src`; keep an [`Analysis`] for repeated use).
pub fn complete(src: &str, offset: usize, catalog: &dyn Catalog) -> Completion {
    Analysis::new(src).complete(offset, catalog)
}

pub fn signature_help(src: &str, offset: usize, catalog: &dyn Catalog) -> Option<SignatureHelp> {
    Analysis::new(src).signature_help(offset, catalog)
}

pub fn hover(src: &str, offset: usize, catalog: &dyn Catalog) -> Option<Hover> {
    Analysis::new(src).hover(offset, catalog)
}

pub fn definition(src: &str, offset: usize, catalog: &dyn Catalog) -> DefinitionResult {
    Analysis::new(src).definition(offset, catalog)
}

pub fn references(src: &str, offset: usize) -> Vec<Range<usize>> {
    Analysis::new(src).references(offset)
}

pub fn outline(src: &str) -> Vec<OutlineItem> {
    Analysis::new(src).outline()
}

pub fn diagnostics(src: &str) -> Vec<Diagnostic> {
    Analysis::new(src).diagnostics().to_vec()
}

#[cfg(test)]
mod tests;
