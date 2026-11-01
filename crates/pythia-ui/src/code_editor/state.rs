//! Editor model: text, cursors, history and all editing operations.
//!
//! Nothing here depends on egui, so every operation is unit-testable. The
//! widget in `view.rs` translates input events into calls on [`EditorState`].

use std::ops::Range;

use ropey::Rope;

use super::highlight::Highlighter;
use super::history::{Edit, EditKind, History};
use super::selection::{Selection, normalize};
use super::snippet::{self, Session};
use super::text::{
    col_from_visual, indent_of, line_col, line_end, line_len, line_start, line_text, next_word,
    pos_of, prev_word, visual_col, word_range,
};
use super::tracked::Tracked;

/// Lines whose trimmed text ends with one of these get an extra indent level
/// on Enter.
const INDENT_AFTER: &[&str] = &["BEGIN", "THEN", "LOOP", "IS", "AS", "DECLARE", "ELSE"];

pub struct EditorState {
    pub rope: Rope,
    /// Sorted, non-overlapping.
    pub sels: Vec<Selection>,
    pub primary: usize,
    pub history: History,
    pub highlighter: Highlighter,
    /// Bumped on every text change.
    pub revision: u64,
    pub tab_width: usize,
    /// Revision at which the last undo step was recorded.
    recorded_revision: u64,
    /// Folds and bookmarks, shifted by every edit.
    pub tracked: Tracked,
    /// Active snippet tab stops, mapped through edits.
    pub snippet: Option<Session>,
}

impl EditorState {
    pub fn new(text: &str) -> Self {
        let rope = Rope::from_str(text);
        let highlighter = Highlighter::new(&rope);
        Self {
            rope,
            sels: vec![Selection::caret(0)],
            primary: 0,
            history: History::default(),
            highlighter,
            revision: 0,
            tab_width: 4,
            recorded_revision: u64::MAX,
            tracked: Tracked::default(),
            snippet: None,
        }
    }

