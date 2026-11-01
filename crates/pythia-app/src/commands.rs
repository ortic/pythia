//! All user actions as commands.
//!
//! Menus, toolbar, keyboard shortcuts and the command palette all dispatch a
//! [`Command`], so every action is reachable from each of them (and from
//! macros later on).

use egui::{Key, KeyboardShortcut, Modifiers};
use pythia_config::KeymapPreset;
use pythia_db::{Dialect, Feature};

macro_rules! commands {
    ($($variant:ident => $label:literal,)*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Command {
            $($variant,)*
        }

        impl Command {
            pub const ALL: &[Command] = &[$(Command::$variant,)*];

            pub fn label(self) -> &'static str {
                match self {
                    $(Command::$variant => $label,)*
                }
            }

            /// Stable name for settings files.
            pub fn id(self) -> &'static str {
                match self {
                    $(Command::$variant => stringify!($variant),)*
                }
            }

            pub fn from_id(id: &str) -> Option<Command> {
                Command::ALL.iter().copied().find(|c| c.id() == id)
            }
        }
    };
}

commands! {
    NewSqlWindow => "File: New SQL Window",
    NewTable => "New: Table…",
    NewView => "New: View…",
    NewMaterializedView => "New: Materialized View…",
    NewSequence => "New: Sequence…",
    NewSynonym => "New: Synonym…",
    NewIndex => "New: Index…",
    NewTrigger => "New: Trigger…",
    NewProcedure => "New: Procedure…",
    NewFunction => "New: Function…",
    NewPackage => "New: Package…",
    NewType => "New: Type…",
    NewJob => "New: Scheduler Job…",
    NewDatabaseLink => "New: Database Link…",
    NewUser => "New: User…",
    NewRole => "New: Role…",
    NewProfile => "New: Profile…",
    NewDirectory => "New: Directory…",
    OpenFile => "File: Open…",
    SaveFile => "File: Save",
    SaveFileAs => "File: Save As…",
    CloseTab => "File: Close Window",
    CloseOtherTabs => "File: Close Other Windows",
    ReopenClosed => "File: Reopen Closed Window",
    Settings => "File: Settings…",
    Connect => "Session: Connect…",
    Disconnect => "Session: Disconnect",
    ExecuteAll => "Session: Execute (selection or all)",
    ExecuteCurrent => "Session: Execute Current Statement",
    ExplainPlan => "Session: Explain Plan",
    ExecuteWithStatistics => "Session: Run with Statistics (actual plan)",
    ExecuteToFile => "Session: Execute Query to File…",
    Profile => "Session: Profile PL/SQL (DBMS_HPROF)",
    SaveAsReport => "Reports: Save Query as Report…",
    Cancel => "Session: Break (cancel execution)",
    Commit => "Session: Commit",
    Rollback => "Session: Rollback",
    ToggleAutocommit => "Session: Toggle Auto-Commit",
    History => "Edit: Recall Statement (history)",
    Format => "Edit: Format Code",
    RenameSymbol => "Edit: Rename Symbol",
    GoToDefinition => "Navigate: Go to Definition",
    PeekDefinition => "Navigate: Peek Definition",
    FindReferences => "Navigate: Find References",
    GoToSymbol => "Navigate: Go to Symbol in File…",
    SwitchSpecBody => "Navigate: Switch Spec/Body",
    NavigateBack => "Navigate: Back",
    NavigateForward => "Navigate: Forward",
    FoldAll => "Edit: Fold All",
    UnfoldAll => "Edit: Unfold All",
    FindObject => "Tools: Find Database Object…",
    FindInSource => "Tools: Find in Database Source…",
    SearchData => "Tools: Search Data in Tables…",
    SchedulerJobs => "Tools: Scheduler Jobs",
    InvalidObjects => "Tools: Invalid Objects",
    MaintenanceScan => "Tools: Maintenance Scan",
    ImportData => "Tools: Import Data (CSV, Excel)…",
    ExportTables => "Tools: Export Tables…",
    CompareSchemas => "Tools: Compare Schemas…",
    CompareData => "Tools: Compare Table Data…",
    DebugStart => "Debug: Debug Block Under Cursor",
    ToggleBreakpoint => "Debug: Toggle Breakpoint",
    DebugContinue => "Debug: Continue",
    DebugStepOver => "Debug: Step Over",
    DebugStepInto => "Debug: Step Into",
    DebugStepOut => "Debug: Step Out",
    DebugRunToCursor => "Debug: Run to Cursor",
    DebugStop => "Debug: Stop",
    DebugSetup => "Debug: Debugger Setup…",
    ShowDebugger => "View: Debugger",
    MonitorDashboard => "Monitor: Dashboard",
    MonitorSessions => "Monitor: Sessions",
    MonitorBlocking => "Monitor: Blocking Sessions",
    MonitorAwr => "Monitor: AWR / ASH Reports",
    MonitorTrace => "Monitor: Trace Analyzer (tkprof)",
    AiAsk => "AI: Ask…",
    AiGenerateSql => "AI: Generate SQL…",
    AiExplain => "AI: Explain Statement",
    AiFix => "AI: Fix Statement",
    AiRewrite => "AI: Rewrite Statement",
    AiAnalyzePlan => "AI: Analyze Execution Plan",
    AiFormat => "AI: Format Unsupported Statements",
    ShowAssistant => "View: AI Assistant",
    ShowPalette => "View: Command Palette",
    ShowBrowser => "View: Object Browser",
    ShowProject => "View: Project",
    ShowLog => "View: Log",
    ShowVariables => "View: Variables (SQL window)",
    ThemeDark => "View: Dark Theme",
    ThemeLight => "View: Light Theme",
    ThemeSystem => "View: System Theme",
    ZoomIn => "View: Zoom In",
    ZoomOut => "View: Zoom Out",
    ZoomReset => "View: Reset Zoom",
    ResetLayout => "View: Reset Layout",
    About => "Help: About Pythia",
    Quit => "File: Exit",
}

