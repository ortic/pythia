//! Rope-backed PL/SQL code editor widget (CONCEPT.md §5.3).
//!
//! [`CodeEditor`] owns the text and all editor state; call
//! [`CodeEditor::ui`] once per frame. Only visible lines are lexed, laid out
//! and painted, so large package bodies stay responsive. The editing logic
//! lives in `state.rs` and is independent of egui.

mod assist;
mod find;
mod fold;
mod highlight;
mod history;
mod matching;
mod search;
mod selection;
mod snippet;
mod state;
mod text;
mod tracked;
mod view;

use std::ops::Range;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use self::assist::{Completion, Hover, Signature};
use self::find::FindBar;
use self::selection::Selection;
use self::state::EditorState;

pub use self::assist::{CompletionEntry, CompletionKind, CompletionTrigger};
pub use self::fold::{Region as FoldRegion, RegionKind as FoldKind};
pub use self::search::SearchQuery;
pub use self::snippet::{Snippet, default_snippets};

/// Display settings, typically from the user's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorConfig {
    pub font_size: f32,
    pub tab_width: usize,
    pub show_line_numbers: bool,
    /// Render spaces as `·` and tabs as `→`.
    pub show_whitespace: bool,
    /// Highlight the line the cursor is on.
    pub highlight_current_line: bool,
    /// Extra space between lines in points.
    pub line_spacing: f32,
    pub caret: CaretShape,
    /// The caret blinks while the editor has focus.
    pub caret_blink: bool,
    pub read_only: bool,
    /// Identifier chars typed before completion is requested automatically.
    pub auto_complete_min_chars: usize,
    /// Typing pause before an automatic completion request.
    pub auto_complete_delay_ms: u64,
    /// How long the mouse must rest on a word before a hover request.
    pub hover_delay_ms: u64,
    /// Rows the completion list shows at most (fewer if the window has no room).
    pub completion_rows: usize,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            font_size: 13.0,
            tab_width: 4,
            show_line_numbers: true,
            show_whitespace: false,
            highlight_current_line: true,
            line_spacing: 0.0,
            caret: CaretShape::Line,
            caret_blink: true,
            read_only: false,
            auto_complete_min_chars: 2,
            auto_complete_delay_ms: 150,
            hover_delay_ms: 400,
            completion_rows: 15,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaretShape {
    /// A thin bar between characters.
    #[default]
    Line,
    /// A box over the character.
    Block,
    /// A line under the character.
    Underline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkerKind {
    Error,
    Warning,
    Breakpoint,
    Bookmark,
    /// Where the debugger is stopped.
    ExecutionPoint,
}

/// A gutter marker, e.g. a compile error or breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// 1-based line number.
    pub line: usize,
    pub kind: MarkerKind,
    /// Shown as tooltip; may be empty.
    pub message: String,
}

/// Result of one frame. Offsets are byte offsets into [`CodeEditor::text`].
pub struct EditorOutput {
    pub response: egui::Response,
    /// Primary caret.
    pub cursor: usize,
    /// Primary selection, if not empty.
    pub selection: Option<Range<usize>>,
    /// 1-based line and column of the primary caret.
    pub line_col: (usize, usize),
    /// The text changed this frame.
    pub changed: bool,
    /// The marker area of the gutter was clicked on this 1-based line
    /// (e.g. to toggle a breakpoint).
    pub gutter_clicked: Option<usize>,
    /// Completion items are wanted; answer with [`CodeEditor::show_completions`].
    pub completion_request: Option<CompletionTrigger>,
    /// Signature help is wanted at this byte offset (after `(` / `,`, or the
    /// caret moved inside the parens of a shown hint); answer with
    /// [`CodeEditor::show_signature`].
    pub signature_request: Option<usize>,
    /// The mouse rests on a word at this byte offset; answer with [`CodeEditor::show_hover`].
    pub hover_request: Option<usize>,
    /// Ctrl+click or F12 on the word at this byte offset.
    pub goto_request: Option<usize>,
    /// Shift+F12 on the word at this byte offset.
    pub references_request: Option<usize>,
    /// Right-click at this byte offset: the context menu opens on
    /// `response`, for what's there.
    pub context_request: Option<usize>,
    /// Text was pasted here (byte range, single caret only).
    pub pasted: Option<Range<usize>>,
    /// A `;` was typed; the byte offset after it.
    pub semicolon_typed: Option<usize>,
}

