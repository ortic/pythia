//! Result grid for query results.
//!
//! Rows are virtualized (only visible rows are laid out), so scrolling through
//! millions of fetched rows stays cheap. Supports client-side sorting,
//! rectangular selection with mouse and keyboard, copy (plain, with header or
//! in any export format), aggregates of the selection, a transposed record
//! view and a viewer for long values. Columns can be resized, hidden,
//! reordered by dragging their header, and frozen at the left.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;

use egui::{Align, Align2, Key, Layout, RichText, Sense, Ui};
use egui_extras::{Column as TableColumn, TableBuilder};
use pythia_data::Format;
use pythia_db::{Cell, Column};

/// Per-result view state, owned by whoever owns the rows.
#[derive(Default)]
pub struct GridState {
    sort: Option<(usize, bool)>,
    /// View order: indices into `rows` (identity when unsorted).
    order: Vec<usize>,
    /// Row count `order` was computed for.
    ordered_rows: usize,
    /// Selection anchor and cursor as (view row, column).
    anchor: Option<(usize, usize)>,
    cursor: Option<(usize, usize)>,
    dragging: bool,
    pub record_view: bool,
    /// Rows must contain this text (case-insensitive) in some cell.
    filter: String,
    viewer: Option<(usize, usize)>,
    scroll_to_cursor: bool,
    edit: Option<Editing>,
    /// Related rows reachable from a row (foreign keys), offered in the
    /// context menu.
    pub links: Vec<GridLink>,
    /// A link was chosen: (index into `links`, row index into `rows`).
    follow: Option<(usize, usize)>,
    /// Columns hidden by the user; left out of copies and exports too.
    pub hidden: BTreeSet<usize>,
    /// The columns in the order shown (hidden ones included); empty: as
    /// queried.
    column_order: Vec<usize>,
    /// Leading shown columns that stay in place when the others scroll
    /// sideways.
    frozen: usize,
    /// Column widths as last drawn (or dragged), by column.
    widths: std::collections::HashMap<usize, f32>,
    /// The tables drop their stored widths next frame and take `widths`
    /// (after columns moved, were shown, hidden or frozen).
    relayout: bool,
    /// The widths were fitted with rows to measure (not just the headers).
    fitted_to_rows: bool,
    /// Vertical scroll shared by the frozen and the scrolling part.
    scroll_y: f32,
    /// The frozen part was scrolled: the other one follows next frame.
    follow_y: Option<f32>,
    /// Width of the frozen part as laid out last frame.
    frozen_width: Option<f32>,
    /// Cell edited in the dialog (long text, dates).
    value_editor: Option<ValueEditor>,
    /// The database the rows come from: *Copy as* INSERT writes its SQL.
    pub dialect: pythia_db::Dialect,
    /// The table the rows come from, as the query names it (one table
    /// only): INSERT statements go into it (#158).
    pub table: Option<String>,
}

/// A header being dragged to another place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ColumnDrag {
    grid: egui::Id,
    col: usize,
}

/// A change to the columns chosen in the header or its menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnAction {
    Hide(usize),
    Show(usize),
    ShowAll,
    /// Move a column next to another one.
    Move {
        col: usize,
        to: usize,
        after: bool,
    },
    /// Keep this many shown columns in place.
    Freeze(usize),
    ResetOrder,
    ResetWidths,
}

struct ValueEditor {
    view_row: usize,
    col: usize,
    text: String,
}

/// Rows related to a row, e.g. its parent through a foreign key.
#[derive(Debug, Clone, PartialEq)]
pub struct GridLink {
    pub label: String,
    /// Child rows (one to many) rather than a parent row.
    pub many: bool,
    /// Result columns whose values identify the related rows.
    pub columns: Vec<usize>,
}

/// Pending edits of an editable result, keyed by row index into `rows`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Changes {
    /// Changed cells of existing rows: row → column → new value.
    pub updates: BTreeMap<usize, BTreeMap<usize, Cell>>,
    /// New rows (one value per column).
    pub inserts: Vec<Vec<Cell>>,
    /// Existing rows to delete.
    pub deletes: BTreeSet<usize>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.inserts.is_empty() && self.deletes.is_empty()
    }

    pub fn count(&self) -> usize {
        self.updates.len() + self.inserts.len() + self.deletes.len()
    }
}

#[derive(Default)]
struct Editing {
    /// First of the trailing columns that can't be edited (the ROWID, the
    /// key columns of a MySQL table).
    locked: Option<usize>,
    changes: Changes,
    /// Cell being edited: (view row, column, text).
    active: Option<(usize, usize, String)>,
    focus_editor: bool,
}

impl Editing {
    fn is_locked(&self, col: usize) -> bool {
        self.locked.is_some_and(|l| col >= l)
    }
}

/// A view row resolved to either a fetched row or a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowRef {
    Existing(usize),
    New(usize),
}

pub struct ResultGrid<'a> {
    columns: &'a [Column],
    rows: &'a [Vec<Cell>],
    state: &'a mut GridState,
    id: egui::Id,
}

pub struct GridOutput {
    /// The last fetched row is visible: time to fetch more if available.
    pub reached_end: bool,
    /// Summary of the selection (count/sum/avg/min/max) for a status line.
    pub selection_summary: Option<String>,
}

impl GridState {
    /// How the rows are exported: in the database's SQL, INSERTs into the
    /// table they come from (else a placeholder name).
    pub fn export_options(&self) -> pythia_data::Options {
        let defaults = pythia_data::Options::default();
        pythia_data::Options {
            dialect: self.dialect,
            table: self.table.clone().unwrap_or(defaults.table.clone()),
            ..defaults
        }
    }

    /// Shows only rows containing `text` (case-insensitive) in any cell.
    pub fn set_filter(&mut self, text: &str) {
        if self.filter != text {
            self.filter = text.to_owned();
            self.ordered_rows = usize::MAX;
            self.anchor = None;
            self.cursor = None;
        }
    }

    /// The link the user chose in the context menu, with the row index.
    pub fn take_follow(&mut self) -> Option<(usize, usize)> {
        self.follow.take()
    }

    /// All column indices in the order shown, hidden ones included.
    pub fn column_order(&self, count: usize) -> Vec<usize> {
        let mut seen = vec![false; count];
        let valid = self.column_order.len() == count
            && self
                .column_order
                .iter()
                .all(|&c| c < count && !std::mem::replace(&mut seen[c], true));
        if valid {
            self.column_order.clone()
        } else {
            (0..count).collect()
        }
    }

    /// Column indices shown, in order.
    pub fn visible_columns(&self, count: usize) -> Vec<usize> {
        self.column_order(count)
            .into_iter()
            .filter(|c| !self.hidden.contains(c))
            .collect()
    }

    /// Shown columns that stay in place when scrolling sideways (at least
    /// one column always scrolls).
    pub fn frozen_columns(&self, count: usize) -> usize {
        self.frozen
            .min(self.visible_columns(count).len().saturating_sub(1))
    }

    /// Moves a column before (or after) another one.
    fn move_column(&mut self, count: usize, col: usize, to: usize, after: bool) {
        if col == to || col >= count || to >= count {
            return;
        }
        let mut order = self.column_order(count);
        order.retain(|&c| c != col);
        let at = order.iter().position(|&c| c == to).unwrap_or(order.len());
        order.insert(at + usize::from(after), col);
        self.column_order = order;
    }

    fn column_action(&mut self, count: usize, action: ColumnAction) {
        match action {
            ColumnAction::Hide(c) => {
                // At least one column stays.
                if self.visible_columns(count).len() > 1 {
                    self.hidden.insert(c);
                }
            }
            ColumnAction::Show(c) => {
                self.hidden.remove(&c);
            }
            ColumnAction::ShowAll => self.hidden.clear(),
            ColumnAction::Move { col, to, after } => self.move_column(count, col, to, after),
            ColumnAction::Freeze(n) => {
                self.frozen = n;
                self.frozen_width = None;
            }
            ColumnAction::ResetOrder => self.column_order.clear(),
            ColumnAction::ResetWidths => {
                self.widths.clear();
                self.frozen_width = None;
            }
        }
    }

    /// Fetched rows passing the filter.
    pub fn visible_rows(&self) -> usize {
        self.order.len()
    }

    /// Indices of the fetched rows passing the filter, in display order.
    pub fn view_order(&mut self, rows: &[Vec<Cell>], columns: &[Column]) -> &[usize] {
        self.sync(rows, columns);
        &self.order
    }

    fn sync(&mut self, rows: &[Vec<Cell>], columns: &[Column]) {
        if self.ordered_rows == rows.len() {
            return;
        }
        let needle = self.filter.trim().to_lowercase();
        self.order = if needle.is_empty() {
            (0..rows.len()).collect()
        } else {
            (0..rows.len())
                .filter(|&i| {
                    rows[i]
                        .iter()
                        .flatten()
                        .any(|v| v.to_lowercase().contains(&needle))
                })
                .collect()
        };
        self.ordered_rows = rows.len();
        if let Some((col, asc)) = self.sort {
            sort_order(
                &mut self.order,
                rows,
                col,
                columns.get(col).is_some_and(|c| c.numeric),
                asc,
            );
        }
    }

    fn set_sort(&mut self, col: usize, rows: &[Vec<Cell>], columns: &[Column]) {
        // Cycle: ascending → descending → unsorted.
        self.sort = match self.sort {
            Some((c, true)) if c == col => Some((col, false)),
            Some((c, false)) if c == col => None,
            _ => Some((col, true)),
        };
        self.ordered_rows = usize::MAX;
        self.sync(rows, columns);
    }

    /// The selected rows (view order) and columns (in the order shown,
    /// between the anchor's and the cursor's).
    fn selection(&self, count: usize) -> Option<(RangeInclusive<usize>, Vec<usize>)> {
        let (ar, ac) = self.anchor?;
        let (cr, cc) = self.cursor.unwrap_or((ar, ac));
        let visible = self.visible_columns(count);
        let position = |c: usize| visible.iter().position(|&v| v == c);
        let (pa, pc) = (position(ac)?, position(cc)?);
        Some((
            ar.min(cr)..=ar.max(cr),
            visible[pa.min(pc)..=pa.max(pc)].to_vec(),
        ))
    }

    fn is_selected(&self, count: usize, row: usize, col: usize) -> bool {
        self.selection(count)
            .is_some_and(|(r, c)| r.contains(&row) && c.contains(&col))
    }

    fn select(&mut self, cell: (usize, usize), extend: bool) {
        if !extend || self.anchor.is_none() {
            self.anchor = Some(cell);
        }
        self.cursor = Some(cell);
    }

    fn row_index(&self, view_row: usize) -> usize {
        self.order.get(view_row).copied().unwrap_or(view_row)
    }

