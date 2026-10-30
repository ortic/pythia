//! SQL / PL/SQL code editor.
//!
//! This is the M0/M1 spike: it wraps `egui::TextEdit` with PL/SQL syntax
//! highlighting and a line number gutter. The public surface (`SqlEditor`,
//! `EditorOutput`) is what the rope-backed, virtualized editor from M2 will
//! have to provide as well, so callers won't change when it is replaced.

use std::ops::Range;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, TextBuffer, Ui};
use pythia_plsql::{TokenKind, tokenize};

/// Token colors for one theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyntaxTheme {
    pub text: Color32,
    pub keyword: Color32,
    pub string: Color32,
    pub number: Color32,
    pub comment: Color32,
    pub hint: Color32,
    pub variable: Color32,
    pub operator: Color32,
    pub error: Color32,
    pub sqlplus: Color32,
}

impl SyntaxTheme {
    pub fn dark() -> Self {
        Self {
            text: Color32::from_rgb(0xd4, 0xd4, 0xd4),
            keyword: Color32::from_rgb(0x56, 0x9c, 0xd6),
            string: Color32::from_rgb(0xce, 0x91, 0x78),
            number: Color32::from_rgb(0xb5, 0xce, 0xa8),
            comment: Color32::from_rgb(0x6a, 0x99, 0x55),
            hint: Color32::from_rgb(0xc5, 0x86, 0xc0),
            variable: Color32::from_rgb(0x9c, 0xdc, 0xfe),
            operator: Color32::from_rgb(0xd4, 0xd4, 0xd4),
            error: Color32::from_rgb(0xf4, 0x47, 0x47),
            sqlplus: Color32::from_rgb(0x4e, 0xc9, 0xb0),
        }
    }

    /// Close to PL/SQL Developer's default colors.
    pub fn light() -> Self {
        Self {
            text: Color32::from_rgb(0x00, 0x00, 0x00),
            keyword: Color32::from_rgb(0x00, 0x00, 0xff),
            string: Color32::from_rgb(0xa3, 0x15, 0x15),
            number: Color32::from_rgb(0x09, 0x86, 0x58),
            comment: Color32::from_rgb(0x00, 0x80, 0x00),
            hint: Color32::from_rgb(0x80, 0x00, 0x80),
            variable: Color32::from_rgb(0x00, 0x70, 0xc1),
            operator: Color32::from_rgb(0x80, 0x00, 0x00),
            error: Color32::from_rgb(0xe0, 0x00, 0x00),
            sqlplus: Color32::from_rgb(0x26, 0x7f, 0x99),
        }
    }

    pub fn for_visuals(visuals: &egui::Visuals) -> Self {
        if visuals.dark_mode {
            Self::dark()
        } else {
            Self::light()
        }
    }

    pub fn color(&self, kind: TokenKind) -> Color32 {
        match kind {
            TokenKind::Keyword | TokenKind::Directive => self.keyword,
            TokenKind::String | TokenKind::QString => self.string,
            TokenKind::Number => self.number,
            TokenKind::LineComment | TokenKind::BlockComment => self.comment,
            TokenKind::Hint => self.hint,
            TokenKind::BindVar | TokenKind::SubstVar => self.variable,
            TokenKind::Operator | TokenKind::Semicolon => self.operator,
            TokenKind::Error => self.error,
            TokenKind::SqlPlus => self.sqlplus,
            _ => self.text,
        }
    }
}

/// Builds a highlighted layout job for `code`.
pub fn highlight(code: &str, font: FontId, theme: &SyntaxTheme) -> LayoutJob {
    let mut job = LayoutJob::default();
    for token in tokenize(code) {
        let format = TextFormat {
            font_id: font.clone(),
            color: theme.color(token.kind),
            italics: matches!(token.kind, TokenKind::LineComment | TokenKind::BlockComment),
            ..Default::default()
        };
        job.append(&code[token.span], 0.0, format);
    }
    job
}

#[derive(Default)]
struct Highlighter;

impl egui::cache::ComputerMut<(&FontId, &SyntaxTheme, &str), LayoutJob> for Highlighter {
    fn compute(&mut self, (font, theme, code): (&FontId, &SyntaxTheme, &str)) -> LayoutJob {
        highlight(code, font.clone(), theme)
    }
}

type HighlightCache = egui::cache::FrameCache<LayoutJob, Highlighter>;

