//! Settings window. Changes apply immediately and are saved on close.

use egui::{RichText, Ui};
use pythia_config::{Settings, ThemeChoice};

#[derive(Default)]
pub struct SettingsDialog {
    pub open: bool,
    section: Section,
    keys: KeysState,
    format: FormatState,
    ai: AiState,
}

/// Where formatter settings are learned from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnFrom {
    /// The focused editor's text (both parts of a program window).
    Editor,
    /// The source of the current schema.
    Schema,
}

#[derive(Default)]
struct FormatState {
    /// Name for a new profile.
    profile_name: String,
    /// Learned settings waiting to be applied, and what they came from.
    learned: Option<(String, pythia_plsql::FormatOptions)>,
    /// The schema's source is being read.
    learning: bool,
    notice: Option<String>,
    /// An export or import file dialog in flight.
    file: Option<(
        std::sync::mpsc::Receiver<Option<std::path::PathBuf>>,
        FileOp,
    )>,
}

enum FileOp {
    Export(String),
    Import,
}

impl FormatState {
    fn export(&mut self, profile: String) {
        self.dialog(FileOp::Export(profile));
    }

    fn import(&mut self) {
        self.dialog(FileOp::Import);
    }

    fn dialog(&mut self, op: FileOp) {
        if self.file.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let name = match &op {
            FileOp::Export(profile) => Some(format!("{profile}.toml")),
            FileOp::Import => None,
        };
        std::thread::spawn(move || {
            let dialog = rfd::FileDialog::new().add_filter("Formatter profile", &["toml"]);
            let path = match name {
                Some(name) => dialog.set_file_name(name).save_file(),
                None => dialog.pick_file(),
            };
            let _ = tx.send(path);
        });
        self.file = Some((rx, op));
    }

    /// Carries out a finished export or import.
    fn poll_file(&mut self, s: &mut Settings) {
        let Some((rx, _)) = &self.file else {
            return;
        };
        let Ok(path) = rx.try_recv() else {
            return;
        };
        let Some((_, op)) = self.file.take() else {
            return;
        };
        let Some(path) = path else {
            return;
        };
        self.notice = Some(match op {
            FileOp::Export(profile) => {
                let options = s
                    .format_profiles
                    .get(&profile)
                    .cloned()
                    .unwrap_or(s.format.clone());
                match pythia_config::save_toml(&path, &options) {
                    Ok(()) => format!("Exported {profile} to {}", path.display()),
                    Err(e) => format!("Could not export: {e:#}"),
                }
            }
            FileOp::Import => {
                let name = path
                    .file_stem()
                    .map_or("imported".into(), |n| n.to_string_lossy().into_owned());
                match pythia_config::load_toml::<pythia_plsql::FormatOptions>(&path) {
                    Ok(options) => {
                        s.format_profiles.insert(name.clone(), options.clone());
                        s.format = options;
                        s.format_profile = Some(name.clone());
                        format!("Imported profile {name}")
                    }
                    Err(e) => format!("Could not import: {e:#}"),
                }
            }
        });
    }
}

#[derive(Default)]
struct AiState {
    /// Index of the backend shown.
    selected: usize,
    /// API key typed for the shown backend, not stored yet.
    key: String,
    /// Whether a key is stored, by backend id (read from the keychain once).
    has_key: std::collections::HashMap<String, bool>,
    /// Test request running: (backend id, job).
    test: Option<(String, crate::ai::Job)>,
    /// Outcome of the last test: (backend id, answer or error).
    tested: Option<(String, Result<String, String>)>,
    /// A keychain problem to show.
    problem: Option<String>,
}

#[derive(Default)]
struct KeysState {
    filter: String,
    /// Waiting for the new shortcut of this command.
    recording: Option<crate::commands::Command>,
    /// What the last change did, e.g. which command lost its shortcut.
    notice: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    #[default]
    Appearance,
    Editor,
    CodeAssist,
    Query,
    Formatter,
    Keyboard,
    Debugger,
    InvalidObjects,
    Ai,
}

impl Section {
    const ALL: [Section; 9] = [
        Section::Appearance,
        Section::Editor,
        Section::CodeAssist,
        Section::Query,
        Section::Formatter,
        Section::Keyboard,
        Section::Debugger,
        Section::InvalidObjects,
        Section::Ai,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Editor => "Editor",
            Section::CodeAssist => "Code assistance",
            Section::Query => "Queries",
            Section::Formatter => "Formatter",
            Section::Keyboard => "Keyboard",
            Section::Debugger => "Debugger",
            Section::InvalidObjects => "Invalid objects",
            Section::Ai => "AI",
        }
    }
}

pub enum SettingsResult {
    /// Nothing changed this frame.
    None,
    /// Learn formatter settings from code; answer with
    /// [`SettingsDialog::learned`].
    Learn(LearnFrom),
    /// Settings changed; apply them (theme, zoom) right away.
    Changed,
    /// The window was closed; persist the settings.
    Closed,
    /// Open the debugger setup.
    DebugSetup,
}

impl SettingsDialog {
    /// Recording a shortcut: the app must not run commands for key presses.
    pub fn is_recording(&self) -> bool {
        self.open && self.keys.recording.is_some()
    }

    #[cfg(test)]
    pub fn show_keyboard(&mut self) {
        self.section = Section::Keyboard;
    }

    pub fn show_ai(&mut self) {
        self.section = Section::Ai;
    }

    #[cfg(test)]
    pub fn show_formatter(&mut self) {
        self.section = Section::Formatter;
    }

    /// Shows the `i`th page; false past the last.
    #[cfg(test)]
    pub fn show_page(&mut self, i: usize) -> bool {
        match Section::ALL.get(i) {
            Some(section) => {
                self.section = *section;
                true
            }
            None => false,
        }
    }