    fn row_ref(&self, view_row: usize) -> RowRef {
        if view_row < self.order.len() {
            RowRef::Existing(self.row_index(view_row))
        } else {
            RowRef::New(view_row - self.order.len())
        }
    }

    /// Makes the grid editable; `locked` and the columns after it stay
    /// read-only.
    pub fn set_editable(&mut self, locked: Option<usize>) {
        self.edit = Some(Editing {
            locked,
            ..Editing::default()
        });
    }

    /// Changes a cell programmatically (as if edited by the user).
    pub fn edit_cell(&mut self, rows: &[Vec<Cell>], view_row: usize, col: usize, value: Cell) {
        self.sync(rows, &[]);
        self.set_value(rows, view_row, col, value);
    }

    /// Adds a new row programmatically (as if by the user).
    pub fn add_row(&mut self, rows: &[Vec<Cell>], columns: usize) {
        self.sync(rows, &[]);
        self.insert_row(columns);
    }

    /// Marks a fetched row for deletion programmatically (as if by the user).
    pub fn delete_row(&mut self, rows: &[Vec<Cell>], view_row: usize) {
        self.sync(rows, &[]);
        if let RowRef::Existing(r) = self.row_ref(view_row)
            && let Some(edit) = &mut self.edit
        {
            edit.changes.deletes.insert(r);
        }
    }

    pub fn set_read_only(&mut self) {
        self.edit = None;
    }

    pub fn is_editable(&self) -> bool {
        self.edit.is_some()
    }

    pub fn changes(&self) -> Option<&Changes> {
        self.edit.as_ref().map(|e| &e.changes)
    }

    pub fn clear_changes(&mut self) {
        if let Some(e) = &mut self.edit {
            e.changes = Changes::default();
            e.active = None;
        }
    }

    /// Rows shown: fetched rows plus inserted ones.
    fn view_len(&self, _rows: &[Vec<Cell>]) -> usize {
        self.order.len() + self.edit.as_ref().map_or(0, |e| e.changes.inserts.len())
    }

    /// The value shown for a cell, taking pending edits into account.
    fn value<'r>(&'r self, rows: &'r [Vec<Cell>], view_row: usize, col: usize) -> Option<&'r str> {
        match self.row_ref(view_row) {
            RowRef::Existing(r) => {
                if let Some(v) = self
                    .edit
                    .as_ref()
                    .and_then(|e| e.changes.updates.get(&r)?.get(&col))
                {
                    return v.as_deref();
                }
                rows.get(r)?.get(col)?.as_deref()
            }
            RowRef::New(i) => self
                .edit
                .as_ref()?
                .changes
                .inserts
                .get(i)?
                .get(col)?
                .as_deref(),
        }
    }

    fn set_value(&mut self, rows: &[Vec<Cell>], view_row: usize, col: usize, value: Cell) {
        let row_ref = self.row_ref(view_row);
        let Some(edit) = self.edit.as_mut() else {
            return;
        };
        if edit.is_locked(col) {
            return;
        }
        match row_ref {
            RowRef::Existing(r) => {
                let original = rows.get(r).and_then(|row| row.get(col)).cloned().flatten();
                let cells = edit.changes.updates.entry(r).or_default();
                if original == value {
                    cells.remove(&col);
                } else {
                    cells.insert(col, value);
                }
                if cells.is_empty() {
                    edit.changes.updates.remove(&r);
                }
            }
            RowRef::New(i) => {
                if let Some(cell) = edit
                    .changes
                    .inserts
                    .get_mut(i)
                    .and_then(|row| row.get_mut(col))
                {
                    *cell = value;
                }
            }
        }
    }

    fn cell_state(&self, view_row: usize, col: usize) -> CellState {
        let Some(edit) = &self.edit else {
            return CellState::Clean;
        };
        match self.row_ref(view_row) {
            RowRef::New(_) => CellState::Inserted,
            RowRef::Existing(r) if edit.changes.deletes.contains(&r) => CellState::Deleted,
            RowRef::Existing(r)
                if edit
                    .changes
                    .updates
                    .get(&r)
                    .is_some_and(|c| c.contains_key(&col)) =>
            {
                CellState::Changed
            }
            RowRef::Existing(_) => CellState::Clean,
        }
    }

    fn insert_row(&mut self, columns: usize) {
        let len = self.order.len();
        let visible = self.visible_columns(columns);
        if let Some(edit) = &mut self.edit {
            edit.changes.inserts.push(vec![None; columns]);
            let row = len + edit.changes.inserts.len() - 1;
            let col = visible
                .iter()
                .copied()
                .find(|&c| !edit.is_locked(c))
                .unwrap_or(0);
            self.anchor = Some((row, col));
            self.cursor = Some((row, col));
            self.scroll_to_cursor = true;
        }
    }

    /// Toggles deletion of the selected rows (new rows are removed).
    fn delete_rows(&mut self, columns: usize) {
        let Some((rows, _)) = self.selection(columns) else {
            return;
        };
        let refs: Vec<RowRef> = rows.map(|r| self.row_ref(r)).collect();
        let Some(edit) = &mut self.edit else { return };
        let mut new_rows: Vec<usize> = Vec::new();
        for r in refs {
            match r {
                RowRef::Existing(r) => {
                    if !edit.changes.deletes.remove(&r) {
                        edit.changes.deletes.insert(r);
                    }
                }
                RowRef::New(i) => new_rows.push(i),
            }
        }
        for i in new_rows.into_iter().rev() {
            if i < edit.changes.inserts.len() {
                edit.changes.inserts.remove(i);
            }
        }
    }

    fn set_null(&mut self, rows: &[Vec<Cell>], columns: usize) {
        let Some((r, c)) = self.selection(columns) else {
            return;
        };
        for row in r {
            for &col in &c {
                self.set_value(rows, row, col, None);
            }
        }
    }

    /// Pastes tab separated text (e.g. an Excel range) at the cursor, adding
    /// new rows past the end. Empty cells become NULL.
    pub fn paste(&mut self, rows: &[Vec<Cell>], columns: usize, text: &str) {
        let Some(locked) = self.edit.as_ref().map(|e| e.locked) else {
            return;
        };
        self.sync(rows, &[]);
        let visible = self.visible_columns(columns);
        let (r0, c0) = self
            .cursor
            .unwrap_or((0, visible.first().copied().unwrap_or(0)));
        // From the cursor's column on, in the order shown.
        let from = visible.iter().position(|&c| c == c0).unwrap_or(0);
        let targets: Vec<usize> = visible[from..]
            .iter()
            .copied()
            .filter(|&c| locked.is_none_or(|l| c < l))
            .collect();
        let text = text.strip_suffix('\n').unwrap_or(text);
        let text = text.strip_suffix('\r').unwrap_or(text);
        let mut last = None;
        for (i, line) in text.split('\n').enumerate() {
            let view_row = r0 + i;
            while view_row >= self.view_len(rows) {
                self.insert_row(columns);
            }
            for (cell, &col) in line.trim_end_matches('\r').split('\t').zip(&targets) {
                let value = (!cell.is_empty()).then(|| cell.to_owned());
                self.set_value(rows, view_row, col, value);
                last = Some((view_row, col));
            }
        }
        if let Some(last) = last {
            self.anchor = Some((r0, targets.first().copied().unwrap_or(c0)));
            self.cursor = Some(last);
            self.scroll_to_cursor = true;
        }
    }

    fn start_edit(&mut self, rows: &[Vec<Cell>], cell: (usize, usize), initial: Option<String>) {
        let value = self.value(rows, cell.0, cell.1).map(str::to_owned);
        // The inline editor is one line; it would lose line breaks.
        let long = value
            .as_deref()
            .is_some_and(|v| v.contains('\n') || v.chars().count() > 200);
        if long && initial.is_none() {
            self.open_value_editor(rows, cell);
            return;
        }
        if let Some(edit) = &mut self.edit
            && !edit.is_locked(cell.1)
        {
            edit.active = Some((cell.0, cell.1, initial.or(value).unwrap_or_default()));
            edit.focus_editor = true;
        }
    }

    /// Edits a cell in the dialog (multi-line text, calendar for dates).
    pub fn open_value_editor(&mut self, rows: &[Vec<Cell>], cell: (usize, usize)) {
        if self.edit.as_ref().is_none_or(|e| e.is_locked(cell.1)) {
            return;
        }
        let text = self
            .value(rows, cell.0, cell.1)
            .unwrap_or_default()
            .to_owned();
        self.value_editor = Some(ValueEditor {
            view_row: cell.0,
            col: cell.1,
            text,
        });
    }

    pub fn close_value_editor(&mut self) {
        self.value_editor = None;
    }

    /// Switches between the grid and the record view. An open inline edit
    /// is committed first.
    fn set_record_view(&mut self, rows: &[Vec<Cell>], on: bool) {
        self.finish_edit(rows, true);
        self.record_view = on;
    }

