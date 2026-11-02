//! Find/replace bar state and UI.

use std::ops::Range;

use egui::{Key, RichText, Ui};

use super::search::{Matcher, SearchQuery, next_match};

#[derive(Default)]
pub struct FindBar {
    pub open: bool,
    pub replace_open: bool,
    pub query: SearchQuery,
    pub replacement: String,
    /// Restrict to this byte range (captured when "in selection" is enabled).
    pub scope: Option<Range<usize>>,
    focus_query: bool,
    /// Byte ranges of all matches; valid for `key`.
    pub matches: Vec<Range<usize>>,
    key: Option<(u64, SearchQuery, Option<Range<usize>>)>,
    pub current: Option<usize>,
    pub error: Option<String>,
}

/// What the user asked the bar to do this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindAction {
    /// The query changed: jump to the first match from the cursor.
    Search,
    Next,
    Prev,
    Replace,
    ReplaceAll,
    Close,
}

impl FindBar {
    pub fn open(&mut self, replace: bool, seed: Option<String>) {
        self.open = true;
        self.replace_open = replace;
        self.focus_query = true;
        if let Some(seed) = seed.filter(|s| !s.is_empty()) {
            self.query.text = seed;
        }
    }

    /// Recomputes matches if the text or query changed.
    pub fn refresh(&mut self, revision: u64, text: impl FnOnce() -> String) {
        let key = (revision, self.query.clone(), self.scope.clone());
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.key = Some(key);
        self.error = None;
        self.matches = match Matcher::new(&self.query) {
            None => Vec::new(),
            Some(Err(e)) => {
                self.error = Some(short_regex_error(&e));
                Vec::new()
            }
            Some(Ok(m)) => {
                let text = text();
                m.find_all(&text, self.scope.clone())
            }
        };
        self.current = self.current.filter(|i| *i < self.matches.len());
    }

    /// Picks the match to go to from byte `cursor`.
    pub fn step(&mut self, cursor: usize, forward: bool) -> Option<Range<usize>> {
        let i = next_match(&self.matches, cursor, forward)?;
        self.current = Some(i);
        Some(self.matches[i].clone())
    }

    /// Moves the end of the "in selection" scope by what `replacements`
    /// (inside it) add or take away, so it still covers the selection.
    pub fn follow_replacements(&mut self, replacements: &[(Range<usize>, String)]) {
        if let Some(scope) = &mut self.scope {
            let grown: isize = replacements
                .iter()
                .map(|(r, new)| new.len() as isize - r.len() as isize)
                .sum();
            scope.end = scope.end.saturating_add_signed(grown).max(scope.start);
        }
    }

    /// Replacement texts for the given matches.
    pub fn replacements(&self, text: &str, ranges: &[Range<usize>]) -> Vec<(Range<usize>, String)> {
        let Some(Ok(m)) = Matcher::new(&self.query) else {
            return Vec::new();
        };
        ranges
            .iter()
            .map(|r| (r.clone(), m.replacement(text, r, &self.replacement)))
            .collect()
    }