    /// Formatter settings learned from `from` (e.g. "the SQL window"), to
    /// show for applying; `None` if there was nothing to learn from.
    pub fn learned(&mut self, from: String, options: Option<pythia_plsql::FormatOptions>) {
        self.format.learning = false;
        match options {
            Some(options) => {
                self.format.learned = Some((from, options));
                self.format.notice = None;
            }
            None => self.format.notice = Some(format!("Nothing to learn from {from}")),
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, settings: &mut Settings) -> SettingsResult {
        if !self.open {
            return SettingsResult::None;
        }
        let before = settings.clone();
        let mut open = true;
        let mut learn = None;
        let mut debug_setup = false;
        let screen = ctx.content_rect();
        egui::Window::new("Settings")
            // Not the title's id: drops the size saved while a hint on one
            // line widened the window past the screen (#113).
            .id(egui::Id::new("settings-window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([780.0, 600.0])
            // Fits the screen, so the title bar and its close button stay
            // visible: a taller page scrolls (#62), a wider one is cut off
            // (#113).
            .max_height((screen.height() - 48.0).max(240.0))
            .max_width((screen.width() - 48.0).max(320.0))
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(screen.center())
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(150.0);
                        for s in Section::ALL {
                            ui.selectable_value(&mut self.section, s, s.label());
                        }
                    });
                    ui.separator();
                    ui.vertical(|ui| {
                        ui.heading(self.section.label());
                        ui.add_space(8.0);
                        egui::ScrollArea::vertical()
                            .id_salt(("settings-page", self.section.label()))
                            .auto_shrink([false, false])
                            .show(ui, |ui| match self.section {
                                Section::Appearance => appearance(ui, settings),
                                Section::Editor => editor(ui, settings),
                                Section::CodeAssist => code_assist(ui, settings),
                                Section::Query => query(ui, settings),
                                Section::Formatter => {
                                    learn = formatter(ui, settings, &mut self.format);
                                }
                                Section::Keyboard => keyboard(ui, settings, &mut self.keys),
                                Section::Debugger => {
                                    if debugger(ui, settings) {
                                        debug_setup = true;
                                    }
                                }
                                Section::InvalidObjects => invalid_objects(ui, settings),
                                Section::Ai => ai(ui, settings, &mut self.ai),
                            });
                    });
                });
            });
        self.open = open;
        if !open {
            self.keys.recording = None;
        }
        if let Some(from) = learn {
            SettingsResult::Learn(from)
        } else if debug_setup {
            SettingsResult::DebugSetup
        } else if !open {
            SettingsResult::Closed
        } else if *settings != before {
            SettingsResult::Changed
        } else {
            SettingsResult::None
        }
    }
}

fn row(ui: &mut Ui, label: &str, hint: &str, add: impl FnOnce(&mut Ui)) {
    let width = label_width(ui);
    ui.horizontal(|ui| {
        let height = ui.spacing().interact_size.y;
        ui.add_sized([width, height], egui::Label::new(label));
        // Controls that don't fit continue on the next line instead of
        // widening the window (#113).
        ui.horizontal_wrapped(add);
    });
    if !hint.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(width + ui.spacing().item_spacing.x - 2.0);
            // A horizontal row doesn't wrap text by itself (#113).
            ui.add(egui::Label::new(RichText::new(hint).small().weak()).wrap());
        });
    }
    ui.add_space(4.0);
}

/// The labels' column: wider with a bigger interface font.
fn label_width(ui: &Ui) -> f32 {
    let size = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .map_or(13.0, |f| f.size);
    180.0 * size / 13.0
}

/// A group of rows under a title.
fn group(ui: &mut Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).strong());
    ui.add_space(4.0);
}

/// A font file's path with Browse… and a button to clear it.
fn font_file(ui: &mut Ui, path: &mut String, built_in: &str) {
    ui.add(
        egui::TextEdit::singleline(path)
            .hint_text(format!("{built_in} (built in)"))
            .desired_width(260.0),
    );
    if ui.button("Browse…").clicked()
        && let Some(file) = rfd::FileDialog::new()
            .add_filter("Fonts", &["ttf", "otf", "ttc"])
            .pick_file()
    {
        *path = file.display().to_string();
    }
    if !path.is_empty() && ui.small_button("×").clicked() {
        path.clear();
    }
}

/// A color picker; "Default" goes back to `default` (shown while unset).
fn color(ui: &mut Ui, color: &mut Option<[u8; 3]>, default: egui::Color32) {
    let mut rgb = color.unwrap_or([default.r(), default.g(), default.b()]);
    if egui::color_picker::color_edit_button_srgb(ui, &mut rgb).changed() {
        *color = Some(rgb);
    }
    if color.is_some() && ui.small_button("Default").clicked() {
        *color = None;
    }
}

/// Whole points from 0 to `max`.
fn slider(value: &mut f32, max: f32) -> egui::Slider<'_> {
    egui::Slider::new(value, 0.0..=max)
        .step_by(1.0)
        .fixed_decimals(0)
}