impl Command {
    /// The commands creating a database object, with its type.
    pub const NEW_OBJECTS: &[(Command, pythia_db::ObjectType)] = &[
        (Command::NewTable, pythia_db::ObjectType::Table),
        (Command::NewView, pythia_db::ObjectType::View),
        (
            Command::NewMaterializedView,
            pythia_db::ObjectType::MaterializedView,
        ),
        (Command::NewSequence, pythia_db::ObjectType::Sequence),
        (Command::NewSynonym, pythia_db::ObjectType::Synonym),
        (Command::NewIndex, pythia_db::ObjectType::Index),
        (Command::NewTrigger, pythia_db::ObjectType::Trigger),
        (Command::NewProcedure, pythia_db::ObjectType::Procedure),
        (Command::NewFunction, pythia_db::ObjectType::Function),
        (Command::NewPackage, pythia_db::ObjectType::Package),
        (Command::NewType, pythia_db::ObjectType::Type),
        (Command::NewJob, pythia_db::ObjectType::Job),
        (
            Command::NewDatabaseLink,
            pythia_db::ObjectType::DatabaseLink,
        ),
        (Command::NewUser, pythia_db::ObjectType::User),
        (Command::NewRole, pythia_db::ObjectType::Role),
        (Command::NewProfile, pythia_db::ObjectType::Profile),
        (Command::NewDirectory, pythia_db::ObjectType::Directory),
    ];