    fn finish_edit(&mut self, rows: &[Vec<Cell>], commit: bool) {
        let Some((r, c, text)) = self.edit.as_mut().and_then(|e| e.active.take()) else {
            return;
        };
        if commit {
            self.set_value(rows, r, c, Some(text));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellState {
    Clean,
    Changed,
    Inserted,
    Deleted,
}

fn sort_order(order: &mut [usize], rows: &[Vec<Cell>], col: usize, numeric: bool, asc: bool) {
    let value = |i: usize| rows[i].get(col).and_then(|c| c.as_deref());
    order.sort_by(|&a, &b| {
        // NULLs always last, like Oracle's default for ascending order.
        let ord = match (value(a), value(b)) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => return Ordering::Greater,
            (Some(_), None) => return Ordering::Less,
            (Some(x), Some(y)) if numeric => match (x.parse::<f64>(), y.parse::<f64>()) {
                (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
                _ => x.cmp(y),
            },
            (Some(x), Some(y)) => x.cmp(y),
        };
        if asc { ord } else { ord.reverse() }
    });
}

impl<'a> ResultGrid<'a> {
    pub fn new(
        columns: &'a [Column],
        rows: &'a [Vec<Cell>],
        state: &'a mut GridState,
        id: impl egui::AsId,
    ) -> Self {
        Self {
            columns,
            rows,
            state,
            id: egui::Id::new(id),
        }
    }

    pub fn show(self, ui: &mut Ui) -> GridOutput {
        let Self {
            columns,
            rows,
            state,
            id,
        } = self;
        state.sync(rows, columns);
        let mut out = GridOutput {
            reached_end: false,
            selection_summary: None,
        };

        // Unique where the grid is placed: the compare window's grids have
        // fixed ids.
        let focus_id = ui.make_persistent_id(id.with("focus"));
        // egui takes the focus from an id no widget uses in a pass: the
        // keys would only reach the grid in the frame after a click.
        ui.interact(
            ui.available_rect_before_wrap(),
            focus_id,
            Sense::focusable_noninteractive(),
        );
        if ui.memory(|m| m.has_focus(focus_id)) {
            handle_keys(ui, state, columns, rows);
        }
        if state.record_view {
            record_view(ui, columns, rows, state, id, focus_id);
        } else {
            out.reached_end = table(ui, columns, rows, state, id, focus_id);
        }
        out.selection_summary = summary(columns, rows, state);
        viewer(ui.ctx(), columns, rows, state, id);
        value_editor(ui.ctx(), columns, rows, state, id);
        out
    }
}

fn handle_keys(ui: &mut Ui, state: &mut GridState, columns: &[Column], rows: &[Vec<Cell>]) {
    if state.edit.as_ref().is_some_and(|e| e.active.is_some()) {
        return;
    }
    let visible = state.visible_columns(columns.len());
    let row_count = state.view_len(rows);
    let (Some(&first_col), Some(&last_col)) = (visible.first(), visible.last()) else {
        return;
    };
    if row_count == 0 {
        return;
    }
    let (r, c) = state.cursor.unwrap_or((0, first_col));
    // Left and right go by the order shown.
    let at = visible.iter().position(|&v| v == c).unwrap_or(0);
    let prev_col = visible[at.saturating_sub(1)];
    let next_col = visible[(at + 1).min(visible.len() - 1)];
    let page = 20;
    let (mut nr, mut nc) = (r, c);
    let mut extend = false;
    let mut copy = None;
    let mut open_viewer = false;
    let mut select_all = false;
    let mut start_edit = None;
    let mut set_null = false;
    let mut paste = None;
    let editable = state.is_editable();
    ui.input(|i| {
        extend = i.modifiers.shift;
        let ctrl = i.modifiers.command;
        for e in &i.events {
            match e {
                egui::Event::Key {
                    key, pressed: true, ..
                } => match key {
                    Key::ArrowUp => nr = nr.saturating_sub(1),
                    Key::ArrowDown => nr = (nr + 1).min(row_count - 1),
                    Key::ArrowLeft => nc = prev_col,
                    Key::ArrowRight => nc = next_col,
                    Key::PageUp => nr = nr.saturating_sub(page),
                    Key::PageDown => nr = (nr + page).min(row_count - 1),
                    Key::Home if ctrl => (nr, nc) = (0, first_col),
                    Key::End if ctrl => (nr, nc) = (row_count - 1, last_col),
                    Key::Home => nc = first_col,
                    Key::End => nc = last_col,
                    Key::A if ctrl => select_all = true,
                    Key::Enter | Key::F2 if editable => start_edit = Some(None),
                    Key::Delete if editable => set_null = true,
                    Key::Enter => open_viewer = true,
                    _ => {}
                },
                egui::Event::Text(t) if editable && !ctrl => start_edit = Some(Some(t.clone())),
                egui::Event::Copy => copy = Some(i.modifiers.shift),
                egui::Event::Paste(text) if editable => paste = Some(text.clone()),
                _ => {}
            }
        }
    });
    if select_all {
        state.anchor = Some((0, first_col));
        state.cursor = Some((row_count - 1, last_col));
    } else if (nr, nc) != (r, c) {
        state.select((nr, nc), extend);
        state.scroll_to_cursor = true;
    }
    if open_viewer {
        state.viewer = state.cursor;
    }
    if let (Some(initial), Some(cell)) = (start_edit, state.cursor) {
        state.start_edit(rows, cell, initial);
    }
    if set_null {
        state.set_null(rows, columns.len());
    }
    if let Some(text) = paste {
        state.paste(rows, columns.len(), &text);
    }
    if let Some(with_header) = copy {
        ui.ctx()
            .copy_text(selection_text(columns, rows, state, with_header));
    }
}

/// How result grids look, from the settings; see [`set_grid_style`].
#[derive(Debug, Clone, PartialEq)]
pub struct GridStyle {
    /// Font of the cells (none: the monospace text style).
    pub font: Option<egui::FontId>,
    /// Every other row shaded.
    pub striped: bool,
    /// Lines between rows.
    pub row_lines: bool,
    /// What a NULL shows.
    pub null_text: String,
    /// Color of NULL (none: weak text).
    pub null_color: Option<egui::Color32>,
}

impl Default for GridStyle {
    fn default() -> Self {
        Self {
            font: None,
            striped: true,
            row_lines: false,
            null_text: "NULL".to_owned(),
            null_color: None,
        }
    }
}

fn grid_style_id() -> egui::Id {
    egui::Id::new("pythia-grid-style")
}

/// Sets how every result grid looks.
pub fn set_grid_style(ctx: &egui::Context, style: GridStyle) {
    ctx.data_mut(|d| d.insert_temp(grid_style_id(), std::sync::Arc::new(style)));
}

fn grid_style(ctx: &egui::Context) -> std::sync::Arc<GridStyle> {
    ctx.data(|d| d.get_temp(grid_style_id()))
        .unwrap_or_default()
}

/// Colors and sizes of the table, from the style.
struct Look {
    font: egui::FontId,
    striped: bool,
    /// Between rows, if they have lines.
    row_line: Option<egui::Stroke>,
    null_text: String,
    null_color: egui::Color32,
    selected_bg: egui::Color32,
    cursor_stroke: egui::Stroke,
    header_bg: egui::Color32,
    changed_bg: egui::Color32,
    inserted_bg: egui::Color32,
    deleted_bg: egui::Color32,
    drop_stroke: egui::Stroke,
    row_height: f32,
    char_width: f32,
}

impl Look {
    fn new(ui: &mut Ui) -> Self {
        let visuals = ui.visuals().clone();
        let tint = |c: egui::Color32| c.gamma_multiply(if visuals.dark_mode { 0.22 } else { 0.18 });
        let grid = grid_style(ui.ctx());
        let font = grid
            .font
            .clone()
            .unwrap_or_else(|| egui::TextStyle::Monospace.resolve(ui.style()));
        let (text_height, char_width) =
            ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
        Self {
            striped: grid.striped,
            row_line: grid
                .row_lines
                .then_some(visuals.widgets.noninteractive.bg_stroke),
            null_text: grid.null_text.clone(),
            null_color: grid.null_color.unwrap_or(visuals.weak_text_color()),
            selected_bg: visuals.selection.bg_fill,
            cursor_stroke: egui::Stroke::new(1.5, visuals.hyperlink_color),
            header_bg: visuals.faint_bg_color,
            changed_bg: tint(visuals.hyperlink_color),
            inserted_bg: tint(egui::Color32::from_rgb(0x3f, 0xb9, 0x50)),
            deleted_bg: tint(visuals.error_fg_color),
            drop_stroke: egui::Stroke::new(2.0, visuals.selection.stroke.color),
            row_height: text_height + 2.0 * ui.spacing().button_padding.y,
            char_width,
            font,
        }
    }

    /// A cell's text in the grid's font.
    fn text(&self, text: impl Into<String>) -> RichText {
        RichText::new(text).font(self.font.clone())
    }

    fn null(&self) -> RichText {
        self.text(&self.null_text).italics().color(self.null_color)
    }

    /// The line above a cell, if rows have lines: in the gap to the row
    /// before, across the gaps between columns. Above, not below: the next
    /// row's stripe would cover it.
    fn row_line(&self, ui: &Ui) {
        let Some(stroke) = self.row_line else {
            return;
        };
        let gap = ui.spacing().item_spacing * 0.5;
        let rect = ui.max_rect().expand2(gap);
        let mut painter = ui.painter().clone();
        painter.set_clip_rect(ui.clip_rect().expand2(gap));
        painter.hline(rect.x_range(), rect.top(), stroke);
    }
}

/// What the user did in the table this frame; applied after drawing.
#[derive(Default)]
struct Interactions {
    reached_end: bool,
    clicked: Option<(usize, usize)>,
    sort_clicked: Option<usize>,
    row_clicked: Option<usize>,
    open_viewer: Option<(usize, usize)>,
    menu_cell: Option<(usize, usize)>,
    edit_done: Option<bool>,
    edit_click: Option<(usize, usize)>,
    column: Option<ColumnAction>,
    /// Widths of the columns drawn: (column, width).
    widths: Vec<(usize, f32)>,
}

/// What the frozen and the scrolling part of the table draw from.
struct Draw<'a> {
    columns: &'a [Column],
    rows: &'a [Vec<Cell>],
    id: egui::Id,
    look: Look,
    /// Shown columns, in order.
    visible: Vec<usize>,
    frozen: usize,
    selected_rows: Option<RangeInclusive<usize>>,
    /// By column: part of the selection.
    selected_cols: Vec<bool>,
    data_rows: usize,
    view_len: usize,
    pointer_down: bool,
    act: Interactions,
}

impl Draw<'_> {
    /// The row numbers' column.
    fn numbers_width(&self) -> f32 {
        let digits = self.view_len.max(1).to_string().len();
        digits as f32 * self.look.char_width + 18.0
    }

    /// A column's width before the user resized it: its header and the first
    /// rows' values, within limits.
    fn initial_width(&self, c: usize) -> f32 {
        let content = self
            .rows
            .iter()
            .take(200)
            .filter_map(|r| r.get(c)?.as_deref())
            .map(|v| v.lines().next().unwrap_or_default().chars().count())
            .max()
            .unwrap_or(4);
        let chars = content.max(self.columns[c].name.chars().count() + 2).max(4) as f32;
        (chars * self.look.char_width + 16.0).min(self.look.char_width * 48.0)
    }
}

fn table(
    ui: &mut Ui,
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &mut GridState,
    id: egui::Id,
    focus_id: egui::Id,
) -> bool {
    let pointer_down = ui.input(|i| i.pointer.primary_down());
    if !pointer_down {
        state.dragging = false;
    }
    let scroll_to = state
        .scroll_to_cursor
        .then_some(state.cursor)
        .flatten()
        .map(|(r, _)| r);
    state.scroll_to_cursor = false;
    let (selected_rows, selected_cols) = match state.selection(columns.len()) {
        Some((r, cols)) => {
            let mut selected = vec![false; columns.len()];
            for c in cols {
                selected[c] = true;
            }
            (Some(r), selected)
        }
        None => (None, vec![false; columns.len()]),
    };
    let mut d = Draw {
        columns,
        rows,
        id,
        look: Look::new(ui),
        visible: state.visible_columns(columns.len()),
        frozen: state.frozen_columns(columns.len()),
        selected_rows,
        selected_cols,
        data_rows: state.order.len(),
        view_len: state.view_len(rows),
        pointer_down,
        act: Interactions::default(),
    };
    // After columns moved, were shown or frozen: the tables forget their
    // stored widths and take the ones remembered per column.
    let mut relayout = std::mem::take(&mut state.relayout);
    // Columns can show before the first rows arrive: fitted to their
    // headers only, they are fitted again to the values.
    if !rows.is_empty() && !state.fitted_to_rows {
        state.fitted_to_rows = true;
        state.widths.clear();
        state.frozen_width = None;
        relayout = true;
    }

    if d.frozen == 0 {
        egui::ScrollArea::horizontal()
            .id_salt(id.with("h"))
            .auto_shrink(false)
            .show(ui, |ui| {
                let cols = d.visible.clone();
                let mut builder = table_builder(ui, &d, state, id, &cols, true);
                if relayout {
                    builder.reset();
                }
                if let Some(row) = scroll_to {
                    builder = builder.scroll_to_row(row, None);
                }
                draw_part(builder, &cols, true, state, &mut d);
            });
    } else {
        let (left, right) = d.visible.split_at(d.frozen);
        let (left, right) = (left.to_vec(), right.to_vec());
        let spacing = ui.spacing().item_spacing.x;
        let width = state
            .frozen_width
            .unwrap_or_else(|| {
                d.numbers_width()
                    + left
                        .iter()
                        .map(|&c| state.widths.get(&c).copied().unwrap_or(d.initial_width(c)))
                        .sum::<f32>()
                    + spacing * (left.len() + 1) as f32
            })
            .min(ui.available_width() - 60.0)
            .max(40.0);
        ui.horizontal_top(|ui| {
            let height = ui.available_height();
            let left_y = ui
                .allocate_ui_with_layout(
                    egui::vec2(width, height),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_min_size(egui::vec2(width, height));
                        let builder = table_builder(ui, &d, state, id.with("frozen"), &left, true)
                            .vertical_scroll_offset(state.scroll_y)
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                            );
                        if relayout {
                            builder.reset();
                        }
                        draw_part(builder, &left, true, state, &mut d)
                    },
                )
                .inner;
            let edge = ui.min_rect().right();
            let line = egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
            ui.painter()
                .vline(edge + spacing / 2.0, ui.min_rect().y_range(), line);
            // Top-down again: header above the rows.
            let rest = egui::vec2(ui.available_width(), height);
            let right_y = ui
                .allocate_ui_with_layout(rest, Layout::top_down(Align::Min), |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt(id.with("h"))
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            let mut builder = table_builder(ui, &d, state, id, &right, false);
                            if relayout {
                                builder.reset();
                            }
                            if let Some(y) = state.follow_y.take() {
                                builder = builder.vertical_scroll_offset(y);
                            }
                            if let Some(row) = scroll_to {
                                builder = builder.scroll_to_row(row, None);
                            }
                            draw_part(builder, &right, false, state, &mut d)
                        })
                        .inner
                })
                .inner;
            // Keep both parts at the same row: whichever was scrolled
            // leads, the other follows next frame.
            if (left_y - state.scroll_y).abs() > 0.5 {
                state.scroll_y = left_y;
                state.follow_y = Some(left_y);
                ui.ctx().request_repaint();
            } else if (right_y - state.scroll_y).abs() > 0.5 {
                state.scroll_y = right_y;
                ui.ctx().request_repaint();
            }
        });
    }

    // A header being dragged follows the pointer.
    if let Some(drag) = egui::DragAndDrop::payload::<ColumnDrag>(ui.ctx())
        && drag.grid == id
        && let Some(pos) = ui.ctx().pointer_hover_pos()
        && let Some(col) = columns.get(drag.col)
    {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        egui::Area::new(id.with("column-drag"))
            .order(egui::Order::Tooltip)
            .fixed_pos(pos + egui::vec2(12.0, 4.0))
            .interactable(false)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(RichText::new(&col.name).strong());
                });
            });
    }

    let act = std::mem::take(&mut d.act);
    let frozen_cols = d.visible[..d.frozen].to_vec();
    for (c, w) in &act.widths {
        state.widths.insert(*c, *w);
    }
    if d.frozen > 0 {
        let spacing = ui.spacing().item_spacing.x;
        let sum: f32 = frozen_cols.iter().filter_map(|c| state.widths.get(c)).sum();
        state.frozen_width = Some(d.numbers_width() + sum + spacing * (d.frozen + 1) as f32);
    }
    if let Some(commit) = act.edit_done {
        state.finish_edit(rows, commit);
        ui.memory_mut(|m| m.request_focus(focus_id));
    }
    if let Some(cell) = act.edit_click {
        state.start_edit(rows, cell, None);
    }
    if let Some(action) = act.column {
        state.column_action(columns.len(), action);
        state.relayout = true;
        state.frozen_width = None;
        ui.ctx().request_repaint();
    }
    if let Some(c) = act.sort_clicked {
        state.set_sort(c, rows, columns);
    }
    let shift = ui.input(|i| i.modifiers.shift);
    if let Some(cell) = act.clicked {
        state.select(cell, shift);
        ui.memory_mut(|m| m.request_focus(focus_id));
    }
    if let Some(cell) = act.menu_cell
        && !state.is_selected(columns.len(), cell.0, cell.1)
    {
        state.select(cell, false);
    }
    if let Some(row) = act.row_clicked
        && let (Some(&first), Some(&last)) = (d.visible.first(), d.visible.last())
    {
        let anchor_row = match state.anchor {
            Some((r, _)) if shift => r,
            _ => row,
        };
        state.anchor = Some((anchor_row, first));
        state.cursor = Some((row, last));
        ui.memory_mut(|m| m.request_focus(focus_id));
    }
    if let Some(cell) = act.open_viewer {
        state.viewer = Some(cell);
    }
    // Arrow keys and Tab stay in the grid while it has focus.
    ui.memory_mut(|m| {
        if m.has_focus(focus_id) {
            m.set_focus_lock_filter(
                focus_id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: false,
                },
            );
        }
    });
    act.reached_end
}

