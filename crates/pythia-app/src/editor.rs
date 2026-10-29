//! Editor state owned by a document.
//!
//! Wraps the rope-backed [`CodeEditor`] and keeps a `String` copy of its text
//! (refreshed when the editor's revision changes), so documents can borrow
//! the text cheaply for statement splitting and dirty checks.

use std::ops::Range;

use egui::Ui;
use pythia_ui::code_editor::{CaretShape, CodeEditor, EditorConfig, EditorOutput};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use pythia_ui::code_editor::{Marker, MarkerKind};

pub struct Editor {
    inner: CodeEditor,
    text: String,
    revision: u64,
    /// Compile errors from the database.
    compile_markers: Vec<Marker>,
    /// Syntax problems found while typing.
    syntax_markers: Vec<Marker>,
    /// Breakpoints and the current execution line while debugging.
    debug_markers: Vec<Marker>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new("")
    }
}

/// Saved form: just the text, or the text with bookmarks and folds.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Saved {
    Text(String),
    Full {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        bookmarks: Vec<(Option<u8>, usize)>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        folds: Vec<usize>,
    },
}

impl Serialize for Editor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let bookmarks = self.inner.bookmarks();
        let folds = self.inner.folded_lines();
        if bookmarks.is_empty() && folds.is_empty() {
            return self.text.serialize(serializer);
        }
        Saved::Full {
            text: self.text.clone(),
            bookmarks,
            folds,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Editor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Saved::deserialize(deserializer)? {
            Saved::Text(text) => Editor::new(text),
            Saved::Full {
                text,
                bookmarks,
                folds,
            } => {
                let mut editor = Editor::new(text);
                editor.inner.set_bookmarks(bookmarks);
                editor.inner.set_folded_lines(&folds);
                editor
            }
        })
    }
}