fn appearance(ui: &mut Ui, s: &mut Settings) {
    use pythia_config::{Density, ScrollBars};
    row(ui, "Theme", "", |ui| {
        ui.selectable_value(&mut s.theme, ThemeChoice::System, "System");
        ui.selectable_value(&mut s.theme, ThemeChoice::Light, "Light");
        ui.selectable_value(&mut s.theme, ThemeChoice::Dark, "Dark");
    });
    row(
        ui,
        "Interface zoom",
        "Scales everything, text and spacing. Ctrl+Plus / Ctrl+Minus / Ctrl+0",
        |ui| {
            ui.add(
                egui::Slider::new(&mut s.ui_scale, 0.5..=3.0)
                    .step_by(0.05)
                    .fixed_decimals(2),
            );
        },
    );

    let a = &mut s.appearance;
    group(ui, "Interface font");
    row(
        ui,
        "Font",
        "A TrueType/OpenType file for menus, tabs, the object browser and dialogs; empty uses \
         the built-in font",
        |ui| font_file(ui, &mut a.font_file, "Ubuntu Light"),
    );
    row(ui, "Size", "Small print and headings scale with it", |ui| {
        ui.add(egui::Slider::new(&mut a.font_size, 8.0..=24.0).step_by(0.5));
    });

    group(ui, "Spacing");
    let d = &mut a.density;
    row(ui, "Density", "", |ui| {
        for (label, preset) in [
            ("Comfortable", Density::COMFORTABLE),
            ("Compact", Density::COMPACT),
        ] {
            if ui.selectable_label(*d == preset, label).clicked() {
                *d = preset;
            }
        }
        if *d != Density::COMFORTABLE && *d != Density::COMPACT {
            ui.weak("Custom");
        }
    });
    row(
        ui,
        "Padding",
        "Above and below the text of buttons and of list, tree and table rows",
        |ui| {
            ui.add(slider(&mut d.padding, 8.0));
        },
    );
    row(ui, "Gaps", "Between widgets and between table rows", |ui| {
        ui.add(slider(&mut d.spacing, 12.0));
    });
    row(ui, "Margins", "Inside tabs, dialogs and panels", |ui| {
        ui.add(slider(&mut d.margin, 24.0));
    });
    row(ui, "Indent", "Per level of a tree", |ui| {
        ui.add(slider(&mut d.indent, 32.0));
    });

    group(ui, "Look");
    row(
        ui,
        "Accent color",
        "Focus, the text cursor and selections",
        |ui| color(ui, &mut a.accent, crate::theme::ACCENT),
    );
    row(ui, "Rounded corners", "Off: square", |ui| {
        ui.checkbox(&mut a.rounded, "");
    });
    row(ui, "Shadows", "Under dialogs, menus and popups", |ui| {
        ui.checkbox(&mut a.shadows, "");
    });
    row(
        ui,
        "Scroll bars",
        "Floating ones show while the mouse is over a scrolling area",
        |ui| {
            ui.selectable_value(&mut a.scroll_bars, ScrollBars::Floating, "Floating");
            ui.selectable_value(&mut a.scroll_bars, ScrollBars::Visible, "Always visible");
            ui.add(
                egui::Slider::new(&mut a.scroll_bar_width, 2.0..=16.0)
                    .step_by(1.0)
                    .fixed_decimals(0)
                    .text("wide"),
            );
        },
    );
    row(
        ui,
        "Animations",
        "Collapsing sections and scrolling to a target",
        |ui| {
            ui.checkbox(&mut a.animations, "");
        },
    );

    group(ui, "Object browser");
    row(ui, "Names", "", |ui| {
        ui.selectable_value(&mut a.names_in_code_font, true, "Code font");
        ui.selectable_value(&mut a.names_in_code_font, false, "Interface font");
    });

    let g = &mut s.grid;
    group(ui, "Result grid");
    row(ui, "Font", "", |ui| {
        ui.selectable_value(&mut g.interface_font, false, "Code font");
        ui.selectable_value(&mut g.interface_font, true, "Interface font");
    });
    row(ui, "Size", "", |ui| {
        ui.add(egui::Slider::new(&mut g.font_size, 8.0..=24.0).step_by(0.5));
    });
    row(ui, "Striped rows", "", |ui| {
        ui.checkbox(&mut g.striped, "");
    });
    row(ui, "Row lines", "", |ui| {
        ui.checkbox(&mut g.row_lines, "");
    });
    row(ui, "NULL shows as", "", |ui| {
        ui.add(
            egui::TextEdit::singleline(&mut g.null_text)
                .hint_text("(nothing)")
                .desired_width(100.0),
        );
        let weak = ui.visuals().weak_text_color();
        color(ui, &mut g.null_color, weak);
    });
}

fn editor(ui: &mut Ui, s: &mut Settings) {
    use pythia_config::CursorShape;
    let e = &mut s.editor;
    row(ui, "Font size", "", |ui| {
        ui.add(egui::Slider::new(&mut e.font_size, 8.0..=32.0).step_by(0.5));
    });
    row(
        ui,
        "Font",
        "A TrueType/OpenType file; empty uses the built-in Hack",
        |ui| font_file(ui, &mut e.font_file, "Hack"),
    );
    row(ui, "Line spacing", "Extra space between lines", |ui| {
        ui.add(slider(&mut e.line_spacing, 12.0));
    });
    row(ui, "Cursor", "", |ui| {
        ui.selectable_value(&mut e.cursor_shape, CursorShape::Line, "Line");
        ui.selectable_value(&mut e.cursor_shape, CursorShape::Block, "Block");
        ui.selectable_value(&mut e.cursor_shape, CursorShape::Underline, "Underline");
        ui.checkbox(&mut e.cursor_blink, "Blinking");
    });
    row(ui, "Tab width", "Tab inserts spaces", |ui| {
        ui.add(egui::Slider::new(&mut e.tab_width, 1..=8));
    });
    row(ui, "Line numbers", "", |ui| {
        ui.checkbox(&mut e.show_line_numbers, "");
    });
    row(ui, "Highlight current line", "", |ui| {
        ui.checkbox(&mut e.highlight_current_line, "");
    });
    row(ui, "Show whitespace", "Spaces as ·, tabs as →", |ui| {
        ui.checkbox(&mut e.show_whitespace, "");
    });
}

