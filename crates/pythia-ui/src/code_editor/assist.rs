//! Editor side of language assistance: completion popup state, signature
//! hint and hover info. The language logic lives elsewhere; the app answers
//! the requests in `EditorOutput` by calling `CodeEditor::show_*`.

use std::ops::Range;

use egui::Color32;
use ropey::Rope;

use super::selection::Selection;
use super::state::EditorState;
use super::text::is_word_char;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionKind {
    Keyword,
    Table,
    View,
    Column,
    Package,
    Procedure,
    Function,
    Variable,
    Parameter,
    Type,
    Snippet,
    Schema,
    Other,
}

impl CompletionKind {
    /// Short glyph shown in front of the label. Placeholder until real icons exist.
    pub fn glyph(self) -> &'static str {
        match self {
            CompletionKind::Keyword => "k",
            CompletionKind::Table => "T",
            CompletionKind::View => "V",
            CompletionKind::Column => "c",
            CompletionKind::Package => "P",
            CompletionKind::Procedure => "p",
            CompletionKind::Function => "f",
            CompletionKind::Variable => "v",
            CompletionKind::Parameter => "a",
            CompletionKind::Type => "t",
            CompletionKind::Snippet => "s",
            CompletionKind::Schema => "S",
            CompletionKind::Other => "·",
        }
    }

    /// Accent color of the glyph.
    pub fn color(self, dark: bool) -> Color32 {
        let (r, g, b) = match self {
            CompletionKind::Keyword => (0x56, 0x9c, 0xd6),
            CompletionKind::Table | CompletionKind::View => (0x4e, 0xc9, 0xb0),
            CompletionKind::Column => (0x9c, 0xdc, 0xfe),
            CompletionKind::Package | CompletionKind::Schema => (0xc5, 0x86, 0xc0),
            CompletionKind::Procedure | CompletionKind::Function => (0xdc, 0xdc, 0xaa),
            CompletionKind::Variable | CompletionKind::Parameter => (0x9c, 0xdc, 0xfe),
            CompletionKind::Type => (0x4e, 0xc9, 0xb0),
            CompletionKind::Snippet => (0xce, 0x91, 0x78),
            CompletionKind::Other => (0x99, 0x99, 0x99),
        };
        let c = Color32::from_rgb(r, g, b);
        // Darken for light backgrounds.
        if dark {
            c
        } else {
            Color32::from_rgb(r / 2, g / 2, b / 2)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionEntry {
    pub label: String,
    /// Dimmed, right-aligned (e.g. data type or owner).
    pub detail: String,
    pub kind: CompletionKind,
    /// Text inserted on accept.
    pub insert_text: String,
    /// Title and text of the pane shown next to the list while the entry is
    /// selected, e.g. what hover shows for it; `None` shows no pane.
    pub documentation: Option<(String, String)>,
}

/// Why and where completion items are wanted. `offset` is a byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionTrigger {
    pub offset: usize,
    /// Ctrl+Space (as opposed to automatic while typing).
    pub explicit: bool,
    /// `.` for member completion after `alias.` / `pkg.`.
    pub trigger_char: Option<char>,
}

/// Open completion popup.
#[derive(Debug, Clone)]
pub struct Completion {
    /// Start of the text being completed (char index).
    start: usize,
    /// Chars after the caret that were part of the replace range when shown.
    tail: usize,
    items: Vec<CompletionEntry>,
    /// Indices into `items`, best match first.
    pub filtered: Vec<usize>,
    pub selected: usize,
    /// Set when the selection moved by keyboard; the view scrolls to it.
    pub scroll_to_selected: bool,
    /// Rows the list showed in the last frame (PageUp/PageDown step).
    pub page: usize,
    /// Scroll offset of the list in the last frame.
    pub scroll: f32,
    /// Width the filtered items need, measured by the view.
    pub width: Option<f32>,
    /// The items were cut short: typing on asks for them again rather
    /// than filtering these, which may lack the names it is after (#160).
    pub incomplete: bool,
}

impl Completion {
    /// Labels currently offered, best match first.
    pub fn labels(&self) -> Vec<&str> {
        self.filtered
            .iter()
            .filter_map(|i| self.items.get(*i))
            .map(|e| e.label.as_str())
            .collect()
    }
}

impl Completion {
    /// Opens a popup replacing the char range `replace`. Returns `None` if no
    /// item matches what is typed.
    pub fn new(
        state: &EditorState,
        replace: Range<usize>,
        items: Vec<CompletionEntry>,
    ) -> Option<Self> {
        let caret = state.primary().head;
        let start = replace.start.min(caret);
        let tail = replace.end.saturating_sub(caret);
        let mut c = Self {
            start,
            tail,
            items,
            filtered: Vec::new(),
            selected: 0,
            scroll_to_selected: true,
            page: 1,
            scroll: 0.0,
            width: None,
            incomplete: false,
        };
        c.update(state).then_some(c)
    }

    /// Selects the item labeled `label`, if it is offered.
    pub fn select_label(&mut self, label: &str) {
        if let Some(i) = self
            .filtered
            .iter()
            .position(|&i| self.items[i].label == label)
        {
            self.selected = i;
            self.scroll_to_selected = true;
        }
    }

    /// Char index where the completed word starts.
    pub fn start(&self) -> usize {
        self.start
    }

    pub fn items(&self) -> &[CompletionEntry] {
        &self.items
    }

    pub fn entry(&self, filtered_index: usize) -> Option<&CompletionEntry> {
        self.filtered.get(filtered_index).map(|&i| &self.items[i])
    }

    pub fn selected_entry(&self) -> Option<&CompletionEntry> {
        self.entry(self.selected)
    }

    /// The typed text the items are filtered by.
    pub fn prefix(&self, state: &EditorState) -> String {
        let caret = state.primary().head;
        state
            .rope
            .slice(self.start..caret.max(self.start))
            .to_string()
    }

    /// Re-validates after input. Returns `false` if the popup should close:
    /// several cursors, a selection, the caret left the word, or nothing matches.
    pub fn update(&mut self, state: &EditorState) -> bool {
        if state.sels.len() != 1 {
            return false;
        }
        let sel = state.primary();
        if !sel.is_empty() || sel.head < self.start || sel.head > state.len() {
            return false;
        }
        let prefix = self.prefix(state);
        if !prefix.chars().all(is_word_char) {
            return false;
        }
        let previous = self.selected_entry().map(|e| e.label.clone());
        let filtered = filter(&self.items, &prefix);
        if filtered != self.filtered {
            self.filtered = filtered;
            self.width = None;
            self.scroll_to_selected = true;
        }
        // Keep the selected item if it still matches.
        self.selected = previous
            .and_then(|label| {
                self.filtered
                    .iter()
                    .position(|&i| self.items[i].label == label)
            })
            .unwrap_or(0);
        !self.filtered.is_empty()
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let n = self.filtered.len() as isize;
        let next = self.selected as isize + delta;
        // Wrap only for single steps, clamp for page jumps.
        self.selected = if delta.abs() == 1 {
            next.rem_euclid(n)
        } else {
            next.clamp(0, n - 1)
        } as usize;
        self.scroll_to_selected = true;
    }

    /// Replaces the completed word with the selected entry as one undo step.
    pub fn accept(&self, state: &mut EditorState, time: f64) -> bool {
        let Some(entry) = self.selected_entry() else {
            return false;
        };
        let caret = state.primary().head;
        let tail = (0..self.tail)
            .take_while(|i| caret + i < state.len() && is_word_char(state.rope.char(caret + i)))
            .count();
        if entry.kind == CompletionKind::Snippet
            && super::snippet::has_tab_stops(&entry.insert_text)
        {
            state.expand_snippet(self.start..caret + tail, &entry.insert_text, time);
            return true;
        }
        state.history.seal();
        let inserted = state.apply_changes(
            vec![(self.start..caret + tail, entry.insert_text.clone())],
            super::history::EditKind::Other,
            time,
        );
        state.history.seal();
        if let Some(r) = inserted.first() {
            state.set_selections(vec![Selection::caret(r.end)], 0);
        }
        true
    }
}

/// Matches as completion ranks (`pythia_lsp_core::match_rank`):
/// prefix, then word starts (`fn` → `first_name`), then substring; a
/// prefix in the typed case first.
pub fn filter(items: &[CompletionEntry], prefix: &str) -> Vec<usize> {
    if prefix.is_empty() {
        return (0..items.len()).collect();
    }
    let mut scored: Vec<((u8, bool), usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let rank = pythia_lsp_core::match_rank(&item.label, prefix)?;
            Some(((rank, !item.label.starts_with(prefix)), i))
        })
        .collect();
    // Stable: equal ranks keep the app's order.
    scored.sort_by_key(|(rank, _)| *rank);
    scored.into_iter().map(|(_, i)| i).collect()
}

