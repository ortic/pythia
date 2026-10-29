//! Reusable widgets: code editor, data grid and execution plan.

pub mod code_editor;
mod editor;
mod grid;
pub mod icons;
pub mod plan;

pub use editor::{EditorOutput, SqlEditor, SyntaxTheme, highlight};
pub use grid::{
    Changes, GridLink, GridOutput, GridState, GridStyle, ResultGrid, selection_data,
    set_grid_style, shown_data, to_tsv,
};

/// Height of a list, tree or table row of body text: the text and the
/// vertical padding of buttons, which the density setting sets.
pub fn row_height(ui: &egui::Ui) -> f32 {
    ui.text_style_height(&egui::TextStyle::Body) + 2.0 * ui.spacing().button_padding.y
}