fn code_assist(ui: &mut Ui, s: &mut Settings) {
    let e = &mut s.editor;
    row(
        ui,
        "Complete while typing",
        "Ctrl+Space always opens completions",
        |ui| {
            ui.checkbox(&mut e.auto_complete, "");
        },
    );
    ui.add_enabled_ui(e.auto_complete, |ui| {
        row(
            ui,
            "After characters",
            "Identifier characters typed before suggestions appear",
            |ui| {
                ui.add(egui::Slider::new(&mut e.auto_complete_min_chars, 1..=6));
            },
        );
        row(
            ui,
            "Delay",
            "Pause after typing before suggestions appear",
            |ui| {
                ui.add(egui::Slider::new(&mut e.auto_complete_delay_ms, 0..=1000).suffix(" ms"));
            },
        );
    });
    row(
        ui,
        "Visible rows",
        "Suggestions the completion list shows at once, if the window has room",
        |ui| {
            ui.add(egui::Slider::new(&mut e.completion_rows, 5..=40));
        },
    );
    ui.add_space(6.0);
    ui.label(
        RichText::new("Typing . after a table alias or package always completes immediately.")
            .small()
            .weak(),
    );
}

fn query(ui: &mut Ui, s: &mut Settings) {
    row(
        ui,
        "Rows per fetch",
        "Rows loaded at once; scrolling to the end fetches more",
        |ui| {
            ui.add(
                egui::DragValue::new(&mut s.fetch_size)
                    .range(10..=100_000)
                    .speed(10),
            );
        },
    );
    row(
        ui,
        "Auto-commit",
        "SQL windows commit after each statement. Never on production connections, where \
         Commit asks first. Also Session → Toggle Auto-Commit.",
        |ui| {
            ui.checkbox(&mut s.autocommit, "");
        },
    );
    row(
        ui,
        "Switch connection per window",
        "Each SQL window keeps its own connection, with a picker above it and in the toolbar's \
         connection menu; the object browser shows the focused window's. Off: one connection \
         for the whole app. The toolbar, object browser, new windows and SQL windows use the \
         active connection; picking another (toolbar menu, object browser) or connecting moves \
         the SQL windows there.",
        |ui| {
            ui.checkbox(&mut s.switch_window_connection, "");
        },
    );
}

fn invalid_objects(ui: &mut Ui, s: &mut Settings) {
    use pythia_config::InvalidScope;
    let invalid = &mut s.invalid;
    row(
        ui,
        "Count in",
        "Which schemas the status bar's invalid objects indicator counts",
        |ui| {
            ui.selectable_value(
                &mut invalid.scope,
                InvalidScope::CurrentSchema,
                "Current schema",
            );
            ui.selectable_value(
                &mut invalid.scope,
                InvalidScope::UserSchemas,
                "All user schemas",
            );
        },
    );
    row(
        ui,
        "Refresh every",
        "The count is also refreshed on connect and after compiles and DDL",
        |ui| {
            ui.add(
                egui::DragValue::new(&mut invalid.refresh_minutes)
                    .range(0..=240)
                    .suffix(" min"),
            );
            if invalid.refresh_minutes == 0 {
                ui.weak("off");
            }
        },
    );
    row(
        ui,
        "Sessions when recompiling",
        "Objects compiled at the same time, in dependency order (1: one after the other)",
        |ui| {
            ui.add(egui::DragValue::new(&mut invalid.parallelism).range(1..=16));
        },
    );
}

/// The Debugger page; true when *Check setup…* was clicked.
fn debugger(ui: &mut Ui, s: &mut Settings) -> bool {
    let mut check = false;
    row(
        ui,
        "Debugger host",
        "How the database server reaches this machine (the database connects back to Pythia). \
         Empty: this computer's host name. For a local container: host.containers.internal",
        |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut s.debug_host)
                    .hint_text(pythia_debugger::target::default_host())
                    .desired_width(260.0),
            );
        },
    );
    row(
        ui,
        "Debugger ports",
        "Ports the debugger listens on, e.g. 4000-4009. Empty: any free port. Firewall rules \
         and network ACL entries for a few ports need them fixed",
        |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut s.debug_ports)
                    .hint_text("any free port")
                    .desired_width(120.0),
            );
            if let Err(e) = pythia_debugger::setup::parse_ports(&s.debug_ports) {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
        },
    );
    row(
        ui,
        "Setup",
        "Finds out what the database needs to connect to the debugger (privileges, network \
         ACL, a host it reaches, the firewall) and fixes it",
        |ui| {
            check = ui.button("Check setup…").clicked();
        },
    );
    row(
        ui,
        "Compile with debug information",
        "Program windows compile with debug information, so Step into can enter their units \
         without a breakpoint there. Oracle runs such code less optimized: keep it off for \
         shared and production databases. Units with breakpoints get debug information before \
         a debug run either way.",
        |ui| {
            ui.checkbox(&mut s.compile_debug_info, "");
        },
    );
    row(
        ui,
        "Plan statistics",
        "The debugged session collects row source statistics (STATISTICS_LEVEL = ALL): the \
         SQL panel shows the actual rows of each statement's plan next to the estimates, and \
         statements off by 10× or more are flagged in the gutter. Costs some time per row; \
         applies from the next start.",
        |ui| {
            ui.checkbox(&mut s.debug_plan_statistics, "");
        },
    );
    row(
        ui,
        "Open the SQL panel on a misestimate",
        "The debugger's SQL panel opens by itself on a statement whose estimates are off by 10× \
         or more, the first time it is flagged. Off: the gutter and the toolbar's SQL button \
         flag it.",
        |ui| {
            ui.add_enabled(
                s.debug_plan_statistics,
                egui::Checkbox::without_text(&mut s.debug_open_sql_panel),
            );
        },
    );
    check
}

