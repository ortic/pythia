//! Code assistance for an editor: completion, signature help, hover,
//! go to definition and references, backed by `pythia_lsp_core`.
//!
//! Lookups never block. When the engine needs dictionary data it reports
//! [`CatalogRequest`]s; the app fetches them in the background and bumps the
//! catalog revision, and the pending request is recomputed then.
//!
//! In a program window, each part (spec, body) has an `Assist` that also
//! knows the other part, its companion: declarations of the spec are known
//! in the body, and jumps and references can lead to the other part.

use std::borrow::Cow;
use std::ops::Range;

use pythia_db::ObjectType;
use pythia_lsp_core::{
    Analysis, Catalog, CatalogRequest, CompletionKind as LspKind, Definition, EmptyCatalog,
    OutlineItem, Severity,
};
use pythia_ui::code_editor::{CompletionEntry, CompletionKind, EditorOutput};

use crate::documents::{DocContext, Effects, ObjectRef, OpenRequest};
use crate::editor::{Editor, Marker, MarkerKind};

#[derive(Default)]
pub struct Assist {
    /// The editor's own text: diagnostics, outline; and navigation without a
    /// companion.
    analysis: Analysis,
    /// The editor's text together with the companion, for navigation.
    combined: Analysis,
    companion: Option<Companion>,
    /// Editor revision the analysis (diagnostics, outline) reflects.
    analyzed_revision: Option<u64>,
    /// Whether that analysis marked syntax errors.
    checked: bool,
    /// When the text last changed without being analyzed yet (egui time).
    changed_at: Option<f64>,
    outline: Vec<OutlineItem>,
    /// Request waiting for dictionary data.
    pending: Option<Pending>,
    /// Catalog revision the pending request last ran against.
    catalog_revision: u64,
    /// What the last right-click was on, for the context menu.
    context: Option<Context>,
    /// Asked for by commands (go to definition, peek, references).
    queued: Vec<Pending>,
    /// Where the caret was before the last jump, for going back.
    jumped_from: Option<usize>,
    /// A jump into the companion, for the window to carry out.
    companion_jump: Option<Range<usize>>,
    /// References found by the last request, for the window to list.
    found_references: Option<Vec<Place>>,
}

/// The other part of a package or type.
struct Companion {
    revision: u64,
    /// The companion comes first (it's the spec of this body).
    before: bool,
    text: String,
}

/// Where something is: in this editor's text or in the companion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Here(Range<usize>),
    Companion(Range<usize>),
}

/// What a right-click in the editor was on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub offset: usize,
    pub target: ContextTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextTarget {
    /// Waiting for dictionary data.
    Looking,
    /// A database object, possibly a member of it (`pkg.proc`).
    Object {
        object: ObjectRef,
        member: Option<String>,
    },
    /// A declaration in this text or the companion.
    Local(Place),
    Nothing,
}

/// Navigation asked for by a command, at the caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Definition,
    Peek,
    References,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// Completion requested with the cursor at this byte offset.
    Complete(usize),
    Signature(usize),
    Hover(usize),
    Goto(usize),
    /// What's at this offset, for the context menu.
    Context(usize),
    Peek(usize),
    References(usize),
}

/// Separates the companion from the editor's text in the analyzed source.
const SEPARATOR: &str = "\n/\n";

/// Where the editor's text and the companion are in the analyzed source.
#[derive(Debug, Clone, Copy)]
struct Layout {
    own: usize,
    own_len: usize,
    companion: Option<(usize, usize)>,
}

impl Layout {
    fn place(&self, range: Range<usize>) -> Option<Place> {
        if range.start >= self.own && range.end <= self.own + self.own_len {
            return Some(Place::Here(range.start - self.own..range.end - self.own));
        }
        let (start, len) = self.companion?;
        (range.start >= start && range.end <= start + len)
            .then(|| Place::Companion(range.start - start..range.end - start))
    }
}