/// A table of `cols`, with the row numbers first if `numbers`.
fn table_builder<'u>(
    ui: &'u mut Ui,
    d: &Draw,
    state: &GridState,
    id: egui::Id,
    cols: &[usize],
    numbers: bool,
) -> TableBuilder<'u> {
    let mut builder = TableBuilder::new(ui)
        .id_salt(id)
        .striped(d.look.striped)
        .resizable(true)
        .auto_shrink(false)
        .sense(Sense::click_and_drag())
        .cell_layout(Layout::left_to_right(Align::Center));
    if numbers {
        builder = builder.column(TableColumn::exact(d.numbers_width()));
    }
    for &c in cols {
        let width = state
            .widths
            .get(&c)
            .copied()
            .unwrap_or_else(|| d.initial_width(c));
        builder = builder.column(TableColumn::initial(width).at_least(30.0).clip(true));
    }
    builder
}

/// Draws the header and the visible rows of `cols`; returns the vertical
/// scroll offset.
fn draw_part(
    builder: TableBuilder<'_>,
    cols: &[usize],
    numbers: bool,
    state: &mut GridState,
    d: &mut Draw,
) -> f32 {
    let mut widths = Vec::new();
    let table = builder.header(d.look.row_height + 4.0, |mut header| {
        if numbers {
            header.col(|ui| {
                ui.painter()
                    .rect_filled(ui.max_rect(), 0.0, d.look.header_bg);
            });
        }
        for &c in cols {
            header.col(|ui| header_cell(ui, c, state, d));
        }
    });
    let output = table.body(|body| {
        widths = body.widths().to_vec();
        body.rows(d.look.row_height, d.view_len, |mut row| {
            let view_row = row.index();
            if view_row + 1 >= d.data_rows {
                d.act.reached_end = true;
            }
            if numbers {
                let (_, number) = row.col(|ui| {
                    d.look.row_line(ui);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            d.look
                                .text((view_row + 1).to_string())
                                .color(ui.visuals().weak_text_color()),
                        );
                    });
                });
                if number.clicked() {
                    d.act.row_clicked = Some(view_row);
                }
            }
            for &c in cols {
                data_cell(&mut row, view_row, c, state, d);
            }
        });
    });
    let skip = usize::from(numbers);
    d.act
        .widths
        .extend(cols.iter().copied().zip(widths.into_iter().skip(skip)));
    output.state.offset.y
}

/// A column's header: click to sort, drag onto another header to move it,
/// right-click for the column menu.
fn header_cell(ui: &mut Ui, c: usize, state: &GridState, d: &mut Draw) {
    let col = &d.columns[c];
    let rect = ui.max_rect();
    ui.painter().rect_filled(rect, 0.0, d.look.header_bg);
    let arrow = match state.sort {
        Some((s, true)) if s == c => " ▲",
        Some((s, false)) if s == c => " ▼",
        _ => "",
    };
    let text = RichText::new(format!("{}{arrow}", col.name)).strong();
    let layout = if col.numeric {
        Layout::right_to_left(Align::Center)
    } else {
        Layout::left_to_right(Align::Center)
    };
    ui.with_layout(layout, |ui| {
        let r = ui.add(
            egui::Label::new(text)
                .sense(Sense::click_and_drag())
                .selectable(false),
        );
        if r.clicked() {
            d.act.sort_clicked = Some(c);
        }
        r.dnd_set_drag_payload(ColumnDrag { grid: d.id, col: c });
        r.context_menu(|ui| {
            if let Some(action) = header_menu(ui, c, state, d) {
                d.act.column = Some(action);
                ui.close();
            }
        });
        r.on_hover_text(format!(
            "{}\nClick to sort, drag to move, right-click for more",
            col.type_name
        ));
    });
    // The whole cell takes a dropped header, before or after this one.
    let zone = ui.interact(rect, d.id.with(("header-drop", c)), Sense::hover());
    let dragged = zone
        .dnd_hover_payload::<ColumnDrag>()
        .filter(|p| p.grid == d.id && p.col != c);
    if let (Some(_), Some(pos)) = (dragged, ui.ctx().pointer_hover_pos()) {
        let after = pos.x > rect.center().x;
        let x = if after { rect.right() } else { rect.left() };
        ui.painter().vline(x, rect.y_range(), d.look.drop_stroke);
        if let Some(drop) = zone.dnd_release_payload::<ColumnDrag>()
            && drop.grid == d.id
        {
            d.act.column = Some(ColumnAction::Move {
                col: drop.col,
                to: c,
                after,
            });
        }
    }
}