/// Requests collected during a frame.
#[derive(Default)]
struct Requests {
    completion: Option<CompletionTrigger>,
    signature: Option<usize>,
    hover: Option<usize>,
    goto: Option<usize>,
    references: Option<usize>,
    context: Option<usize>,
    pasted: Option<Range<usize>>,
    semicolon: Option<usize>,
}

/// An automatic completion request waiting for the typing pause.
#[derive(Debug, Clone, Copy)]
struct PendingCompletion {
    deadline: f64,
    caret: usize,
    revision: u64,
}

/// Where the mouse rests, for hover requests.
#[derive(Debug, Clone, Copy)]
struct HoverProbe {
    word_start: usize,
    since: f64,
    requested: bool,
}

/// How the mouse is currently selecting.
#[derive(Debug, Clone, Copy)]
enum Drag {
    /// Plain drag from a char position.
    Chars { anchor: usize },
    /// Alt+drag block selection from (line, visual column); `moved` is false
    /// until the pointer moves (an Alt+click without moving adds a cursor).
    Block {
        from: (usize, usize),
        at: usize,
        moved: bool,
    },
}

pub struct CodeEditor {
    state: EditorState,
    markers: Vec<Marker>,
    find: FindBar,
    /// Input of the go-to-line popup, if open.
    goto_line: Option<String>,
    id_salt: Option<egui::Id>,
    focus_requested: bool,
    scroll_to_cursor: bool,
    drag: Option<Drag>,
    /// Time of the last caret-affecting input, for blinking.
    blink_epoch: f64,
    /// Rows visible in the last frame (for PageUp/PageDown).
    page_rows: usize,
    pairs_cache: Option<(u64, Vec<matching::Pair>)>,
    width_cache: Option<(u64, usize)>,
    completion: Option<Completion>,
    pending_completion: Option<PendingCompletion>,
    signature: Option<Signature>,
    /// Caret for which the last signature request was made.
    signature_caret: Option<usize>,
    hover: Option<Hover>,
    hover_probe: Option<HoverProbe>,
    /// Byte ranges to box, valid for the given revision.
    references: Option<(u64, Vec<Range<usize>>)>,
    /// Word under the mouse while Ctrl is held (char range), drawn as a link.
    link: Option<Range<usize>>,
    /// Hover delay from the config, in seconds.
    hover_delay: f64,
    requests: Requests,
    /// Foldable regions, computed lazily per revision.
    regions_cache: Option<(u64, Vec<fold::Region>)>,
    /// Screen rects of the "⋯" pills of folded lines (from the last frame).
    fold_pills: Vec<(egui::Rect, usize)>,
    /// Geometry of the last frame, for [`CodeEditor::offset_pos`].
    last_layout: Option<view::Layout>,
    /// Time Ctrl+K was pressed, waiting for the second key of a chord.
    chord: Option<f64>,
    snippets: Vec<Snippet>,
}

impl Default for CodeEditor {
    fn default() -> Self {
        Self::new("")
    }
}

impl CodeEditor {
    pub fn new(text: &str) -> Self {
        Self {
            state: EditorState::new(text),
            markers: Vec::new(),
            find: FindBar::default(),
            goto_line: None,
            id_salt: None,
            focus_requested: false,
            scroll_to_cursor: false,
            drag: None,
            blink_epoch: 0.0,
            page_rows: 20,
            pairs_cache: None,
            width_cache: None,
            completion: None,
            pending_completion: None,
            signature: None,
            signature_caret: None,
            hover: None,
            hover_probe: None,
            references: None,
            link: None,
            last_layout: None,
            hover_delay: 0.4,
            requests: Requests::default(),
            regions_cache: None,
            fold_pills: Vec::new(),
            chord: None,
            snippets: default_snippets(),
        }
    }

    /// Sets the id used for focus and scroll state. Needed when several
    /// editors share one parent `Ui`; otherwise derived from the parent id.
    pub fn set_id_salt(&mut self, salt: impl egui::AsId) {
        self.id_salt = Some(egui::Id::new(salt));
    }

    pub fn text(&self) -> String {
        self.state.text()
    }