impl Assist {
    /// Re-analyzes the text shortly after typing stops: refreshes syntax
    /// markers (with `check`: the parser knows Oracle's SQL and PL/SQL,
    /// other databases' scripts would show errors that aren't) and the
    /// outline. Call every frame after `Editor::ui`.
    pub fn background(&mut self, editor: &mut Editor, ui: &egui::Ui, check: bool) {
        const IDLE: f64 = 0.35;
        if self.analyzed_revision == Some(editor.revision()) && self.checked == check {
            return;
        }
        let now = ui.input(|i| i.time);
        // Analyze right away the first time (just opened / loaded).
        let changed_at = *self
            .changed_at
            .get_or_insert(if self.analyzed_revision.is_none() {
                now - IDLE
            } else {
                now
            });
        if now - changed_at < IDLE {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64(IDLE));
            return;
        }
        self.changed_at = None;
        self.analyzed_revision = Some(editor.revision());
        self.checked = check;
        self.analysis.update(editor.text());
        let text = editor.text();
        let diagnostics = if check {
            self.analysis.diagnostics()
        } else {
            &[]
        };
        let markers = diagnostics
            .iter()
            .map(|d| Marker {
                line: text[..d.span.start.min(text.len())].matches('\n').count() + 1,
                kind: match d.severity {
                    Severity::Error => MarkerKind::Error,
                    Severity::Warning => MarkerKind::Warning,
                },
                message: d.message.clone(),
            })
            .collect();
        self.outline = self.analysis.outline();
        editor.set_syntax_markers(markers);
    }

    pub fn outline(&self) -> &[OutlineItem] {
        &self.outline
    }

    /// Sets the other part of a package or type: `Some((revision, before))`
    /// with `before` when it's the spec of this body. `text` is only called
    /// when the revision changed.
    pub fn set_companion(&mut self, companion: Option<(u64, bool)>, text: impl FnOnce() -> String) {
        match companion {
            None => self.companion = None,
            Some((revision, before)) => {
                if self
                    .companion
                    .as_ref()
                    .is_none_or(|c| c.revision != revision || c.before != before)
                {
                    self.companion = Some(Companion {
                        revision,
                        before,
                        text: text(),
                    });
                }
            }
        }
    }

    /// Whether the companion is the spec (this is a body).
    pub fn companion_is_spec(&self) -> Option<bool> {
        self.companion.as_ref().map(|c| c.before)
    }

    /// What the last right-click was on.
    pub fn context(&self) -> Option<&Context> {
        self.context.as_ref()
    }

    /// Asks for navigation at `offset` (handled with the next
    /// [`Assist::handle`]).
    pub fn request(&mut self, request: Request, offset: usize) {
        self.queued.push(match request {
            Request::Definition => Pending::Goto(offset),
            Request::Peek => Pending::Peek(offset),
            Request::References => Pending::References(offset),
        });
    }

    /// Where the caret was before the last jump (taken once).
    pub fn take_jumped_from(&mut self) -> Option<usize> {
        self.jumped_from.take()
    }

    /// A jump into the companion to carry out (taken once).
    pub fn take_companion_jump(&mut self) -> Option<Range<usize>> {
        self.companion_jump.take()
    }

    /// References found by the last request (taken once).
    pub fn take_references(&mut self) -> Option<Vec<Place>> {
        self.found_references.take()
    }

    /// The analyzed source: the editor's text, with the companion if any.
    fn source<'t>(&self, own: &'t str) -> (Cow<'t, str>, Layout) {
        match &self.companion {
            None => (
                Cow::Borrowed(own),
                Layout {
                    own: 0,
                    own_len: own.len(),
                    companion: None,
                },
            ),
            Some(c) if c.before => (
                Cow::Owned(format!("{}{SEPARATOR}{own}", c.text)),
                Layout {
                    own: c.text.len() + SEPARATOR.len(),
                    own_len: own.len(),
                    companion: Some((0, c.text.len())),
                },
            ),
            Some(c) => (
                Cow::Owned(format!("{own}{SEPARATOR}{}", c.text)),
                Layout {
                    own: 0,
                    own_len: own.len(),
                    companion: Some((own.len() + SEPARATOR.len(), c.text.len())),
                },
            ),
        }
    }

    fn analyzed(&self) -> &Analysis {
        if self.companion.is_some() {
            &self.combined
        } else {
            &self.analysis
        }
    }

    /// Handles the editor's requests of this frame; call after `Editor::ui`.
    pub fn handle(&mut self, editor: &mut Editor, out: &EditorOutput, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        let empty = EmptyCatalog;
        let catalog: &dyn Catalog = match cx.catalog {
            Some(c) => c,
            None => &empty,
        };

        let mut work: Vec<Pending> = std::mem::take(&mut self.queued);
        // A context menu still looking up its name retries on its own: a
        // hover over the same word must not take its place.
        if cx.catalog_revision != self.catalog_revision
            && let Some(context) = &self.context
            && context.target == ContextTarget::Looking
        {
            work.push(Pending::Context(context.offset));
        }
        if let Some(p) = self.pending
            && cx.catalog_revision != self.catalog_revision
        {
            // New dictionary data arrived: retry, unless the user moved on.
            let still_relevant = match p {
                Pending::Complete(at) => {
                    // Still at the same place, or only typed more of the word.
                    let cursor = editor.cursor();
                    cursor >= at
                        && editor.text().get(at..cursor).is_some_and(|typed| {
                            typed
                                .chars()
                                .all(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '#')
                        })
                }
                _ => true,
            };
            self.pending = None;
            if still_relevant {
                work.push(p);
            }
        }
        if out.completion_request.is_some() {
            work.push(Pending::Complete(editor.cursor()));
        }
        work.extend(out.signature_request.map(Pending::Signature));
        work.extend(out.hover_request.map(Pending::Hover));
        work.extend(out.goto_request.map(Pending::Goto));
        work.extend(out.references_request.map(Pending::References));
        if let Some(offset) = out.context_request {
            self.context = Some(Context {
                offset,
                target: ContextTarget::Looking,
            });
            work.push(Pending::Context(offset));
        }
        if work.is_empty() {
            return effects;
        }
        let (source, layout) = self.source(editor.text());
        let source = source.into_owned();
        if self.companion.is_some() {
            self.combined.update(&source);
        } else {
            self.analysis.update(&source);
        }
        self.catalog_revision = cx.catalog_revision;

        for request in work {
            let missing = self.run(request, layout, editor, catalog, cx, &mut effects);
            if !missing.is_empty() {
                if !matches!(request, Pending::Context(_)) {
                    self.pending = Some(request);
                }
                effects.catalog_requests.extend(missing);
            }
        }
        effects
    }

    fn run(
        &mut self,
        request: Pending,
        layout: Layout,
        editor: &mut Editor,
        catalog: &dyn Catalog,
        cx: &DocContext,
        effects: &mut Effects,
    ) -> Vec<CatalogRequest> {
        // Offsets of the editor's text in the analyzed source.
        let at = |offset: usize| offset + layout.own;
        let here = |range: Range<usize>| match layout.place(range) {
            Some(Place::Here(r)) => Some(r),
            _ => None,
        };
        match request {
            Pending::Complete(_) => {
                let completion = self.analyzed().complete(at(editor.cursor()), catalog);
                let Some(replace) = here(completion.replace.clone()) else {
                    return completion.missing;
                };
                let mut entries: Vec<CompletionEntry> =
                    completion.items.into_iter().map(entry).collect();
                // Snippets for a plain word, not after `alias.` or `pkg.`.
                let text = editor.text();
                let member = text[..replace.start.min(text.len())].ends_with('.');
                if let Some(prefix) = text.get(replace.clone()).map(str::to_owned)
                    && !prefix.is_empty()
                    && !member
                {
                    entries.extend(editor.inner().snippet_completions(&prefix));
                }
                editor
                    .inner()
                    .show_completions(replace, entries, completion.incomplete);
                completion.missing
            }
            Pending::Signature(offset) => match self.analyzed().signature_help(at(offset), catalog)
            {
                Some(help) => {
                    match help.signatures.get(help.active_signature) {
                        Some(sig) => {
                            let active = sig.params.get(help.active_param).cloned();
                            editor.inner().show_signature(sig.label.clone(), active);
                        }
                        None => editor.inner().hide_signature(),
                    }
                    help.missing
                }
                None => {
                    editor.inner().hide_signature();
                    Vec::new()
                }
            },
            Pending::Hover(offset) => match self.analyzed().hover(at(offset), catalog) {
                Some(hover) => {
                    if let Some(range) = here(hover.range) {
                        editor.inner().show_hover(range, hover.title, hover.detail);
                    }
                    hover.missing
                }
                None => Vec::new(),
            },
            Pending::Goto(offset) => {
                let result = self.analyzed().definition(at(offset), catalog);
                let from = editor.cursor();
                match result.target {
                    Some(Definition::Local(range)) => match layout.place(range) {
                        Some(Place::Here(range)) => {
                            self.jumped_from = Some(from);
                            editor.select(range);
                        }
                        Some(Place::Companion(range)) => {
                            self.jumped_from = Some(from);
                            self.companion_jump = Some(range);
                        }
                        None => {}
                    },
                    Some(Definition::Object {
                        owner,
                        name,
                        object_type,
                        member,
                        parameter,
                    }) => {
                        if let (Some(connection), Some(t)) = (
                            cx.connection.clone(),
                            ObjectType::from_dictionary(&object_type),
                        ) {
                            // A parameter of a standalone procedure: the
                            // procedure itself is the member.
                            let member =
                                member.or_else(|| parameter.as_ref().map(|_| name.clone()));
                            let object = ObjectRef {
                                owner,
                                name,
                                object_type: t,
                            };
                            self.jumped_from = Some(from);
                            effects.open.push(match member {
                                // `pkg.proc`: the package at `proc`.
                                Some(member) if t.has_source() => OpenRequest::ProgramMember {
                                    connection,
                                    object,
                                    member,
                                    parameter,
                                },
                                _ if t.has_source() => OpenRequest::Program { connection, object },
                                _ => OpenRequest::Object { connection, object },
                            });
                        }
                    }
                    None => {}
                }
                result.missing
            }
            Pending::Context(offset) => {
                let result = self.analyzed().definition(at(offset), catalog);
                let target = match result.target {
                    Some(Definition::Local(range)) => match layout.place(range) {
                        Some(place) => ContextTarget::Local(place),
                        None => ContextTarget::Nothing,
                    },
                    Some(Definition::Object {
                        owner,
                        name,
                        object_type,
                        member,
                        ..
                    }) => match ObjectType::from_dictionary(&object_type) {
                        Some(t) => ContextTarget::Object {
                            object: ObjectRef {
                                owner,
                                name,
                                object_type: t,
                            },
                            // Columns are no subprograms to open at.
                            member: member.filter(|_| t.has_source()),
                        },
                        None => ContextTarget::Nothing,
                    },
                    None if result.missing.is_empty() => ContextTarget::Nothing,
                    None => ContextTarget::Looking,
                };
                if let Some(context) = self.context.as_mut().filter(|c| c.offset == offset) {
                    context.target = target;
                }
                result.missing
            }
            Pending::Peek(offset) => {
                let analysis = self.analyzed();
                let hover = analysis.hover(at(offset), catalog);
                let result = analysis.definition(at(offset), catalog);
                let mut missing = result.missing;
                let Some(hover) = hover else {
                    return missing;
                };
                missing.extend(hover.missing.iter().cloned());
                let Some(word) = here(hover.range.clone()) else {
                    return missing;
                };
                let (title, body) = match result.target {
                    Some(Definition::Local(range)) => {
                        let source = analysis.source();
                        let (part, part_start) = match layout.place(range.clone()) {
                            Some(Place::Companion(_)) => {
                                let spec = self.companion_is_spec() == Some(true);
                                (
                                    if spec { "spec" } else { "body" },
                                    layout.companion.map_or(0, |c| c.0),
                                )
                            }
                            _ => ("here", layout.own),
                        };
                        let line = source[part_start..range.start].matches('\n').count() + 1;
                        (
                            format!("{} — {part}, line {line}", hover.title),
                            peek_text(source, range.start),
                        )
                    }
                    // Database objects: what hover knows (signature, columns).
                    _ => (hover.title, hover.detail),
                };
                editor.inner().show_hover(word, title, body);
                missing
            }
            Pending::References(offset) => {
                let refs: Vec<Place> = self
                    .analyzed()
                    .references(at(offset))
                    .into_iter()
                    .filter_map(|r| layout.place(r))
                    .collect();
                let local = refs
                    .iter()
                    .filter_map(|p| match p {
                        Place::Here(r) => Some(r.clone()),
                        Place::Companion(_) => None,
                    })
                    .collect();
                editor.inner().highlight_references(local);
                self.found_references = Some(refs);
                Vec::new()
            }
        }
    }
}