/// The menu of a column's header.
fn header_menu(ui: &mut Ui, c: usize, state: &GridState, d: &Draw) -> Option<ColumnAction> {
    let count = d.columns.len();
    let name = &d.columns[c].name;
    let mut action = None;
    if ui
        .add_enabled(
            d.visible.len() > 1,
            egui::Button::new(format!("Hide {name}")),
        )
        .clicked()
    {
        action = Some(ColumnAction::Hide(c));
    }
    let position = d.visible.iter().position(|&v| v == c);
    if let Some(p) = position.filter(|p| p + 1 < d.visible.len()) {
        let label = if p + 1 == d.frozen {
            format!("Frozen up to {name}")
        } else {
            format!("Freeze up to {name}")
        };
        if ui
            .add_enabled(p + 1 != d.frozen, egui::Button::new(label))
            .on_hover_text(
                "Keep the row numbers and the columns up to here in place when scrolling sideways",
            )
            .clicked()
        {
            action = Some(ColumnAction::Freeze(p + 1));
        }
    }
    if d.frozen > 0 && ui.button("Unfreeze columns").clicked() {
        action = Some(ColumnAction::Freeze(0));
    }
    ui.separator();
    ui.menu_button("Columns", |ui| {
        for col in state.column_order(count) {
            let mut shown = !state.hidden.contains(&col);
            let only = shown && d.visible.len() == 1;
            if ui
                .add_enabled(!only, egui::Checkbox::new(&mut shown, &d.columns[col].name))
                .changed()
            {
                action = Some(if shown {
                    ColumnAction::Show(col)
                } else {
                    ColumnAction::Hide(col)
                });
            }
        }
        ui.separator();
        if ui
            .add_enabled(!state.hidden.is_empty(), egui::Button::new("Show all"))
            .clicked()
        {
            action = Some(ColumnAction::ShowAll);
            ui.close();
        }
        let reordered = state.column_order(count) != (0..count).collect::<Vec<_>>();
        if ui
            .add_enabled(reordered, egui::Button::new("Reset order"))
            .on_hover_text("Back to the query's order")
            .clicked()
        {
            action = Some(ColumnAction::ResetOrder);
            ui.close();
        }
        if ui.button("Reset widths").clicked() {
            action = Some(ColumnAction::ResetWidths);
            ui.close();
        }
    });
    let hidden = state.hidden.len();
    if hidden > 0
        && ui
            .button(format!("Show all columns ({hidden} hidden)"))
            .clicked()
    {
        action = Some(ColumnAction::ShowAll);
    }
    action
}

/// A data cell: value (or the inline editor), selection, cursor, edit state.
fn data_cell(
    row: &mut egui_extras::TableRow<'_, '_>,
    view_row: usize,
    c: usize,
    state: &mut GridState,
    d: &mut Draw,
) {
    let col = &d.columns[c];
    let look = &d.look;
    let selected = d.selected_cols[c]
        && d.selected_rows
            .as_ref()
            .is_some_and(|r| r.contains(&view_row));
    let is_cursor = state.cursor == Some((view_row, c));
    let cell_state = state.cell_state(view_row, c);
    let editing_here = state
        .edit
        .as_ref()
        .and_then(|e| e.active.as_ref())
        .is_some_and(|(r, ec, _)| *r == view_row && *ec == c);
    let value = state.value(d.rows, view_row, c).map(str::to_owned);
    let mut edit_done = None;
    let (_, response) = row.col(|ui| {
        let rect = ui.max_rect();
        look.row_line(ui);
        let tint = match cell_state {
            CellState::Changed => Some(look.changed_bg),
            CellState::Inserted => Some(look.inserted_bg),
            CellState::Deleted => Some(look.deleted_bg),
            CellState::Clean => None,
        };
        if let Some(tint) = tint {
            ui.painter().rect_filled(rect, 0.0, tint);
        }
        if editing_here
            && let Some(edit) = state.edit.as_mut()
            && let Some((_, _, text)) = edit.active.as_mut()
        {
            let r = ui.add(
                egui::TextEdit::singleline(text)
                    .font(look.font.clone())
                    .desired_width(f32::INFINITY)
                    .margin(egui::Margin::symmetric(2, 0)),
            );
            if std::mem::take(&mut edit.focus_editor) {
                r.request_focus();
            }
            let (enter, escape) = ui.input(|i| {
                (
                    i.key_pressed(Key::Enter) || i.key_pressed(Key::Tab),
                    i.key_pressed(Key::Escape),
                )
            });
            if escape {
                edit_done = Some(false);
            } else if enter || r.lost_focus() {
                edit_done = Some(true);
            }
            return;
        }
        if selected {
            ui.painter().rect_filled(rect, 0.0, look.selected_bg);
        }
        if is_cursor {
            ui.painter().rect_stroke(
                rect.shrink(1.0),
                2.0,
                look.cursor_stroke,
                egui::StrokeKind::Inside,
            );
        }
        let layout = if col.numeric {
            Layout::right_to_left(Align::Center)
        } else {
            Layout::left_to_right(Align::Center)
        };
        let strike = cell_state == CellState::Deleted;
        let binary = is_binary(col);
        ui.with_layout(layout, |ui| match value.as_deref() {
            Some(text) if binary => {
                ui.add(
                    egui::Label::new(
                        look.text(binary_summary(text))
                            .color(ui.visuals().weak_text_color()),
                    )
                    .truncate()
                    .selectable(false),
                );
            }
            Some(text) => {
                let first = text.lines().next().unwrap_or_default();
                let more = if first.len() < text.len() { " …" } else { "" };
                let mut rich = look.text(format!("{first}{more}"));
                if strike {
                    rich = rich.strikethrough();
                }
                ui.add(egui::Label::new(rich).truncate().selectable(false));
            }
            _ => {
                ui.add(egui::Label::new(look.null()).truncate().selectable(false));
            }
        });
    });
    if edit_done.is_some() {
        d.act.edit_done = edit_done;
    }
    if response.drag_started() || response.clicked() {
        d.act.clicked = Some((view_row, c));
        state.dragging = response.drag_started();
    } else if state.dragging && d.pointer_down && response.contains_pointer() {
        state.cursor = Some((view_row, c));
    }
    if response.double_clicked() {
        if state.is_editable() {
            d.act.edit_click = Some((view_row, c));
        } else {
            d.act.open_viewer = Some((view_row, c));
        }
    }
    if response.secondary_clicked() {
        d.act.menu_cell = Some((view_row, c));
    }
    response.context_menu(|ui| context_menu(ui, d.columns, d.rows, state));
}

fn context_menu(ui: &mut Ui, columns: &[Column], rows: &[Vec<Cell>], state: &mut GridState) {
    if state.is_editable() {
        if ui.button("Edit cell").clicked() {
            if let Some(cell) = state.cursor {
                state.start_edit(rows, cell, None);
            }
            ui.close();
        }
        if ui.button("Edit in dialog…").clicked() {
            if let Some(cell) = state.cursor {
                state.open_value_editor(rows, cell);
            }
            ui.close();
        }
        if ui.button("Set NULL").clicked() {
            state.set_null(rows, columns.len());
            ui.close();
        }
        if ui.button("Insert row").clicked() {
            state.insert_row(columns.len());
            ui.close();
        }
        if ui.button("Delete / undelete rows").clicked() {
            state.delete_rows(columns.len());
            ui.close();
        }
        ui.separator();
    }
    if ui.button("Copy").clicked() {
        ui.ctx()
            .copy_text(selection_text(columns, rows, state, false));
        ui.close();
    }
    if ui.button("Copy with header").clicked() {
        ui.ctx()
            .copy_text(selection_text(columns, rows, state, true));
        ui.close();
    }
    ui.menu_button("Copy as", |ui| {
        for format in Format::ALL
            .iter()
            .filter(|f| f.is_text() && **f != Format::Tsv)
        {
            if ui.button(format.label()).clicked() {
                let (cols, data) = selection_data(columns, rows, state);
                if let Ok(bytes) =
                    pythia_data::export_to_vec(*format, &cols, &data, &state.export_options())
                {
                    ui.ctx()
                        .copy_text(String::from_utf8_lossy(&bytes).into_owned());
                }
                ui.close();
            }
        }
    });
    let row = state
        .cursor
        .and_then(|(view_row, _)| state.order.get(view_row).copied());
    if let Some(row) = row
        && !state.links.is_empty()
    {
        let mut chosen = None;
        ui.separator();
        for many in [false, true] {
            let links: Vec<(usize, &GridLink)> = state
                .links
                .iter()
                .enumerate()
                .filter(|(_, l)| l.many == many)
                .collect();
            if links.is_empty() {
                continue;
            }
            let title = if many { "Child rows" } else { "Parent row" };
            ui.menu_button(title, |ui| {
                for (i, link) in links {
                    // A NULL key has no related rows.
                    let has_key = link
                        .columns
                        .iter()
                        .all(|&c| rows[row].get(c).is_some_and(Option::is_some));
                    if ui
                        .add_enabled(has_key, egui::Button::new(&link.label))
                        .clicked()
                    {
                        chosen = Some((i, row));
                        ui.close();
                    }
                }
            });
        }
        if chosen.is_some() {
            state.follow = chosen;
        }
    }
    ui.separator();
    if ui.button("View value…").clicked() {
        state.viewer = state.cursor;
        ui.close();
    }
    if ui
        .button(if state.record_view {
            "Grid view"
        } else {
            "Single record view"
        })
        .clicked()
    {
        state.set_record_view(rows, !state.record_view);
        ui.close();
    }
}

/// The selected block as columns + rows (in view order); everything if
/// nothing is selected.
pub fn selection_data(
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &GridState,
) -> (Vec<Column>, Vec<Vec<Cell>>) {
    let Some((r, keep)) = state.selection(columns.len()) else {
        return shown_data(columns, rows, state);
    };
    let cols = keep.iter().map(|&i| columns[i].clone()).collect();
    let data = r
        .filter_map(|vr| rows.get(state.row_index(vr)))
        .map(|row| {
            keep.iter()
                .map(|&i| row.get(i).cloned().flatten())
                .collect()
        })
        .collect();
    (cols, data)
}

/// All fetched rows (as fetched) with the columns shown, in the order shown.
pub fn shown_data(
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &GridState,
) -> (Vec<Column>, Vec<Vec<Cell>>) {
    let keep = state.visible_columns(columns.len());
    if keep.iter().copied().eq(0..columns.len()) {
        return (columns.to_vec(), rows.to_vec());
    }
    let cols = keep.iter().map(|&i| columns[i].clone()).collect();
    let data = rows
        .iter()
        .map(|row| {
            keep.iter()
                .map(|&i| row.get(i).cloned().flatten())
                .collect()
        })
        .collect();
    (cols, data)
}