    /// Replaces the whole text and clears the undo history.
    pub fn set_text(&mut self, text: &str) {
        self.state.set_text(text);
        self.markers
            .retain(|m| m.line <= self.state.rope.len_lines());
    }

    pub fn len_bytes(&self) -> usize {
        self.state.rope.len_bytes()
    }

    pub fn line_count(&self) -> usize {
        self.state.rope.len_lines()
    }

    /// Increments on every change of the text.
    pub fn revision(&self) -> u64 {
        self.state.revision
    }

    /// Primary caret as byte offset.
    pub fn cursor(&self) -> usize {
        self.state.rope.char_to_byte(self.state.primary().head)
    }

    /// Primary selection as byte range, if not empty.
    pub fn selection(&self) -> Option<Range<usize>> {
        let s = self.state.primary();
        (!s.is_empty())
            .then(|| self.state.rope.char_to_byte(s.start())..self.state.rope.char_to_byte(s.end()))
    }

    pub fn selected_text(&self) -> Option<String> {
        let s = self.state.primary();
        (!s.is_empty()).then(|| self.state.rope.slice(s.range()).to_string())
    }

    /// 1-based line and column of the primary caret.
    pub fn line_col(&self) -> (usize, usize) {
        let (l, c) = text::line_col(&self.state.rope, self.state.primary().head);
        (l + 1, c + 1)
    }

    /// Selects a byte range (clamped, snapped to char boundaries) and scrolls to it.
    pub fn select(&mut self, range: Range<usize>) {
        let rope = &self.state.rope;
        let len = rope.len_bytes();
        let a = rope.byte_to_char(range.start.min(len));
        let b = rope.byte_to_char(range.end.min(len));
        self.state.set_selections(vec![Selection::new(a, b)], 0);
        self.scroll_to_cursor = true;
    }

    /// Selects several byte ranges at once (multi-cursor), e.g. every
    /// occurrence of a symbol to rename it by typing. The range containing
    /// `primary` (a byte offset) becomes the primary selection.
    pub fn select_ranges(&mut self, ranges: &[Range<usize>], primary: usize) {
        let rope = &self.state.rope;
        let len = rope.len_bytes();
        let sels: Vec<Selection> = ranges
            .iter()
            .map(|r| {
                Selection::new(
                    rope.byte_to_char(r.start.min(len)),
                    rope.byte_to_char(r.end.min(len)),
                )
            })
            .collect();
        let index = ranges
            .iter()
            .position(|r| r.start <= primary && primary <= r.end)
            .unwrap_or(0);
        self.state.set_selections(sels, index);
        self.scroll_to_cursor = true;
    }

    /// Moves the caret to the start of a 1-based line and scrolls to it.
    pub fn goto_line(&mut self, line: usize) {
        let pos = text::pos_of(&self.state.rope, line.saturating_sub(1), 0);
        self.state.set_caret(pos);
        self.scroll_to_cursor = true;
    }

    /// Moves the caret to a 1-based line and column.
    pub fn goto(&mut self, line: usize, col: usize) {
        let pos = text::pos_of(
            &self.state.rope,
            line.saturating_sub(1),
            col.saturating_sub(1),
        );
        self.state.set_caret(pos);
        self.scroll_to_cursor = true;
    }

    pub fn set_markers(&mut self, markers: Vec<Marker>) {
        self.markers = markers;
    }

    pub fn markers(&self) -> &[Marker] {
        &self.markers
    }

    /// Inserts text at every caret (replacing selections), as one undo step.
    pub fn insert_at_cursor(&mut self, text: &str) {
        self.state.history.seal();
        self.state.insert(text, f64::MAX);
        self.state.history.seal();
        self.scroll_to_cursor = true;
    }

    /// Replaces the primary selection (or inserts at the caret).
    pub fn replace_selection(&mut self, text: &str) {
        let primary = self.state.primary();
        self.state.set_selections(vec![primary], 0);
        self.insert_at_cursor(text);
    }

    /// Replaces a byte range as one undo step (e.g. formatter output).
    pub fn replace_range(&mut self, range: Range<usize>, text: &str) {
        self.state.history.seal();
        self.state
            .replace_byte_ranges(vec![(range, text.to_owned())], f64::MAX);
        self.state.history.seal();
    }