    /// Label without the category prefix, for menus.
    pub fn short_label(self) -> &'static str {
        let label = self.label();
        label.split_once(": ").map_or(label, |(_, rest)| rest)
    }

    /// What the connection's database must support for the command to be
    /// offered (see [`Dialect::supports`]).
    pub fn feature(self) -> Option<Feature> {
        use Command::*;
        Some(match self {
            ExplainPlan => Feature::ExplainPlan,
            ExecuteWithStatistics => Feature::Statistics,
            ExecuteToFile => Feature::ExecuteToFile,
            Profile => Feature::Profiler,
            SaveAsReport => Feature::Reports,
            FindInSource => Feature::SourceSearch,
            SearchData => Feature::DataSearch,
            SchedulerJobs => Feature::SchedulerJobs,
            InvalidObjects => Feature::InvalidObjects,
            ImportData => Feature::Import,
            ExportTables => Feature::ExportTables,
            CompareSchemas | CompareData => Feature::Compare,
            SwitchSpecBody => Feature::PlSql,
            DebugStart | ToggleBreakpoint | DebugContinue | DebugStepOver | DebugStepInto
            | DebugStepOut | DebugRunToCursor | DebugStop | DebugSetup | ShowDebugger => {
                Feature::Debugger
            }
            NewTable | NewView | NewMaterializedView | NewSequence | NewSynonym | NewIndex
            | NewTrigger | NewProcedure | NewFunction | NewPackage | NewType | NewJob
            | NewDatabaseLink | NewProfile | NewDirectory => Feature::ObjectEditors,
            NewUser | NewRole => Feature::AccountEditors,
            MonitorDashboard => Feature::Dashboard,
            MonitorSessions | MonitorBlocking => Feature::Sessions,
            MonitorAwr | MonitorTrace => Feature::Monitor,
            AiAnalyzePlan => Feature::ExplainPlan,
            _ => return None,
        })
    }

    /// Whether the command is offered for a connection of `dialect`.
    pub fn available(self, dialect: Dialect) -> bool {
        self.feature().is_none_or(|f| dialect.supports(f))
    }
}

/// Maps commands to keyboard shortcuts.
pub struct Keymap {
    bindings: Vec<(Command, KeyboardShortcut)>,
}