fn selection_text(
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &GridState,
    header: bool,
) -> String {
    let (cols, data) = selection_data(columns, rows, state);
    let mut lines = Vec::with_capacity(data.len() + 1);
    if header {
        lines.push(
            cols.iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join("\t"),
        );
    }
    lines.extend(data.iter().map(|r| {
        r.iter()
            .map(|c| c.as_deref().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\t")
    }));
    lines.join("\n")
}

fn summary(columns: &[Column], rows: &[Vec<Cell>], state: &GridState) -> Option<String> {
    let (r, c) = state.selection(columns.len())?;
    let cells = (r.end() - r.start() + 1) * c.len();
    if cells < 2 {
        return None;
    }
    let mut numbers = Vec::new();
    let mut non_null = 0;
    for vr in r {
        let Some(row) = rows.get(state.row_index(vr)) else {
            continue;
        };
        for &ci in &c {
            if let Some(Some(v)) = row.get(ci) {
                non_null += 1;
                if columns.get(ci).is_some_and(|col| col.numeric)
                    && let Ok(n) = v.parse::<f64>()
                {
                    numbers.push(n);
                }
            }
        }
    }
    if numbers.is_empty() {
        return Some(format!("{cells} cells · {non_null} not null"));
    }
    let sum: f64 = numbers.iter().sum();
    let min = numbers.iter().copied().fold(f64::INFINITY, f64::min);
    let max = numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Some(format!(
        "Count {non_null} · Sum {} · Avg {} · Min {} · Max {}",
        fmt_num(sum),
        fmt_num(sum / numbers.len() as f64),
        fmt_num(min),
        fmt_num(max)
    ))
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{n:.0}")
    } else {
        format!("{n:.4}").trim_end_matches('0').to_owned()
    }
}

/// One row at a time, a field per column. The fields edit like the grid's
/// cells, and the records are the grid's rows: the ones passing the filter,
/// then the inserted ones.
fn record_view(
    ui: &mut Ui,
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &mut GridState,
    id: egui::Id,
    focus_id: egui::Id,
) {
    let visible = state.visible_columns(columns.len());
    let count = state.view_len(rows);
    let (shown, c) = state
        .cursor
        .unwrap_or((0, visible.first().copied().unwrap_or(0)));
    let mut r = shown.min(count.saturating_sub(1));
    ui.horizontal(|ui| {
        if ui.button("◀").clicked() {
            r = r.saturating_sub(1);
        }
        if count == 0 {
            ui.label("No records");
        } else {
            ui.label(format!("Record {} of {}", r + 1, count));
        }
        if ui.button("▶").clicked() && r + 1 < count {
            r += 1;
        }
        if ui.button("Grid view").clicked() {
            state.set_record_view(rows, false);
        }
        let status = match state.cell_state(r, c) {
            _ if count == 0 => None,
            CellState::Inserted => Some("New row"),
            CellState::Deleted => Some("To be deleted"),
            _ => None,
        };
        if let Some(status) = status {
            ui.weak(status);
        }
    });
    // An edit left on the record before would stay open, unseen.
    if r != shown {
        state.finish_edit(rows, true);
    }
    state.cursor = Some((r, c));
    state.anchor = state.cursor;
    if count == 0 {
        return;
    }
    let look = Look::new(ui);
    let editable = state.is_editable();
    let focused = ui.memory(|m| m.has_focus(focus_id));
    let mut edit_done = None;
    egui::ScrollArea::vertical()
        .id_salt(id.with("record"))
        .auto_shrink(false)
        .show(ui, |ui| {
            egui::Grid::new(id.with("record-grid"))
                .striped(true)
                .num_columns(3)
                .spacing([16.0, 6.0])
                .show(ui, |ui| {
                    for col in visible {
                        let column = &columns[col];
                        ui.label(RichText::new(&column.name).strong());
                        ui.weak(&column.type_name);
                        let editing_here = state
                            .edit
                            .as_ref()
                            .and_then(|e| e.active.as_ref())
                            .is_some_and(|(er, ec, _)| (*er, *ec) == (r, col));
                        if editing_here
                            && let Some(edit) = state.edit.as_mut()
                            && let Some((_, _, text)) = edit.active.as_mut()
                        {
                            let response = ui.add(
                                egui::TextEdit::singleline(text)
                                    .font(look.font.clone())
                                    .desired_width(ui.available_width()),
                            );
                            if std::mem::take(&mut edit.focus_editor) {
                                response.request_focus();
                            }
                            let (enter, escape) = ui.input(|i| {
                                (
                                    i.key_pressed(Key::Enter) || i.key_pressed(Key::Tab),
                                    i.key_pressed(Key::Escape),
                                )
                            });
                            if escape {
                                edit_done = Some(false);
                            } else if enter || response.lost_focus() {
                                edit_done = Some(true);
                            }
                            ui.end_row();
                            continue;
                        }
                        let cell_state = state.cell_state(r, col);
                        let tint = match cell_state {
                            CellState::Changed => look.changed_bg,
                            CellState::Inserted => look.inserted_bg,
                            CellState::Deleted => look.deleted_bg,
                            CellState::Clean => egui::Color32::TRANSPARENT,
                        };
                        let mut text = match state.value(rows, r, col) {
                            Some(v) => look.text(v),
                            None => look.null(),
                        };
                        if cell_state == CellState::Deleted {
                            text = text.strikethrough();
                        }
                        // The cursor's outline only while the keys come
                        // here: without, it looks like an open editor.
                        // Transparent elsewhere, so the text doesn't move.
                        let mut stroke = look.cursor_stroke;
                        if col != c || !focused {
                            stroke.color = egui::Color32::TRANSPARENT;
                        }
                        // Top-down: in the cell's centered row layout, the
                        // frame would keep the height of the row before.
                        let field = ui
                            .with_layout(Layout::top_down(Align::Min), |ui| {
                                egui::Frame::new()
                                    .fill(tint)
                                    .stroke(stroke)
                                    .corner_radius(2.0)
                                    .inner_margin(egui::Margin::symmetric(4, 1))
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.add(egui::Label::new(text).wrap().selectable(!editable))
                                    })
                            })
                            .inner;
                        // Editable, the whole field takes the clicks;
                        // read-only, the text stays selectable.
                        let response = if editable {
                            ui.interact(
                                field.response.rect,
                                id.with(("record-field", col)),
                                Sense::click(),
                            )
                        } else {
                            field.inner
                        };
                        if col == c && state.scroll_to_cursor {
                            response.scroll_to_me(None);
                        }
                        if response.clicked() || response.secondary_clicked() {
                            state.select((r, col), false);
                            ui.memory_mut(|m| m.request_focus(focus_id));
                        }
                        if response.double_clicked() && editable {
                            state.start_edit(rows, (r, col), None);
                        }
                        response.context_menu(|ui| context_menu(ui, columns, rows, state));
                        ui.end_row();
                    }
                });
        });
    state.scroll_to_cursor = false;
    if let Some(commit) = edit_done {
        state.finish_edit(rows, commit);
        ui.memory_mut(|m| m.request_focus(focus_id));
    }
}

fn is_temporal(col: &Column) -> bool {
    let t = col.type_name.to_ascii_uppercase();
    t.starts_with("DATE") || t.starts_with("TIMESTAMP")
}

/// The dialog editing one cell: multi-line text, a calendar for dates.
fn value_editor(
    ctx: &egui::Context,
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &mut GridState,
    id: egui::Id,
) {
    let Some(ed) = state.value_editor.as_mut() else {
        return;
    };
    let column = columns.get(ed.col);
    let name = column.map_or("", |c| c.name.as_str());
    let date = column.is_some_and(is_temporal);
    let mut open = true;
    let mut result: Option<Cell> = None;
    egui::Window::new(format!("Edit {name} — row {}", ed.view_row + 1))
        .id(id.with("value-editor"))
        .open(&mut open)
        .pivot(Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .default_size(if date { [480.0, 120.0] } else { [560.0, 360.0] })
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            if date {
                ui.horizontal(|ui| {
                    // The date part from the picker, the time kept from the text.
                    let today = jiff::Zoned::now().date();
                    let mut day = ed
                        .text
                        .get(..10)
                        .and_then(|d| d.parse::<jiff::civil::Date>().ok())
                        .unwrap_or(today);
                    ui.label("Date");
                    if ui
                        .add(
                            egui_extras::DatePickerButton::new(&mut day)
                                .id_salt("grid-date")
                                .calendar_week(true),
                        )
                        .changed()
                    {
                        let time = ed.text.get(10..).unwrap_or_default().to_owned();
                        let time = if time.trim().is_empty() {
                            " 00:00:00".to_owned()
                        } else {
                            time
                        };
                        ed.text = format!("{day}{time}");
                    }
                    ui.weak("Format YYYY-MM-DD HH24:MI:SS");
                });
            }
            let height = if date {
                ui.text_style_height(&egui::TextStyle::Monospace) + 10.0
            } else {
                (ui.available_height() - 36.0).max(80.0)
            };
            ui.add_sized(
                [ui.available_width(), height],
                egui::TextEdit::multiline(&mut ed.text)
                    .font(egui::TextStyle::Monospace)
                    .lock_focus(true),
            );
            ui.horizontal(|ui| {
                if ui.button("Apply").clicked() {
                    result = Some(Some(ed.text.clone()));
                }
                if ui.button("Set NULL").clicked() {
                    result = Some(None);
                }
                ui.weak(format!("{} characters", ed.text.chars().count()));
            });
        });
    if let Some(value) = result {
        let (view_row, col) = (ed.view_row, ed.col);
        state.value_editor = None;
        state.set_value(rows, view_row, col, value);
    } else if !open {
        state.value_editor = None;
    }
}

fn viewer(
    ctx: &egui::Context,
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &mut GridState,
    id: egui::Id,
) {
    let Some((r, c)) = state.viewer else { return };
    let value = rows
        .get(state.row_index(r))
        .and_then(|row| row.get(c))
        .cloned()
        .flatten();
    let name = columns.get(c).map_or("", |col| col.name.as_str());
    let bytes = columns
        .get(c)
        .filter(|col| is_binary(col))
        .and(value.as_deref())
        .and_then(decode_hex);
    let pretty = value
        .as_deref()
        .and_then(|v| serde_json::from_str::<serde_json::Value>(v.trim()).ok())
        .filter(|j| j.is_object() || j.is_array())
        .and_then(|j| serde_json::to_string_pretty(&j).ok());
    let mut text = pretty.clone().or_else(|| value.clone()).unwrap_or_default();
    let mut open = true;
    egui::Window::new(format!("{name} — row {}", r + 1))
        .id(id.with("viewer"))
        .open(&mut open)
        .default_size([560.0, 380.0])
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                match (&value, &bytes) {
                    (None, _) => ui.weak("NULL"),
                    (_, Some(b)) => ui.weak(format!(
                        "{} · {}",
                        human_bytes(b.len()),
                        image_kind(b).unwrap_or("binary")
                    )),
                    (Some(v), None) => ui.weak(format!("{} characters", v.chars().count())),
                };
                if pretty.is_some() {
                    ui.weak("· JSON, formatted");
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Copy").clicked() {
                        ctx.copy_text(value.clone().unwrap_or_default());
                    }
                });
            });
            ui.separator();
            if let Some(b) = &bytes {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    if let Some(kind) = image_kind(b) {
                        let uri = format!("bytes://pythia-grid/{r}-{c}.{kind}");
                        ui.add(
                            egui::Image::from_bytes(uri, b.clone())
                                .max_width(ui.available_width())
                                .max_height(480.0),
                        );
                        ui.separator();
                    }
                    ui.label(RichText::new(hex_dump(b, 4096)).monospace());
                });
                return;
            }
            egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .frame(egui::Frame::NONE),
                );
            });
        });
    if !open {
        state.viewer = None;
    }
}