fn ai(ui: &mut Ui, s: &mut Settings, st: &mut AiState) {
    use pythia_config::{AiBackend, AiKind};
    let ai = &mut s.ai;
    if let Some((id, job)) = &st.test
        && let Some(result) = job.poll()
    {
        st.tested = Some((id.clone(), result));
        st.test = None;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Models").strong());
        ui.add_space(8.0);
        let default = ai.default_backend().map(|b| b.id.clone());
        for (i, b) in ai.backends.iter().enumerate() {
            let label = if default.as_ref() == Some(&b.id) {
                format!("{} (default)", b.name)
            } else {
                b.name.clone()
            };
            if ui
                .selectable_label(st.selected == i, label)
                .on_hover_text(b.kind.label())
                .clicked()
            {
                st.selected = i;
                st.key.clear();
            }
        }
        ui.menu_button("Add…", |ui| {
            for kind in AiKind::ALL {
                if ui.button(kind.label()).clicked() {
                    let mut backend = AiBackend::new(kind, AiBackend::new_id());
                    let base = backend.name.clone();
                    let mut n = 2;
                    while ai.backends.iter().any(|b| b.name == backend.name) {
                        backend.name = format!("{base} {n}");
                        n += 1;
                    }
                    ai.backends.push(backend);
                    st.selected = ai.backends.len() - 1;
                    st.key.clear();
                    ui.close();
                }
            }
        });
    });
    ui.add_space(6.0);
    egui::ScrollArea::vertical()
        .id_salt("ai-backend")
        .max_height(360.0)
        .auto_shrink([false, true])
        .show(ui, |ui| selected_backend(ui, ai, st));
    ui.add_space(12.0);
    ui.label(RichText::new("What is sent").strong());
    ui.label(
        RichText::new(
            "Table and column definitions, keys, indexes, statistics, execution plans, source \
             code and errors go with requests. Data only if allowed here:",
        )
        .small()
        .weak(),
    );
    ui.add_space(4.0);
    row(
        ui,
        "Query results",
        "The first rows of a result shown for the statement",
        |ui| {
            ui.checkbox(&mut ai.send_result_data, "Send");
        },
    );
    row(
        ui,
        "Bind variable values",
        "Values entered for :binds, and binds captured in traces",
        |ui| {
            ui.checkbox(&mut ai.send_bind_values, "Send");
        },
    );
    row(ui, "Wait for answers", "", |ui| {
        ui.add(
            egui::DragValue::new(&mut ai.timeout_secs)
                .range(10..=3600)
                .suffix(" s"),
        );
    });
}

/// The fields and buttons of the backend picked in Settings → AI.
fn selected_backend(ui: &mut Ui, ai: &mut pythia_config::AiSettings, st: &mut AiState) {
    let default_id = ai.default_backend().map(|b| b.id.clone());
    let Some(b) = ai.backends.get_mut(st.selected) else {
        ui.add(
            egui::Label::new(
                "Add the model the assistant uses: a cloud API (Anthropic, or any API \
                     compatible with OpenAI's chat completions), a model server on this \
                     machine or network (Ollama, llama.cpp, LM Studio), or a command line \
                     tool.",
            )
            .wrap(),
        );
        return;
    };
    let mut action = None;
    backend_ui(ui, b, st);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let testing = st.test.as_ref().is_some_and(|(id, _)| *id == b.id);
        if ui
            .add_enabled(!testing, egui::Button::new("Test"))
            .on_hover_text("Send a short request and show the answer")
            .clicked()
        {
            action = Some("test");
        }
        if default_id.as_ref() != Some(&b.id) && ui.button("Make default").clicked() {
            action = Some("default");
        }
        if ui.button("Remove").clicked() {
            action = Some("remove");
        }
    });
    match action {
        Some("test") => {
            if !st.key.trim().is_empty() {
                save_key(b, st);
            }
            let request = pythia_ai::ChatRequest {
                system: String::new(),
                messages: vec![pythia_ai::Message {
                    role: pythia_ai::Role::User,
                    content: "This is a connection test. Reply with the single word OK.".into(),
                }],
            };
            let ctx = ui.ctx().clone();
            let job = crate::ai::spawn(
                b.clone(),
                request,
                std::time::Duration::from_secs(ai.timeout_secs.max(5)),
                move || ctx.request_repaint(),
                None,
            );
            st.test = Some((b.id.clone(), job));
            st.tested = None;
        }
        Some("default") => ai.default_backend = b.id.clone(),
        Some("remove") => {
            crate::secrets::forget_api_key(&b.id);
            ai.backends.remove(st.selected);
            st.selected = st.selected.saturating_sub(1);
            st.key.clear();
            return;
        }
        _ => {}
    }
    if let Some((id, job)) = &st.test
        && *id == b.id
    {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!(
                "Testing… {:.0} s",
                job.started.elapsed().as_secs_f64()
            ));
        });
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    }
    if let Some((id, result)) = &st.tested
        && *id == b.id
    {
        let (color, text) = match result {
            Ok(answer) => (
                crate::theme::SUCCESS,
                format!("Works. The answer: {}", answer.trim()),
            ),
            Err(e) => (ui.visuals().error_fg_color, e.clone()),
        };
        ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
    }
}