impl Keymap {
    /// The preset with the user's overrides (command id → shortcut text).
    pub fn with_overrides(
        preset: KeymapPreset,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Self {
        let mut keymap = Self::new(preset);
        for (id, text) in overrides {
            let Some(command) = Command::from_id(id) else {
                continue;
            };
            let shortcut = parse_shortcut(text);
            if let Some(s) = shortcut.filter(takes_editing_key) {
                tracing::warn!(
                    "ignoring the shortcut {} of {id}: the editor needs the key",
                    shortcut_string(&s)
                );
                continue;
            }
            keymap.bindings.retain(|(c, _)| *c != command);
            if let Some(shortcut) = shortcut {
                keymap.bindings.push((command, shortcut));
            }
        }
        keymap
    }

    /// The command a shortcut triggers, if any.
    pub fn command_for(&self, shortcut: &KeyboardShortcut) -> Option<Command> {
        self.bindings
            .iter()
            .find(|(_, s)| s == shortcut)
            .map(|(c, _)| *c)
    }

    pub fn new(preset: KeymapPreset) -> Self {
        let ctrl = Modifiers::COMMAND;
        let shift = Modifiers::SHIFT;
        let none = Modifiers::NONE;
        let bindings = match preset {
            KeymapPreset::PlsqlDeveloper => vec![
                (Command::NewSqlWindow, KeyboardShortcut::new(ctrl, Key::N)),
                (Command::OpenFile, KeyboardShortcut::new(ctrl, Key::O)),
                (
                    Command::ReopenClosed,
                    KeyboardShortcut::new(ctrl | shift, Key::T),
                ),
                (Command::DebugStart, KeyboardShortcut::new(ctrl, Key::F9)),
                (
                    Command::ToggleBreakpoint,
                    KeyboardShortcut::new(ctrl, Key::B),
                ),
                (
                    Command::DebugRunToCursor,
                    KeyboardShortcut::new(ctrl, Key::F7),
                ),
                (
                    Command::SaveFileAs,
                    KeyboardShortcut::new(ctrl | shift, Key::S),
                ),
                (Command::SaveFile, KeyboardShortcut::new(ctrl, Key::S)),
                (Command::CloseTab, KeyboardShortcut::new(ctrl, Key::W)),
                (Command::Settings, KeyboardShortcut::new(ctrl, Key::Comma)),
                (Command::ExecuteAll, KeyboardShortcut::new(none, Key::F8)),
                (
                    Command::ExecuteCurrent,
                    KeyboardShortcut::new(ctrl, Key::Enter),
                ),
                (Command::ExplainPlan, KeyboardShortcut::new(none, Key::F5)),
                (
                    Command::ExecuteWithStatistics,
                    KeyboardShortcut::new(ctrl, Key::F5),
                ),
                (Command::Cancel, KeyboardShortcut::new(shift, Key::Escape)),
                (Command::Commit, KeyboardShortcut::new(none, Key::F10)),
                (Command::Rollback, KeyboardShortcut::new(shift, Key::F10)),
                (Command::History, KeyboardShortcut::new(ctrl, Key::E)),
                (Command::Format, KeyboardShortcut::new(ctrl | shift, Key::F)),
                (Command::RenameSymbol, KeyboardShortcut::new(shift, Key::F6)),
                (
                    Command::GoToDefinition,
                    KeyboardShortcut::new(none, Key::F12),
                ),
                (
                    Command::PeekDefinition,
                    KeyboardShortcut::new(Modifiers::ALT, Key::F12),
                ),
                (
                    Command::FindReferences,
                    KeyboardShortcut::new(shift, Key::F12),
                ),
                (Command::GoToSymbol, KeyboardShortcut::new(ctrl, Key::F12)),
                (
                    Command::SwitchSpecBody,
                    KeyboardShortcut::new(ctrl | shift, Key::B),
                ),
                (
                    Command::NavigateBack,
                    KeyboardShortcut::new(Modifiers::ALT, Key::ArrowLeft),
                ),
                (
                    Command::NavigateForward,
                    KeyboardShortcut::new(Modifiers::ALT, Key::ArrowRight),
                ),
                (
                    Command::FindObject,
                    KeyboardShortcut::new(ctrl | shift, Key::O),
                ),
                (
                    Command::FindInSource,
                    KeyboardShortcut::new(ctrl | Modifiers::ALT, Key::F),
                ),
                (
                    Command::ShowPalette,
                    KeyboardShortcut::new(ctrl | shift, Key::P),
                ),
                (Command::ZoomIn, KeyboardShortcut::new(ctrl, Key::Plus)),
                (Command::ZoomIn, KeyboardShortcut::new(ctrl, Key::Equals)),
                (Command::ZoomOut, KeyboardShortcut::new(ctrl, Key::Minus)),
                (Command::ZoomReset, KeyboardShortcut::new(ctrl, Key::Num0)),
            ],
        };
        Self { bindings }
    }

    pub fn shortcut(&self, command: Command) -> Option<&KeyboardShortcut> {
        self.bindings
            .iter()
            .find(|(c, _)| *c == command)
            .map(|(_, s)| s)
    }

    /// Formatted shortcut for menus and tooltips (empty if none).
    pub fn shortcut_text(&self, ctx: &egui::Context, command: Command) -> String {
        self.shortcut(command)
            .map(|s| ctx.format_shortcut(s))
            .unwrap_or_default()
    }

    /// Consumes pressed shortcuts and returns the triggered commands.
    pub fn triggered(&self, ctx: &egui::Context) -> Vec<Command> {
        // Most specific shortcuts first, so Shift+F10 isn't eaten by F10.
        let mut bindings: Vec<_> = self.bindings.iter().collect();
        bindings.sort_by_key(|(_, s)| std::cmp::Reverse(modifier_count(s.modifiers)));
        ctx.input_mut(|input| {
            bindings
                .into_iter()
                .filter(|(_, s)| input.consume_shortcut(s))
                .map(|(c, _)| *c)
                .collect()
        })
    }
}

/// Whether a shortcut would take a key that typing and editing text need
/// (#191): commands get their keys before the editor and the grid do.
/// Those are, alone or with Shift, what types text, Enter, Tab, Delete,
/// Backspace, Insert and the moves (Escape alone); with Ctrl, the moves,
/// Delete and Backspace, and A, C, V, X, Y, Z (select all, the clipboard,
/// undo). Function keys, Alt and other Ctrl shortcuts are free.
pub fn takes_editing_key(s: &KeyboardShortcut) -> bool {
    let m = s.modifiers;
    if m.alt {
        return false;
    }
    let key = s.logical_key;
    if m.command || m.ctrl || m.mac_cmd {
        return matches!(
            key,
            Key::ArrowUp
                | Key::ArrowDown
                | Key::ArrowLeft
                | Key::ArrowRight
                | Key::Home
                | Key::End
                | Key::PageUp
                | Key::PageDown
                | Key::Delete
                | Key::Backspace
                | Key::A
                | Key::C
                | Key::V
                | Key::X
                | Key::Y
                | Key::Z
        );
    }
    if key == Key::Escape {
        return !m.shift;
    }
    let name = key.name();
    let function_key =
        name.len() > 1 && name.starts_with('F') && name[1..].chars().all(|c| c.is_ascii_digit());
    !function_key
}

/// Parses `Ctrl+Shift+F5`, `Alt+X`, `Shift+Esc` (case-insensitive; `Cmd`
/// and `Ctrl` both mean the platform's command key).
pub fn parse_shortcut(text: &str) -> Option<KeyboardShortcut> {
    let mut modifiers = Modifiers::NONE;
    let mut key = None;
    for part in text.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "cmd" | "command" => modifiers |= Modifiers::COMMAND,
            "shift" => modifiers |= Modifiers::SHIFT,
            "alt" | "option" => modifiers |= Modifiers::ALT,
            "esc" => key = Some(Key::Escape),
            "del" => key = Some(Key::Delete),
            "ins" => key = Some(Key::Insert),
            "pgup" => key = Some(Key::PageUp),
            "pgdn" => key = Some(Key::PageDown),
            _ => {
                key = Some(
                    Key::from_name(part)
                        .or_else(|| Key::from_name(&part.to_ascii_uppercase()))
                        .or_else(|| {
                            let mut c = part.chars();
                            let first = c.next()?.to_ascii_uppercase();
                            Key::from_name(&format!("{first}{}", c.as_str().to_ascii_lowercase()))
                        })?,
                )
            }
        }
    }
    Some(KeyboardShortcut::new(modifiers, key?))
}

