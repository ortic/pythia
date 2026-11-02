//! Side-by-side diff of two texts: a file against its committed version or
//! against the object in the database.

use egui::{RichText, Ui};
use pythia_compare::diff::{LineDiff, side_by_side};
use serde::{Deserialize, Serialize};

use crate::documents::{DocContext, Effects, OpenRequest};

#[derive(Serialize, Deserialize)]
pub struct TextDiff {
    pub title: String,
    pub left_name: String,
    pub left: String,
    pub right_name: String,
    pub right: String,
    /// The file on the right, to open it for editing.
    #[serde(default, with = "crate::serde_path::option")]
    pub file: Option<std::path::PathBuf>,
    #[serde(skip)]
    diff: Option<LineDiff>,
    #[serde(skip)]
    hunk: usize,
    #[serde(skip)]
    jump: Option<usize>,
}

impl TextDiff {
    pub fn new(
        title: String,
        (left_name, left): (String, String),
        (right_name, right): (String, String),
        file: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            title,
            left_name,
            left,
            right_name,
            right,
            file,
            diff: None,
            hunk: 0,
            jump: None,
        }
    }

    pub fn title(&self) -> String {
        self.title.clone()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        let diff = self
            .diff
            .get_or_insert_with(|| side_by_side(&self.left, &self.right));
        let hunks = diff.hunk_starts();
        ui.horizontal(|ui| {
            ui.label(RichText::new(&self.left_name).strong());
            ui.weak("→");
            ui.label(RichText::new(&self.right_name).strong());
            ui.separator();
            if diff.is_equal() {
                ui.label(RichText::new("No differences").color(ui.visuals().weak_text_color()));
            } else {
                let green = egui::Color32::from_rgb(0x2f, 0xa0, 0x4c);
                ui.label(RichText::new(format!("+{}", diff.added)).color(green));
                ui.label(
                    RichText::new(format!("−{}", diff.removed)).color(ui.visuals().error_fg_color),
                );
                ui.label(
                    RichText::new(format!("~{}", diff.changed)).color(ui.visuals().warn_fg_color),
                );
                ui.separator();
                let n = hunks.len();
                if ui
                    .add_enabled(n > 0, egui::Button::new("↑"))
                    .on_hover_text("Previous change")
                    .clicked()
                {
                    self.hunk = (self.hunk + n - 1) % n;
                    self.jump = hunks.get(self.hunk).copied();
                }
                if ui
                    .add_enabled(n > 0, egui::Button::new("↓"))
                    .on_hover_text("Next change")
                    .clicked()
                {
                    self.hunk = (self.hunk + 1) % n;
                    self.jump = hunks.get(self.hunk).copied();
                }
                ui.weak(if n == 1 {
                    "1 change".to_owned()
                } else {
                    format!("{n} changes")
                });
            }
            if let Some(file) = &self.file {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Open file").clicked() {
                        effects.open.push(OpenRequest::File {
                            path: file.clone(),
                            connection: cx.connection.clone(),
                        });
                    }
                });
            }
        });
        ui.separator();
        pythia_compare::ui::side_by_side_ui(ui, ("text-diff", cx.id), diff, self.jump.take());
        effects
    }
}