/// Number of identifier chars directly before `pos`.
pub fn word_prefix_len(rope: &Rope, pos: usize) -> usize {
    (0..pos)
        .rev()
        .take_while(|&i| is_word_char(rope.char(i)))
        .count()
}

/// Char index of the unmatched `(` enclosing `pos` within the current
/// statement, if any. Strings and comments are not skipped; good enough for
/// deciding when to hide a signature hint.
pub fn enclosing_paren(rope: &Rope, pos: usize) -> Option<usize> {
    const MAX_SCAN: usize = 4000;
    let mut depth = 0usize;
    let mut i = pos.min(rope.len_chars());
    let stop = pos.saturating_sub(MAX_SCAN);
    while i > stop {
        i -= 1;
        match rope.char(i) {
            ')' => depth += 1,
            '(' if depth == 0 => return Some(i),
            '(' => depth -= 1,
            ';' => return None,
            _ => {}
        }
    }
    None
}

/// Signature hint shown above the caret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub label: String,
    /// Byte range within `label` of the active parameter.
    pub active: Option<Range<usize>>,
    /// The `(` this hint belongs to (char index).
    pub open_paren: Option<usize>,
}

/// Hover info shown near a word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hover {
    /// Char range of the hovered word.
    pub range: Range<usize>,
    pub title: String,
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(label: &str, kind: CompletionKind) -> CompletionEntry {
        CompletionEntry {
            label: label.into(),
            detail: String::new(),
            kind,
            insert_text: label.into(),
            documentation: None,
        }
    }

    fn items() -> Vec<CompletionEntry> {
        vec![
            entry("EMPLOYEES", CompletionKind::Table),
            entry("EMP_NAME", CompletionKind::Column),
            entry("DEPARTMENTS", CompletionKind::Table),
            entry("emp", CompletionKind::Variable),
        ]
    }

    fn state(text: &str, caret: usize) -> EditorState {
        let mut s = EditorState::new(text);
        s.set_caret(caret);
        s
    }

    fn labels(c: &Completion) -> Vec<&str> {
        c.filtered
            .iter()
            .map(|&i| c.items()[i].label.as_str())
            .collect()
    }

    #[test]
    fn filters_while_typing() {
        let mut s = state("select e", 8);
        let mut c = Completion::new(&s, 7..8, items()).unwrap();
        assert_eq!(labels(&c), ["emp", "EMPLOYEES", "EMP_NAME", "DEPARTMENTS"]);
        s.insert("m", 1.0);
        s.insert("p", 1.1);
        s.insert("_", 1.2);
        assert!(c.update(&s));
        assert_eq!(labels(&c), ["EMP_NAME"]);
        s.insert("x", 1.3);
        assert!(!c.update(&s), "no match closes the popup");
    }

    #[test]
    fn prefix_word_start_and_substring_order() {
        assert_eq!(filter(&items(), "EMP"), vec![0, 1, 3]);
        // Word start, then substring.
        assert_eq!(filter(&items(), "nam"), vec![1]);
        assert_eq!(filter(&items(), "part"), vec![2]);
        // Letters scattered through a word don't match (#160).
        assert!(filter(&items(), "dpt").is_empty());
    }

    #[test]
    fn accept_replaces_typed_word_and_tail() {
        // Caret inside "em|xx": the tail of the word is replaced too.
        let mut s = state("select emxx from t", 9);
        let mut c = Completion::new(&s, 7..11, items()).unwrap();
        s.insert("p", 2.0); // typed since shown: "emp|xx"
        assert!(c.update(&s));
        c.move_selection(1);
        let chosen = c.selected_entry().unwrap().label.clone();
        assert!(c.accept(&mut s, 2.1));
        assert_eq!(s.text(), format!("select {chosen} from t"));
        assert_eq!(s.primary().head, 7 + chosen.len());
        // One undo step back to before the accept.
        s.undo();
        assert_eq!(s.text(), "select empxx from t");
    }

    #[test]
    fn accepting_a_snippet_expands_it() {
        let mut s = state("x := nv", 7);
        let snippet = CompletionEntry {
            insert_text: "nvl(${1:expr}, ${2:0})".into(),
            ..entry("nvl", CompletionKind::Snippet)
        };
        let c = Completion::new(&s, 5..7, vec![snippet]).unwrap();
        assert!(c.accept(&mut s, 1.0));
        assert_eq!(s.text(), "x := nvl(expr, 0)");
        assert_eq!(s.rope.slice(s.primary().range()).to_string(), "expr");
        assert!(s.snippet_step(true));
        assert_eq!(s.rope.slice(s.primary().range()).to_string(), "0");
        // Plain snippet entries without tab stops insert verbatim.
        let mut s = state("x", 1);
        let c = Completion::new(&s, 0..1, vec![entry("xyz", CompletionKind::Snippet)]).unwrap();
        assert!(c.accept(&mut s, 1.0));
        assert_eq!(s.text(), "xyz");
        assert!(s.snippet.is_none());
    }

    #[test]
    fn dismissal() {
        let mut s = state("select em", 9);
        let mut c = Completion::new(&s, 7..9, items()).unwrap();
        s.move_left(false, false);
        assert!(c.update(&s), "moving within the word keeps it open");
        s.move_cursors(false, |_, _| 6);
        assert!(!c.update(&s), "moving before the word closes it");
        let mut s = state("select em", 9);
        let mut c = Completion::new(&s, 7..9, items()).unwrap();
        s.insert(" ", 3.0);
        assert!(!c.update(&s), "typing a non-word char closes it");
        let s = state("select em", 9);
        assert!(Completion::new(&s, 7..9, vec![entry("dual", CompletionKind::Table)]).is_none());
    }

    #[test]
    fn selection_wraps_and_clamps() {
        let s = state("x", 1);
        let mut c = Completion::new(&s, 1..1, items()).unwrap();
        c.move_selection(-1);
        assert_eq!(c.selected, 3);
        c.move_selection(1);
        assert_eq!(c.selected, 0);
        c.move_selection(10);
        assert_eq!(c.selected, 3);
    }

    #[test]
    fn parens() {
        let rope = Rope::from_str("x; f(a, g(b), c");
        assert_eq!(enclosing_paren(&rope, rope.len_chars()), Some(4));
        assert_eq!(enclosing_paren(&rope, 11), Some(9));
        assert_eq!(enclosing_paren(&rope, 2), None);
        assert_eq!(word_prefix_len(&rope, 6), 1);
    }
}