    pub fn select_all(&mut self) {
        self.state.select_all();
    }

    /// Comments or uncomments the selected lines (`time` groups undo).
    pub fn toggle_comment(&mut self, time: f64) {
        self.state.toggle_comment(time);
    }

    pub fn request_focus(&mut self) {
        self.focus_requested = true;
    }

    pub fn undo(&mut self) -> bool {
        self.state.undo()
    }

    pub fn redo(&mut self) -> bool {
        self.state.redo()
    }

    pub fn can_undo(&self) -> bool {
        self.state.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.state.history.can_redo()
    }

    /// Opens the find bar (with replace if `replace`).
    pub fn open_find(&mut self, replace: bool) {
        let seed = self.selected_text().filter(|t| !t.contains('\n'));
        self.find.open(replace, seed);
    }

    // ----- language assistance (answers to `EditorOutput` requests) -----

    /// Opens the completion popup. `replace` is the byte range the accepted
    /// item replaces (usually the identifier prefix before the caret);
    /// typing afterwards extends it and filters the list, or asks again if
    /// the list is `incomplete` (cut short, #160). An empty or non-matching
    /// list closes the popup. A new list for an open popup keeps its
    /// selection, if offered.
    pub fn show_completions(
        &mut self,
        replace: Range<usize>,
        items: Vec<CompletionEntry>,
        incomplete: bool,
    ) {
        let rope = &self.state.rope;
        let len = rope.len_bytes();
        let chars =
            rope.byte_to_char(replace.start.min(len))..rope.byte_to_char(replace.end.min(len));
        let selected = self
            .completion
            .as_ref()
            .and_then(Completion::selected_entry)
            .map(|e| e.label.clone());
        self.completion = Completion::new(&self.state, chars, items).map(|mut c| {
            c.incomplete = incomplete;
            if let Some(label) = &selected {
                c.select_label(label);
            }
            c
        });
        self.pending_completion = None;
    }

    pub fn hide_completions(&mut self) {
        self.completion = None;
    }

    pub fn is_completion_open(&self) -> bool {
        self.completion.is_some()
    }

    /// Labels in the open completion popup, best match first.
    pub fn completion_labels(&self) -> Vec<&str> {
        self.completion
            .as_ref()
            .map(|c| c.labels())
            .unwrap_or_default()
    }

    /// Shows a signature hint above the caret. `active_param` is a byte
    /// range within `label` that is emphasized.
    pub fn show_signature(&mut self, label: String, active_param: Option<Range<usize>>) {
        let caret = self.state.primary().head;
        let open_paren = assist::enclosing_paren(&self.state.rope, caret);
        self.signature = Some(Signature {
            label,
            active: active_param,
            open_paren,
        });
        self.signature_caret = Some(caret);
    }

    pub fn hide_signature(&mut self) {
        self.signature = None;
        self.signature_caret = None;
    }

    /// Shows hover info for the word at byte `range` while the mouse stays on it.
    pub fn show_hover(&mut self, range: Range<usize>, title: String, body: String) {
        let rope = &self.state.rope;
        let len = rope.len_bytes();
        let range = rope.byte_to_char(range.start.min(len))..rope.byte_to_char(range.end.min(len));
        self.hover = Some(Hover { range, title, body });
    }

    pub fn hide_hover(&mut self) {
        self.hover = None;
    }

    /// Boxes the given byte ranges (e.g. references of the symbol under the
    /// caret) until the text changes.
    pub fn highlight_references(&mut self, ranges: Vec<Range<usize>>) {
        self.references = Some((self.state.revision, ranges));
    }

    pub fn clear_references(&mut self) {
        self.references = None;
    }

    // ----- folding ----------------------------------------------------------

    /// Foldable regions (0-based lines), cached per revision.
    fn regions(&mut self) -> &[fold::Region] {
        let rev = self.state.revision;
        if self.regions_cache.as_ref().is_none_or(|(r, _)| *r != rev) {
            self.regions_cache = Some((rev, fold::regions(&self.state.text())));
        }
        &self.regions_cache.as_ref().expect("just set").1
    }

    /// Foldable regions with 1-based lines.
    pub fn fold_regions(&mut self) -> Vec<FoldRegion> {
        self.regions()
            .iter()
            .map(|r| FoldRegion {
                start: r.start + 1,
                end: r.end + 1,
                ..*r
            })
            .collect()
    }