/// BLOB, RAW and LONG RAW values arrive as hex text.
fn is_binary(col: &Column) -> bool {
    let t = col.type_name.to_ascii_uppercase();
    t.starts_with("BLOB") || t.starts_with("RAW") || t.starts_with("LONG RAW") || t == "BFILE"
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn human_bytes(n: usize) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} bytes"),
    }
}

/// `0x89504E47… · 12.3 KB` for a hex-encoded value.
fn binary_summary(hex: &str) -> String {
    let head: String = hex.chars().take(16).collect();
    let more = if hex.len() > 16 { "…" } else { "" };
    format!("0x{head}{more} · {}", human_bytes(hex.len() / 2))
}

fn image_kind(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("jpg"),
        [b'G', b'I', b'F', b'8', ..] => Some("gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("webp"),
        [b'B', b'M', ..] => Some("bmp"),
        _ => None,
    }
}

/// Classic hex dump (offset, 16 bytes, ASCII), limited to `max` bytes.
fn hex_dump(bytes: &[u8], max: usize) -> String {
    let mut out = String::new();
    for (i, chunk) in bytes[..bytes.len().min(max)].chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02X}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        out.push_str(&format!("{:08X}  {:<47}  {ascii}\n", i * 16, hex.join(" ")));
    }
    if bytes.len() > max {
        out.push_str(&format!("… {} more", human_bytes(bytes.len() - max)));
    }
    out
}