impl Editor {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let inner = CodeEditor::new(&text);
        let revision = inner.revision();
        Self {
            inner,
            text,
            revision,
            compile_markers: Vec::new(),
            syntax_markers: Vec::new(),
            debug_markers: Vec::new(),
        }
    }

    fn refresh(&mut self) {
        if self.inner.revision() != self.revision {
            self.text = self.inner.text();
            self.revision = self.inner.revision();
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.inner.set_text(&text);
        self.text = text;
        self.revision = self.inner.revision();
        self.inner.select(0..0);
    }

    pub fn cursor(&self) -> usize {
        self.inner.cursor().min(self.text.len())
    }

    /// Non-empty selection as a byte range.
    pub fn selection(&self) -> Option<Range<usize>> {
        self.inner
            .selection()
            .filter(|r| r.end <= self.text.len() && !r.is_empty())
    }

    pub fn line_col(&self) -> (usize, usize) {
        self.inner.line_col()
    }

    pub fn select(&mut self, range: Range<usize>) {
        let end = range.end.min(self.text.len());
        self.inner.select(range.start.min(end)..end);
        self.inner.request_focus();
    }

    /// Moves the cursor to a 1-based line and column.
    pub fn goto(&mut self, line: usize, column: usize) {
        self.inner.goto(line, column);
        self.inner.request_focus();
    }

    /// Compile error markers (replacing the previous ones).
    pub fn set_markers(&mut self, markers: Vec<Marker>) {
        self.compile_markers = markers;
        self.push_markers();
    }

    /// Syntax markers from live analysis (replacing the previous ones).
    pub fn set_syntax_markers(&mut self, markers: Vec<Marker>) {
        self.syntax_markers = markers;
        self.push_markers();
    }

    fn push_markers(&mut self) {
        // Compile errors are authoritative; skip syntax markers on their lines.
        let mut all = self.compile_markers.clone();
        all.extend(
            self.syntax_markers
                .iter()
                .filter(|m| !self.compile_markers.iter().any(|c| c.line == m.line))
                .cloned(),
        );
        all.extend(self.debug_markers.iter().cloned());
        self.inner.set_markers(all);
    }

    /// The execution line marker (1-based), if shown.
    #[cfg(test)]
    pub fn execution_line(&self) -> Option<usize> {
        self.debug_markers
            .iter()
            .find(|m| m.kind == MarkerKind::ExecutionPoint)
            .map(|m| m.line)
    }

    /// Breakpoint lines and the execution line (1-based) from the debugger.
    pub fn set_debug_lines(&mut self, breakpoints: &[u32], current: Option<u32>) {
        let mut markers: Vec<Marker> = breakpoints
            .iter()
            .map(|l| Marker {
                line: *l as usize,
                kind: MarkerKind::Breakpoint,
                message: "Breakpoint".into(),
            })
            .collect();
        if let Some(line) = current {
            markers.push(Marker {
                line: line as usize,
                kind: MarkerKind::ExecutionPoint,
                message: "Stopped here".into(),
            });
        }
        let same = markers.len() == self.debug_markers.len()
            && markers
                .iter()
                .zip(&self.debug_markers)
                .all(|(a, b)| a.line == b.line && a.kind == b.kind);
        if !same {
            self.debug_markers = markers;
            self.push_markers();
            if let Some(line) = current {
                self.inner.goto_line(line as usize);
            }
        }
    }

    /// Replaces `range` with `text` as one undo step and puts the cursor on
    /// `line` (1-based).
    pub fn replace(&mut self, range: Range<usize>, text: &str, line: usize) {
        self.inner.replace_range(range, text);
        self.refresh();
        self.inner.goto_line(line.max(1));
    }

    /// Replaces the text with its formatted version as one undo step,
    /// changing only the part that differs; the caret keeps its place in the
    /// unchanged text, or goes to the end of the changed part.
    pub fn apply_format(&mut self, formatted: &str) -> bool {
        let old = self.text.clone();
        if old == formatted {
            return false;
        }
        let prefix = common_prefix(&old, formatted);
        let suffix = common_suffix(&old[prefix..], &formatted[prefix..]);
        let changed = prefix..old.len() - suffix;
        let new_end = formatted.len() - suffix;
        let caret = self.cursor();
        let caret = if caret >= changed.end {
            new_end + (caret - changed.end)
        } else if caret <= prefix {
            caret
        } else {
            new_end
        };
        self.inner
            .replace_range(changed, &formatted[prefix..new_end]);
        self.refresh();
        self.inner.select(caret..caret);
        true
    }

    /// Formats the whole text (format on save); returns whether it changed.
    pub fn format_all(&mut self, options: &pythia_plsql::FormatOptions) -> bool {
        let formatted = pythia_plsql::format(self.text(), options);
        formatted.changed && self.apply_format(&formatted.text)
    }

    /// Formats the editor's output of this frame as the settings ask: the
    /// statements pasted text lands in, a SQL statement when its `;` is
    /// typed.
    pub fn format_as_typed(
        &mut self,
        out: &EditorOutput,
        triggers: pythia_config::FormatTriggers,
        options: &pythia_plsql::FormatOptions,
    ) {
        use pythia_plsql::{StatementKind, split_statements};
        let wanted: Option<Range<usize>> =
            if let Some(pasted) = out.pasted.clone().filter(|_| triggers.on_paste) {
                // Statements the paste brought in whole, and SQL statements it
                // touched; not a whole package for a pasted line.
                let affected: Vec<Range<usize>> = split_statements(self.text())
                    .into_iter()
                    .filter(|s| s.range.start < pasted.end && pasted.start < s.range.end)
                    .filter(|s| {
                        s.kind == StatementKind::Sql
                            || (pasted.start <= s.range.start && s.range.end <= pasted.end)
                    })
                    .map(|s| s.range)
                    .collect();
                affected
                    .first()
                    .zip(affected.last())
                    .map(|(a, b)| a.start..b.end)
            } else if let Some(after) = out.semicolon_typed.filter(|_| triggers.on_statement_end) {
                // The SQL statement this `;` ends.
                split_statements(self.text())
                    .into_iter()
                    .find(|s| s.kind == StatementKind::Sql && s.range.end + 1 == after)
                    .map(|s| s.range)
            } else {
                None
            };
        if let Some(range) = wanted {
            let formatted = pythia_plsql::format_range(self.text(), range, options);
            if formatted.changed {
                self.apply_format(&formatted.text);
            }
        }
    }

    /// Changes whenever the text changes.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replaces the selection (or inserts at the cursor), as one undo step.
    pub fn insert(&mut self, text: &str) {
        self.inner.replace_selection(text);
        self.refresh();
    }

    /// Takes the keyboard focus with the next frame.
    pub fn request_focus(&mut self) {
        self.inner.request_focus();
    }

    pub fn select_all(&mut self) {
        self.inner.select_all();
        self.inner.request_focus();
    }

    /// Comments or uncomments the selected lines.
    pub fn toggle_comment(&mut self, time: f64) {
        self.inner.toggle_comment(time);
        self.refresh();
        self.inner.request_focus();
    }

    /// Labels in the open completion popup.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by UI tests"))]
    pub fn completion_labels(&self) -> Vec<&str> {
        self.inner.completion_labels()
    }

    /// The underlying widget, for code assistance popups.
    pub fn inner(&mut self) -> &mut CodeEditor {
        &mut self.inner
    }

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        id: impl egui::AsId,
        settings: &pythia_config::EditorSettings,
    ) -> EditorOutput {
        self.show(ui, id, settings, false)
    }

    pub fn ui_read_only(
        &mut self,
        ui: &mut Ui,
        id: impl egui::AsId,
        settings: &pythia_config::EditorSettings,
    ) -> EditorOutput {
        self.show(ui, id, settings, true)
    }

    fn show(
        &mut self,
        ui: &mut Ui,
        id: impl egui::AsId,
        settings: &pythia_config::EditorSettings,
        read_only: bool,
    ) -> EditorOutput {
        let config = EditorConfig {
            font_size: settings.font_size,
            tab_width: settings.tab_width,
            show_line_numbers: settings.show_line_numbers,
            show_whitespace: settings.show_whitespace,
            highlight_current_line: settings.highlight_current_line,
            line_spacing: settings.line_spacing,
            caret: match settings.cursor_shape {
                pythia_config::CursorShape::Line => CaretShape::Line,
                pythia_config::CursorShape::Block => CaretShape::Block,
                pythia_config::CursorShape::Underline => CaretShape::Underline,
            },
            caret_blink: settings.cursor_blink,
            // Automatic completion off = only Ctrl+Space.
            auto_complete_min_chars: if settings.auto_complete {
                settings.auto_complete_min_chars.max(1)
            } else {
                usize::MAX
            },
            auto_complete_delay_ms: settings.auto_complete_delay_ms,
            completion_rows: settings.completion_rows,
            read_only,
            ..EditorConfig::default()
        };
        self.inner.set_id_salt(id);
        let out = self.inner.ui(ui, &config);
        // Names dragged from the object browser.
        if !read_only && let Some(dropped) = out.response.dnd_release_payload::<DroppedText>() {
            self.inner.insert_at_cursor(&dropped.0);
            out.response.request_focus();
        }
        self.refresh();
        out
    }
}