/// The fields of one backend.
fn backend_ui(ui: &mut Ui, b: &mut pythia_config::AiBackend, st: &mut AiState) {
    use pythia_config::AiKind;
    row(ui, "Name", b.kind.label(), |ui| {
        ui.add(egui::TextEdit::singleline(&mut b.name).desired_width(260.0));
    });
    if b.kind == AiKind::Cli {
        row(
            ui,
            "Command",
            "The request goes to its standard input, the answer comes from its standard output",
            |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut b.command)
                        .hint_text("e.g. claude -p  or  ollama run llama3.1")
                        .desired_width(320.0),
                );
            },
        );
    } else {
        let hint = match b.kind {
            AiKind::Anthropic => "The Messages API, or a gateway that speaks it",
            AiKind::LocalServer => "llama.cpp's server: http://localhost:8080/v1/chat/completions",
            _ => "The chat completions URL",
        };
        row(ui, "Endpoint", hint, |ui| {
            ui.add(egui::TextEdit::singleline(&mut b.endpoint).desired_width(320.0));
        });
        let model_hint = match b.kind {
            AiKind::LocalServer => "e.g. llama3.1 (ollama list shows the installed ones)",
            AiKind::OpenAi => "The provider's model name",
            _ => "",
        };
        row(ui, "Model", "", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut b.model)
                    .hint_text(model_hint)
                    .desired_width(260.0),
            );
        });
    }
    if b.kind.uses_key() {
        let stored = *st
            .has_key
            .entry(b.id.clone())
            .or_insert_with(|| crate::secrets::load_api_key(&b.id).is_some());
        let hint = if stored {
            "Stored in the system keychain; enter a new one to replace it"
        } else {
            "Kept in the system keychain, never in the settings file"
        };
        let mut save = false;
        let mut remove = false;
        row(ui, "API key", hint, |ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut st.key)
                    .password(true)
                    .hint_text(if stored {
                        "••••••••"
                    } else {
                        ""
                    })
                    .desired_width(260.0),
            );
            save = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui
                .add_enabled(!st.key.trim().is_empty(), egui::Button::new("Save"))
                .clicked()
            {
                save = true;
            }
            if stored && ui.button("Remove").clicked() {
                remove = true;
            }
        });
        if save && !st.key.trim().is_empty() {
            save_key(b, st);
        }
        if remove {
            crate::secrets::forget_api_key(&b.id);
            st.has_key.insert(b.id.clone(), false);
        }
        if let Some(problem) = &st.problem {
            ui.colored_label(ui.visuals().error_fg_color, problem);
        }
    }
    if b.kind != AiKind::Cli {
        let hint = if b.kind == AiKind::Anthropic {
            "Longest answer, in tokens"
        } else {
            "Longest answer, in tokens (0: the server's default)"
        };
        row(ui, "Max answer tokens", hint, |ui| {
            ui.add(
                egui::DragValue::new(&mut b.max_tokens)
                    .range(0..=128_000)
                    .speed(100),
            );
        });
    }
    row(
        ui,
        "Context limit",
        "Characters of context sent at most; small local models need less",
        |ui| {
            ui.add(
                egui::DragValue::new(&mut b.context_chars)
                    .range(2_000..=4_000_000)
                    .speed(1000)
                    .suffix(" chars"),
            );
        },
    );
}

fn save_key(b: &pythia_config::AiBackend, st: &mut AiState) {
    match crate::secrets::store_api_key(&b.id, st.key.trim()) {
        Ok(()) => {
            st.has_key.insert(b.id.clone(), true);
            st.key.clear();
            st.problem = None;
        }
        Err(e) => st.problem = Some(e),
    }
}

const FORMAT_SAMPLE: &str = "select e.employee_id, e.last_name as name, d.department_name as dept from employees e join departments d on d.department_id = e.department_id where e.salary > 1000 and e.hire_date > sysdate - 30;\n\ncreate or replace procedure raise(p_id in number, p_pct in number default 5) is\n  l_old number; l_new_salary number;\nbegin\n  select salary into l_old from employees where employee_id = p_id;\n  l_new_salary := l_old * (1 + p_pct / 100);\n  if l_new_salary > 10000 then raise_application_error(-20001, 'too much'); end if;\nend;\n/\n";