/// Renders rows as tab separated text with a header line.
pub fn to_tsv(columns: &[Column], rows: &[Vec<Cell>]) -> String {
    let mut out = columns
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join("\t");
    for row in rows {
        out.push('\n');
        let line: Vec<&str> = row.iter().map(|c| c.as_deref().unwrap_or("")).collect();
        out.push_str(&line.join("\t"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// INSERTs go into the result's table, or a placeholder (#158).
    #[test]
    fn export_options_name_the_table() {
        let mut state = GridState {
            dialect: pythia_db::Dialect::MYSQL,
            ..Default::default()
        };
        let options = state.export_options();
        assert_eq!(options.table, "my_table");
        assert_eq!(options.dialect, pythia_db::Dialect::MYSQL);
        state.table = Some("shop.orders".into());
        assert_eq!(state.export_options().table, "shop.orders");
    }

    fn cols() -> Vec<Column> {
        vec![
            Column {
                name: "A".into(),
                type_name: "NUMBER".into(),
                numeric: true,
            },
            Column {
                name: "B".into(),
                type_name: "VARCHAR2(10)".into(),
                numeric: false,
            },
        ]
    }

    fn rows() -> Vec<Vec<Cell>> {
        vec![
            vec![Some("10".into()), Some("x".into())],
            vec![None, Some("a".into())],
            vec![Some("9".into()), None],
        ]
    }

    #[test]
    fn binary() {
        assert_eq!(decode_hex("89504E47"), Some(vec![0x89, b'P', b'N', b'G']));
        assert_eq!(
            image_kind(&decode_hex("89504E470D0A").unwrap()),
            Some("png")
        );
        assert_eq!(binary_summary("DEADBEEF"), "0xDEADBEEF · 4 bytes");
        assert!(hex_dump(b"Hello", 16).starts_with("00000000  48 65 6C 6C 6F"));
    }

    #[test]
    fn tsv() {
        assert_eq!(to_tsv(&cols(), &rows()[..1]), "A\tB\n10\tx");
    }

    #[test]
    fn numeric_sort_nulls_last() {
        let (c, r) = (cols(), rows());
        let mut s = GridState::default();
        s.sync(&r, &c);
        s.set_sort(0, &r, &c);
        assert_eq!(s.order, [2, 0, 1]);
        s.set_sort(0, &r, &c);
        assert_eq!(s.order, [0, 2, 1]);
        s.set_sort(0, &r, &c);
        assert_eq!(s.order, [0, 1, 2]);
    }

    #[test]
    fn filter_rows() {
        let (c, r) = (cols(), rows());
        let mut s = GridState::default();
        s.set_filter("A");
        s.sync(&r, &c);
        assert_eq!(s.order, [1]);
        s.set_filter("");
        s.sync(&r, &c);
        assert_eq!(s.visible_rows(), 3);
    }

    #[test]
    fn selection_copy_and_summary() {
        let (c, r) = (cols(), rows());
        let mut s = GridState::default();
        s.sync(&r, &c);
        s.select((0, 0), false);
        s.select((2, 1), true);
        assert_eq!(selection_text(&c, &r, &s, true), "A\tB\n10\tx\n\ta\n9\t");
        assert_eq!(
            summary(&c, &r, &s).unwrap(),
            "Count 4 · Sum 19 · Avg 9.5 · Min 9 · Max 10"
        );
    }

    #[test]
    fn hidden_columns_are_left_out() {
        let columns = cols();
        let rows = vec![vec![Some("1".into()), Some("x".into())]];
        let mut state = GridState::default();
        state.hidden.insert(0);
        let (c, d) = selection_data(&columns, &rows, &state);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "B");
        assert_eq!(d, vec![vec![Some("x".to_owned())]]);
        assert_eq!(state.visible_columns(2), vec![1]);
    }

    #[test]
    fn pastes_ranges_and_adds_rows() {
        let columns = cols();
        let rows = vec![vec![Some("1".into()), Some("x".into())]];
        let mut state = GridState::default();
        state.set_editable(None);
        state.sync(&rows, &columns);
        state.cursor = Some((0, 0));
        state.paste(&rows, 2, "5\ty\r\n6\t\r\n");
        let changes = state.changes().unwrap();
        assert_eq!(changes.updates[&0][&0], Some("5".into()));
        assert_eq!(changes.updates[&0][&1], Some("y".into()));
        assert_eq!(changes.inserts, vec![vec![Some("6".into()), None]]);
        assert_eq!(state.cursor, Some((1, 1)));
    }

    #[test]
    fn trailing_key_columns_stay_read_only() {
        let rows = vec![vec![Some("a".into()), Some("1".into()), Some("2".into())]];
        let mut state = GridState::default();
        // Columns 1 and 2 hold the key.
        state.set_editable(Some(1));
        state.edit_cell(&rows, 0, 0, Some("b".into()));
        state.edit_cell(&rows, 0, 1, Some("9".into()));
        state.edit_cell(&rows, 0, 2, Some("9".into()));
        state.paste(&rows, 3, "c\td\te");
        assert_eq!(
            state.changes().unwrap().updates[&0],
            BTreeMap::from([(0, Some("c".into()))])
        );
        state.add_row(&rows, 3);
        assert_eq!(state.cursor, Some((1, 0)));
        state.edit_cell(&rows, 1, 0, Some("new".into()));
        state.delete_row(&rows, 0);
        let changes = state.changes().unwrap();
        assert_eq!(changes.inserts, vec![vec![Some("new".into()), None, None]]);
        assert_eq!(changes.deletes, BTreeSet::from([0]));
    }

    #[test]
    fn long_values_open_the_dialog() {
        let columns = cols();
        let rows = vec![vec![Some("1".into()), Some("line 1\nline 2".into())]];
        let mut state = GridState::default();
        state.set_editable(None);
        state.sync(&rows, &columns);
        state.start_edit(&rows, (0, 1), None);
        assert!(state.edit.as_ref().unwrap().active.is_none());
        let ed = state.value_editor.as_mut().unwrap();
        assert_eq!(ed.text, "line 1\nline 2");
        ed.text.push_str("\nline 3");
        let text = ed.text.clone();
        state.value_editor = None;
        state.set_value(&rows, 0, 1, Some(text));
        assert_eq!(
            state.changes().unwrap().updates[&0][&1].as_deref(),
            Some("line 1\nline 2\nline 3")
        );
        // Short values still edit inline.
        state.start_edit(&rows, (0, 0), None);
        assert!(state.edit.as_ref().unwrap().active.is_some());
    }

    fn three() -> (Vec<Column>, Vec<Vec<Cell>>) {
        let mut columns = cols();
        columns.push(Column {
            name: "C".into(),
            type_name: "VARCHAR2(10)".into(),
            numeric: false,
        });
        let row = |a: &str, b: &str, c: &str| vec![Some(a.into()), Some(b.into()), Some(c.into())];
        (columns, vec![row("1", "x", "p"), row("2", "y", "q")])
    }

    #[test]
    fn columns_move_hide_and_freeze() {
        let mut s = GridState::default();
        assert_eq!(s.column_order(3), [0, 1, 2]);
        s.move_column(3, 0, 2, true);
        assert_eq!(s.column_order(3), [1, 2, 0]);
        s.move_column(3, 2, 1, false);
        assert_eq!(s.column_order(3), [2, 1, 0]);
        // Another result (other columns) starts in query order.
        assert_eq!(s.column_order(2), [0, 1]);
        s.column_action(3, ColumnAction::ResetOrder);
        assert_eq!(s.column_order(3), [0, 1, 2]);
        // One column always stays.
        for c in 0..3 {
            s.column_action(3, ColumnAction::Hide(c));
        }
        assert_eq!(s.visible_columns(3), [2]);
        s.column_action(3, ColumnAction::ShowAll);
        // At least one column scrolls.
        s.column_action(3, ColumnAction::Freeze(5));
        assert_eq!(s.frozen_columns(3), 2);
        s.hidden.insert(1);
        assert_eq!(s.frozen_columns(3), 1);
    }

    #[test]
    fn reordered_columns_select_copy_and_paste_as_shown() {
        let (columns, rows) = three();
        let mut s = GridState::default();
        s.sync(&rows, &columns);
        s.move_column(3, 2, 0, false);
        assert_eq!(s.visible_columns(3), [2, 0, 1]);
        // From C to A: two columns, in the order shown.
        s.select((0, 2), false);
        s.select((1, 0), true);
        assert_eq!(
            selection_text(&columns, &rows, &s, true),
            "C\tA\np\t1\nq\t2"
        );
        assert_eq!(
            summary(&columns, &rows, &s).unwrap(),
            "Count 4 · Sum 3 · Avg 1.5 · Min 1 · Max 2"
        );
        // A hidden column in the range stays out.
        s.hidden.insert(0);
        s.select((0, 2), false);
        s.select((0, 1), true);
        assert_eq!(selection_text(&columns, &rows, &s, true), "C\tB\np\tx");
        // Without a selection: all rows, the columns shown, in order.
        s.anchor = None;
        s.cursor = None;
        let (c, d) = selection_data(&columns, &rows, &s);
        let names: Vec<&str> = c.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["C", "B"]);
        assert_eq!(d[1], vec![Some("q".to_owned()), Some("y".to_owned())]);
        let (shown, all) = shown_data(&columns, &rows, &s);
        assert_eq!(shown.len(), c.len());
        assert_eq!(all, d);
        // Pasting goes right in the order shown: A, then B.
        s.hidden.clear();
        s.set_editable(None);
        s.cursor = Some((0, 0));
        s.paste(&rows, 3, "9\tz");
        let updates = &s.changes().unwrap().updates[&0];
        assert_eq!(updates[&0].as_deref(), Some("9"));
        assert_eq!(updates[&1].as_deref(), Some("z"));
        assert!(!updates.contains_key(&2));
    }

    /// The grid style sets the cells' font, the NULL text and the lines; rows
    /// follow the font and the padding.
    #[test]
    fn grid_style() {
        let (c, r) = (cols(), rows());
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        // The texts drawn and the grid's look, with the buttons' vertical
        // padding at `padding`.
        let mut frame = |padding: f32| {
            let mut look = None;
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.spacing_mut().button_padding.y = padding;
                look = Some(Look::new(ui));
                ResultGrid::new(&c, &r, &mut state, "grid").show(ui);
            });
            output.textures_delta.clear();
            let texts: Vec<String> = output
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Text(t) => Some(t.galley.text().to_owned()),
                    _ => None,
                })
                .collect();
            (texts, look.unwrap())
        };
        let (texts, look) = frame(3.0);
        assert_eq!(
            texts.iter().filter(|t| *t == "NULL").count(),
            2,
            "{texts:?}"
        );
        assert_eq!(look.font, egui::FontId::monospace(13.0));
        assert!(look.striped && look.row_line.is_none());
        let text_height = look.row_height - 6.0;

        set_grid_style(
            &ctx,
            GridStyle {
                font: Some(egui::FontId::proportional(20.0)),
                striped: false,
                row_lines: true,
                null_text: "(null)".into(),
                null_color: Some(egui::Color32::RED),
            },
        );
        let (texts, look) = frame(1.0);
        assert_eq!(
            texts.iter().filter(|t| *t == "(null)").count(),
            2,
            "{texts:?}"
        );
        assert!(!texts.iter().any(|t| t == "NULL"), "{texts:?}");
        assert_eq!(look.font, egui::FontId::proportional(20.0));
        assert_eq!(look.null_color, egui::Color32::RED);
        assert!(!look.striped && look.row_line.is_some());
        assert!(
            look.row_height - 2.0 > text_height * 1.4,
            "{} for a 20 pt font",
            look.row_height
        );
    }

    /// Columns drawn before the first rows arrive are fitted again once
    /// they're there, instead of staying as wide as their header.
    #[test]
    fn widths_fit_the_first_rows() {
        let columns = vec![Column {
            name: "C".into(),
            type_name: "VARCHAR2(40)".into(),
            numeric: false,
        }];
        let rows = vec![vec![Some("a value much wider than its header".to_owned())]];
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        let frame = |rows: &[Vec<Cell>], state: &mut GridState| {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                ResultGrid::new(&columns, rows, state, "grid").show(ui);
            });
            // Nothing is rendered.
            output.textures_delta.clear();
        };
        frame(&[], &mut state);
        let header_only = state.widths[&0];
        frame(&rows, &mut state);
        frame(&rows, &mut state);
        assert!(
            state.widths[&0] > header_only * 3.0,
            "{} vs {header_only}",
            state.widths[&0]
        );
    }

    /// Runs a frame of the grid with `events`: the texts drawn, with their
    /// centers.
    fn frame(
        ctx: &egui::Context,
        columns: &[Column],
        rows: &[Vec<Cell>],
        state: &mut GridState,
        events: Vec<egui::Event>,
    ) -> Vec<(String, egui::Pos2)> {
        frame_after(ctx, columns, rows, state, events, 1.0 / 60.0)
    }

    /// A frame `dt` seconds after the one before.
    fn frame_after(
        ctx: &egui::Context,
        columns: &[Column],
        rows: &[Vec<Cell>],
        state: &mut GridState,
        events: Vec<egui::Event>,
        dt: f64,
    ) -> Vec<(String, egui::Pos2)> {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            time: Some(ctx.input(|i| i.time) + dt),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            ResultGrid::new(columns, rows, state, "grid").show(ui);
        });
        output.textures_delta.clear();
        output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some((
                    t.galley.text().to_owned(),
                    t.pos + t.galley.rect.center().to_vec2(),
                )),
                _ => None,
            })
            .collect()
    }

    fn texts(drawn: &[(String, egui::Pos2)]) -> Vec<&str> {
        drawn.iter().map(|(t, _)| t.as_str()).collect()
    }

    /// Clicks (twice: double click) the text drawn last frame.
    fn click(
        ctx: &egui::Context,
        columns: &[Column],
        rows: &[Vec<Cell>],
        state: &mut GridState,
        text: &str,
        times: usize,
    ) {
        let drawn = frame(ctx, columns, rows, state, vec![]);
        let pos = drawn
            .iter()
            .find(|(t, _)| t == text)
            .unwrap_or_else(|| panic!("{text} not in {:?}", texts(&drawn)))
            .1;
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        // Long enough after the clicks before not to count with them.
        let moved = vec![egui::Event::PointerMoved(pos)];
        frame_after(ctx, columns, rows, state, moved, 1.0);
        for _ in 0..times {
            frame(ctx, columns, rows, state, vec![button(true)]);
            frame(ctx, columns, rows, state, vec![button(false)]);
        }
    }

    fn key(key: Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// The keys keep reaching the grid after the click that focused it
    /// (#143).
    #[test]
    fn keys_reach_the_grid_after_a_click() {
        let (c, r) = (cols(), rows());
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        click(&ctx, &c, &r, &mut state, "x", 1);
        assert_eq!(state.cursor, Some((0, 1)));
        for _ in 0..3 {
            frame(&ctx, &c, &r, &mut state, vec![]);
        }
        frame(&ctx, &c, &r, &mut state, vec![key(Key::ArrowDown)]);
        assert_eq!(state.cursor, Some((1, 1)));
        frame(&ctx, &c, &r, &mut state, vec![key(Key::ArrowDown)]);
        assert_eq!(state.cursor, Some((2, 1)));
    }

    /// The record view shows the pending edits and the inserted rows, and
    /// its fields edit like the grid's cells (#141).
    #[test]
    fn record_view_edits() {
        let (c, r) = (cols(), rows());
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        state.set_editable(None);
        state.edit_cell(&r, 0, 1, Some("changed".into()));
        state.add_row(&r, 2);
        state.cursor = Some((0, 1));
        state.set_record_view(&r, true);
        let drawn = frame(&ctx, &c, &r, &mut state, vec![]);
        let shown = texts(&drawn);
        assert!(shown.contains(&"Record 1 of 4"), "{shown:?}");
        assert!(
            shown.contains(&"changed") && !shown.contains(&"x"),
            "{shown:?}"
        );

        // Typing on a field edits it, Enter keeps the value.
        click(&ctx, &c, &r, &mut state, "10", 1);
        assert_eq!(state.cursor, Some((0, 0)));
        frame(
            &ctx,
            &c,
            &r,
            &mut state,
            vec![egui::Event::Text("7".into())],
        );
        assert!(texts(&frame(&ctx, &c, &r, &mut state, vec![])).contains(&"7"));
        frame(&ctx, &c, &r, &mut state, vec![key(Key::Enter)]);
        let changes = state.changes().unwrap();
        assert_eq!(changes.updates[&0][&0].as_deref(), Some("7"));
        assert!(state.edit.as_ref().unwrap().active.is_none());

        // A double click opens the editor, Escape drops it.
        click(&ctx, &c, &r, &mut state, "changed", 2);
        let active = state.edit.as_ref().unwrap().active.clone();
        assert_eq!(active, Some((0, 1, "changed".into())));
        frame(&ctx, &c, &r, &mut state, vec![]);
        frame(&ctx, &c, &r, &mut state, vec![key(Key::Escape)]);
        assert!(state.edit.as_ref().unwrap().active.is_none());
        assert_eq!(
            state.changes().unwrap().updates[&0][&1].as_deref(),
            Some("changed")
        );

        // Going to another record keeps an open edit; the inserted row is
        // the last record.
        state.start_edit(&r, (0, 1), Some("kept".into()));
        for _ in 0..3 {
            click(&ctx, &c, &r, &mut state, "▶", 1);
        }
        assert_eq!(
            state.changes().unwrap().updates[&0][&1].as_deref(),
            Some("kept")
        );
        let shown = frame(&ctx, &c, &r, &mut state, vec![]);
        let shown = texts(&shown);
        assert!(shown.contains(&"Record 4 of 4"), "{shown:?}");
        assert!(shown.contains(&"New row"), "{shown:?}");
        click(&ctx, &c, &r, &mut state, "NULL", 2);
        frame(&ctx, &c, &r, &mut state, vec![]);
        state.edit.as_mut().unwrap().active.as_mut().unwrap().2 = "11".into();
        frame(&ctx, &c, &r, &mut state, vec![key(Key::Enter)]);
        assert_eq!(
            state.changes().unwrap().inserts[0][0].as_deref(),
            Some("11")
        );

        // The filter leaves out the same rows as in the grid.
        state.set_filter("a");
        state.cursor = Some((5, 0));
        let shown = frame(&ctx, &c, &r, &mut state, vec![]);
        let shown = texts(&shown);
        assert!(shown.contains(&"Record 2 of 2"), "{shown:?}");
        assert!(shown.contains(&"11"), "{shown:?}");

        // Switching views keeps an open edit.
        state.start_edit(&r, (0, 1), Some("b".into()));
        state.set_record_view(&r, false);
        assert!(state.edit.as_ref().unwrap().active.is_none());
        assert_eq!(
            state.changes().unwrap().updates[&1][&1].as_deref(),
            Some("b")
        );
    }

    /// A value wraps in the record view's first frame, when its column is
    /// still narrow; the field is one line high again after that.
    #[test]
    fn record_view_rows_fit_their_fields() {
        let columns = cols();
        let rows = vec![vec![Some("1".into()), Some("2020-01-01 00:00:00".into())]];
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        state.set_editable(None);
        state.record_view = true;
        let mut drawn = Vec::new();
        for _ in 0..3 {
            drawn = frame(&ctx, &columns, &rows, &mut state, vec![]);
        }
        let y = |text: &str| drawn.iter().find(|(t, _)| t == text).unwrap().1.y;
        let (a, b) = (y("A"), y("B"));
        assert!(b - a < 40.0, "A at {a}, B at {b}");
    }
}
