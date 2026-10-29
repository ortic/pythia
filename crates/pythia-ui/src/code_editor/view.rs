//! Painting and input handling for [`CodeEditor`].

use std::ops::Range;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Color32, CursorIcon, Event, EventFilter, FontId, ImeEvent, Key, Modifiers, Pos2, Rect, Sense,
    Shape, Stroke, Ui, pos2, vec2,
};
use pythia_plsql::TokenKind;

use super::assist::{self, Completion, CompletionEntry, CompletionTrigger};
use super::find::FindAction;
use super::fold::{DisplayMap, Region, RegionKind};
use super::highlight::lex_line;
use super::matching::pair_at;
use super::selection::Selection;
use super::text::{
    col_from_visual, is_word_char, line_end, line_start, line_text, pos_of, visual_col, word_range,
};
use super::{
    CaretShape, CodeEditor, Drag, EditorConfig, EditorOutput, HoverProbe, MarkerKind,
    PendingCompletion,
};
use crate::editor::SyntaxTheme;

/// Editor chrome colors, derived from the egui theme.
struct Palette {
    text: Color32,
    bg: Color32,
    gutter_bg: Color32,
    gutter_line: Color32,
    line_number: Color32,
    line_number_current: Color32,
    current_line: Color32,
    selection: Color32,
    selection_inactive: Color32,
    find_match: Color32,
    find_current: Color32,
    pair: Color32,
    caret: Color32,
    whitespace: Color32,
    error: Color32,
    warning: Color32,
    breakpoint: Color32,
    bookmark: Color32,
    execution: Color32,
    execution_line: Color32,
    link: Color32,
    fold_pill: Color32,
    snippet: Color32,
}

impl Palette {
    fn new(v: &egui::Visuals) -> Self {
        let dark = v.dark_mode;
        let selection = v.selection.bg_fill;
        Self {
            text: v.text_color(),
            bg: v.extreme_bg_color,
            gutter_bg: v.extreme_bg_color,
            gutter_line: v.widgets.noninteractive.bg_stroke.color,
            line_number: v.weak_text_color().gamma_multiply(0.8),
            line_number_current: v.text_color(),
            current_line: if dark {
                Color32::from_white_alpha(9)
            } else {
                Color32::from_black_alpha(9)
            },
            selection,
            selection_inactive: selection.gamma_multiply(0.45),
            find_match: Color32::from_rgba_unmultiplied(255, 170, 0, if dark { 50 } else { 70 }),
            find_current: Color32::from_rgba_unmultiplied(
                255,
                150,
                0,
                if dark { 130 } else { 150 },
            ),
            pair: if dark {
                Color32::from_white_alpha(30)
            } else {
                Color32::from_black_alpha(25)
            },
            caret: if dark {
                Color32::from_rgb(0xae, 0xaf, 0xad)
            } else {
                Color32::from_rgb(0x10, 0x10, 0x10)
            },
            whitespace: v.weak_text_color().gamma_multiply(0.5),
            error: v.error_fg_color,
            warning: v.warn_fg_color,
            breakpoint: Color32::from_rgb(0xe5, 0x39, 0x35),
            bookmark: Color32::from_rgb(0x42, 0x85, 0xf4),
            execution: Color32::from_rgb(0xf5, 0xc5, 0x18),
            execution_line: Color32::from_rgba_unmultiplied(
                245,
                197,
                24,
                if dark { 38 } else { 60 },
            ),
            link: v.hyperlink_color,
            fold_pill: if dark {
                Color32::from_white_alpha(20)
            } else {
                Color32::from_black_alpha(16)
            },
            snippet: if dark {
                Color32::from_rgb(0x5c, 0xa8, 0xe8)
            } else {
                Color32::from_rgb(0x1f, 0x6f, 0xc4)
            },
        }
    }
}

/// Geometry of the current frame. All rects in screen coordinates.
pub(super) struct Layout {
    /// Top-left of the (virtual) content.
    origin: Pos2,
    /// Visible part of the content, relative to `origin`.
    viewport: Rect,
    row_h: f32,
    /// From the top of a row to the top of its text: half the line spacing.
    text_dy: f32,
    char_w: f32,
    gutter_w: f32,
    marker_w: f32,
    /// Width of the fold chevron column at the right edge of the gutter.
    fold_w: f32,
    /// Lines hidden by folds.
    map: DisplayMap,
}

impl Layout {
    fn text_x(&self) -> f32 {
        self.origin.x + self.gutter_w
    }

    /// Top of the row showing `line` (a hidden line maps to its fold's row).
    fn row_y(&self, line: usize) -> f32 {
        self.origin.y + self.map.line_to_row(line) as f32 * self.row_h
    }

    /// Top of the text of the row showing `line`.
    fn text_y(&self, line: usize) -> f32 {
        self.row_y(line) + self.text_dy
    }

    /// Display row under a screen y (unclamped; may be past the last row).
    fn row_at(&self, y: f32) -> usize {
        ((y - self.origin.y) / self.row_h).floor().max(0.0) as usize
    }

    /// Visible line under a screen y, clamped to the text.
    fn line_at(&self, y: f32) -> usize {
        self.map
            .row_to_line(self.row_at(y).min(self.map.rows().saturating_sub(1)))
    }

    fn fold_column(&self) -> Range<f32> {
        let right = self.gutter_left() + self.gutter_w;
        right - self.fold_w..right
    }

    fn gutter_left(&self) -> f32 {
        self.origin.x + self.viewport.min.x
    }

    fn visible_rect(&self) -> Rect {
        self.viewport.translate(self.origin.to_vec2())
    }

    /// Document lines spanned by the visible rows (some may be hidden).
    fn visible_lines(&self) -> Range<usize> {
        let rows = self.map.rows();
        let first = (self.viewport.min.y / self.row_h).floor().max(0.0) as usize;
        let last = ((self.viewport.max.y / self.row_h).ceil() as usize + 1).min(rows);
        if first >= last {
            return 0..0;
        }
        self.map.row_to_line(first)..self.map.row_to_line(last - 1) + 1
    }

    /// Whether a line is in view and not folded away.
    fn shows(&self, lines: &Range<usize>, line: usize) -> bool {
        lines.contains(&line) && !self.map.is_hidden(line)
    }
}

const BLINK_PERIOD: f64 = 1.1;
/// Gap between the caret line and the completion list, and between the list
/// and its details pane.
const POPUP_GAP: f32 = 2.0;
/// Width of the completion list; longer labels and details are cut with "…".
const POPUP_MIN_WIDTH: f32 = 200.0;
const POPUP_MAX_WIDTH: f32 = 560.0;
/// Space for the kind glyph before a completion label, between label and
/// detail, and after the detail.
const ROW_LEAD: f32 = 26.0;
const ROW_GAP: f32 = 16.0;
const ROW_TRAIL: f32 = 6.0;
/// Widest the details pane next to the completion list gets.
const DETAILS_MAX_WIDTH: f32 = 380.0;