pub struct SqlEditor<'a> {
    text: &'a mut String,
    id_salt: egui::Id,
    font_size: f32,
    line_numbers: bool,
    select: Option<Range<usize>>,
    read_only: bool,
}

/// Cursor and selection after the frame, as byte offsets into the text.
pub struct EditorOutput {
    pub response: egui::Response,
    pub cursor: usize,
    /// Non-empty selection, if any.
    pub selection: Option<Range<usize>>,
    /// 1-based line and column of the cursor, for the status bar.
    pub line_col: (usize, usize),
}

impl<'a> SqlEditor<'a> {
    pub fn new(text: &'a mut String, id_salt: impl egui::AsId) -> Self {
        Self {
            text,
            id_salt: egui::Id::new(id_salt),
            font_size: 13.0,
            line_numbers: true,
            select: None,
            read_only: false,
        }
    }

    /// Selects the byte range, focuses the editor and scrolls it into view.
    pub fn select(mut self, range: Option<Range<usize>>) -> Self {
        self.select = range;
        self
    }

    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    pub fn font_size(mut self, size: f32) -> Self {
        self.font_size = size;
        self
    }

    pub fn line_numbers(mut self, show: bool) -> Self {
        self.line_numbers = show;
        self
    }

    pub fn show(self, ui: &mut Ui) -> EditorOutput {
        let font = FontId::monospace(self.font_size);
        let theme = SyntaxTheme::for_visuals(ui.visuals());
        let mut layouter = |ui: &Ui, buf: &dyn TextBuffer, wrap_width: f32| {
            let mut job = ui.ctx().memory_mut(|mem| {
                mem.caches
                    .cache::<HighlightCache>()
                    .get((&font, &theme, buf.as_str()))
                    .clone()
            });
            // Code never soft-wraps; long lines scroll horizontally.
            let _ = wrap_width;
            job.wrap.max_width = f32::INFINITY;
            ui.fonts_mut(|f| f.layout_job(job))
        };

        let line_count = self.text.lines().count().max(1) + usize::from(self.text.ends_with('\n'));
        let gutter_color = ui.visuals().weak_text_color();
        let text = self.text;
        let select = self.select.map(|r| {
            let to_char = |b: usize| text[..b.min(text.len())].chars().count();
            (to_char(r.start), to_char(r.end))
        });
        let read_only = self.read_only;

        let output = egui::ScrollArea::both()
            .id_salt(self.id_salt.with("scroll"))
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    if self.line_numbers {
                        let digits = line_count.to_string().len().max(3);
                        let numbers: String = (1..=line_count)
                            .map(|n| format!("{n:>digits$}\n"))
                            .collect();
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(numbers)
                                    .font(FontId::monospace(self.font_size))
                                    .color(gutter_color),
                            )
                            .selectable(false),
                        );
                    }
                    let mut output = egui::TextEdit::multiline(text)
                        .id_salt(self.id_salt)
                        .code_editor()
                        .interactive(!read_only)
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY)
                        .desired_rows(30)
                        .lock_focus(true)
                        .layouter(&mut layouter)
                        .show(ui);
                    if let Some((start, end)) = select {
                        use egui::text::{CCursor, CCursorRange};
                        let range = CCursorRange::two(CCursor::new(start), CCursor::new(end));
                        output.state.cursor.set_char_range(Some(range));
                        output
                            .state
                            .clone()
                            .store(ui.ctx(), output.response.response.id);
                        output.response.response.request_focus();
                        let rect = output.galley.pos_from_cursor(CCursor::new(start));
                        ui.scroll_to_rect(
                            rect.translate(output.galley_pos.to_vec2()).expand(40.0),
                            None,
                        );
                    }
                    output
                })
                .inner
            })
            .inner;

        let to_byte = |char_idx: usize| {
            text.char_indices()
                .nth(char_idx)
                .map_or(text.len(), |(b, _)| b)
        };
        // Read the persisted state rather than `output.cursor_range`, which is
        // only set while focused: toolbar buttons steal focus but should still
        // act on the cursor position.
        let (cursor, selection) = match output.state.cursor.char_range() {
            Some(range) => {
                let chars = range.as_sorted_char_range();
                let sel = (chars.start != chars.end)
                    .then(|| to_byte(chars.start.0)..to_byte(chars.end.0));
                (to_byte(range.primary.index.0), sel)
            }
            None => (text.len(), None),
        };
        let before = &text[..cursor];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;

        EditorOutput {
            response: output.response.response,
            cursor,
            selection,
            line_col: (line, col),
        }
    }
}