    fn display_map(&self) -> fold::DisplayMap {
        fold::DisplayMap::new(
            self.state.rope.len_lines(),
            self.state.tracked.folds.iter().copied(),
        )
    }

    fn add_fold(&mut self, start: usize, end: usize) {
        let folds = &mut self.state.tracked.folds;
        if folds.iter().any(|(s, _)| *s == start) {
            return;
        }
        folds.push((start, end));
        folds.sort_unstable();
        // Carets inside the hidden lines move to the end of the first line.
        let rope = &self.state.rope;
        let line_end = text::line_end(rope, start);
        let mut moved = false;
        let sels: Vec<Selection> = self
            .state
            .sels
            .iter()
            .map(|s| {
                let line = rope.char_to_line(s.head);
                if line > start && line <= end {
                    moved = true;
                    Selection::caret(line_end)
                } else {
                    *s
                }
            })
            .collect();
        if moved {
            let primary = self.state.primary;
            self.state.set_selections(sels, primary);
        }
    }

    /// Folds or unfolds the region starting on a 1-based line. Returns false
    /// if no region starts there.
    pub fn toggle_fold_at(&mut self, line: usize) -> bool {
        let l = line.saturating_sub(1);
        if let Some(i) = self.state.tracked.folds.iter().position(|(s, _)| *s == l) {
            self.state.tracked.folds.remove(i);
            return true;
        }
        let Some(region) = self.regions().iter().find(|r| r.start == l).copied() else {
            return false;
        };
        self.add_fold(region.start, region.end);
        true
    }

    /// Folds the innermost unfolded region around the caret.
    pub fn fold_at_cursor(&mut self) {
        let line = self.state.rope.char_to_line(self.state.primary().head);
        let folded: Vec<usize> = self.state.tracked.folds.iter().map(|(s, _)| *s).collect();
        let region = self
            .regions()
            .iter()
            .filter(|r| r.start <= line && line <= r.end && !folded.contains(&r.start))
            .max_by_key(|r| r.start)
            .copied();
        if let Some(r) = region {
            self.add_fold(r.start, r.end);
        }
    }

    /// Unfolds the fold on the caret's line (or the ones around it).
    pub fn unfold_at_cursor(&mut self) {
        let line = self.state.rope.char_to_line(self.state.primary().head);
        let folds = &mut self.state.tracked.folds;
        if let Some(i) = folds.iter().position(|(s, _)| *s == line) {
            folds.remove(i);
        } else {
            self.state.tracked.reveal(line);
        }
    }

    /// Folds every block, subprogram, comment and multi-line statement
    /// (not parenthesized lists, and not a file-wide package body).
    pub fn fold_all(&mut self) {
        let regions = self.regions().to_vec();
        for r in &regions {
            let outermost_unit = r.depth == 0
                && r.kind == fold::RegionKind::Unit
                && regions.iter().any(|o| {
                    o.kind == fold::RegionKind::Unit && o.start > r.start && o.end <= r.end
                });
            if r.kind != fold::RegionKind::Paren && !outermost_unit {
                self.add_fold(r.start, r.end);
            }
        }
    }

    pub fn unfold_all(&mut self) {
        self.state.tracked.folds.clear();
    }

    /// First lines (1-based) of the folded regions, e.g. to persist them.
    pub fn folded_lines(&self) -> Vec<usize> {
        self.state
            .tracked
            .folds
            .iter()
            .map(|(s, _)| s + 1)
            .collect()
    }

    /// Folds the regions starting on these 1-based lines (others are ignored).
    pub fn set_folded_lines(&mut self, lines: &[usize]) {
        self.state.tracked.folds.clear();
        let regions = self.regions().to_vec();
        for line in lines {
            if let Some(r) = regions.iter().find(|r| r.start + 1 == *line) {
                self.add_fold(r.start, r.end);
            }
        }
    }

    /// Unfolds whatever hides a caret (after search, go to line, edits…).
    fn reveal_carets(&mut self) {
        if self.state.tracked.folds.is_empty() {
            return;
        }
        let lines: Vec<usize> = self
            .state
            .sels
            .iter()
            .map(|s| self.state.rope.char_to_line(s.head))
            .collect();
        for line in lines {
            self.state.tracked.reveal(line);
        }
    }