/// Lines of a declaration for peeking: from its line on, up to the end of a
/// subprogram's header or a dozen lines.
fn peek_text(source: &str, start: usize) -> String {
    const MAX_LINES: usize = 12;
    let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
    let mut out: Vec<&str> = Vec::new();
    for line in source[line_start..].lines().take(MAX_LINES) {
        out.push(line);
        let upper = line.trim_end().to_ascii_uppercase();
        if upper.ends_with(';') || upper.ends_with(" IS") || upper.ends_with(" AS") {
            break;
        }
    }
    // Keep the indentation relative to the first line.
    let indent = out.first().map_or(0, |l| l.len() - l.trim_start().len());
    out.iter()
        .map(|l| {
            l.get(indent.min(l.len() - l.trim_start().len())..)
                .unwrap_or(l)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn entry(item: pythia_lsp_core::CompletionItem) -> CompletionEntry {
    let kind = match item.kind {
        LspKind::Keyword => CompletionKind::Keyword,
        LspKind::Snippet => CompletionKind::Snippet,
        LspKind::Variable
        | LspKind::Constant
        | LspKind::Cursor
        | LspKind::Exception
        | LspKind::Alias => CompletionKind::Variable,
        LspKind::Parameter => CompletionKind::Parameter,
        LspKind::Type => CompletionKind::Type,
        LspKind::Procedure => CompletionKind::Procedure,
        LspKind::Function | LspKind::Builtin => CompletionKind::Function,
        LspKind::Field | LspKind::Column => CompletionKind::Column,
        LspKind::Table => CompletionKind::Table,
        LspKind::View => CompletionKind::View,
        LspKind::Package => CompletionKind::Package,
        LspKind::Schema => CompletionKind::Schema,
        LspKind::Sequence | LspKind::Synonym | LspKind::Object => CompletionKind::Other,
    };
    CompletionEntry {
        label: item.label,
        detail: item.detail,
        kind,
        insert_text: item.insert_text,
        documentation: item.documentation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_places_ranges() {
        // Spec first (companion), then this body.
        let layout = Layout {
            own: 13,
            own_len: 20,
            companion: Some((0, 10)),
        };
        assert_eq!(layout.place(14..16), Some(Place::Here(1..3)));
        assert_eq!(layout.place(2..4), Some(Place::Companion(2..4)));
        assert_eq!(layout.place(10..12), None, "the separator");
    }

    #[test]
    fn peeks_at_a_declaration_header() {
        let src = "package body p as\n  procedure log_change(p_id number,\n                       p_note varchar2) is\n  begin\n    null;\n  end;\nend;";
        let at = src.find("log_change").unwrap();
        assert_eq!(
            peek_text(src, at),
            "procedure log_change(p_id number,\n                     p_note varchar2) is"
        );
        let src = "  g_count number := 0;\n  g_x number;";
        assert_eq!(peek_text(src, 2), "g_count number := 0;");
    }
}