impl CodeEditor {
    pub fn ui(&mut self, ui: &mut Ui, config: &EditorConfig) -> EditorOutput {
        self.state.tab_width = config.tab_width.max(1);
        let id = self.id_salt.unwrap_or_else(|| ui.id().with("code-editor"));
        let revision_before = self.state.revision;
        let font = FontId::monospace(config.font_size);
        let (text_h, char_w) = ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, 'M')));
        let spacing = config.line_spacing.clamp(0.0, text_h);
        let row_h = text_h + spacing;
        let text_dy = (spacing / 2.0).round();
        let theme = SyntaxTheme::for_visuals(ui.visuals());
        let pal = Palette::new(ui.visuals());
        let time = ui.input(|i| i.time);
        self.hover_delay = config.hover_delay_ms as f64 / 1000.0;

        let mut find_action = None;
        if self.find.open {
            egui::Frame::new()
                .fill(ui.visuals().panel_fill)
                .inner_margin(egui::Margin::symmetric(6, 3))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    find_action = self.find.ui(ui, self.selection());
                });
        }

        let n_lines = self.state.rope.len_lines();
        let digits = n_lines.to_string().len().max(3);
        let marker_w = (row_h * 1.1).round();
        let fold_w = (row_h * 0.85).round();
        let gutter_w = if config.show_line_numbers {
            marker_w + (digits as f32 + 0.75) * char_w + fold_w
        } else {
            marker_w + fold_w
        };
        let content_w = gutter_w + (self.max_line_chars() as f32 + 4.0) * char_w;
        let map = self.display_map();
        let content_h = (map.rows() as f32 + 3.0) * row_h;

        let mut gutter_clicked = None;
        let scroll = egui::ScrollArea::both()
            .id_salt(id.with("scroll"))
            .auto_shrink(false)
            .show_viewport(ui, |ui, viewport| {
                let size = vec2(
                    content_w.max(viewport.width()),
                    content_h.max(viewport.height()),
                );
                let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
                let response = ui.interact(rect, id, Sense::click_and_drag());
                // egui takes focus from the editor when a click lands outside
                // it (on release), before the completion list further down
                // sees the click; without focus the list would close first.
                // The list belongs to the editor, so the editor keeps focus.
                if self.completion.is_some()
                    && ui.memory(|m| m.had_focus_last_frame(id))
                    && ui.input(|i| i.pointer.any_click())
                    && ui
                        .input(|i| i.pointer.interact_pos())
                        .and_then(|pos| ui.ctx().layer_id_at(pos))
                        == Some(completion_layer(ui))
                {
                    response.request_focus();
                }
                let mut layout = Layout {
                    origin: rect.min,
                    viewport,
                    row_h,
                    text_dy,
                    char_w,
                    gutter_w,
                    marker_w,
                    fold_w,
                    map,
                };
                self.page_rows = ((viewport.height() / row_h) as usize)
                    .saturating_sub(1)
                    .max(1);

                if std::mem::take(&mut self.focus_requested) {
                    response.request_focus();
                }
                self.handle_pointer(ui, &response, &layout, &mut gutter_clicked, time);
                let focused = response.has_focus();
                if focused {
                    ui.memory_mut(|m| {
                        m.set_focus_lock_filter(
                            id,
                            EventFilter {
                                tab: true,
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                escape: true,
                            },
                        );
                    });
                    if let Some(action) = self.handle_keys(ui, config, time) {
                        find_action = Some(action);
                    }
                }
                if let Some(action) = find_action.take() {
                    self.run_find(action, time, &response);
                }
                if self.find.open {
                    let state = &self.state;
                    self.find.refresh(state.revision, || state.text());
                }
                // Carets never rest in hidden lines: whatever put one there
                // (find, go to line, horizontal moves, edits) opens the fold.
                self.state.check_snippet();
                self.reveal_carets();
                layout.map = self.display_map();
                self.update_assist(ui, focused, time);

                self.paint(ui, &layout, config, &pal, &theme, &font, focused, time);
                self.marker_tooltip(ui, &response, &layout);
                self.fold_tooltip(ui, &response, &font);

                let caret_rect = self.caret_rect(&layout);
                if layout.visible_rect().contains(caret_rect.center()) {
                    let rows = config.completion_rows;
                    self.completion_popup(ui, caret_rect, char_w, &font, focused, rows);
                    self.signature_popup(ui, caret_rect, &font);
                }
                self.hover_popup(ui, &layout, &font);
                if std::mem::take(&mut self.scroll_to_cursor) {
                    let margin = vec2(2.0 * char_w, row_h);
                    let target = Rect::from_min_max(
                        pos2(
                            caret_rect.min.x - gutter_w - margin.x,
                            caret_rect.min.y - margin.y,
                        ),
                        caret_rect.max + margin,
                    );
                    ui.scroll_to_rect(target, None);
                }
                if focused {
                    let visible = layout.visible_rect();
                    ui.ctx().output_mut(|o| {
                        o.ime = Some(egui::output::IMEOutput {
                            purpose: Default::default(),
                            rect: visible,
                            cursor_rect: caret_rect,
                            should_interrupt_composition: false,
                        });
                    });
                }
                self.last_layout = Some(layout);
                response
            });

        self.goto_line_popup(ui, scroll.inner_rect, &scroll.inner);

        let (line, col) = self.line_col();
        let requests = std::mem::take(&mut self.requests);
        EditorOutput {
            response: scroll.inner,
            cursor: self.cursor(),
            selection: self.selection(),
            line_col: (line, col),
            changed: self.state.revision != revision_before,
            gutter_clicked,
            completion_request: requests.completion,
            signature_request: requests.signature,
            hover_request: requests.hover,
            goto_request: requests.goto,
            references_request: requests.references,
            context_request: requests.context,
            pasted: requests.pasted,
            semicolon_typed: requests.semicolon,
        }
    }

    // ----- input --------------------------------------------------------

    /// Char position under a screen point: (line, visual column, char index).
    fn hit(&self, layout: &Layout, pos: Pos2) -> (usize, usize, usize) {
        let line = layout.line_at(pos.y);
        let vcol = ((pos.x - layout.text_x()) / layout.char_w).round().max(0.0) as usize;
        let text = line_text(&self.state.rope, line);
        let char_pos = pos_of(
            &self.state.rope,
            line,
            col_from_visual(&text, vcol, self.state.tab_width),
        );
        (line, vcol, char_pos)
    }

    /// Char index and word range of the identifier under a screen point.
    fn word_under(&self, layout: &Layout, pos: Pos2) -> Option<(usize, Range<usize>)> {
        let rope = &self.state.rope;
        if pos.x < layout.gutter_left() + layout.gutter_w || pos.y < layout.origin.y {
            return None;
        }
        if layout.row_at(pos.y) >= layout.map.rows() {
            return None;
        }
        let line = layout.line_at(pos.y);
        let vcol = ((pos.x - layout.text_x()) / layout.char_w).floor().max(0.0) as usize;
        let text = line_text(rope, line);
        let tw = self.state.tab_width;
        // Column of the char whose cell contains `vcol`.
        let mut v = 0;
        let col = text.chars().position(|c| {
            let next = if c == '\t' { (v / tw + 1) * tw } else { v + 1 };
            let hit = vcol < next;
            v = next;
            hit
        })?;
        let char_pos = line_start(rope, line) + col;
        if !is_word_char(rope.char(char_pos)) {
            return None;
        }
        let (a, b) = word_range(rope, char_pos);
        Some((char_pos, a..b))
    }

    fn handle_pointer(
        &mut self,
        ui: &Ui,
        response: &egui::Response,
        layout: &Layout,
        gutter_clicked: &mut Option<usize>,
        time: f64,
    ) {
        let (pressed, down, pos, mods) = ui.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.interact_pos(),
                i.modifiers,
            )
        });
        let Some(pos) = pos else {
            return;
        };
        let in_gutter = pos.x < layout.gutter_left() + layout.gutter_w;
        if response.secondary_clicked() && !in_gutter {
            // The caret moves to the click unless it's in the selection,
            // which the menu's actions then apply to.
            let (_, _, char_pos) = self.hit(layout, pos);
            let at = self.state.rope.char_to_byte(char_pos);
            if !self.selection().is_some_and(|s| s.contains(&at)) {
                self.state.set_caret(char_pos);
            }
            self.requests.context = Some(at);
            return;
        }
        let pill = response
            .hovered()
            .then(|| {
                self.fold_pills
                    .iter()
                    .find(|(r, _)| r.contains(pos))
                    .map(|(_, l)| *l)
            })
            .flatten();
        let word = if response.hovered() && pill.is_none() {
            self.word_under(layout, pos)
        } else {
            None
        };
        self.link = word
            .as_ref()
            .filter(|_| mods.command && !down)
            .map(|(_, r)| r.clone());
        if response.hovered() && !in_gutter {
            let icon = if self.link.is_some() || pill.is_some() {
                CursorIcon::PointingHand
            } else {
                CursorIcon::Text
            };
            ui.ctx().set_cursor_icon(icon);
        }
        self.probe_hover(ui, word.as_ref(), down, mods, time);

        if pressed && response.hovered() {
            response.request_focus();
            self.blink_epoch = time;
            if let Some(line) = pill {
                self.state.tracked.folds.retain(|(s, _)| *s != line);
                self.drag = None;
                return;
            }
            let (line, vcol, char_pos) = self.hit(layout, pos);
            if in_gutter {
                if layout.fold_column().contains(&pos.x) {
                    if layout.row_at(pos.y) < layout.map.rows() {
                        self.toggle_fold_at(line + 1);
                    }
                    self.drag = None;
                } else if pos.x < layout.gutter_left() + layout.marker_w {
                    *gutter_clicked = Some(line + 1);
                } else {
                    self.state.select_line_at(char_pos);
                    self.drag = None;
                }
                return;
            }
            if mods.alt {
                // Alt+drag selects a block; Alt+click adds a cursor on release.
                self.drag = Some(Drag::Block {
                    from: (line, vcol),
                    at: char_pos,
                    moved: false,
                });
            } else if mods.command {
                self.state.set_caret(char_pos);
                self.drag = None;
                if let Some((at, _)) = &word {
                    self.requests.goto = Some(self.state.rope.char_to_byte(*at));
                }
            } else if mods.shift {
                let primary = self.state.primary().moved_to(char_pos, true);
                self.state.set_selections(vec![primary], 0);
                self.drag = Some(Drag::Chars {
                    anchor: primary.anchor,
                });
            } else {
                self.state.set_caret(char_pos);
                self.drag = Some(Drag::Chars { anchor: char_pos });
            }
        } else if down && let Some(drag) = self.drag {
            let (line, vcol, char_pos) = self.hit(layout, pos);
            match drag {
                Drag::Chars { anchor } => {
                    if self.state.primary() != Selection::new(anchor, char_pos) {
                        self.state
                            .set_selections(vec![Selection::new(anchor, char_pos)], 0);
                        self.scroll_to_cursor = true;
                    }
                }
                Drag::Block { from, at, moved } => {
                    if moved || (line, vcol) != from {
                        self.drag = Some(Drag::Block {
                            from,
                            at,
                            moved: true,
                        });
                        self.state.select_block(from, (line, vcol));
                    }
                }
            }
        }
        if !down {
            if let Some(Drag::Block {
                at, moved: false, ..
            }) = self.drag
            {
                self.state.add_cursor(at);
            }
            self.drag = None;
        }

        if !in_gutter && pill.is_none() {
            let (_, _, char_pos) = self.hit(layout, pos);
            if response.triple_clicked() {
                self.state.select_line_at(char_pos);
            } else if response.double_clicked() {
                self.state.select_word_at(char_pos);
            }
        }
    }

    fn handle_keys(&mut self, ui: &Ui, config: &EditorConfig, time: f64) -> Option<FindAction> {
        let events = ui.input(|i| i.events.clone());
        let ro = config.read_only;
        let mut find = None;
        for event in events {
            if let Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = &event
                && self.completion_key(*key, *modifiers, time)
            {
                self.blink_epoch = time;
                continue;
            }
            match event {
                Event::Text(text) if !ro => {
                    let text: String = text.chars().filter(|c| !c.is_control()).collect();
                    if !text.is_empty() {
                        self.state.insert(&text, time);
                        self.after_typing(&text, config, time);
                        if text == ";" {
                            self.requests.semicolon = Some(self.cursor());
                        }
                    }
                }
                Event::Ime(ImeEvent::Commit(text)) if !ro && !text.is_empty() => {
                    self.state.insert(&text, time)
                }
                Event::Copy => ui.ctx().copy_text(self.state.copy_text()),
                Event::Cut if ro => ui.ctx().copy_text(self.state.copy_text()),
                Event::Cut => {
                    let text = self.state.cut(time);
                    ui.ctx().copy_text(text);
                }
                Event::Paste(text) if !ro => {
                    let single = self.state.sels.len() == 1;
                    let from = self.state.primary().start();
                    self.state.paste(&text, time);
                    if single {
                        let rope = &self.state.rope;
                        let to = self.state.primary().head;
                        self.requests.pasted =
                            Some(rope.char_to_byte(from)..rope.char_to_byte(to.max(from)));
                    }
                }
                Event::Key {
                    key,
                    physical_key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    // Digits and brackets by position, so Ctrl+Shift+1 or
                    // Ctrl+Shift+[ work whatever the layout shifts them to.
                    let physical = physical_key.unwrap_or(key);
                    if self.editor_key(key, physical, modifiers, ro, time) {
                    } else if let Some(action) = self.handle_key(key, modifiers, ro, time) {
                        find = Some(action);
                    }
                }
                _ => continue,
            }
            self.blink_epoch = time;
            self.scroll_to_cursor = true;
        }
        find
    }

    /// Folding, bookmark and snippet keys. Returns true if consumed.
    fn editor_key(&mut self, key: Key, physical: Key, m: Modifiers, ro: bool, time: f64) -> bool {
        let (ctrl, shift) = (m.command, m.shift);
        // Second key of a Ctrl+K chord.
        if self.chord.take().is_some_and(|t| time - t < 2.0) {
            match physical {
                Key::Num0 => self.fold_all(),
                Key::J => self.unfold_all(),
                _ => return false,
            }
            return true;
        }
        let caret_line = self.state.rope.char_to_line(self.state.primary().head);
        match key {
            Key::K if ctrl && !shift && !m.alt => self.chord = Some(time),
            _ if ctrl && shift && matches!(physical, Key::OpenBracket | Key::OpenCurlyBracket) => {
                self.fold_at_cursor()
            }
            _ if ctrl
                && shift
                && matches!(physical, Key::CloseBracket | Key::CloseCurlyBracket) =>
            {
                self.unfold_at_cursor()
            }
            _ if ctrl && !m.alt && digit(physical).is_some() => {
                let n = digit(physical).expect("checked");
                if shift {
                    self.state.tracked.toggle_bookmark(caret_line, Some(n));
                } else if let Some(b) = self
                    .state
                    .tracked
                    .bookmarks
                    .iter()
                    .find(|b| b.number == Some(n))
                {
                    self.jump_to_line(b.line);
                }
            }
            Key::F2 if ctrl => self.state.tracked.toggle_bookmark(caret_line, None),
            Key::F2 if !m.alt => {
                if let Some(line) = self.state.tracked.next_bookmark(caret_line, !shift) {
                    self.jump_to_line(line);
                }
            }
            Key::Tab if !ctrl && self.state.snippet.is_some() => {
                self.state.snippet_step(!shift);
            }
            Key::Tab if !ro && !ctrl && !shift && self.expand_trigger(time) => {}
            Key::Escape if self.state.snippet.is_some() => self.state.snippet = None,
            _ => return false,
        }
        true
    }

    fn handle_key(&mut self, key: Key, m: Modifiers, ro: bool, time: f64) -> Option<FindAction> {
        let (ctrl, shift, alt) = (m.command, m.shift, m.alt);
        // Vertical moves step over folded lines.
        let map = self.display_map();
        let rows = |line: usize, delta: isize| map.offset_line(line, delta);
        let page = self.page_rows as isize;
        let st = &mut self.state;
        match key {
            Key::ArrowLeft => st.move_left(shift, ctrl),
            Key::ArrowRight => st.move_right(shift, ctrl),
            Key::ArrowUp if alt && !ro => st.move_lines(true, time),
            Key::ArrowDown if alt && !ro => st.move_lines(false, time),
            Key::ArrowUp => st.move_vertical_mapped(-1, shift, &rows),
            Key::ArrowDown => st.move_vertical_mapped(1, shift, &rows),
            Key::Home if ctrl => st.move_doc_start(shift),
            Key::Home => st.move_home(shift),
            Key::End if ctrl => st.move_doc_end(shift),
            Key::End => st.move_end(shift),
            Key::PageUp => st.move_vertical_mapped(-page, shift, &rows),
            Key::PageDown => st.move_vertical_mapped(page, shift, &rows),
            Key::Enter if !ro && !ctrl && !alt => st.newline(time),
            Key::Backspace if !ro => st.backspace(ctrl, time),
            Key::Delete if !ro => st.delete(ctrl, time),
            Key::Tab if !ro && !ctrl && shift => st.outdent_lines(time),
            Key::Tab if !ro && !ctrl => st.tab(time),
            Key::Escape => {
                if self.goto_line.is_some() {
                    self.goto_line = None;
                } else if self.find.open {
                    return Some(FindAction::Close);
                } else {
                    st.single_cursor();
                }
            }
            Key::A if ctrl => st.select_all(),
            Key::Z if ctrl && shift && !ro => {
                st.redo();
            }
            Key::Z if ctrl && !ro => {
                st.undo();
            }
            Key::Y if ctrl && !ro => {
                st.redo();
            }
            Key::D if ctrl && !ro => st.duplicate_lines(time),
            Key::Slash if ctrl && !ro => st.toggle_comment(time),
            Key::U if ctrl && !ro => st.change_case(shift, time),
            Key::F if ctrl => self.open_find(false),
            Key::H if ctrl && !ro => self.open_find(true),
            Key::G if ctrl => self.goto_line = Some(String::new()),
            Key::Space if ctrl => {
                let offset = self.cursor();
                self.requests.completion = Some(CompletionTrigger {
                    offset,
                    explicit: true,
                    trigger_char: None,
                });
                self.pending_completion = None;
            }
            Key::F12 if shift => self.requests.references = Some(self.cursor()),
            Key::F12 => self.requests.goto = Some(self.cursor()),
            Key::F3 if self.find.query.text.is_empty() => self.open_find(false),
            Key::F3 => {
                return Some(if shift {
                    FindAction::Prev
                } else {
                    FindAction::Next
                });
            }
            _ => {}
        }
        None
    }

    fn run_find(&mut self, action: FindAction, time: f64, response: &egui::Response) {
        let state = &self.state;
        self.find.refresh(state.revision, || state.text());
        let sel_start = self.selection().map_or(self.cursor(), |r| r.start);
        let sel_end = self.selection().map_or(self.cursor(), |r| r.end);
        let target = match action {
            FindAction::Search => self.find.step(sel_start, true),
            FindAction::Next => self.find.step(sel_end, true),
            FindAction::Prev => self.find.step(sel_start, false),
            FindAction::Replace => {
                let current = self.selection().filter(|s| self.find.matches.contains(s));
                if let Some(range) = current {
                    let text = self.state.text();
                    let replacements = self.find.replacements(&text, &[range]);
                    let inserted = self.state.replace_byte_ranges(replacements, time);
                    let after = inserted
                        .first()
                        .map_or(sel_end, |r| self.state.rope.char_to_byte(r.end));
                    let state = &self.state;
                    self.find.refresh(state.revision, || state.text());
                    self.find.step(after, true)
                } else {
                    self.find.step(sel_end, true)
                }
            }
            FindAction::ReplaceAll => {
                let text = self.state.text();
                let replacements = self.find.replacements(&text, &self.find.matches.clone());
                if !replacements.is_empty() {
                    self.state.history.seal();
                    self.state.replace_byte_ranges(replacements, time);
                    self.state.history.seal();
                }
                None
            }
            FindAction::Close => {
                self.find.open = false;
                self.find.scope = None;
                response.request_focus();
                None
            }
        };
        if let Some(range) = target {
            self.select(range);
        }
    }

    // ----- language assistance ------------------------------------------

    /// Keys the completion popup handles itself. Returns true if consumed.
    fn completion_key(&mut self, key: Key, m: Modifiers, time: f64) -> bool {
        let Some(completion) = self.completion.as_mut() else {
            return false;
        };
        if m.command || m.alt {
            return false;
        }
        let page = completion.page.max(1) as isize;
        match key {
            Key::ArrowUp => completion.move_selection(-1),
            Key::ArrowDown => completion.move_selection(1),
            Key::PageUp => completion.move_selection(-page),
            Key::PageDown => completion.move_selection(page),
            Key::Enter | Key::Tab => {
                let completion = self.completion.take().expect("checked above");
                completion.accept(&mut self.state, time);
                self.scroll_to_cursor = true;
                self.pending_completion = None;
            }
            Key::Escape => self.completion = None,
            _ => return false,
        }
        true
    }

    /// Decides which requests typing `text` triggers.
    fn after_typing(&mut self, text: &str, config: &EditorConfig, time: f64) {
        let Some(last) = text.chars().last() else {
            return;
        };
        if self.state.sels.len() != 1 {
            self.pending_completion = None;
            return;
        }
        let caret = self.state.primary().head;
        match last {
            '.' => {
                self.completion = None;
                self.pending_completion = None;
                self.requests.completion = Some(CompletionTrigger {
                    offset: self.state.rope.char_to_byte(caret),
                    explicit: false,
                    trigger_char: Some('.'),
                });
            }
            '(' | ',' => {
                self.pending_completion = None;
                self.requests.signature = Some(self.state.rope.char_to_byte(caret));
                self.signature_caret = Some(caret);
            }
            c if is_word_char(c) => {
                let typed = assist::word_prefix_len(&self.state.rope, caret);
                let enough = typed >= config.auto_complete_min_chars.max(1);
                // With the popup open, typing just filters it, unless its
                // list was cut short: then it asks again (#160).
                let ask = self.completion.as_ref().is_none_or(|c| c.incomplete);
                self.pending_completion = (enough && ask).then(|| PendingCompletion {
                    deadline: time + config.auto_complete_delay_ms as f64 / 1000.0,
                    caret,
                    revision: self.state.revision,
                });
            }
            _ => self.pending_completion = None,
        }
    }

    /// Per-frame bookkeeping: popup validity, debounce, signature tracking.
    fn update_assist(&mut self, ui: &Ui, focused: bool, time: f64) {
        if let Some(c) = self.completion.as_mut()
            && (!focused || !c.update(&self.state))
        {
            self.completion = None;
        }

        if let Some(p) = self.pending_completion {
            let caret = self.state.primary().head;
            if caret != p.caret || self.state.revision != p.revision || !focused {
                self.pending_completion = None;
            } else if time >= p.deadline {
                self.pending_completion = None;
                self.requests.completion = Some(CompletionTrigger {
                    offset: self.state.rope.char_to_byte(caret),
                    explicit: false,
                    trigger_char: None,
                });
            } else {
                let wait = std::time::Duration::from_secs_f64(p.deadline - time);
                ui.ctx().request_repaint_after(wait);
            }
        }

        if let Some(sig) = &self.signature {
            let caret = self.state.primary().head;
            let paren = assist::enclosing_paren(&self.state.rope, caret);
            if paren.is_none() || paren != sig.open_paren || !focused {
                self.hide_signature();
            } else if self.signature_caret != Some(caret) {
                self.signature_caret = Some(caret);
                self.requests.signature = Some(self.state.rope.char_to_byte(caret));
            }
        }

        if self
            .references
            .as_ref()
            .is_some_and(|(rev, _)| *rev != self.state.revision)
        {
            self.references = None;
        }
    }

    /// Tracks the word under the mouse and requests hover info once it rests.
    fn probe_hover(
        &mut self,
        ui: &Ui,
        word: Option<&(usize, Range<usize>)>,
        down: bool,
        mods: Modifiers,
        time: f64,
    ) {
        let Some((at, range)) = word.filter(|_| !down && !mods.command) else {
            self.hover_probe = None;
            self.hover = None;
            return;
        };
        if self.hover.as_ref().is_some_and(|h| h.range != *range) {
            self.hover = None;
        }
        let delay = self.hover_delay;
        match &mut self.hover_probe {
            Some(p) if p.word_start == range.start => {
                if !p.requested && time - p.since >= delay {
                    p.requested = true;
                    self.requests.hover = Some(self.state.rope.char_to_byte(*at));
                } else if !p.requested {
                    let wait = std::time::Duration::from_secs_f64(delay - (time - p.since));
                    ui.ctx().request_repaint_after(wait);
                }
            }
            _ => {
                self.hover_probe = Some(HoverProbe {
                    word_start: range.start,
                    since: time,
                    requested: false,
                });
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs_f64(delay));
            }
        }
    }

    fn completion_popup(
        &mut self,
        ui: &Ui,
        caret: Rect,
        char_w: f32,
        font: &FontId,
        focused: bool,
        max_rows: usize,
    ) {
        if !focused {
            return;
        }
        let Some(completion) = self.completion.as_mut() else {
            return;
        };
        let row_h = (caret.height() + 4.0).max(18.0);
        let frame = egui::Frame::popup(ui.style()).inner_margin(egui::Margin::same(3));
        let chrome = frame.total_margin().sum();
        let screen = ui.ctx().content_rect();
        // The side follows all items, not the filtered ones, so the list
        // doesn't jump across the line while typing.
        let wanted = completion.items().len().min(max_rows.max(1));
        let (above, fit) = popup_rows(caret, screen, row_h, chrome.y, wanted);
        let list_h = completion.filtered.len().min(fit) as f32 * row_h;
        let measured = match completion.width {
            Some(w) => w,
            None => *completion.width.insert(list_width(ui, completion, font)),
        };
        let width = measured
            .clamp(POPUP_MIN_WIDTH, POPUP_MAX_WIDTH)
            .min((screen.width() - chrome.x).max(POPUP_MIN_WIDTH));
        // Align the labels with the start of the word being completed.
        let typed = self.state.primary().head.saturating_sub(completion.start());
        let word_x = caret.min.x - typed as f32 * char_w;
        let x = (word_x - ROW_LEAD - chrome.x / 2.0)
            .min(screen.max.x - width - chrome.x)
            .max(screen.min.x);
        let y = if above {
            caret.min.y - POPUP_GAP - list_h - chrome.y
        } else {
            caret.max.y + POPUP_GAP
        };
        let mut accept = None;
        let layer = completion_layer(ui);
        let list = egui::Area::new(layer.id)
            .order(layer.order)
            .fixed_pos(pos2(x, y))
            .show(ui.ctx(), |ui| {
                frame.show(ui, |ui| {
                    ui.set_width(width);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let mut scroll = egui::ScrollArea::vertical()
                        .max_height(list_h)
                        // The area's last size limits its content: without
                        // this, a short list would cap all later ones.
                        .min_scrolled_height(list_h)
                        .auto_shrink([false, true]);
                    if completion.scroll_to_selected {
                        let top = completion.selected as f32 * row_h;
                        let offset = completion.scroll.min(top).max(top + row_h - list_h);
                        scroll = scroll.vertical_scroll_offset(offset);
                    }
                    let n = completion.filtered.len();
                    let out = scroll.show_rows(ui, row_h, n, |ui, range| {
                        for i in range {
                            let item = &completion.items()[completion.filtered[i]];
                            let size = vec2(ui.available_width(), row_h);
                            let (rect, response) = ui.allocate_exact_size(size, Sense::click());
                            let selected = i == completion.selected;
                            paint_completion_row(
                                ui,
                                rect,
                                item,
                                selected,
                                response.hovered(),
                                font,
                            );
                            if response.clicked() {
                                accept = Some(i);
                            }
                        }
                    });
                    completion.page = ((out.inner_rect.height() / row_h).round() as usize).max(1);
                    completion.scroll = out.state.offset.y;
                    completion.scroll_to_selected = false;
                });
            })
            .response
            .rect;
        if let Some((title, text)) = completion
            .selected_entry()
            .and_then(|e| e.documentation.as_ref())
        {
            completion_details(ui, list, above, font, title, text);
        }
        if let Some(i) = accept {
            let mut completion = self.completion.take().expect("open");
            completion.selected = i;
            completion.accept(&mut self.state, ui.input(|i| i.time));
            self.scroll_to_cursor = true;
            self.focus_requested = true;
            self.pending_completion = None;
        }
    }

    fn signature_popup(&self, ui: &Ui, caret: Rect, font: &FontId) {
        let Some(sig) = &self.signature else {
            return;
        };
        if self.completion.is_some() {
            return;
        }
        let visuals = ui.visuals();
        let normal = TextFormat {
            font_id: font.clone(),
            color: visuals.text_color(),
            ..Default::default()
        };
        let active = TextFormat {
            font_id: font.clone(),
            color: visuals.strong_text_color(),
            underline: Stroke::new(1.0, visuals.strong_text_color()),
            ..Default::default()
        };
        let label = &sig.label;
        let valid = |r: &Range<usize>| {
            r.start <= r.end
                && r.end <= label.len()
                && label.is_char_boundary(r.start)
                && label.is_char_boundary(r.end)
        };
        let mut job = LayoutJob::default();
        match sig.active.clone().filter(valid) {
            Some(r) => {
                job.append(&label[..r.start], 0.0, normal.clone());
                job.append(&label[r.clone()], 0.0, active);
                job.append(&label[r.end..], 0.0, normal);
            }
            None => job.append(label, 0.0, normal),
        }
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        let height = galley.size().y + 10.0;
        egui::Area::new(ui.id().with("signature-popup"))
            .order(egui::Order::Foreground)
            .interactable(false)
            .fixed_pos(pos2(caret.min.x - 8.0, caret.min.y - height - 4.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::symmetric(6, 4))
                    .show(ui, |ui| {
                        ui.label(galley);
                    });
            });
    }

    fn hover_popup(&self, ui: &Ui, layout: &Layout, font: &FontId) {
        let Some(hover) = &self.hover else {
            return;
        };
        let rope = &self.state.rope;
        let (line, col) = super::text::line_col(rope, hover.range.start.min(rope.len_chars()));
        if layout.map.is_hidden(line) {
            return;
        }
        let vcol = visual_col(&line_text(rope, line), col, self.state.tab_width);
        let pos = pos2(
            layout.text_x() + vcol as f32 * layout.char_w,
            layout.row_y(line) + layout.row_h + 2.0,
        );
        if !layout.visible_rect().contains(pos) {
            return;
        }
        egui::Area::new(ui.id().with("hover-popup"))
            .order(egui::Order::Tooltip)
            .interactable(false)
            .fixed_pos(pos)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.set_max_width(520.0);
                        if !hover.title.is_empty() {
                            ui.label(
                                egui::RichText::new(&hover.title)
                                    .font(font.clone())
                                    .strong(),
                            );
                        }
                        if !hover.body.is_empty() {
                            if !hover.title.is_empty() {
                                ui.separator();
                            }
                            ui.label(&hover.body);
                        }
                    });
            });
    }

    // ----- painting -----------------------------------------------------

    /// Where the character at byte `offset` was drawn in the last frame
    /// (its center), e.g. to point at a word.
    pub fn offset_pos(&self, offset: usize) -> Option<Pos2> {
        let layout = self.last_layout.as_ref()?;
        let rope = &self.state.rope;
        let at = rope.byte_to_char(offset.min(rope.len_bytes()));
        let (line, col) = super::text::line_col(rope, at);
        let vcol = visual_col(&line_text(rope, line), col, self.state.tab_width);
        Some(pos2(
            layout.text_x() + (vcol as f32 + 0.5) * layout.char_w,
            layout.row_y(line) + layout.row_h / 2.0,
        ))
    }

    fn caret_rect(&self, layout: &Layout) -> Rect {
        let head = self.state.primary().head;
        let (line, col) = super::text::line_col(&self.state.rope, head);
        let vcol = visual_col(
            &line_text(&self.state.rope, line),
            col,
            self.state.tab_width,
        );
        let x = layout.text_x() + vcol as f32 * layout.char_w;
        let y = layout.row_y(line);
        Rect::from_min_size(pos2(x, y), vec2(2.0, layout.row_h))
    }

    #[allow(clippy::too_many_arguments)]
    fn paint(
        &mut self,
        ui: &Ui,
        layout: &Layout,
        config: &EditorConfig,
        pal: &Palette,
        theme: &SyntaxTheme,
        font: &FontId,
        focused: bool,
        time: f64,
    ) {
        // Bracket / keyword pair under the caret (needs `&mut self` for the cache).
        let lines = layout.visible_lines();
        // Foldable regions starting in view (for chevrons and pill previews).
        let regions: Vec<Region> = if self.state.rope.len_lines() > 1 {
            self.regions()
                .iter()
                .filter(|r| lines.contains(&r.start))
                .copied()
                .collect()
        } else {
            Vec::new()
        };
        let pair = {
            let primary = self.state.primary();
            if primary.is_empty() && caret_on_pairable(&self.state.rope, primary.head) {
                let caret_byte = self.state.rope.char_to_byte(primary.head);
                pair_at(self.pairs(), caret_byte).cloned()
            } else {
                None
            }
        };

        let painter = ui.painter();
        let visible = layout.visible_rect();
        painter.rect_filled(visible, 0.0, pal.bg);

        let rope = &self.state.rope;
        let tw = self.state.tab_width;
        let shown = |l: usize| layout.shows(&lines, l);
        let texts: Vec<String> = lines
            .clone()
            .map(|l| {
                if shown(l) {
                    line_text(rope, l)
                } else {
                    String::new()
                }
            })
            .collect();
        let text_of = |l: usize| texts[l - lines.start].as_str();
        let full_row = |l: usize| {
            Rect::from_min_size(
                pos2(visible.min.x, layout.row_y(l)),
                vec2(visible.width(), layout.row_h),
            )
        };

        // Line backgrounds: debugger position, current line.
        for m in &self.markers {
            let l = m.line.wrapping_sub(1);
            if m.kind == MarkerKind::ExecutionPoint && shown(l) {
                painter.rect_filled(full_row(l), 0.0, pal.execution_line);
            }
        }
        let primary = self.state.primary();
        let caret_line = rope.char_to_line(primary.head);
        if config.highlight_current_line && shown(caret_line) {
            painter.rect_filled(full_row(caret_line), 0.0, pal.current_line);
        }

        // Range highlights in char coordinates.
        let range_rects = |range: Range<usize>| -> Vec<Rect> {
            let mut out = Vec::new();
            if range.is_empty() {
                return out;
            }
            let (l0, l1) = (rope.char_to_line(range.start), rope.char_to_line(range.end));
            for l in l0.max(lines.start)..=l1.min(lines.end.saturating_sub(1)) {
                if !shown(l) {
                    continue;
                }
                let (ls, le) = (line_start(rope, l), line_end(rope, l));
                let s = range.start.max(ls);
                let e = range.end.min(le);
                let text = text_of(l);
                let vs = visual_col(text, s.saturating_sub(ls), tw);
                let ve = visual_col(text, e.saturating_sub(ls), tw);
                let newline = if range.end > le {
                    layout.char_w * 0.6
                } else {
                    0.0
                };
                let x0 = layout.text_x() + vs as f32 * layout.char_w;
                let x1 = layout.text_x() + ve as f32 * layout.char_w + newline;
                if x1 > x0 {
                    out.push(Rect::from_min_max(
                        pos2(x0, layout.row_y(l)),
                        pos2(x1, layout.row_y(l) + layout.row_h),
                    ));
                }
            }
            out
        };

        // References of the symbol under the caret.
        if let Some((rev, refs)) = &self.references
            && *rev == self.state.revision
        {
            let len = rope.len_bytes();
            for r in refs {
                let chars = rope.byte_to_char(r.start.min(len))..rope.byte_to_char(r.end.min(len));
                for rect in range_rects(chars) {
                    painter.rect_filled(rect, 2.0, pal.pair);
                }
            }
        }

        // Search matches.
        if self.find.open && !self.find.matches.is_empty() && !lines.is_empty() {
            let vis_start = rope.char_to_byte(line_start(rope, lines.start));
            let vis_end = rope.char_to_byte(line_end(rope, lines.end - 1));
            let first = self.find.matches.partition_point(|m| m.end < vis_start);
            for (i, m) in self.find.matches.iter().enumerate().skip(first) {
                if m.start > vis_end {
                    break;
                }
                let color = if self.find.current == Some(i) {
                    pal.find_current
                } else {
                    pal.find_match
                };
                let chars = rope.byte_to_char(m.start)..rope.byte_to_char(m.end);
                for r in range_rects(chars) {
                    painter.rect_filled(r, 2.0, color);
                }
            }
        }

        // Selections.
        let sel_color = if focused {
            pal.selection
        } else {
            pal.selection_inactive
        };
        for s in &self.state.sels {
            for r in range_rects(s.range()) {
                painter.rect_filled(r, 2.0, sel_color);
            }
        }

        // Snippet placeholders: the active one framed, the others hinted.
        if let Some(session) = &self.state.snippet {
            for (i, stop) in session.stops.iter().enumerate() {
                let active = i == session.current;
                let color = pal.snippet.gamma_multiply(if active { 0.9 } else { 0.4 });
                for r in stop {
                    let rects = range_rects(r.clone());
                    if rects.is_empty() {
                        // Empty stop: a short bar where the caret will land.
                        let (line, col) = super::text::line_col(rope, r.start);
                        if shown(line) {
                            let x = layout.text_x()
                                + visual_col(text_of(line), col, tw) as f32 * layout.char_w;
                            let y = layout.row_y(line);
                            painter.vline(
                                x,
                                y + 2.0..=y + layout.row_h - 2.0,
                                Stroke::new(1.5, color),
                            );
                        }
                    }
                    for rect in rects {
                        if active {
                            painter.rect_filled(rect, 2.0, pal.snippet.gamma_multiply(0.12));
                        }
                        painter.rect_stroke(
                            rect.expand(0.5),
                            2.0,
                            Stroke::new(1.0, color),
                            egui::StrokeKind::Outside,
                        );
                    }
                }
            }
        }

        if let Some(pair) = pair {
            for r in [pair.open, pair.close] {
                let chars = rope.byte_to_char(r.start)..rope.byte_to_char(r.end);
                for rect in range_rects(chars) {
                    painter.rect_filled(rect, 2.0, pal.pair);
                    painter.rect_stroke(
                        rect,
                        2.0,
                        Stroke::new(1.0, pal.pair.gamma_multiply(2.0)),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }

        // Text.
        let highlighted = |text: &str, l: usize| {
            let (tokens, _) = lex_line(text, self.state.highlighter.state(l));
            let mut job = LayoutJob::default();
            let mut vcol = 0;
            for token in tokens {
                let ws = token.kind == TokenKind::Whitespace;
                let mut piece = String::with_capacity(token.range.len());
                for c in text[token.range].chars() {
                    if c == '\t' {
                        let n = tw - vcol % tw;
                        piece.push(if config.show_whitespace { '→' } else { ' ' });
                        piece.extend(std::iter::repeat_n(' ', n - 1));
                        vcol += n;
                    } else {
                        piece.push(if ws && config.show_whitespace && c == ' ' {
                            '·'
                        } else {
                            c
                        });
                        vcol += 1;
                    }
                }
                let color = if ws {
                    pal.whitespace
                } else {
                    theme.color(token.kind)
                };
                let format = TextFormat {
                    font_id: font.clone(),
                    color,
                    italics: matches!(token.kind, TokenKind::LineComment | TokenKind::BlockComment),
                    ..Default::default()
                };
                job.append(&piece, 0.0, format);
            }
            ui.fonts_mut(|f| f.layout_job(job))
        };
        for l in lines.clone() {
            let text = text_of(l);
            if !text.is_empty() {
                painter.galley(
                    pos2(layout.text_x(), layout.text_y(l)),
                    highlighted(text, l),
                    pal.text,
                );
            }
        }

        // Folded lines end in a "⋯" pill, followed by the closing line of
        // a block (`end if;`, `)`) so the structure stays readable.
        let mut pills = Vec::new();
        for &(start, end) in &self.state.tracked.folds {
            if !shown(start) {
                continue;
            }
            let text = text_of(start);
            let width = visual_col(text, text.chars().count(), tw);
            let y = layout.row_y(start);
            let x = layout.text_x() + (width as f32 + 0.6) * layout.char_w;
            let pill = Rect::from_min_size(
                pos2(x, y + 2.0),
                vec2(layout.char_w * 2.6, layout.row_h - 4.0),
            );
            let hovered = ui
                .ctx()
                .pointer_hover_pos()
                .is_some_and(|p| pill.contains(p));
            let has_error = self
                .markers
                .iter()
                .any(|m| m.kind == MarkerKind::Error && m.line > start + 1 && m.line <= end + 1);
            let fill = if hovered {
                pal.fold_pill.gamma_multiply(2.0)
            } else {
                pal.fold_pill
            };
            painter.rect_filled(pill, pill.height() * 0.5, fill);
            if has_error {
                painter.rect_stroke(
                    pill,
                    pill.height() * 0.5,
                    Stroke::new(1.0, pal.error),
                    egui::StrokeKind::Inside,
                );
            }
            let dot = if has_error {
                pal.error
            } else {
                pal.line_number
            };
            for i in -1..=1 {
                painter.circle_filled(
                    pill.center() + vec2(i as f32 * layout.char_w * 0.55, 0.0),
                    1.4,
                    dot,
                );
            }
            pills.push((pill, start));
            let closes = regions.iter().find(|r| r.start == start).is_some_and(|r| {
                matches!(
                    r.kind,
                    RegionKind::Unit | RegionKind::Block | RegionKind::Paren
                )
            });
            if closes {
                let closing = line_text(rope, end);
                let closing = closing.trim();
                if !closing.is_empty() && closing.chars().count() <= 40 {
                    let galley = highlighted(closing, end);
                    painter.galley(
                        pos2(pill.max.x + layout.char_w * 0.6, y + layout.text_dy),
                        galley,
                        pal.text,
                    );
                }
            }
        }

        // Ctrl+hover link underline.
        if let Some(link) = &self.link {
            for rect in range_rects(link.clone()) {
                painter.hline(
                    rect.x_range(),
                    rect.max.y - layout.text_dy - 1.5,
                    Stroke::new(1.0, pal.link),
                );
            }
        }

        // Error / warning squiggles.
        for m in &self.markers {
            let l = m.line.wrapping_sub(1);
            let color = match m.kind {
                MarkerKind::Error => pal.error,
                MarkerKind::Warning => pal.warning,
                _ => continue,
            };
            if !shown(l) {
                continue;
            }
            let text = text_of(l);
            let indent = super::text::indent_of(text).chars().count();
            let len = text.chars().count();
            let (v0, v1) = (
                visual_col(text, indent, tw),
                visual_col(text, len, tw).max(visual_col(text, indent, tw) + 1),
            );
            let y = layout.row_y(l) + layout.row_h - layout.text_dy - 2.0;
            let (x0, x1) = (
                layout.text_x() + v0 as f32 * layout.char_w,
                layout.text_x() + v1 as f32 * layout.char_w,
            );
            let step = 2.5;
            let points: Vec<Pos2> = (0..=((x1 - x0) / step) as usize)
                .map(|i| {
                    pos2(
                        x0 + i as f32 * step,
                        y + if i % 2 == 0 { -1.2 } else { 1.2 },
                    )
                })
                .collect();
            painter.add(Shape::line(points, Stroke::new(1.0, color)));
        }

        // Carets.
        let blink_on =
            !config.caret_blink || ((time - self.blink_epoch) % BLINK_PERIOD) < BLINK_PERIOD * 0.6;
        if focused {
            if blink_on {
                let text_h = layout.row_h - 2.0 * layout.text_dy;
                for s in &self.state.sels {
                    let (line, col) = super::text::line_col(rope, s.head);
                    if !shown(line) {
                        continue;
                    }
                    let vcol = visual_col(text_of(line), col, tw);
                    let x = layout.text_x() + vcol as f32 * layout.char_w;
                    match config.caret {
                        CaretShape::Line => {
                            let r = Rect::from_min_size(
                                pos2(x - 0.5, layout.row_y(line)),
                                vec2(2.0, layout.row_h),
                            );
                            painter.rect_filled(r, 0.0, pal.caret);
                        }
                        // See-through, so the character stays readable.
                        CaretShape::Block => {
                            let r = Rect::from_min_size(
                                pos2(x, layout.text_y(line)),
                                vec2(layout.char_w, text_h),
                            );
                            painter.rect_filled(r, 0.0, pal.caret.gamma_multiply(0.45));
                        }
                        CaretShape::Underline => {
                            let r = Rect::from_min_size(
                                pos2(x, layout.text_y(line) + text_h - 2.0),
                                vec2(layout.char_w, 2.0),
                            );
                            painter.rect_filled(r, 0.0, pal.caret);
                        }
                    }
                }
            }
            if config.caret_blink {
                let phase = (time - self.blink_epoch) % BLINK_PERIOD;
                let next = if blink_on {
                    BLINK_PERIOD * 0.6 - phase
                } else {
                    BLINK_PERIOD - phase
                };
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs_f64(next.max(0.01)));
            }
        }

        self.paint_gutter(ui, layout, config, pal, font, &lines, caret_line, &regions);
        self.fold_pills = pills;
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_gutter(
        &self,
        ui: &Ui,
        layout: &Layout,
        config: &EditorConfig,
        pal: &Palette,
        font: &FontId,
        lines: &Range<usize>,
        caret_line: usize,
        regions: &[Region],
    ) {
        let painter = ui.painter();
        let visible = layout.visible_rect();
        let x0 = layout.gutter_left();
        let gutter = Rect::from_min_max(
            pos2(x0, visible.min.y),
            pos2(x0 + layout.gutter_w, visible.max.y),
        );
        painter.rect_filled(gutter, 0.0, pal.gutter_bg);
        painter.vline(
            gutter.max.x - 0.5,
            gutter.y_range(),
            Stroke::new(1.0, pal.gutter_line),
        );

        let shown = |l: usize| layout.shows(lines, l);
        if config.show_line_numbers {
            let right = gutter.max.x - layout.fold_w;
            for l in lines.clone().filter(|l| shown(*l)) {
                let color = if l == caret_line {
                    pal.line_number_current
                } else {
                    pal.line_number
                };
                painter.text(
                    pos2(right, layout.text_y(l)),
                    egui::Align2::RIGHT_TOP,
                    (l + 1).to_string(),
                    font.clone(),
                    color,
                );
            }
        }

        // Bookmarks: a ribbon in the marker column, numbered ones labelled.
        for b in &self.state.tracked.bookmarks {
            if !shown(b.line) {
                continue;
            }
            let center = pos2(
                x0 + layout.marker_w * 0.5,
                layout.row_y(b.line) + layout.row_h * 0.5,
            );
            let (w, h) = (layout.row_h * 0.62, layout.row_h * 0.82);
            let rect = Rect::from_center_size(center, vec2(w, h));
            painter.rect_filled(
                rect,
                egui::CornerRadius {
                    nw: 2,
                    ne: 2,
                    sw: 0,
                    se: 0,
                },
                pal.bookmark,
            );
            let notch = vec![
                pos2(rect.min.x - 0.5, rect.max.y + 0.5),
                pos2(rect.max.x + 0.5, rect.max.y + 0.5),
                pos2(rect.center().x, rect.max.y - h * 0.24),
            ];
            painter.add(Shape::convex_polygon(notch, pal.gutter_bg, Stroke::NONE));
            if let Some(n) = b.number {
                painter.text(
                    pos2(rect.center().x, rect.min.y + h * 0.38),
                    egui::Align2::CENTER_CENTER,
                    n.to_string(),
                    FontId::monospace((layout.row_h * 0.56).round()),
                    Color32::WHITE,
                );
            }
        }

        for m in &self.markers {
            let l = m.line.wrapping_sub(1);
            if !shown(l) {
                continue;
            }
            let center = pos2(
                x0 + layout.marker_w * 0.5,
                layout.row_y(l) + layout.row_h * 0.5,
            );
            let r = layout.row_h * 0.3;
            match m.kind {
                MarkerKind::Breakpoint => {
                    painter.circle_filled(center, r * 1.15, pal.breakpoint);
                }
                MarkerKind::Error => {
                    painter.circle_filled(center, r, pal.error);
                    let d = r * 0.5;
                    let s = Stroke::new(1.5, pal.bg);
                    painter.line_segment([center + vec2(-d, -d), center + vec2(d, d)], s);
                    painter.line_segment([center + vec2(-d, d), center + vec2(d, -d)], s);
                }
                MarkerKind::Warning => {
                    let pts = vec![
                        center + vec2(0.0, -r * 1.1),
                        center + vec2(r * 1.1, r * 0.9),
                        center + vec2(-r * 1.1, r * 0.9),
                    ];
                    painter.add(Shape::convex_polygon(pts, pal.warning, Stroke::NONE));
                }
                MarkerKind::Bookmark => {
                    let rect = Rect::from_center_size(center, vec2(r * 1.4, r * 2.0));
                    painter.rect_filled(rect, 1.5, pal.bookmark);
                }
                MarkerKind::ExecutionPoint => {
                    let pts = vec![
                        center + vec2(-r, -r),
                        center + vec2(r * 1.2, 0.0),
                        center + vec2(-r, r),
                    ];
                    painter.add(Shape::convex_polygon(
                        pts,
                        pal.execution,
                        Stroke::new(1.0, pal.execution.gamma_multiply(0.6)),
                    ));
                }
            }
        }

        self.paint_fold_chevrons(ui, layout, pal, gutter, regions, &shown);
    }

    /// Chevrons in the fold column: always for folded lines, for every
    /// region while the pointer is over the gutter. The hovered region's
    /// extent is traced next to the text.
    fn paint_fold_chevrons(
        &self,
        ui: &Ui,
        layout: &Layout,
        pal: &Palette,
        gutter: Rect,
        regions: &[Region],
        shown: &dyn Fn(usize) -> bool,
    ) {
        let painter = ui.painter();
        let hover = ui.ctx().pointer_hover_pos().filter(|p| gutter.contains(*p));
        let hover_line = hover
            .filter(|p| layout.row_at(p.y) < layout.map.rows())
            .map(|p| layout.line_at(p.y));
        let cx = (layout.fold_column().start + layout.fold_column().end) * 0.5;
        let s = (layout.row_h * 0.17).round();
        let folds = &self.state.tracked.folds;
        for r in regions {
            if !shown(r.start) {
                continue;
            }
            let folded = folds.iter().any(|(f, _)| *f == r.start);
            if !folded && hover.is_none() {
                continue;
            }
            let hot = hover_line == Some(r.start);
            let color = if hot {
                pal.line_number_current
            } else if folded {
                pal.line_number.gamma_multiply(1.3)
            } else {
                pal.line_number.gamma_multiply(0.8)
            };
            let c = pos2(cx, layout.row_y(r.start) + layout.row_h * 0.5);
            if hot || folded {
                let bg = Rect::from_center_size(c, vec2(layout.fold_w - 4.0, layout.row_h - 4.0));
                painter.rect_filled(
                    bg,
                    3.0,
                    pal.fold_pill.gamma_multiply(if hot { 1.5 } else { 0.8 }),
                );
            }
            let stroke = Stroke::new(1.4, color);
            let pts = if folded {
                vec![
                    c + vec2(-s * 0.5, -s),
                    c + vec2(s * 0.5, 0.0),
                    c + vec2(-s * 0.5, s),
                ]
            } else {
                vec![
                    c + vec2(-s, -s * 0.5),
                    c + vec2(0.0, s * 0.5),
                    c + vec2(s, -s * 0.5),
                ]
            };
            painter.add(Shape::line(pts, stroke));
            if hot {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                if !folded {
                    let y0 = layout.row_y(r.start) + layout.row_h;
                    let y1 = layout.row_y(r.end) + layout.row_h * 0.5;
                    let x = gutter.max.x - 1.5;
                    painter.vline(
                        x,
                        y0..=y1,
                        Stroke::new(2.0, pal.line_number.gamma_multiply(0.7)),
                    );
                }
            }
        }
    }

    /// Hovering a "⋯" pill previews the hidden lines.
    fn fold_tooltip(&self, ui: &Ui, response: &egui::Response, font: &FontId) {
        let Some(pos) = ui.ctx().pointer_hover_pos().filter(|_| response.hovered()) else {
            return;
        };
        let Some(&(_, start)) = self.fold_pills.iter().find(|(r, _)| r.contains(pos)) else {
            return;
        };
        let Some(&(_, end)) = self.state.tracked.folds.iter().find(|(s, _)| *s == start) else {
            return;
        };
        let rope = &self.state.rope;
        const PREVIEW: usize = 12;
        let shown_end = end.min(start + PREVIEW);
        let body: Vec<String> = (start + 1..=shown_end)
            .map(|l| line_text(rope, l))
            .collect();
        let indent = body
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| super::text::indent_of(l).len())
            .min()
            .unwrap_or(0);
        let theme = SyntaxTheme::for_visuals(ui.visuals());
        let font = FontId::monospace(font.size * 0.9);
        response.clone().on_hover_ui_at_pointer(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, l) in body.iter().enumerate() {
                let text = l.get(indent..).unwrap_or(l).replace('\t', "    ");
                let (tokens, _) = lex_line(&text, self.state.highlighter.state(start + 1 + i));
                let mut job = LayoutJob::default();
                for token in tokens {
                    let format = TextFormat {
                        font_id: font.clone(),
                        color: theme.color(token.kind),
                        italics: matches!(
                            token.kind,
                            TokenKind::LineComment | TokenKind::BlockComment
                        ),
                        ..Default::default()
                    };
                    job.append(&text[token.range], 0.0, format);
                }
                ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
            }
            ui.add_space(4.0);
            let more = if end > shown_end {
                format!(" ({} more)", end - shown_end)
            } else {
                String::new()
            };
            ui.label(
                egui::RichText::new(format!(
                    "{} hidden lines{more} · click to unfold",
                    end - start
                ))
                .small()
                .weak(),
            );
        });
    }

    fn marker_tooltip(&self, ui: &Ui, response: &egui::Response, layout: &Layout) {
        if self.markers.is_empty() || !response.hovered() {
            return;
        }
        let Some(pos) = ui.input(|i| i.pointer.hover_pos()) else {
            return;
        };
        if layout.row_at(pos.y) >= layout.map.rows() {
            return;
        }
        let line = layout.line_at(pos.y) + 1;
        let messages: Vec<&str> = self
            .markers
            .iter()
            .filter(|m| m.line == line && !m.message.is_empty())
            .map(|m| m.message.as_str())
            .collect();
        if messages.is_empty() {
            return;
        }
        let in_gutter = pos.x < layout.gutter_left() + layout.gutter_w;
        let on_error_line = self
            .markers
            .iter()
            .any(|m| m.line == line && matches!(m.kind, MarkerKind::Error | MarkerKind::Warning));
        if in_gutter || on_error_line {
            response.clone().on_hover_ui_at_pointer(|ui| {
                for msg in messages {
                    ui.label(egui::RichText::new(msg).monospace());
                }
            });
        }
    }

    fn goto_line_popup(&mut self, ui: &Ui, editor_rect: Rect, editor: &egui::Response) {
        let Some(input) = self.goto_line.as_mut() else {
            return;
        };
        let n = self.state.rope.len_lines();
        let mut close = false;
        let mut target = None;
        egui::Area::new(editor.id.with("goto-line"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(
                editor_rect.center().x - 110.0,
                editor_rect.min.y + 8.0,
            ))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("Go to line (1–{n}):"));
                        let field = ui.add(egui::TextEdit::singleline(input).desired_width(70.0));
                        field.request_focus();
                        if field.lost_focus() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                            if ui.input(|i| i.key_pressed(Key::Enter)) {
                                target = input.trim().parse::<usize>().ok();
                            }
                        }
                    });
                });
            });
        if close {
            self.goto_line = None;
            self.focus_requested = true;
            if let Some(line) = target {
                self.goto_line(line.clamp(1, n));
            }
        }
    }
}