    // ----- bookmarks --------------------------------------------------------

    /// Bookmarks as (number 1–9 or none, 1-based line), sorted by line.
    pub fn bookmarks(&self) -> Vec<(Option<u8>, usize)> {
        self.state
            .tracked
            .bookmarks
            .iter()
            .map(|b| (b.number, b.line + 1))
            .collect()
    }

    /// Replaces the bookmarks (e.g. restored from a saved workspace).
    pub fn set_bookmarks(&mut self, bookmarks: Vec<(Option<u8>, usize)>) {
        let n = self.state.rope.len_lines();
        let marks = &mut self.state.tracked.bookmarks;
        *marks = bookmarks
            .into_iter()
            .filter(|(_, l)| (1..=n).contains(l))
            .map(|(number, line)| tracked::Bookmark {
                number,
                line: line - 1,
            })
            .collect();
        marks.sort_by_key(|b| (b.line, b.number.is_none()));
        marks.dedup_by_key(|b| b.line);
    }

    /// Toggles a bookmark on a 1-based line (numbered 1–9 or unnamed).
    pub fn toggle_bookmark(&mut self, line: usize, number: Option<u8>) {
        self.state
            .tracked
            .toggle_bookmark(line.saturating_sub(1), number);
    }

    /// Moves the caret to the first non-blank char of a 0-based line.
    fn jump_to_line(&mut self, line: usize) {
        let rope = &self.state.rope;
        let pos = text::pos_of(rope, line, 0)
            + text::indent_of(&text::line_text(rope, line))
                .chars()
                .count();
        self.state.set_caret(pos);
        self.scroll_to_cursor = true;
    }

    // ----- snippets ---------------------------------------------------------

    pub fn snippets(&self) -> &[Snippet] {
        &self.snippets
    }

    /// Snippets that expand when their trigger is typed and Tab is pressed.
    pub fn set_snippets(&mut self, snippets: Vec<Snippet>) {
        self.snippets = snippets;
    }

    /// Inserts a snippet at the primary caret (replacing the selection) and
    /// selects its first placeholder.
    pub fn insert_snippet(&mut self, snippet: &Snippet) {
        let primary = self.state.primary();
        self.state.set_selections(vec![primary], 0);
        self.state
            .expand_snippet(primary.range(), &snippet.body, f64::MAX);
        self.scroll_to_cursor = true;
    }

    /// Whether Tab currently moves between snippet placeholders.
    pub fn is_snippet_active(&self) -> bool {
        self.state.snippet.is_some()
    }

    /// Completion entries for the snippets whose trigger starts with `prefix`
    /// (kind [`CompletionKind::Snippet`]; accepting one expands it).
    pub fn snippet_completions(&self, prefix: &str) -> Vec<CompletionEntry> {
        let prefix = prefix.to_lowercase();
        let unit = self.state.indent_unit();
        self.snippets
            .iter()
            .filter(|s| s.trigger.to_lowercase().starts_with(&prefix))
            .map(|s| CompletionEntry {
                label: s.trigger.clone(),
                detail: s.description.clone(),
                kind: CompletionKind::Snippet,
                insert_text: s.body.clone(),
                // What it inserts, placeholders filled in.
                documentation: Some((
                    s.description.clone(),
                    snippet::expand(&s.body, "", &unit).text,
                )),
            })
            .collect()
    }

    /// Expands the snippet whose trigger is the word before the caret.
    fn expand_trigger(&mut self, time: f64) -> bool {
        let st = &self.state;
        let p = st.primary();
        if st.sels.len() != 1 || !p.is_empty() {
            return false;
        }
        let typed = assist::word_prefix_len(&st.rope, p.head);
        if typed == 0 {
            return false;
        }
        let word = st.rope.slice(p.head - typed..p.head).to_string();
        let Some(body) = self
            .snippets
            .iter()
            .find(|s| s.trigger.eq_ignore_ascii_case(&word))
            .map(|s| s.body.clone())
        else {
            return false;
        };
        self.state
            .expand_snippet(p.head - typed..p.head, &body, time);
        self.scroll_to_cursor = true;
        true
    }