/// Text dragged onto an editor (e.g. an object name from the browser).
#[derive(Debug, Clone)]
pub struct DroppedText(pub String);

/// The dotted path ending with the word at `offset`, e.g. `l_rec.name` when
/// hovering `name` (or `l_rec` when hovering `l_rec`), with its byte range.
pub fn path_at(text: &str, offset: usize) -> Option<(Range<usize>, &str)> {
    let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '$' | '#');
    let offset = offset.min(text.len());
    if !text.is_char_boundary(offset) {
        return None;
    }
    let end = text[offset..]
        .find(|c: char| !word(c))
        .map_or(text.len(), |i| offset + i);
    // Back over the word and any `name.` / `name(1).` before it.
    let mut start = text[..offset]
        .rfind(|c: char| !word(c))
        .map_or(0, |i| i + 1);
    loop {
        let before = &text[..start];
        let Some(head) = before.strip_suffix('.') else {
            break;
        };
        let head = head.trim_end_matches(|c: char| c.is_ascii_digit() || c == '(' || c == ')');
        let begin = head.rfind(|c: char| !word(c)).map_or(0, |i| i + 1);
        if begin == head.len() {
            break;
        }
        start = begin;
    }
    (start < end).then(|| (start..end, &text[start..end]))
}

/// Answers a hover request with the debugger's value of the path under the
/// mouse, if stopped in `unit`. Returns whether it did.
pub fn debug_hover(
    editor: &mut Editor,
    offset: usize,
    debug: &pythia_debugger::ui::DebugSession,
    unit: &pythia_debugger::Unit,
) -> bool {
    let Some((range, path)) = path_at(editor.text(), offset) else {
        return false;
    };
    let Some((title, body)) = debug.hover(unit, path) else {
        return false;
    };
    editor.inner().show_hover(range, title, body);
    true
}