fn digit(key: Key) -> Option<u8> {
    let n = match key {
        Key::Num1 => 1,
        Key::Num2 => 2,
        Key::Num3 => 3,
        Key::Num4 => 4,
        Key::Num5 => 5,
        Key::Num6 => 6,
        Key::Num7 => 7,
        Key::Num8 => 8,
        Key::Num9 => 9,
        _ => return None,
    };
    Some(n)
}

/// Whether the caret touches a bracket or a block keyword (avoids computing
/// pairs for every caret move).
fn caret_on_pairable(rope: &ropey::Rope, pos: usize) -> bool {
    let len = rope.len_chars();
    let is_bracket = |c: char| matches!(c, '(' | ')' | '[' | ']');
    if (pos < len && is_bracket(rope.char(pos))) || (pos > 0 && is_bracket(rope.char(pos - 1))) {
        return true;
    }
    let (a, b) = super::text::word_range(rope, pos.min(len));
    if b <= a || b - a > 5 {
        return false;
    }
    let word = rope.slice(a..b).to_string();
    ["BEGIN", "END", "IF", "LOOP", "CASE"]
        .iter()
        .any(|k| word.eq_ignore_ascii_case(k))
}

/// Layer of the completion list; `ui` is the editor's content ui.
fn completion_layer(ui: &Ui) -> egui::LayerId {
    egui::LayerId::new(egui::Order::Foreground, ui.id().with("completion-popup"))
}