fn formatter(ui: &mut Ui, s: &mut Settings, st: &mut FormatState) -> Option<LearnFrom> {
    use pythia_plsql::{Case, CommaStyle, Wrap};
    st.poll_file(s);
    let mut learn = None;

    // Profiles: named rule sets to switch between and share.
    row(
        ui,
        "Profile",
        "Named rule sets; export one to share a style",
        |ui| {
            let current = s.format_profile.clone();
            let modified = current
                .as_ref()
                .and_then(|n| s.format_profiles.get(n))
                .is_some_and(|p| *p != s.format);
            let label = match &current {
                Some(name) if modified => format!("{name} (modified)"),
                Some(name) => name.clone(),
                None => "(not saved)".into(),
            };
            egui::ComboBox::from_id_salt("format-profile")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    for (name, options) in &s.format_profiles {
                        if ui
                            .selectable_label(current.as_ref() == Some(name), name)
                            .clicked()
                        {
                            s.format = options.clone();
                            s.format_profile = Some(name.clone());
                        }
                    }
                    if s.format_profiles.is_empty() {
                        ui.weak("No profiles yet");
                    }
                });
            if let Some(name) = current.clone().filter(|_| modified)
                && ui.button("Save").clicked()
            {
                s.format_profiles.insert(name, s.format.clone());
            }
            if current.is_some()
                && ui.button("Delete").clicked()
                && let Some(name) = s.format_profile.take()
            {
                s.format_profiles.remove(&name);
            }
            if let Some(name) = current
                && ui.button("Export…").clicked()
            {
                st.export(name);
            }
            if ui.button("Import…").clicked() {
                st.import();
            }
        },
    );
    let width = label_width(ui);
    ui.horizontal(|ui| {
        let height = ui.spacing().interact_size.y;
        let label = ui.add_sized([width, height], egui::Label::new("New profile"));
        ui.add(
            egui::TextEdit::singleline(&mut st.profile_name)
                .hint_text("name")
                .desired_width(180.0),
        )
        .labelled_by(label.id);
        let name = st.profile_name.trim().to_owned();
        if ui
            .add_enabled(!name.is_empty(), egui::Button::new("Save as profile"))
            .clicked()
        {
            s.format_profiles.insert(name.clone(), s.format.clone());
            s.format_profile = Some(name);
            st.profile_name.clear();
        }
    });
    ui.add_space(4.0);
    row(
        ui,
        "Learn from",
        "Settings that match how existing code is written; shown before they apply",
        |ui| {
            if ui.button("Focused editor").clicked() {
                learn = Some(LearnFrom::Editor);
            }
            if ui.button("Current schema").clicked() {
                learn = Some(LearnFrom::Schema);
                st.learning = true;
            }
            if st.learning {
                ui.spinner();
            }
        },
    );
    if let Some(notice) = &st.notice {
        ui.weak(notice);
    }
    let mut learned_done = None;
    if let Some((from, learned)) = &st.learned {
        let changes = differences(&s.format, learned);
        egui::Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .corner_radius(6.0)
            .inner_margin(8.0)
            .show(ui, |ui| {
                if changes.is_empty() {
                    ui.label(format!("Learned from {from}: the settings already match."));
                } else {
                    ui.label(format!("Learned from {from}:"));
                    for change in &changes {
                        ui.label(RichText::new(format!("  {change}")).monospace());
                    }
                    ui.weak("The preview below uses them.");
                }
                ui.horizontal(|ui| {
                    if !changes.is_empty() && ui.button("Apply").clicked() {
                        learned_done = Some(true);
                    }
                    if ui.button("Discard").clicked() {
                        learned_done = Some(false);
                    }
                });
            });
    }
    if let Some(apply) = learned_done
        && let Some((_, learned)) = st.learned.take()
        && apply
    {
        s.format = learned;
    }
    ui.add_space(4.0);

    let f = &mut s.format;
    let case = |ui: &mut Ui, value: &mut Case| {
        ui.selectable_value(value, Case::Lower, "lower");
        ui.selectable_value(value, Case::Upper, "UPPER");
        ui.selectable_value(value, Case::Preserve, "as written");
    };
    let wrap = |ui: &mut Ui, value: &mut Wrap| {
        ui.selectable_value(value, Wrap::Fit, "on one line if it fits");
        ui.selectable_value(value, Wrap::Always, "one per line");
    };
    row(ui, "Keywords", "", |ui| case(ui, &mut f.keyword_case));
    row(
        ui,
        "Identifiers",
        "Quoted identifiers are never changed",
        |ui| case(ui, &mut f.identifier_case),
    );
    row(ui, "Indent", "", |ui| {
        ui.add(egui::Slider::new(&mut f.indent, 1..=8).suffix(" spaces"));
    });
    row(ui, "Line width", "", |ui| {
        ui.add(egui::Slider::new(&mut f.max_width, 60..=200));
    });
    row(ui, "Commas", "", |ui| {
        ui.selectable_value(&mut f.comma, CommaStyle::Trailing, "a,  b");
        ui.selectable_value(&mut f.comma, CommaStyle::Leading, ", b");
    });
    row(ui, "Align", "", |ui| {
        ui.checkbox(&mut f.align_declarations, "declarations");
        ui.checkbox(&mut f.align_assignments, ":=");
        ui.checkbox(&mut f.align_aliases, "aliases");
    });
    row(ui, "JOIN … ON", "", |ui| {
        ui.checkbox(&mut f.on_new_line, "ON on its own line");
    });
    row(ui, "CASE arms", "WHEN / ELSE of CASE expressions", |ui| {
        wrap(ui, &mut f.case_arms)
    });
    row(
        ui,
        "Subqueries",
        "Subqueries, inline views and WITH queries in parentheses",
        |ui| wrap(ui, &mut f.subqueries),
    );
    row(
        ui,
        "Parameters",
        "Of procedures, functions and cursors",
        |ui| wrap(ui, &mut f.parameters),
    );
    row(ui, "INSERT lists", "Column and VALUES lists", |ui| {
        wrap(ui, &mut f.insert_lists)
    });
    row(ui, "IF / LOOP", "", |ui| {
        ui.checkbox(&mut f.then_on_new_line, "THEN and LOOP on their own line");
    });
    let auto = &mut s.format_auto;
    row(
        ui,
        "Format automatically",
        "Only what changes is replaced, as one undo step",
        |ui| {
            ui.checkbox(&mut auto.on_save, "on save and compile");
            ui.checkbox(&mut auto.on_paste, "on paste");
            ui.checkbox(&mut auto.on_statement_end, "when a statement's ; is typed");
        },
    );
    ui.add_space(6.0);
    ui.label(RichText::new("Preview").strong());
    let shown = st.learned.as_ref().map_or(&s.format, |(_, o)| o);
    let preview = pythia_plsql::format(FORMAT_SAMPLE, shown).text;
    egui::Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .corner_radius(6.0)
        .inner_margin(8.0)
        .show(ui, |ui| {
            egui::ScrollArea::both().max_height(360.0).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(preview).monospace()).extend());
            });
        });
    learn
}

/// The settings `learned` changes, as "Indent: 2 → 4" lines.
fn differences(
    current: &pythia_plsql::FormatOptions,
    learned: &pythia_plsql::FormatOptions,
) -> Vec<String> {
    use pythia_plsql::{Case, CommaStyle, Wrap};
    let case = |c: Case| match c {
        Case::Lower => "lower",
        Case::Upper => "UPPER",
        Case::Preserve => "as written",
    };
    let wrap = |w: Wrap| match w {
        Wrap::Fit => "on one line if it fits",
        Wrap::Always => "one per line",
    };
    let comma = |c: CommaStyle| match c {
        CommaStyle::Trailing => "trailing",
        CommaStyle::Leading => "leading",
    };
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    let (a, b) = (current, learned);
    let mut out = Vec::new();
    let mut add = |name: &str, from: String, to: String| {
        if from != to {
            out.push(format!("{name}: {from} → {to}"));
        }
    };
    add(
        "Keywords",
        case(a.keyword_case).into(),
        case(b.keyword_case).into(),
    );
    add(
        "Identifiers",
        case(a.identifier_case).into(),
        case(b.identifier_case).into(),
    );
    add("Indent", a.indent.to_string(), b.indent.to_string());
    add(
        "Line width",
        a.max_width.to_string(),
        b.max_width.to_string(),
    );
    add("Commas", comma(a.comma).into(), comma(b.comma).into());
    add(
        "Align declarations",
        yes_no(a.align_declarations).into(),
        yes_no(b.align_declarations).into(),
    );
    add(
        "Align :=",
        yes_no(a.align_assignments).into(),
        yes_no(b.align_assignments).into(),
    );
    add(
        "Align aliases",
        yes_no(a.align_aliases).into(),
        yes_no(b.align_aliases).into(),
    );
    add(
        "ON on its own line",
        yes_no(a.on_new_line).into(),
        yes_no(b.on_new_line).into(),
    );
    add(
        "Blank lines kept",
        a.blank_lines.to_string(),
        b.blank_lines.to_string(),
    );
    add(
        "CASE arms",
        wrap(a.case_arms).into(),
        wrap(b.case_arms).into(),
    );
    add(
        "Subqueries",
        wrap(a.subqueries).into(),
        wrap(b.subqueries).into(),
    );
    add(
        "Parameters",
        wrap(a.parameters).into(),
        wrap(b.parameters).into(),
    );
    add(
        "INSERT lists",
        wrap(a.insert_lists).into(),
        wrap(b.insert_lists).into(),
    );
    add(
        "THEN / LOOP on own line",
        yes_no(a.then_on_new_line).into(),
        yes_no(b.then_on_new_line).into(),
    );
    out
}

