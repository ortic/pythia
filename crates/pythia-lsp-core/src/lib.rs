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
use pythia_plsql::Token;
use pythia_plsql::ast::Ast;
use sema::Sema;

/// Parsed state of one source text.
#[derive(Debug, Clone, Default)]
pub struct Analysis {
    src: String,
    tokens: Vec<Token>,
    ast: Ast,
}

impl Analysis {
    pub fn new(src: &str) -> Self {
        let tokens = pythia_plsql::tokenize(src);
        let ast = pythia_plsql::parse_tokens(src, &tokens);
        Self {
            src: src.to_owned(),
            tokens,
            ast,
        }
    }

    /// Re-parses if `src` differs from the analyzed text. Returns whether it did.
    pub fn update(&mut self, src: &str) -> bool {
        if self.src == src && !(src.is_empty() && self.ast.scopes.is_empty()) {
            return false;
        }
        *self = Self::new(src);
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