/// Where the completion list goes for `wanted` rows: below the caret line if
/// they fit there, else on the side with more room. Returns whether it's
/// above and how many rows fit (at least one). `chrome` is the frame height.
fn popup_rows(caret: Rect, screen: Rect, row_h: f32, chrome: f32, wanted: usize) -> (bool, usize) {
    let fit = |space: f32| ((space - POPUP_GAP - chrome) / row_h).floor().max(0.0) as usize;
    let below = fit(screen.max.y - caret.max.y);
    let above = fit(caret.min.y - screen.min.y);
    if below >= wanted || below >= above {
        (false, below.min(wanted).max(1))
    } else {
        (true, above.min(wanted).max(1))
    }
}

/// Width the completion rows need to show the filtered entries uncut.
fn list_width(ui: &Ui, completion: &Completion, font: &FontId) -> f32 {
    let detail_font = detail_font(font);
    ui.fonts_mut(|f| {
        let mut width = |text: &str, font: &FontId| -> f32 {
            text.chars().map(|c| f.glyph_width(font, c)).sum()
        };
        completion
            .filtered
            .iter()
            .map(|&i| {
                let item = &completion.items()[i];
                let detail = if item.detail.is_empty() {
                    0.0
                } else {
                    ROW_GAP + width(&item.detail, &detail_font)
                };
                // A little slack: shaping may differ from the glyph widths.
                ROW_LEAD + width(&item.label, font) + detail + ROW_TRAIL + 2.0
            })
            .fold(0.0, f32::max)
    })
}