fn keyboard(ui: &mut Ui, s: &mut Settings, st: &mut KeysState) {
    use crate::commands::{Command, Keymap, shortcut_string};
    let preset = Keymap::new(s.keymap);
    let current = Keymap::with_overrides(s.keymap, &s.keybindings);

    if let Some(command) = st.recording {
        // The next key press (with modifiers) becomes the shortcut.
        let pressed = ui.ctx().input_mut(|i| {
            let found = i.events.iter().find_map(|e| match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => Some((*key, *modifiers)),
                _ => None,
            });
            if let Some((key, modifiers)) = found {
                i.consume_key(modifiers, key);
            }
            found
        });
        match pressed {
            Some((egui::Key::Escape, m)) if m.is_none() => st.recording = None,
            Some((key, m)) => {
                let mut modifiers = egui::Modifiers::NONE;
                if m.command || m.ctrl || m.mac_cmd {
                    modifiers |= egui::Modifiers::COMMAND;
                }
                if m.shift {
                    modifiers |= egui::Modifiers::SHIFT;
                }
                if m.alt {
                    modifiers |= egui::Modifiers::ALT;
                }
                let shortcut = egui::KeyboardShortcut::new(modifiers, key);
                st.notice = None;
                if let Some(other) = current.command_for(&shortcut).filter(|c| *c != command) {
                    s.keybindings.insert(other.id().to_owned(), String::new());
                    st.notice = Some(format!(
                        "{} was taken from \"{}\"",
                        shortcut_string(&shortcut),
                        other.label()
                    ));
                }
                if preset.shortcut(command) == Some(&shortcut) {
                    s.keybindings.remove(command.id());
                } else {
                    s.keybindings
                        .insert(command.id().to_owned(), shortcut_string(&shortcut));
                }
                st.recording = None;
            }
            None => {}
        }
    }

    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut st.filter)
                .hint_text("Filter commands or shortcuts…")
                .desired_width(260.0),
        );
        if !s.keybindings.is_empty()
            && ui
                .button("Reset all")
                .on_hover_text("Back to the PL/SQL Developer compatible preset")
                .clicked()
        {
            s.keybindings.clear();
            st.notice = None;
        }
    });
    if let Some(notice) = &st.notice {
        ui.label(
            RichText::new(notice)
                .small()
                .color(ui.visuals().warn_fg_color),
        );
    }
    ui.add_space(4.0);
    let needle = st.filter.trim().to_lowercase();
    let current = Keymap::with_overrides(s.keymap, &s.keybindings);
    egui::ScrollArea::vertical()
        .max_height(440.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Grid::new("keybindings")
                .num_columns(3)
                .striped(true)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    for &command in Command::ALL {
                        let text = current
                            .shortcut(command)
                            .map(shortcut_string)
                            .unwrap_or_default();
                        if !needle.is_empty()
                            && !command.label().to_lowercase().contains(&needle)
                            && !text.to_lowercase().contains(&needle)
                        {
                            continue;
                        }
                        let changed = s.keybindings.contains_key(command.id());
                        let mut label = RichText::new(command.label());
                        if changed {
                            label = label.strong();
                        }
                        ui.label(label);
                        let recording = st.recording == Some(command);
                        let button = if recording {
                            egui::Button::new(
                                RichText::new("Press keys… (Esc cancels)")
                                    .color(ui.visuals().hyperlink_color),
                            )
                        } else if text.is_empty() {
                            egui::Button::new(RichText::new("—").weak())
                        } else {
                            egui::Button::new(RichText::new(&text).monospace())
                        };
                        if ui
                            .add_sized([200.0, 20.0], button)
                            .on_hover_text("Click, then press the new shortcut")
                            .clicked()
                        {
                            st.recording = (!recording).then_some(command);
                        }
                        ui.horizontal(|ui| {
                            if !text.is_empty()
                                && ui.small_button("×").on_hover_text("No shortcut").clicked()
                            {
                                s.keybindings.insert(command.id().to_owned(), String::new());
                            }
                            if changed
                                && ui
                                    .small_button("Reset")
                                    .on_hover_text(match preset.shortcut(command) {
                                        Some(d) => format!("Default: {}", shortcut_string(d)),
                                        None => "Default: none".into(),
                                    })
                                    .clicked()
                            {
                                s.keybindings.remove(command.id());
                            }
                        });
                        ui.end_row();
                    }
                });
        });
    ui.add_space(6.0);
    ui.label(
        RichText::new("Editor keys (folding, bookmarks, multi-cursor) are fixed.")
            .small()
            .weak(),
    );
}