    pub fn set_text(&mut self, text: &str) {
        self.rope = Rope::from_str(text);
        self.highlighter.reset(&self.rope);
        self.sels = vec![Selection::caret(0)];
        self.primary = 0;
        self.history.clear();
        self.tracked.clear();
        self.snippet = None;
        self.revision += 1;
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn primary(&self) -> Selection {
        self.sels[self.primary.min(self.sels.len() - 1)]
    }

    pub fn len(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn indent_unit(&self) -> String {
        " ".repeat(self.tab_width.max(1))
    }

    pub fn set_selections(&mut self, mut sels: Vec<Selection>, primary: usize) {
        let len = self.len();
        for s in &mut sels {
            s.anchor = s.anchor.min(len);
            s.head = s.head.min(len);
        }
        if sels.is_empty() {
            sels.push(Selection::caret(0));
        }
        self.primary = normalize(&mut sels, primary);
        self.sels = sels;
        self.history.seal();
    }

    pub fn set_caret(&mut self, pos: usize) {
        self.set_selections(vec![Selection::caret(pos)], 0);
    }

    // ----- primitive edits ----------------------------------------------

    fn apply_edit(&mut self, edit: &Edit) {
        let removed_chars = edit.removed.chars().count();
        let line = self.rope.char_to_line(edit.at);
        let removed_breaks = edit.removed.matches('\n').count();
        self.rope.remove(edit.at..edit.at + removed_chars);
        self.rope.insert(edit.at, &edit.inserted);
        let inserted_breaks = edit.inserted.matches('\n').count();
        self.highlighter
            .edited(&self.rope, line, removed_breaks, inserted_breaks);
        let at_line_start = self.rope.line_to_char(line) == edit.at;
        self.tracked
            .shift(line, removed_breaks, inserted_breaks, at_line_start);
    }

    /// Replaces each (non-overlapping, sorted) range with its text as one
    /// undo step. Existing selections are mapped through the change.
    /// Returns the inserted ranges in new coordinates.
    pub fn apply_changes(
        &mut self,
        mut changes: Vec<(Range<usize>, String)>,
        kind: EditKind,
        time: f64,
    ) -> Vec<Range<usize>> {
        changes.sort_by_key(|(r, _)| r.start);
        changes.retain(|(r, text)| !(r.is_empty() && text.is_empty()));
        if changes.is_empty() {
            return Vec::new();
        }
        let before = self.sels.clone();
        let mut edits = Vec::with_capacity(changes.len());
        let mut inserted = Vec::with_capacity(changes.len());
        let mut delta: isize = 0;
        let mut mapping = Vec::with_capacity(changes.len());
        for (range, text) in &changes {
            let at = (range.start as isize + delta) as usize;
            let len = range.end - range.start;
            let removed = self.rope.slice(at..at + len).to_string();
            let edit = Edit {
                at,
                removed,
                inserted: text.clone(),
            };
            self.apply_edit(&edit);
            let new_len = text.chars().count();
            inserted.push(at..at + new_len);
            mapping.push((range.clone(), new_len));
            delta += new_len as isize - len as isize;
            edits.push(edit);
        }
        for s in &mut self.sels {
            s.anchor = map_pos(s.anchor, &mapping);
            s.head = map_pos(s.head, &mapping);
            s.goal = None;
        }
        if let Some(session) = &mut self.snippet {
            for r in session.stops.iter_mut().flatten() {
                // Text typed at a placeholder's end extends it.
                *r = map_start(r.start, &mapping)..map_pos(r.end, &mapping);
            }
        }
        self.primary = normalize(&mut self.sels, self.primary);
        self.revision += 1;
        self.history
            .record(edits, before, self.sels.clone(), kind, time);
        self.recorded_revision = self.revision;
        inserted
    }

    /// Sets the selections after an edit and stores them as the undo step's
    /// "after" state.
    fn set_after(&mut self, mut sels: Vec<Selection>) {
        let primary = self.primary.min(sels.len().saturating_sub(1));
        self.primary = normalize(&mut sels, primary);
        self.sels = sels;
        // Only if this operation actually recorded a step.
        if self.recorded_revision == self.revision {
            self.history.set_last_after(self.sels.clone());
        }
    }

    pub fn undo(&mut self) -> bool {
        self.snippet = None;
        let Some(tx) = self.history.undo().cloned() else {
            return false;
        };
        for edit in tx.edits.iter().rev() {
            let inverse = Edit {
                at: edit.at,
                removed: edit.inserted.clone(),
                inserted: edit.removed.clone(),
            };
            self.apply_edit(&inverse);
        }
        self.sels = tx.before.clone();
        self.primary = self.primary.min(self.sels.len() - 1);
        self.revision += 1;
        true
    }

    pub fn redo(&mut self) -> bool {
        self.snippet = None;
        let Some(tx) = self.history.redo().cloned() else {
            return false;
        };
        for edit in &tx.edits {
            self.apply_edit(edit);
        }
        self.sels = tx.after.clone();
        self.primary = self.primary.min(self.sels.len() - 1);
        self.revision += 1;
        true
    }

    // ----- typing -----------------------------------------------------

    /// Replaces every selection with `text` (typing, paste).
    pub fn insert(&mut self, text: &str, time: f64) {
        let kind = if text.chars().count() == 1 {
            EditKind::Insert
        } else {
            EditKind::Other
        };
        let changes = self
            .sels
            .iter()
            .map(|s| (s.range(), text.to_owned()))
            .collect();
        let inserted = self.apply_changes(changes, kind, time);
        self.set_after(inserted.iter().map(|r| Selection::caret(r.end)).collect());
    }

    /// Inserts a different text at each cursor (multi-cursor paste).
    pub fn insert_each(&mut self, texts: &[String], time: f64) {
        let changes = self
            .sels
            .iter()
            .zip(texts)
            .map(|(s, t)| (s.range(), t.clone()))
            .collect();
        let inserted = self.apply_changes(changes, EditKind::Other, time);
        self.set_after(inserted.iter().map(|r| Selection::caret(r.end)).collect());
    }

    /// Enter: line break plus the current line's indentation, one level more
    /// after block openers.
    pub fn newline(&mut self, time: f64) {
        let changes = self
            .sels
            .iter()
            .map(|s| {
                let (line, col) = line_col(&self.rope, s.start());
                let text = line_text(&self.rope, line);
                let before: String = text.chars().take(col).collect();
                let mut indent = indent_of(&before).to_owned();
                if opens_block(&before) {
                    indent.push_str(&self.indent_unit());
                }
                (s.range(), format!("\n{indent}"))
            })
            .collect();
        let inserted = self.apply_changes(changes, EditKind::Other, time);
        self.set_after(inserted.iter().map(|r| Selection::caret(r.end)).collect());
    }

    pub fn backspace(&mut self, word: bool, time: f64) {
        let ranges: Vec<Range<usize>> = self
            .sels
            .iter()
            .map(|s| {
                if !s.is_empty() {
                    return s.range();
                }
                let pos = s.head;
                if word {
                    return prev_word(&self.rope, pos)..pos;
                }
                if pos == 0 {
                    return 0..0;
                }
                let (line, col) = line_col(&self.rope, pos);
                if col == 0 {
                    let crlf = pos >= 2 && self.rope.char(pos - 2) == '\r';
                    return pos - if crlf { 2 } else { 1 }..pos;
                }
                // In leading spaces, delete back to the previous tab stop.
                let text = line_text(&self.rope, line);
                let before: String = text.chars().take(col).collect();
                if before.chars().all(|c| c == ' ') {
                    let n = (col - 1) % self.tab_width.max(1) + 1;
                    return pos - n..pos;
                }
                pos - 1..pos
            })
            .collect();
        self.delete_ranges(ranges, time);
    }

    pub fn delete(&mut self, word: bool, time: f64) {
        let len = self.len();
        let ranges: Vec<Range<usize>> = self
            .sels
            .iter()
            .map(|s| {
                if !s.is_empty() {
                    return s.range();
                }
                let pos = s.head;
                if word {
                    return pos..next_word(&self.rope, pos);
                }
                if pos >= len {
                    return pos..pos;
                }
                let crlf =
                    self.rope.char(pos) == '\r' && pos + 1 < len && self.rope.char(pos + 1) == '\n';
                pos..pos + if crlf { 2 } else { 1 }
            })
            .collect();
        self.delete_ranges(ranges, time);
    }

    fn delete_ranges(&mut self, ranges: Vec<Range<usize>>, time: f64) {
        let changes = merge_ranges(ranges)
            .into_iter()
            .map(|r| (r, String::new()))
            .collect();
        let inserted = self.apply_changes(changes, EditKind::Delete, time);
        if !inserted.is_empty() {
            self.set_after(inserted.iter().map(|r| Selection::caret(r.start)).collect());
        }
    }

    // ----- line operations ----------------------------------------------

    /// Line blocks covered by the selections, merged; inclusive line ranges.
    pub fn line_blocks(&self) -> Vec<(usize, usize)> {
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        for s in &self.sels {
            let first = self.rope.char_to_line(s.start());
            let mut last = self.rope.char_to_line(s.end());
            // A selection ending at column 0 doesn't include that line.
            if last > first && s.end() == line_start(&self.rope, last) {
                last -= 1;
            }
            match blocks.last_mut() {
                Some(b) if first <= b.1 + 1 => b.1 = b.1.max(last),
                _ => blocks.push((first, last)),
            }
        }
        blocks
    }

    fn spans_lines(&self) -> bool {
        self.sels
            .iter()
            .any(|s| self.rope.char_to_line(s.start()) != self.rope.char_to_line(s.end()))
    }

    /// Tab: indent selected lines, or insert spaces to the next tab stop.
    pub fn tab(&mut self, time: f64) {
        if self.spans_lines() {
            return self.indent_lines(time);
        }
        let tw = self.tab_width.max(1);
        let changes = self
            .sels
            .iter()
            .map(|s| {
                let (line, col) = line_col(&self.rope, s.start());
                let v = visual_col(&line_text(&self.rope, line), col, tw);
                (s.range(), " ".repeat(tw - v % tw))
            })
            .collect();
        let inserted = self.apply_changes(changes, EditKind::Other, time);
        self.set_after(inserted.iter().map(|r| Selection::caret(r.end)).collect());
    }

    pub fn indent_lines(&mut self, time: f64) {
        let unit = self.indent_unit();
        let changes = self
            .line_blocks()
            .into_iter()
            .flat_map(|(a, b)| a..=b)
            .filter(|&l| line_len(&self.rope, l) > 0)
            .map(|l| {
                let p = line_start(&self.rope, l);
                (p..p, unit.clone())
            })
            .collect();
        self.apply_changes(changes, EditKind::Other, time);
    }

    pub fn outdent_lines(&mut self, time: f64) {
        let tw = self.tab_width.max(1);
        let changes = self
            .line_blocks()
            .into_iter()
            .flat_map(|(a, b)| a..=b)
            .filter_map(|l| {
                let text = line_text(&self.rope, l);
                let n = if text.starts_with('\t') {
                    1
                } else {
                    text.chars().take(tw).take_while(|c| *c == ' ').count()
                };
                let p = line_start(&self.rope, l);
                (n > 0).then(|| (p..p + n, String::new()))
            })
            .collect();
        self.apply_changes(changes, EditKind::Other, time);
    }

    /// Ctrl+/: toggles `-- ` line comments on the selected lines.
    pub fn toggle_comment(&mut self, time: f64) {
        let lines: Vec<usize> = self
            .line_blocks()
            .into_iter()
            .flat_map(|(a, b)| a..=b)
            .collect();
        let texts: Vec<(usize, String)> = lines
            .iter()
            .map(|&l| (l, line_text(&self.rope, l)))
            .collect();
        let non_blank: Vec<&(usize, String)> =
            texts.iter().filter(|(_, t)| !t.trim().is_empty()).collect();
        if non_blank.is_empty() {
            return;
        }
        let all_commented = non_blank
            .iter()
            .all(|(_, t)| t.trim_start().starts_with("--"));
        let changes = if all_commented {
            non_blank
                .iter()
                .map(|(l, t)| {
                    let indent = indent_of(t).chars().count();
                    let rest = &t[indent_of(t).len() + 2..];
                    let n = 2 + usize::from(rest.starts_with(' '));
                    let p = line_start(&self.rope, *l) + indent;
                    (p..p + n, String::new())
                })
                .collect()
        } else {
            let min_indent = non_blank
                .iter()
                .map(|(_, t)| indent_of(t).chars().count())
                .min()
                .unwrap_or(0);
            non_blank
                .iter()
                .map(|(l, _)| {
                    let p = line_start(&self.rope, *l) + min_indent;
                    (p..p, "-- ".to_owned())
                })
                .collect()
        };
        self.apply_changes(changes, EditKind::Other, time);
    }

    /// Upper/lower-cases the selections (or the word at each caret).
    pub fn change_case(&mut self, upper: bool, time: f64) {
        let ranges: Vec<Range<usize>> = self
            .sels
            .iter()
            .map(|s| {
                if s.is_empty() {
                    let (a, b) = word_range(&self.rope, s.head);
                    a..b
                } else {
                    s.range()
                }
            })
            .collect();
        let ranges = merge_ranges(ranges);
        let changes: Vec<_> = ranges
            .iter()
            .map(|r| {
                let text = self.rope.slice(r.clone()).to_string();
                (
                    r.clone(),
                    if upper {
                        text.to_uppercase()
                    } else {
                        text.to_lowercase()
                    },
                )
            })
            .collect();
        let had_selection = self.sels.iter().any(|s| !s.is_empty());
        let carets: Vec<usize> = self.sels.iter().map(|s| s.head).collect();
        let inserted = self.apply_changes(changes, EditKind::Other, time);
        if had_selection {
            self.set_after(
                inserted
                    .into_iter()
                    .map(|r| Selection::new(r.start, r.end))
                    .collect(),
            );
        } else {
            self.set_after(carets.into_iter().map(Selection::caret).collect());
        }
    }

    /// Ctrl+D: duplicates the selected lines below themselves.
    pub fn duplicate_lines(&mut self, time: f64) {
        let blocks = self.line_blocks();
        let mut changes = Vec::new();
        let mut moved = Vec::new();
        let mut shift = 0;
        for (a, b) in blocks {
            let start = line_start(&self.rope, a);
            let end = line_end(&self.rope, b);
            let block = self.rope.slice(start..end).to_string();
            let len = block.chars().count() + 1;
            changes.push((end..end, format!("\n{block}")));
            for s in self
                .sels
                .iter()
                .filter(|s| s.start() >= start && s.end() <= end)
            {
                moved.push(Selection::new(s.anchor + shift + len, s.head + shift + len));
            }
            shift += len;
        }
        self.apply_changes(changes, EditKind::Other, time);
        self.set_after(moved);
    }

    /// Alt+Up / Alt+Down: moves the selected lines.
    pub fn move_lines(&mut self, up: bool, time: f64) {
        let blocks = self.line_blocks();
        let last_line = self.rope.len_lines() - 1;
        if blocks
            .iter()
            .any(|&(a, b)| if up { a == 0 } else { b >= last_line })
        {
            return;
        }
        let mut changes = Vec::new();
        let mut moved = Vec::new();
        for (a, b) in blocks {
            let block_start = line_start(&self.rope, a);
            let block_end = line_end(&self.rope, b);
            let block = self.rope.slice(block_start..block_end).to_string();
            let (range, text, new_start) = if up {
                let other = line_text(&self.rope, a - 1);
                let range_start = line_start(&self.rope, a - 1);
                (
                    range_start..block_end,
                    format!("{block}\n{other}"),
                    range_start,
                )
            } else {
                let other = line_text(&self.rope, b + 1);
                let range_end = line_end(&self.rope, b + 1);
                let new_start = block_start + other.chars().count() + 1;
                (
                    block_start..range_end,
                    format!("{other}\n{block}"),
                    new_start,
                )
            };
            for s in self
                .sels
                .iter()
                .filter(|s| s.start() >= block_start && s.end() <= block_end)
            {
                moved.push(Selection::new(
                    s.anchor - block_start + new_start,
                    s.head - block_start + new_start,
                ));
            }
            changes.push((range, text));
        }
        // Every change keeps its length (up to normalized line breaks), so
        // the positions computed above stay valid.
        self.apply_changes(changes, EditKind::Other, time);
        self.set_after(moved);
    }

    // ----- clipboard ------------------------------------------------------

    /// Text to copy. With no selection, copies the whole current line(s).
    pub fn copy_text(&self) -> String {
        if self.sels.iter().all(Selection::is_empty) {
            let mut out = String::new();
            for (a, b) in self.line_blocks() {
                let end = if b + 1 < self.rope.len_lines() {
                    line_start(&self.rope, b + 1)
                } else {
                    self.len()
                };
                out.push_str(&self.rope.slice(line_start(&self.rope, a)..end).to_string());
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            return out;
        }
        self.sels
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| self.rope.slice(s.range()).to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn cut(&mut self, time: f64) -> String {
        let text = self.copy_text();
        if self.sels.iter().all(Selection::is_empty) {
            let ranges = self
                .line_blocks()
                .into_iter()
                .map(|(a, b)| {
                    let end = if b + 1 < self.rope.len_lines() {
                        line_start(&self.rope, b + 1)
                    } else {
                        self.len()
                    };
                    line_start(&self.rope, a)..end
                })
                .collect();
            let changes = merge_ranges(ranges)
                .into_iter()
                .map(|r| (r, String::new()))
                .collect();
            self.apply_changes(changes, EditKind::Other, time);
        } else {
            let changes = self
                .sels
                .iter()
                .map(|s| (s.range(), String::new()))
                .collect();
            let inserted = self.apply_changes(changes, EditKind::Other, time);
            self.set_after(inserted.iter().map(|r| Selection::caret(r.start)).collect());
        }
        text
    }

    pub fn paste(&mut self, text: &str, time: f64) {
        let text = text.replace("\r\n", "\n");
        let lines: Vec<String> = text
            .trim_end_matches('\n')
            .split('\n')
            .map(str::to_owned)
            .collect();
        if self.sels.len() > 1 && lines.len() == self.sels.len() {
            self.insert_each(&lines, time);
        } else {
            self.insert(&text, time);
        }
    }

    // ----- movement -----------------------------------------------------

    /// Moves every cursor to `f(cursor)`, extending the selection if `extend`.
    pub fn move_cursors(&mut self, extend: bool, f: impl Fn(&Self, &Selection) -> usize) {
        let sels: Vec<Selection> = self
            .sels
            .iter()
            .map(|s| s.moved_to(f(self, s), extend))
            .collect();
        self.set_selections(sels, self.primary);
    }

    pub fn move_left(&mut self, extend: bool, word: bool) {
        self.move_cursors(extend, |st, s| {
            if !s.is_empty() && !extend && !word {
                return s.start();
            }
            if word {
                return prev_word(&st.rope, s.head);
            }
            let p = s.head;
            if p >= 2 && st.rope.char(p - 1) == '\n' && st.rope.char(p - 2) == '\r' {
                p - 2
            } else {
                p.saturating_sub(1)
            }
        });
    }

    pub fn move_right(&mut self, extend: bool, word: bool) {
        let len = self.len();
        self.move_cursors(extend, |st, s| {
            if !s.is_empty() && !extend && !word {
                return s.end();
            }
            if word {
                return next_word(&st.rope, s.head);
            }
            let p = s.head;
            if p + 1 < len && st.rope.char(p) == '\r' && st.rope.char(p + 1) == '\n' {
                p + 2
            } else {
                (p + 1).min(len)
            }
        });
    }

    /// Moves vertically by `lines` (negative = up), keeping the goal column.
    #[cfg(test)]
    pub fn move_vertical(&mut self, lines: isize, extend: bool) {
        self.move_vertical_mapped(lines, extend, &|line, delta| line as isize + delta);
    }

    /// Like [`Self::move_vertical`], with `target(line, delta)` choosing the
    /// destination line (e.g. skipping folded lines). Out-of-range targets go
    /// to the start or end of the text.
    pub fn move_vertical_mapped(
        &mut self,
        lines: isize,
        extend: bool,
        target: &dyn Fn(usize, isize) -> isize,
    ) {
        let tw = self.tab_width;
        let last = self.rope.len_lines() as isize - 1;
        let sels: Vec<Selection> = self
            .sels
            .iter()
            .map(|s| {
                let (line, col) = line_col(&self.rope, s.head);
                let goal = s
                    .goal
                    .unwrap_or_else(|| visual_col(&line_text(&self.rope, line), col, tw));
                let target = target(line, lines);
                let pos = if target < 0 {
                    0
                } else if target > last {
                    self.len()
                } else {
                    let t = target as usize;
                    pos_of(
                        &self.rope,
                        t,
                        col_from_visual(&line_text(&self.rope, t), goal, tw),
                    )
                };
                let mut moved = s.moved_to(pos, extend);
                moved.goal = Some(goal);
                moved
            })
            .collect();
        self.sels = sels;
        self.primary = normalize(&mut self.sels, self.primary);
        self.history.seal();
    }

    /// Home: first non-blank char, or column 0 if already there.
    pub fn move_home(&mut self, extend: bool) {
        self.move_cursors(extend, |st, s| {
            let (line, col) = line_col(&st.rope, s.head);
            let text = line_text(&st.rope, line);
            let first = indent_of(&text).chars().count();
            line_start(&st.rope, line) + if col == first { 0 } else { first }
        });
    }

    pub fn move_end(&mut self, extend: bool) {
        self.move_cursors(extend, |st, s| {
            line_end(&st.rope, st.rope.char_to_line(s.head))
        });
    }

    pub fn move_doc_start(&mut self, extend: bool) {
        self.move_cursors(extend, |_, _| 0);
    }

    pub fn move_doc_end(&mut self, extend: bool) {
        let len = self.len();
        self.move_cursors(extend, |_, _| len);
    }

    pub fn select_all(&mut self) {
        self.set_selections(vec![Selection::new(0, self.len())], 0);
    }

    /// Keeps only the primary cursor (Esc).
    pub fn single_cursor(&mut self) {
        let p = self.primary();
        self.set_selections(vec![p], 0);
    }

    pub fn add_cursor(&mut self, pos: usize) {
        let mut sels = self.sels.clone();
        sels.push(Selection::caret(pos));
        let primary = sels.len() - 1;
        self.set_selections(sels, primary);
    }

    pub fn select_word_at(&mut self, pos: usize) {
        let (a, b) = word_range(&self.rope, pos);
        self.set_selections(vec![Selection::new(a, b)], 0);
    }

    pub fn select_line_at(&mut self, pos: usize) {
        let line = self.rope.char_to_line(pos);
        let end = if line + 1 < self.rope.len_lines() {
            line_start(&self.rope, line + 1)
        } else {
            self.len()
        };
        self.set_selections(vec![Selection::new(line_start(&self.rope, line), end)], 0);
    }

    /// Block selection: one cursor per line between the two (line, visual col) corners.
    pub fn select_block(&mut self, from: (usize, usize), to: (usize, usize)) {
        let tw = self.tab_width;
        let (l0, l1) = (from.0.min(to.0), from.0.max(to.0));
        let sels: Vec<Selection> = (l0..=l1.min(self.rope.len_lines() - 1))
            .map(|l| {
                let text = line_text(&self.rope, l);
                let a = pos_of(&self.rope, l, col_from_visual(&text, from.1, tw));
                let h = pos_of(&self.rope, l, col_from_visual(&text, to.1, tw));
                Selection::new(a, h)
            })
            .collect();
        let primary = if to.0 >= from.0 { sels.len() - 1 } else { 0 };
        self.set_selections(sels, primary);
    }

    // ----- snippets ---------------------------------------------------------

    /// Replaces the char range `range` with a snippet body and selects its
    /// first tab stop. Returns whether a snippet session started.
    pub fn expand_snippet(&mut self, range: Range<usize>, body: &str, time: f64) -> bool {
        let line = self.rope.char_to_line(range.start.min(self.len()));
        let indent = indent_of(&line_text(&self.rope, line)).to_owned();
        let expanded = snippet::expand(body, &indent, &self.indent_unit());
        self.history.seal();
        let inserted = self.apply_changes(vec![(range, expanded.text)], EditKind::Other, time);
        self.history.seal();
        let at = inserted.first().map_or(0, |r| r.start);
        let stops: Vec<Vec<Range<usize>>> = expanded
            .stops
            .into_iter()
            .map(|rs| rs.into_iter().map(|r| at + r.start..at + r.end).collect())
            .collect();
        let session = Session { stops, current: 0 };
        self.select_stop(&session);
        if session.is_last() {
            self.snippet = None;
            false
        } else {
            self.snippet = Some(session);
            true
        }
    }

    fn select_stop(&mut self, session: &Session) {
        let sels: Vec<Selection> = session.stops[session.current]
            .iter()
            .map(|r| Selection::new(r.start, r.end))
            .collect();
        self.set_selections(sels, 0);
    }

    /// Tab / Shift+Tab inside a snippet. Returns false if there is none.
    pub fn snippet_step(&mut self, forward: bool) -> bool {
        let Some(mut session) = self.snippet.take() else {
            return false;
        };
        if forward {
            session.current += 1;
        } else {
            session.current = session.current.saturating_sub(1);
        }
        let current = session.current.min(session.stops.len() - 1);
        session.current = current;
        self.select_stop(&session);
        if !session.is_last() {
            self.snippet = Some(session);
        }
        true
    }

    /// Ends the snippet if the caret left the current stop.
    pub fn check_snippet(&mut self) {
        let head = self.primary().head;
        if self.snippet.as_ref().is_some_and(|s| !s.contains(head)) {
            self.snippet = None;
        }
    }

    /// Replaces byte ranges (e.g. search matches) as one undo step.
    pub fn replace_byte_ranges(
        &mut self,
        ranges: Vec<(Range<usize>, String)>,
        time: f64,
    ) -> Vec<Range<usize>> {
        let changes = ranges
            .into_iter()
            .map(|(r, t)| {
                (
                    self.rope.byte_to_char(r.start)..self.rope.byte_to_char(r.end),
                    t,
                )
            })
            .collect();
        self.apply_changes(changes, EditKind::Other, time)
    }
}

/// Maps a position through sorted changes `(old range, new length)`.
/// Positions at an insertion point move after the inserted text.
fn map_pos(pos: usize, changes: &[(Range<usize>, usize)]) -> usize {
    let mut delta: isize = 0;
    for (r, new_len) in changes {
        if pos < r.start || (pos == r.start && !r.is_empty()) {
            break;
        }
        if pos == r.start || pos < r.end {
            // At an insertion point, or inside a replaced range.
            return (r.start as isize + delta) as usize + new_len;
        }
        delta += *new_len as isize - (r.end - r.start) as isize;
    }
    (pos as isize + delta) as usize
}

/// Like [`map_pos`], but a position at an insertion point stays before the
/// inserted text and a position inside a replaced range moves to its start.
fn map_start(pos: usize, changes: &[(Range<usize>, usize)]) -> usize {
    let mut delta: isize = 0;
    for (r, new_len) in changes {
        if pos <= r.start {
            break;
        }
        if pos < r.end {
            return (r.start as isize + delta) as usize;
        }
        delta += *new_len as isize - (r.end - r.start) as isize;
    }
    (pos as isize + delta) as usize
}

fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last)
                if r.start <= last.end
                    && !(r.is_empty() && r.start == last.end && last.start != last.end) =>
            {
                last.end = last.end.max(r.end)
            }
            _ => out.push(r),
        }
    }
    out.retain(|r| !r.is_empty());
    out
}