fn detail_font(font: &FontId) -> FontId {
    FontId::proportional(font.size * 0.9)
}

/// Widths for a row's label and detail within `room`. Both stay whole if
/// they fit; else the label keeps up to 60 % of the row and the detail gets
/// the rest. A detail left with too little room is dropped (width 0).
fn fit_columns(label: f32, detail: f32, room: f32) -> (f32, f32) {
    if detail <= 0.0 || label + ROW_GAP + detail <= room {
        return (label.min(room), detail);
    }
    let label_w = label.min((room - ROW_GAP - detail).max(room * 0.6));
    let detail_w = room - ROW_GAP - label_w;
    if detail_w < detail.min(3.0 * ROW_GAP) {
        (label.min(room), 0.0)
    } else {
        (label_w, detail_w)
    }
}

/// One line of text, cut with "…" beyond `max_width`.
fn one_line(
    ui: &Ui,
    text: &str,
    font: &FontId,
    color: Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font.clone(), color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width);
    ui.fonts_mut(|f| f.layout_job(job))
}

/// One row of the completion list: kind glyph, label and the detail on the
/// right, cut rather than overlapping.
fn paint_completion_row(
    ui: &Ui,
    rect: Rect,
    item: &CompletionEntry,
    selected: bool,
    hovered: bool,
    font: &FontId,
) {
    let visuals = ui.visuals();
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 3.0, visuals.selection.bg_fill);
    } else if hovered {
        painter.rect_filled(rect, 3.0, visuals.widgets.hovered.weak_bg_fill);
    }
    let glyph_rect =
        Rect::from_center_size(pos2(rect.min.x + 12.0, rect.center().y), vec2(16.0, 16.0));
    let color = item.kind.color(visuals.dark_mode);
    painter.rect_filled(glyph_rect, 3.0, color.gamma_multiply(0.22));
    painter.text(
        glyph_rect.center(),
        egui::Align2::CENTER_CENTER,
        item.kind.glyph(),
        FontId::monospace(font.size * 0.85),
        color,
    );
    let text_color = if selected {
        visuals.selection.stroke.color
    } else {
        visuals.text_color()
    };
    let detail_font = detail_font(font);
    let weak = visuals.weak_text_color();
    let left = rect.min.x + ROW_LEAD;
    let right = rect.max.x - ROW_TRAIL;
    let mut label = one_line(ui, &item.label, font, text_color, f32::INFINITY);
    let mut detail = (!item.detail.is_empty())
        .then(|| one_line(ui, &item.detail, &detail_font, weak, f32::INFINITY));
    let detail_w = detail.as_ref().map_or(0.0, |d| d.size().x);
    let (label_max, detail_max) = fit_columns(label.size().x, detail_w, right - left);
    if label.size().x > label_max {
        label = one_line(ui, &item.label, font, text_color, label_max);
    }
    if detail_max <= 0.0 {
        detail = None;
    } else if detail_w > detail_max {
        detail = Some(one_line(ui, &item.detail, &detail_font, weak, detail_max));
    }
    let top = |g: &egui::Galley| rect.center().y - g.size().y / 2.0;
    painter.galley(pos2(left, top(&label)), label, text_color);
    if let Some(detail) = detail {
        painter.galley(pos2(right - detail.size().x, top(&detail)), detail, weak);
    }
}