/// Length of the common start of two strings, at a char boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    a.char_indices()
        .zip(b.chars())
        .find(|((_, x), y)| x != y)
        .map_or(a.len().min(b.len()), |((i, _), _)| i)
}

/// Length of the common end of two strings, at a char boundary.
fn common_suffix(a: &str, b: &str) -> usize {
    a.chars()
        .rev()
        .zip(b.chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(c, _)| c.len_utf8())
        .sum()
}

/// Byte offset of the start of a 1-based line (end of text if out of range).
pub fn line_start(text: &str, line: usize) -> usize {
    if line <= 1 {
        return 0;
    }
    text.match_indices('\n')
        .nth(line - 2)
        .map_or(text.len(), |(i, _)| i + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_under_the_mouse() {
        let text = "x := l_rec.items(2).name + l_total;";
        let at = |word: &str| path_at(text, text.find(word).unwrap() + 1).map(|(_, p)| p);
        assert_eq!(at("name"), Some("l_rec.items(2).name"));
        assert_eq!(at("items"), Some("l_rec.items"));
        assert_eq!(at("l_rec"), Some("l_rec"));
        assert_eq!(at("l_total"), Some("l_total"));
        assert_eq!(path_at(text, text.find(":=").unwrap()), None);
    }

    #[test]
    fn lines() {
        let t = "ab\ncd\nef";
        assert_eq!(line_start(t, 1), 0);
        assert_eq!(line_start(t, 2), 3);
        assert_eq!(line_start(t, 3), 6);
        assert_eq!(line_start(t, 9), 8);
    }

    #[test]
    fn formatting_keeps_the_caret() {
        let mut e = Editor::new("select a,b from t;\nselect 1 from dual;");
        let at = e.text().find("select 1").unwrap() + 3;
        e.inner().select(at..at);
        assert!(e.apply_format("select a, b from t;\nselect 1 from dual;"));
        assert_eq!(e.text(), "select a, b from t;\nselect 1 from dual;");
        assert_eq!(e.cursor(), at + 1, "after the change: shifted");
        // In the changed part ("  ,"): its end.
        let mut e = Editor::new("select a  ,b from t;");
        e.inner().select(9..9);
        e.apply_format("select a, b from t;");
        assert_eq!(e.cursor(), "select a, ".len());
        assert!(!e.apply_format("select a, b from t;"), "nothing to do");
    }

    #[test]
    fn text_follows_edits() {
        let mut e = Editor::new("select 1");
        e.select(0..6);
        e.insert("SELECT");
        assert_eq!(e.text(), "SELECT 1");
        assert_eq!(serde_json::to_string(&e).unwrap(), "\"SELECT 1\"");
    }

    #[test]
    fn bookmarks_and_folds_survive_saving() {
        let mut e = Editor::new("begin\n  null;\n  null;\nend;\nselect 1 from dual;\n");
        e.inner().toggle_bookmark(5, Some(3));
        e.inner().toggle_fold_at(1);
        let json = serde_json::to_string(&e).unwrap();
        let mut back: Editor = serde_json::from_str(&json).unwrap();
        assert_eq!(back.text(), e.text());
        assert_eq!(back.inner().bookmarks(), vec![(Some(3), 5)]);
        assert_eq!(back.inner().folded_lines(), vec![1]);
        // Old sessions saved just the text.
        let old: Editor = serde_json::from_str("\"select 2\"").unwrap();
        assert_eq!(old.text(), "select 2");
    }
}