    pub fn ui(&mut self, ui: &mut Ui, selection: Option<Range<usize>>) -> Option<FindAction> {
        let mut action = None;
        let before = (self.query.clone(), self.scope.clone());
        ui.horizontal(|ui| {
            let toggle = |ui: &mut Ui, on: &mut bool, label: &str, tip: &str| {
                ui.toggle_value(on, RichText::new(label).monospace())
                    .on_hover_text(tip);
            };
            let replace_icon = if self.replace_open { "⏷" } else { "⏵" };
            if ui
                .small_button(replace_icon)
                .on_hover_text("Toggle replace (Ctrl+H)")
                .clicked()
            {
                self.replace_open = !self.replace_open;
            }
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.query.text)
                    .hint_text("Find")
                    .desired_width(220.0)
                    .font(egui::TextStyle::Monospace),
            );
            if std::mem::take(&mut self.focus_query) {
                field.request_focus();
                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), field.id) {
                    let all = egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        egui::text::CCursor::new(self.query.text.chars().count()),
                    );
                    state.cursor.set_char_range(Some(all));
                    state.store(ui.ctx(), field.id);
                }
            }
            if field.lost_focus() {
                let (enter, shift, esc) = ui.input(|i| {
                    (
                        i.key_pressed(Key::Enter),
                        i.modifiers.shift,
                        i.key_pressed(Key::Escape),
                    )
                });
                if esc {
                    action = Some(FindAction::Close);
                } else if enter {
                    action = Some(if shift {
                        FindAction::Prev
                    } else {
                        FindAction::Next
                    });
                    field.request_focus();
                }
            }
            toggle(ui, &mut self.query.case_sensitive, "Aa", "Match case");
            toggle(ui, &mut self.query.whole_word, "W", "Whole word");
            toggle(ui, &mut self.query.regex, ".*", "Regular expression");
            let mut in_sel = self.scope.is_some();
            if ui
                .toggle_value(&mut in_sel, RichText::new("⊏").monospace())
                .on_hover_text("In selection")
                .changed()
            {
                self.scope = if in_sel { selection.clone() } else { None };
            }

            let status = match (&self.error, self.matches.len(), self.current) {
                (Some(e), _, _) => RichText::new(e).color(ui.visuals().error_fg_color),
                (None, 0, _) if !self.query.text.is_empty() => {
                    RichText::new("No results").color(ui.visuals().warn_fg_color)
                }
                (None, 0, _) => RichText::new(""),
                (None, n, Some(i)) => RichText::new(format!("{} of {n}", i + 1)),
                (None, n, None) => RichText::new(format!("{n} found")),
            };
            ui.label(status.small());
            if ui
                .small_button("⏶")
                .on_hover_text("Previous (Shift+F3)")
                .clicked()
            {
                action = Some(FindAction::Prev);
            }
            if ui.small_button("⏷").on_hover_text("Next (F3)").clicked() {
                action = Some(FindAction::Next);
            }
            if ui.small_button("🗙").on_hover_text("Close (Esc)").clicked() {
                action = Some(FindAction::Close);
            }
        });
        if self.replace_open {
            ui.horizontal(|ui| {
                ui.add_space(ui.spacing().interact_size.y + ui.spacing().item_spacing.x);
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.replacement)
                        .hint_text(if self.query.regex {
                            "Replace ($1 for groups)"
                        } else {
                            "Replace"
                        })
                        .desired_width(220.0)
                        .font(egui::TextStyle::Monospace),
                );
                if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    action = Some(FindAction::Replace);
                    field.request_focus();
                }
                if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    action = Some(FindAction::Close);
                }
                if ui.button("Replace").clicked() {
                    action = Some(FindAction::Replace);
                }
                if ui.button("Replace all").clicked() {
                    action = Some(FindAction::ReplaceAll);
                }
            });
        }
        if action.is_none() && (self.query.clone(), self.scope.clone()) != before {
            action = Some(FindAction::Search);
        }
        action
    }
}

fn short_regex_error(e: &regex::Error) -> String {
    // The full message spans several lines with a caret diagram.
    e.to_string()
        .lines()
        .last()
        .unwrap_or("invalid regex")
        .trim()
        .trim_start_matches("error: ")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replacing in a selection moves its end, so it still covers the
    /// selection: after a replace all that took text out, the old end was
    /// inside a character, and the next search panicked (#418).
    #[test]
    fn the_scope_follows_replacements() {
        let mut find = FindBar {
            scope: Some(2..10),
            ..Default::default()
        };
        find.follow_replacements(&[(3..4, String::new()), (6..7, "xyz".into())]);
        assert_eq!(find.scope, Some(2..11));
        find.follow_replacements(&[(2..11, String::new())]);
        assert_eq!(find.scope, Some(2..2));
    }
}