    fn pairs(&mut self) -> &[matching::Pair] {
        let rev = self.state.revision;
        if self.pairs_cache.as_ref().is_none_or(|(r, _)| *r != rev) {
            self.pairs_cache = Some((rev, matching::find_pairs(&self.state.text())));
        }
        &self.pairs_cache.as_ref().expect("just set").1
    }

    /// Widest line in chars (tabs counted as one), cached per revision.
    fn max_line_chars(&mut self) -> usize {
        let rev = self.state.revision;
        if let Some((r, w)) = self.width_cache
            && r == rev
        {
            return w;
        }
        let w = self
            .state
            .rope
            .lines()
            .map(|l| l.len_chars())
            .max()
            .unwrap_or(0);
        self.width_cache = Some((rev, w));
        w
    }
}

impl Serialize for CodeEditor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text())
    }
}

impl<'de> Deserialize<'de> for CodeEditor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ok(Self::new(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_is_plain_text() {
        let editor = CodeEditor::new("select 1\nfrom dual");
        let json = serde_json::to_string(&editor).unwrap();
        assert_eq!(json, "\"select 1\\nfrom dual\"");
        let back: CodeEditor = serde_json::from_str(&json).unwrap();
        assert_eq!(back.text(), editor.text());
    }

    const PKG: &str = "create or replace package body p as\n  procedure a is\n  begin\n    if x then\n      null;\n    end if;\n  end a;\n  procedure b is\n  begin\n    null;\n  end b;\nend p;\n";

    #[test]
    fn folding_api() {
        let mut e = CodeEditor::new(PKG);
        assert!(e.toggle_fold_at(2));
        assert_eq!(e.folded_lines(), [2]);
        assert_eq!(e.display_map().rows(), 13 - 5);
        assert!(e.toggle_fold_at(2));
        assert!(e.folded_lines().is_empty());
        assert!(!e.toggle_fold_at(5));
        e.fold_all();
        // Both procedures and the IF, but not the file-wide package body.
        assert_eq!(e.folded_lines(), [2, 4, 8]);
        e.unfold_all();
        e.set_folded_lines(&[8, 99]);
        assert_eq!(e.folded_lines(), [8]);
    }

    #[test]
    fn carets_leave_and_reveal_folds() {
        let mut e = CodeEditor::new(PKG);
        e.goto(5, 3);
        e.fold_at_cursor();
        // The innermost region around line 5 is the IF on line 4.
        assert_eq!(e.folded_lines(), [4]);
        assert_eq!(e.line_col().0, 4);
        e.goto(5, 1);
        e.reveal_carets();
        assert!(e.folded_lines().is_empty());
    }

    #[test]
    fn bookmarks_api() {
        let mut e = CodeEditor::new(PKG);
        e.toggle_bookmark(3, Some(1));
        e.toggle_bookmark(9, None);
        e.goto(1, 1);
        e.insert_at_cursor("-- header\n");
        assert_eq!(e.bookmarks(), [(Some(1), 4), (None, 10)]);
        e.set_bookmarks(vec![(Some(2), 1), (None, 500)]);
        assert_eq!(e.bookmarks(), [(Some(2), 1)]);
    }

    #[test]
    fn snippet_trigger_and_completion() {
        let mut e = CodeEditor::new("  sel");
        e.goto(1, 6);
        assert!(e.expand_trigger(0.0));
        assert_eq!(e.text(), "  select *\n    from table_name\n   where 1 = 1");
        assert_eq!(e.selected_text().as_deref(), Some("*"));
        assert!(e.is_snippet_active());
        let items = e.snippet_completions("pk");
        assert_eq!(
            items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            ["pkg", "pkgb"]
        );
    }

    #[test]
    fn byte_offsets_with_multibyte_text() {
        let mut editor = CodeEditor::new("-- grüße\nselect 1");
        editor.goto(2, 3);
        assert_eq!(editor.cursor(), "-- grüße\nse".len());
        assert_eq!(editor.line_col(), (2, 3));
        editor.select(3..9);
        assert_eq!(editor.selected_text().as_deref(), Some("grüß"));
        assert_eq!(editor.selection(), Some(3..9));
        editor.replace_selection("x");
        assert_eq!(editor.text(), "-- xe\nselect 1");
        assert!(editor.undo());
        assert_eq!(editor.text(), "-- grüße\nselect 1");
    }
}