/// The settings-file form of a shortcut, e.g. `Ctrl+Shift+F5`.
pub fn shortcut_string(s: &KeyboardShortcut) -> String {
    let mut parts = Vec::new();
    if s.modifiers.command || s.modifiers.ctrl || s.modifiers.mac_cmd {
        parts.push("Ctrl".to_owned());
    }
    if s.modifiers.alt {
        parts.push("Alt".to_owned());
    }
    if s.modifiers.shift {
        parts.push("Shift".to_owned());
    }
    parts.push(s.logical_key.name().to_owned());
    parts.join("+")
}

fn modifier_count(m: Modifiers) -> u32 {
    u32::from(m.alt) + u32::from(m.ctrl || m.command) + u32::from(m.shift) + u32::from(m.mac_cmd)
}

/// Fuzzy subsequence match. Returns a score (higher is better) or `None`.
///
/// Consecutive matches and matches at word starts score higher, so "ex cur"
/// ranks "Execute Current Statement" above "Execute (selection or all)".
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let mut score = 0;
    let mut prev_match: Option<usize> = None;
    let cand: Vec<char> = candidate.chars().collect();
    let mut pos = 0;
    for q in query.chars().filter(|c| !c.is_whitespace()) {
        let q = q.to_ascii_lowercase();
        let matches = |i: &usize| cand[*i].to_ascii_lowercase() == q;
        let word_start = |i: &usize| *i == 0 || !cand[*i - 1].is_alphanumeric();
        // Prefer continuing the current run, then the start of a later word,
        // then any occurrence.
        let found = Some(pos)
            .filter(|i| prev_match.is_some() && *i < cand.len() && matches(i))
            .or_else(|| (pos..cand.len()).find(|i| matches(i) && word_start(i)))
            .or_else(|| (pos..cand.len()).find(matches))?;
        score += 1;
        if prev_match == Some(found.wrapping_sub(1)) {
            score += 5;
        }
        if word_start(&found) {
            score += 3;
        }
        prev_match = Some(found);
        pos = found + 1;
    }
    Some(score - cand.len() as i32 / 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_round_trip() {
        for text in [
            "Ctrl+Shift+F5",
            "Alt+X",
            "Shift+Escape",
            "F8",
            "Ctrl+Enter",
            "Ctrl+0",
        ] {
            let s = parse_shortcut(text).unwrap();
            assert_eq!(parse_shortcut(&shortcut_string(&s)), Some(s), "{text}");
        }
        assert_eq!(
            parse_shortcut("ctrl+shift+f"),
            Some(KeyboardShortcut::new(
                Modifiers::COMMAND | Modifiers::SHIFT,
                Key::F
            ))
        );
        assert_eq!(
            parse_shortcut("shift+esc").map(|s| s.logical_key),
            Some(Key::Escape)
        );
        assert_eq!(parse_shortcut("Ctrl+"), None);
        assert_eq!(parse_shortcut("Ctrl+Nope"), None);
    }

    #[test]
    fn overrides_replace_and_remove() {
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("Format".to_owned(), "Alt+F".to_owned());
        overrides.insert("ExecuteAll".to_owned(), String::new());
        overrides.insert("NoSuchCommand".to_owned(), "F1".to_owned());
        let keymap = Keymap::with_overrides(KeymapPreset::PlsqlDeveloper, &overrides);
        let alt_f = KeyboardShortcut::new(Modifiers::ALT, Key::F);
        assert_eq!(keymap.shortcut(Command::Format), Some(&alt_f));
        assert_eq!(keymap.shortcut(Command::ExecuteAll), None);
        assert_eq!(keymap.command_for(&alt_f), Some(Command::Format));
        assert_eq!(Command::from_id("Format"), Some(Command::Format));
    }

    /// A shortcut can't take a key of typing and editing (#191); the
    /// presets' don't, and one set by hand in the settings is ignored.
    #[test]
    fn editing_keys_stay_the_editors() {
        let ctrl = Modifiers::COMMAND;
        let shift = Modifiers::SHIFT;
        for (modifiers, key) in [
            (Modifiers::NONE, Key::Delete),
            (Modifiers::NONE, Key::Enter),
            (Modifiers::NONE, Key::Tab),
            (Modifiers::NONE, Key::A),
            (Modifiers::NONE, Key::Escape),
            (shift, Key::Delete),
            (shift, Key::ArrowDown),
            (shift, Key::Num1),
            (ctrl, Key::Backspace),
            (ctrl | shift, Key::ArrowLeft),
            (ctrl, Key::C),
            (ctrl | shift, Key::Z),
        ] {
            let s = KeyboardShortcut::new(modifiers, key);
            assert!(takes_editing_key(&s), "{}", shortcut_string(&s));
        }
        for (modifiers, key) in [
            (Modifiers::NONE, Key::F8),
            (shift, Key::F10),
            (shift, Key::Escape),
            (ctrl, Key::Enter),
            (ctrl, Key::N),
            (ctrl | shift, Key::F),
            (Modifiers::ALT, Key::ArrowLeft),
            (Modifiers::ALT, Key::Delete),
        ] {
            let s = KeyboardShortcut::new(modifiers, key);
            assert!(!takes_editing_key(&s), "{}", shortcut_string(&s));
        }
        for (command, s) in &Keymap::new(KeymapPreset::PlsqlDeveloper).bindings {
            assert!(!takes_editing_key(s), "{command:?}: {}", shortcut_string(s));
        }
        let overrides =
            std::collections::BTreeMap::from([("NewSqlWindow".to_owned(), "Delete".to_owned())]);
        let keymap = Keymap::with_overrides(KeymapPreset::PlsqlDeveloper, &overrides);
        assert_eq!(
            keymap.shortcut(Command::NewSqlWindow),
            Keymap::new(KeymapPreset::PlsqlDeveloper).shortcut(Command::NewSqlWindow)
        );
    }

    #[test]
    fn fuzzy() {
        assert!(fuzzy_score("xyz", "Session: Commit").is_none());
        assert!(fuzzy_score("", "anything").is_some());
        let current = fuzzy_score("ex cur", Command::ExecuteCurrent.label()).unwrap();
        let all = fuzzy_score("ex cur", Command::ExecuteAll.label());
        assert!(all.is_none_or(|all| current > all));
    }

    #[test]
    fn new_object_commands() {
        let news: Vec<Command> = Command::ALL
            .iter()
            .copied()
            .filter(|c| c.id().starts_with("New") && *c != Command::NewSqlWindow)
            .collect();
        assert_eq!(news.len(), Command::NEW_OBJECTS.len());
        for (command, t) in Command::NEW_OBJECTS {
            assert!(news.contains(command));
            assert!(crate::object_editor::can_create(*t), "{t:?}");
        }
    }

    #[test]
    fn labels_unique() {
        let mut labels: Vec<_> = Command::ALL.iter().map(|c| c.label()).collect();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), Command::ALL.len());
    }
}