/// The selected completion entry's documentation next to the list: right of
/// it, or left if there's no room there. It grows away from the caret line.
fn completion_details(ui: &Ui, list: Rect, above: bool, font: &FontId, title: &str, text: &str) {
    let frame = egui::Frame::popup(ui.style()).inner_margin(egui::Margin::symmetric(8, 6));
    let chrome = frame.total_margin().sum();
    let screen = ui.ctx().content_rect();
    let visuals = ui.visuals();
    let wrap = DETAILS_MAX_WIDTH - chrome.x;
    let room = if above {
        list.max.y - screen.min.y
    } else {
        screen.max.y - list.min.y
    } - chrome.y;
    let layout = |text: &str, font: FontId, color: Color32, max_rows: usize| {
        let mut job = LayoutJob::simple(text.to_owned(), font, color, wrap);
        job.wrap.max_rows = max_rows;
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let strong = visuals.strong_text_color();
    let title = layout(title, font.clone(), strong, 2);
    // A separator line with space around it.
    let rule = 9.0;
    let body_font = egui::TextStyle::Body.resolve(ui.style());
    let body_row = ui.fonts_mut(|f| f.row_height(&body_font));
    let body_rows = ((room - title.size().y - rule) / body_row).floor().max(0.0) as usize;
    let body = (!text.is_empty() && body_rows > 0)
        .then(|| layout(text, body_font, visuals.text_color(), body_rows));
    let size = vec2(
        title
            .size()
            .x
            .max(body.as_ref().map_or(0.0, |b| b.size().x)),
        title.size().y + body.as_ref().map_or(0.0, |b| rule + b.size().y),
    );
    let outer = size + chrome;
    let x = if list.max.x + POPUP_GAP + outer.x <= screen.max.x {
        list.max.x + POPUP_GAP
    } else if list.min.x - POPUP_GAP - outer.x >= screen.min.x {
        list.min.x - POPUP_GAP - outer.x
    } else {
        return;
    };
    let y = if above {
        list.max.y - outer.y
    } else {
        list.min.y
    };
    let line = visuals.widgets.noninteractive.bg_stroke;
    let text_color = visuals.text_color();
    egui::Area::new(ui.id().with("completion-details"))
        .order(egui::Order::Foreground)
        .interactable(false)
        .fixed_pos(pos2(x, y))
        .show(ui.ctx(), |ui| {
            frame.show(ui, |ui| {
                let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
                let painter = ui.painter();
                let below_title = rect.min.y + title.size().y;
                painter.galley(rect.min, title, strong);
                if let Some(body) = body {
                    painter.hline(rect.x_range(), below_title + rule / 2.0, line);
                    painter.galley(pos2(rect.min.x, below_title + rule), body, text_color);
                }
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_editor::CompletionKind;

    /// Runs frames of an editor filling a 1000×800 window; `events` go to the first.
    fn frames(ctx: &egui::Context, editor: &mut CodeEditor, n: usize, events: Vec<Event>) {
        frames_with(ctx, editor, n, events, &EditorConfig::default());
    }

    /// [`frames`] with `config`; returns the last frame's output.
    fn frames_with(
        ctx: &egui::Context,
        editor: &mut CodeEditor,
        n: usize,
        mut events: Vec<Event>,
        config: &EditorConfig,
    ) -> egui::FullOutput {
        let mut last = None;
        for _ in 0..n {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0))),
                events: std::mem::take(&mut events),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                editor.ui(ui, config);
            });
            output.textures_delta.clear();
            last = Some(output);
        }
        last.expect("a frame")
    }

    /// Line spacing adds room between lines, with the text in the middle;
    /// clicks still land on the line drawn there.
    #[test]
    fn line_spacing() {
        let ctx = egui::Context::default();
        let mut editor = CodeEditor::new("one\ntwo\nthree");
        frames(&ctx, &mut editor, 2, vec![]);
        let plain = editor.last_layout.as_ref().unwrap().row_h;
        let config = EditorConfig {
            line_spacing: 6.0,
            ..EditorConfig::default()
        };
        let output = frames_with(&ctx, &mut editor, 2, vec![], &config);
        let layout = editor.last_layout.as_ref().unwrap();
        assert_eq!(layout.row_h, plain + 6.0);
        assert_eq!(layout.text_y(1), layout.row_y(1) + 3.0);
        let two = output
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                Shape::Text(t) if t.galley.text() == "two" => Some(t.pos.y),
                _ => None,
            })
            .expect("line 2 drawn");
        assert_eq!(two, layout.text_y(1));
        // The bottom of line 2's row, in the spacing below its text.
        let at = pos2(layout.text_x() + 1.0, layout.row_y(2) - 1.0);
        frames_with(
            &ctx,
            &mut editor,
            2,
            vec![pointer(at, true), pointer(at, false)],
            &config,
        );
        assert_eq!(editor.line_col().0, 2);
    }

    /// A blinking caret asks for the next blink; a steady one doesn't.
    #[test]
    fn caret_blink() {
        let ctx = egui::Context::default();
        let mut editor = CodeEditor::new("x");
        editor.request_focus();
        let repaint = |output: &egui::FullOutput| {
            output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
        };
        let blinking = frames_with(&ctx, &mut editor, 3, vec![], &EditorConfig::default());
        assert!(repaint(&blinking) < std::time::Duration::from_secs(2));
        let config = EditorConfig {
            caret_blink: false,
            caret: CaretShape::Block,
            ..EditorConfig::default()
        };
        let steady = frames_with(&ctx, &mut editor, 3, vec![], &config);
        assert_eq!(repaint(&steady), std::time::Duration::MAX);
    }

    fn entry(label: &str) -> CompletionEntry {
        CompletionEntry {
            label: label.into(),
            detail: "number".into(),
            kind: CompletionKind::Column,
            insert_text: label.into(),
            documentation: None,
        }
    }

    fn key(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    /// Rows the completion list showed in the last frame.
    fn visible_rows(editor: &CodeEditor) -> usize {
        editor.completion.as_ref().expect("open").page
    }

    #[test]
    fn completion_list_grows_back_after_a_short_list() {
        let ctx = egui::Context::default();
        let mut editor = CodeEditor::new("select ");
        editor.goto(1, 8);
        editor.request_focus();
        let rows = EditorConfig::default().completion_rows;
        let mut items: Vec<_> = (0..50).map(|i| entry(&format!("col_{i:02}"))).collect();
        items.extend(["zz_a", "zz_b", "zz_c"].map(entry));
        frames(&ctx, &mut editor, 2, vec![]);
        editor.show_completions(7..7, items.clone(), false);
        frames(&ctx, &mut editor, 3, vec![]);
        assert_eq!(visible_rows(&editor), rows);

        frames(&ctx, &mut editor, 3, vec![Event::Text("zz".into())]);
        assert_eq!(editor.completion_labels(), ["zz_a", "zz_b", "zz_c"]);
        assert_eq!(visible_rows(&editor), 3);

        // The area remembers the short list; the next one must still grow.
        frames(&ctx, &mut editor, 2, vec![key(Key::Escape)]);
        assert!(!editor.is_completion_open());
        frames(
            &ctx,
            &mut editor,
            2,
            vec![key(Key::Backspace), key(Key::Backspace)],
        );
        assert_eq!(editor.text(), "select ");
        editor.show_completions(7..7, items, false);
        frames(&ctx, &mut editor, 3, vec![]);
        assert_eq!(visible_rows(&editor), rows);
        // PageDown moves by what is shown.
        frames(&ctx, &mut editor, 2, vec![key(Key::PageDown)]);
        assert_eq!(editor.completion.as_ref().unwrap().selected, rows);
    }

    /// Typing into an open popup filters a complete list, but asks for
    /// completions again when the list was cut short (#160).
    #[test]
    fn typing_into_a_cut_list_asks_again() {
        let ctx = egui::Context::default();
        let mut editor = CodeEditor::new("select mx");
        editor.goto(1, 10);
        editor.request_focus();
        frames(&ctx, &mut editor, 2, vec![]);
        let items: Vec<_> = (0..20).map(|i| entry(&format!("mxn_a{i:02}"))).collect();
        for incomplete in [false, true] {
            editor.show_completions(7..9, items.clone(), incomplete);
            frames(&ctx, &mut editor, 1, vec![]);
            let mut events = vec![Event::Text("n".into())];
            let mut asked = false;
            // Past the debounce (150 ms at 1/60 s a frame).
            for _ in 0..30 {
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0))),
                    events: std::mem::take(&mut events),
                    ..Default::default()
                };
                let mut output = ctx.run_ui(input, |ui| {
                    asked |= editor
                        .ui(ui, &EditorConfig::default())
                        .completion_request
                        .is_some();
                });
                output.textures_delta.clear();
            }
            assert_eq!(editor.text(), "select mxn");
            assert_eq!(asked, incomplete, "incomplete: {incomplete}");
            assert!(editor.is_completion_open(), "still filtered meanwhile");
            frames(&ctx, &mut editor, 2, vec![key(Key::Backspace)]);
        }
    }

    #[test]
    fn completion_popups_stay_off_the_line() {
        let ctx = egui::Context::default();
        let mut editor = CodeEditor::new(&"x\n".repeat(100));
        let items: Vec<_> = (0..50)
            .map(|i| CompletionEntry {
                documentation: Some(("column T.X".into(), "NUMBER NOT NULL".into())),
                ..entry(&format!("x_{i:02}"))
            })
            .collect();
        for (line, above) in [(3, false), (40, true)] {
            editor.goto(line, 2);
            editor.request_focus();
            frames(&ctx, &mut editor, 2, vec![]);
            let at = editor.cursor();
            editor.show_completions(at - 1..at, items.clone(), false);
            frames(&ctx, &mut editor, 3, vec![]);
            let layout = editor.last_layout.as_ref().unwrap();
            let (top, bottom) = (layout.row_y(line - 1), layout.row_y(line));
            // The list and the details pane.
            let popups: Vec<Rect> = ctx.memory(|m| {
                let layers = m.areas().visible_layer_ids();
                layers
                    .into_iter()
                    .filter(|l| l.order == egui::Order::Foreground)
                    .filter_map(|l| m.area_rect(l.id))
                    .collect()
            });
            assert_eq!(popups.len(), 2, "line {line}");
            for rect in popups {
                if above {
                    assert!(rect.max.y <= top, "line {line}: {rect:?} over {top}");
                } else {
                    assert!(rect.min.y >= bottom, "line {line}: {rect:?} over {bottom}");
                }
            }
            editor.hide_completions();
        }
    }

    fn pointer(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn clicking_a_completion_row_accepts_it() {
        let ctx = egui::Context::default();
        let mut editor = CodeEditor::new("select ");
        editor.goto(1, 8);
        editor.request_focus();
        frames(&ctx, &mut editor, 2, vec![]);
        editor.show_completions(7..7, ["alpha", "beta", "gamma"].map(entry).to_vec(), false);
        frames(&ctx, &mut editor, 3, vec![]);
        // The list is the only foreground area (no details pane).
        let list = ctx
            .memory(|m| {
                let layers = m.areas().visible_layer_ids();
                layers
                    .into_iter()
                    .filter(|l| l.order == egui::Order::Foreground)
                    .find_map(|l| m.area_rect(l.id))
            })
            .expect("list shown");
        // Middle of the second row (the frame has a 3 px margin).
        let row_h = (list.height() - 6.0) / 3.0;
        let at = pos2(list.center().x, list.min.y + 3.0 + 1.5 * row_h);
        frames(&ctx, &mut editor, 1, vec![Event::PointerMoved(at)]);
        frames(&ctx, &mut editor, 1, vec![pointer(at, true)]);
        frames(&ctx, &mut editor, 1, vec![pointer(at, false)]);
        assert_eq!(editor.text(), "select beta");
        assert!(!editor.is_completion_open());
        // Typing goes on in the editor.
        frames(&ctx, &mut editor, 2, vec![Event::Text(" ".into())]);
        assert_eq!(editor.text(), "select beta ");
    }

    #[test]
    fn completion_list_opens_where_there_is_room() {
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0));
        let line = |y: f32| Rect::from_min_size(pos2(100.0, y), vec2(2.0, 16.0));
        // Room below: below, as many rows as wanted.
        assert_eq!(popup_rows(line(100.0), screen, 20.0, 8.0, 15), (false, 15));
        // Near the bottom: above, not over the line.
        assert_eq!(popup_rows(line(700.0), screen, 20.0, 8.0, 15), (true, 15));
        // Short list near the bottom: still below.
        assert_eq!(popup_rows(line(700.0), screen, 20.0, 8.0, 3), (false, 3));
        // Little room either way: the larger side, limited.
        let small = Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 200.0));
        assert_eq!(popup_rows(line(60.0), small, 20.0, 8.0, 15), (false, 5));
        assert_eq!(popup_rows(line(120.0), small, 20.0, 8.0, 15), (true, 5));
    }

    #[test]
    fn label_and_detail_share_the_row() {
        // Both fit: unchanged.
        assert_eq!(fit_columns(100.0, 80.0, 300.0), (100.0, 80.0));
        // A long detail is cut, the label stays whole.
        assert_eq!(fit_columns(100.0, 250.0, 300.0), (100.0, 184.0));
        // A long label gets at most 60 % if the detail needs the rest.
        assert_eq!(fit_columns(400.0, 200.0, 300.0), (180.0, 104.0));
        // A short detail stays whole, the label gives way.
        assert_eq!(fit_columns(290.0, 10.0, 300.0), (274.0, 10.0));
        // No detail: the label gets the row.
        assert_eq!(fit_columns(400.0, 0.0, 300.0), (300.0, 0.0));
    }
}