fn opens_block(before_cursor: &str) -> bool {
    let trimmed = before_cursor.trim_end();
    if trimmed.ends_with('(') {
        return true;
    }
    let word: String = trimmed
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    let word: String = word.chars().rev().collect();
    INDENT_AFTER.iter().any(|k| word.eq_ignore_ascii_case(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a state from text where `|` marks carets and `[`..`]` selections.
    fn st(marked: &str) -> EditorState {
        let mut text = String::new();
        let mut sels = Vec::new();
        let mut anchor = None;
        for c in marked.chars() {
            let pos = text.chars().count();
            match c {
                '|' => sels.push(Selection::caret(pos)),
                '[' => anchor = Some(pos),
                ']' => sels.push(Selection::new(anchor.take().unwrap(), pos)),
                c => text.push(c),
            }
        }
        let mut s = EditorState::new(&text);
        s.set_selections(sels, 0);
        s
    }

    /// Renders text with selection markers, inverse of `st`.
    fn show(s: &EditorState) -> String {
        let text: Vec<char> = s.text().chars().collect();
        let mut out = String::new();
        for i in 0..=text.len() {
            for sel in &s.sels {
                if sel.is_empty() && sel.head == i {
                    out.push('|');
                } else if !sel.is_empty() && sel.end() == i {
                    out.push(']');
                }
            }
            for sel in &s.sels {
                if !sel.is_empty() && sel.start() == i {
                    out.push('[');
                }
            }
            if let Some(c) = text.get(i) {
                out.push(*c);
            }
        }
        out
    }

    #[test]
    fn typing_and_undo_grouping() {
        let mut s = st("|");
        for (i, c) in "select x".chars().enumerate() {
            s.insert(&c.to_string(), i as f64 * 0.1);
        }
        assert_eq!(show(&s), "select x|");
        s.undo();
        assert_eq!(show(&s), "select|");
        s.undo();
        assert_eq!(show(&s), "|");
        s.redo();
        s.redo();
        assert_eq!(show(&s), "select x|");
    }

    #[test]
    fn multi_cursor_typing() {
        let mut s = st("a|b\nc|d\n[ef]");
        s.insert("X", 0.0);
        assert_eq!(show(&s), "aX|b\ncX|d\nX|");
        s.backspace(false, 0.1);
        assert_eq!(show(&s), "a|b\nc|d\n|");
        s.undo();
        assert_eq!(show(&s), "aX|b\ncX|d\nX|");
    }

    #[test]
    fn backspace_and_delete() {
        let mut s = st("x\r\n|y");
        s.backspace(false, 0.0);
        assert_eq!(show(&s), "x|y");
        let mut s = st("        |x");
        s.backspace(false, 0.0);
        assert_eq!(show(&s), "    |x");
        let mut s = st("select emp|");
        s.backspace(true, 0.0);
        assert_eq!(show(&s), "select |");
        let mut s = st("|select emp");
        s.delete(true, 0.0);
        assert_eq!(show(&s), "|emp");
        let mut s = st("|\r\nx");
        s.delete(false, 0.0);
        assert_eq!(show(&s), "|x");
    }

    #[test]
    fn newline_auto_indent() {
        let mut s = st("  begin|");
        s.newline(0.0);
        assert_eq!(show(&s), "  begin\n      |");
        let mut s = st("  x := 1;|");
        s.newline(0.0);
        assert_eq!(show(&s), "  x := 1;\n  |");
        let mut s = st("if a then|");
        s.newline(0.0);
        assert_eq!(show(&s), "if a then\n    |");
        let mut s = st("f(|");
        s.newline(0.0);
        assert_eq!(show(&s), "f(\n    |");
        let mut s = st("this|");
        s.newline(0.0);
        assert_eq!(show(&s), "this\n|");
    }

    #[test]
    fn indent_outdent() {
        let mut s = st("[a\nb\n]c");
        s.tab(0.0);
        assert_eq!(s.text(), "    a\n    b\nc");
        s.outdent_lines(0.1);
        assert_eq!(s.text(), "a\nb\nc");
        let mut s = st("ab|c");
        s.tab(0.0);
        assert_eq!(show(&s), "ab  |c");
        let mut s = st("\tx|");
        s.outdent_lines(0.0);
        assert_eq!(show(&s), "x|");
    }

    #[test]
    fn comment_toggle() {
        let mut s = st("  [select 1\n\n    from dual]");
        s.toggle_comment(0.0);
        assert_eq!(s.text(), "  -- select 1\n\n  --   from dual");
        s.toggle_comment(0.1);
        assert_eq!(s.text(), "  select 1\n\n    from dual");
    }

    #[test]
    fn case_conversion() {
        let mut s = st("select [emp]");
        s.change_case(true, 0.0);
        assert_eq!(show(&s), "select [EMP]");
        let mut s = st("sel|ect x");
        s.change_case(true, 0.0);
        assert_eq!(show(&s), "SEL|ECT x");
    }

    #[test]
    fn duplicate_and_move() {
        let mut s = st("a\nb|\nc");
        s.duplicate_lines(0.0);
        assert_eq!(show(&s), "a\nb\nb|\nc");
        let mut s = st("a\nb|\nc");
        s.move_lines(true, 0.0);
        assert_eq!(show(&s), "b|\na\nc");
        s.move_lines(true, 0.1);
        assert_eq!(show(&s), "b|\na\nc");
        s.move_lines(false, 0.2);
        s.move_lines(false, 0.3);
        assert_eq!(show(&s), "a\nc\nb|");
        s.undo();
        assert_eq!(show(&s), "a\nb|\nc");
    }

    #[test]
    fn clipboard() {
        let mut s = st("ab|c\ndef");
        assert_eq!(s.copy_text(), "abc\n");
        assert_eq!(s.cut(0.0), "abc\n");
        assert_eq!(s.text(), "def");
        let mut s = st("|x\n|y");
        s.paste("1\n2\n", 0.0);
        assert_eq!(show(&s), "1|x\n2|y");
        let mut s = st("[ab]c");
        s.paste("Z", 0.0);
        assert_eq!(show(&s), "Z|c");
    }

    #[test]
    fn movement() {
        let mut s = st("ab|cdef\nx\nlonger line");
        s.move_vertical(1, false);
        assert_eq!(show(&s), "abcdef\nx|\nlonger line");
        s.move_vertical(1, false);
        assert_eq!(show(&s), "abcdef\nx\nlo|nger line"); // goal column kept
        s.move_vertical(1, false);
        assert_eq!(s.primary().head, s.len());
        let mut s = st("    se|lect");
        s.move_home(false);
        assert_eq!(show(&s), "    |select");
        s.move_home(false);
        assert_eq!(show(&s), "|    select");
        s.move_end(true);
        assert_eq!(show(&s), "[    select]");
        s.move_left(false, false);
        assert_eq!(show(&s), "|    select");
        let mut s = st("a\r\n|b");
        s.move_left(false, false);
        assert_eq!(show(&s), "a|\r\nb");
        s.move_right(false, false);
        assert_eq!(show(&s), "a\r\n|b");
    }

    #[test]
    fn block_selection() {
        let mut s = st("abcd\nab\nabcdef");
        s.select_block((0, 1), (2, 3));
        assert_eq!(show(&s), "a[bc]d\na[b]\na[bc]def");
        s.insert("X", 0.0);
        assert_eq!(show(&s), "aX|d\naX|\naX|def");
    }

    #[test]
    fn snippet_session() {
        let mut s = st("  x|");
        s.sels = vec![Selection::caret(3)];
        assert!(s.expand_snippet(2..3, "if ${1:cond} then\n\t${2:null;}\nend if;$0", 0.0));
        assert_eq!(show(&s), "  if [cond] then\n      null;\n  end if;");
        s.insert("a > 1", 0.1);
        assert_eq!(show(&s), "  if a > 1| then\n      null;\n  end if;");
        assert!(s.snippet_step(true));
        assert_eq!(show(&s), "  if a > 1 then\n      [null;]\n  end if;");
        assert!(s.snippet_step(true));
        assert_eq!(show(&s), "  if a > 1 then\n      null;\n  end if;|");
        assert!(s.snippet.is_none());
    }

    #[test]
    fn snippet_mirrors_and_exit() {
        let mut s = st("|");
        s.expand_snippet(0..0, "${1:t} := ${1:t} + 1;$0", 0.0);
        assert_eq!(show(&s), "[t] := [t] + 1;");
        s.insert("x", 0.0);
        assert_eq!(show(&s), "x| := x| + 1;");
        s.check_snippet();
        assert!(s.snippet.is_some());
        s.move_doc_end(false);
        s.check_snippet();
        assert!(s.snippet.is_none());
    }

    #[test]
    fn folds_and_bookmarks_follow_edits() {
        let mut s = st("a\nb\nc\n|d\ne");
        s.tracked.folds = vec![(3, 4)];
        s.tracked.toggle_bookmark(4, Some(1));
        s.newline(0.0);
        assert_eq!(s.tracked.folds, [(4, 5)]);
        assert_eq!(s.tracked.bookmarks[0].line, 5);
        s.undo();
        assert_eq!(s.tracked.folds, [(3, 4)]);
        assert_eq!(s.tracked.bookmarks[0].line, 4);
    }

    #[test]
    fn vertical_movement_skips_folds() {
        let mut s = st("a|\nb\nc\nd");
        let map = super::super::fold::DisplayMap::new(4, [(0, 2)]);
        s.move_vertical_mapped(1, false, &|l, d| map.offset_line(l, d));
        assert_eq!(show(&s), "a\nb\nc\nd|");
    }

    #[test]
    fn map_positions() {
        let changes = [(2..2, 3), (5..7, 1)];
        assert_eq!(map_pos(1, &changes), 1);
        assert_eq!(map_pos(2, &changes), 5);
        assert_eq!(map_pos(4, &changes), 7);
        assert_eq!(map_pos(6, &changes), 9);
        assert_eq!(map_pos(8, &changes), 10);
    }

    #[test]
    fn highlighter_stays_in_sync() {
        let mut s = st("select 1|");
        s.insert(" /* x", 0.0);
        s.newline(0.1);
        s.insert("y */", 0.2);
        s.newline(0.3);
        s.undo();
        s.undo();
        let full = Highlighter::new(&s.rope);
        for l in 0..s.rope.len_lines() {
            assert_eq!(s.highlighter.state(l), full.state(l));
        }
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn large_buffer_edits_are_fast() {
        let mut text = String::new();
        for i in 0..50_000 {
            text.push_str(&format!("  l_value_{i} := l_value_{i} + 1; -- step {i}\n"));
        }
        let mut s = EditorState::new(&text);
        s.set_caret(s.rope.line_to_char(25_000));
        let start = std::time::Instant::now();
        for i in 0..1000 {
            s.insert("x", i as f64);
            if i % 50 == 0 {
                s.newline(i as f64);
            }
        }
        let elapsed = start.elapsed();
        assert!(elapsed.as_millis() < 500, "1000 edits took {elapsed:?}");
    }
}
