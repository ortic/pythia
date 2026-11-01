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
use pythia_db::{Cell, Column, DbKind, Dialect};

/// The characters of a value's first line a cell lays out at most: more
/// than any column is wide, and a 10 MB line isn't laid out whole (#212).
const SHOWN_CHARS: usize = 500;

/// The characters of a value a field of the record view lays out at most:
/// a few screens of it, wrapped (#220).
const RECORD_CHARS: usize = 10_000;

/// The characters of a value the viewer shows in one text field at most;
/// a longer one shows in rows, laid out as they come into view (#284).
const VIEWER_CHARS: usize = 100_000;

/// The characters of a row of a long line in the viewer.
const VIEWER_ROW_CHARS: usize = 1_000;

/// The changes Ctrl+Z can take back at most, and the cells their states
/// hold together, oldest left out first (#257).
const UNDO_STEPS: usize = 100;
const UNDO_CELLS: usize = 1_000_000;

/// Per-result view state, owned by whoever owns the rows.
#[derive(Default)]
pub struct GridState {
    sort: Option<(usize, bool)>,
    /// View order: indices into `rows` (identity when unsorted).
    order: Vec<usize>,
    /// Row count `order` was computed for.
    ordered_rows: usize,
    /// Counts the times `order` was computed.
    order_revision: u64,
    /// The selection's summary, and what it was worked out for.
    summary: Option<(SummaryKey, Option<String>)>,
    /// Counts the times the pending changes changed, for the summary.
    changes_revision: u64,
    /// Selection anchor and cursor as (view row, column).
    anchor: Option<(usize, usize)>,
    cursor: Option<(usize, usize)>,
    dragging: bool,
    pub record_view: bool,
    /// Rows must contain this text (case-insensitive) in some cell.
    filter: String,
    viewer: Option<(usize, usize)>,
    /// What the viewer shows of its cell.
    viewed: Option<Viewed>,
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
    /// The time zone MySQL showed the rows' TIMESTAMP values in: INSERT
    /// statements set it (#213).
    pub time_zone: Option<String>,
    /// Why a copy failed, for the owner to show (a value held cut short).
    failed: Option<String>,
}

/// What the summary of a selection was worked out for: the selection,
/// the rows (where they are, how many), their order and the pending
/// changes (#277).
#[derive(PartialEq)]
struct SummaryKey {
    selection: Option<(RangeInclusive<usize>, Vec<usize>)>,
    rows: (usize, usize),
    order: u64,
    changes: u64,
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

    /// The cells these changes hold, for the size of the undo history.
    fn cells(&self) -> usize {
        self.updates.values().map(BTreeMap::len).sum::<usize>()
            + self.inserts.iter().map(Vec::len).sum::<usize>()
            + self.deletes.len()
    }
}

#[derive(Default)]
struct Editing {
    /// First of the trailing columns that can't be edited (the ROWID, the
    /// key columns of a MySQL table).
    locked: Option<usize>,
    /// Other columns that can't be edited: a query's expressions and
    /// renamed columns (#218).
    read_only: BTreeSet<usize>,
    changes: Changes,
    /// The changes as they were before each of the last ones, for Ctrl+Z
    /// (#257).
    undo: Vec<Changes>,
    /// Cell being edited: (view row, column, text).
    active: Option<(usize, usize, String)>,
    focus_editor: bool,
}

impl Editing {
    fn is_locked(&self, col: usize) -> bool {
        self.locked.is_some_and(|l| col >= l) || self.read_only.contains(&col)
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
    /// table they come from (else a placeholder name), in the time zone
    /// they were shown in.
    pub fn export_options(&self) -> pythia_data::Options {
        let defaults = pythia_data::Options::default();
        pythia_data::Options {
            dialect: self.dialect,
            table: self.table.clone().unwrap_or(defaults.table.clone()),
            time_zone: self.time_zone.clone(),
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

    /// Why *Copy as* copied nothing, once.
    pub fn take_failed(&mut self) -> Option<String> {
        self.failed.take()
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
        let order = self.order_of(rows, columns);
        // Rows fetched since, sorted in among the others or after them, or
        // a sort or a filter (`set_sort`, `set_filter`).
        if self.order_revision > 0 {
            let chosen = self.ordered_rows == usize::MAX;
            self.follow_rows(rows, &order, chosen);
        }
        self.order = order;
        self.ordered_rows = rows.len();
        self.order_revision += 1;
    }

    /// Moves what is held by view row (the selection, the cursor, the cell
    /// being edited, the viewer and the dialog's cell) to where its row is
    /// in `order`, the rows' new order: they stayed in place, on other rows
    /// (#279, #296). New rows come after the fetched ones. After a sort or
    /// a filter (`chosen`), a selection of several rows, which don't stand
    /// together in another order, keeps the cursor's row. On a row the
    /// filter hides, the viewer and the dialog close, and the value being
    /// typed is kept.
    fn follow_rows(&mut self, rows: &[Vec<Cell>], order: &[usize], chosen: bool) {
        let mut place = vec![None; rows.len()];
        for (at, &r) in order.iter().enumerate() {
            place[r] = Some(at);
        }
        let now = order.len();
        let to = |old: &[usize], view_row: usize| match old.get(view_row) {
            Some(&r) => place.get(r).copied().flatten(),
            None => Some(view_row - old.len() + now),
        };
        if let Some((view_row, ..)) = self.edit.as_ref().and_then(|e| e.active.as_ref())
            && to(&self.order, *view_row).is_none()
        {
            self.finish_edit(rows, true);
        }
        let old = std::mem::take(&mut self.order);
        let at = |(view_row, col): (usize, usize)| Some((to(&old, view_row)?, col));
        let cursor = self.cursor.and_then(at);
        self.anchor = match (self.anchor, cursor) {
            (Some((_, col)), Some((view_row, _))) if chosen => Some((view_row, col)),
            (anchor, _) => anchor.and_then(at),
        };
        self.cursor = cursor;
        self.viewer = self.viewer.and_then(at);
        self.value_editor = self.value_editor.take().and_then(|mut editor| {
            editor.view_row = to(&old, editor.view_row)?;
            Some(editor)
        });
        if let Some((view_row, ..)) = self.edit.as_mut().and_then(|e| e.active.as_mut())
            && let Some(moved) = to(&old, *view_row)
        {
            *view_row = moved;
        }
    }

    /// The rows passing the filter, in the order shown.
    fn order_of(&self, rows: &[Vec<Cell>], columns: &[Column]) -> Vec<usize> {
        let needle = self.filter.trim().to_lowercase();
        let mut order: Vec<usize> = if needle.is_empty() {
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
        if let Some((col, asc)) = self.sort {
            sort_order(
                &mut order,
                rows,
                col,
                columns.get(col).is_some_and(|c| c.numeric),
                asc,
            );
        }
        order
    }

    /// [`summary`] of the selection, worked out again only when the
    /// selection or the rows change: over a million selected cells it took
    /// a quarter of a second, on every frame (#241).
    fn selection_summary(&mut self, columns: &[Column], rows: &[Vec<Cell>]) -> Option<String> {
        let key = SummaryKey {
            selection: self.selection(columns.len()),
            rows: (rows.as_ptr().addr(), rows.len()),
            order: self.order_revision,
            changes: self.changes_revision,
        };
        if let Some((kept, summary)) = &self.summary
            && *kept == key
        {
            return summary.clone();
        }
        let summary = summary(columns, rows, self);
        self.summary = Some((key, summary.clone()));
        summary
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

    /// Of `columns`, those copies and exports take: not the ones editing
    /// added to find the rows (the ROWID, a key as `pythia$…`), which the
    /// table hasn't (#249).
    fn copied(&self, columns: Vec<usize>) -> Result<Vec<usize>, NotCopied> {
        let added = self.edit.as_ref().and_then(|e| e.locked);
        let table: Vec<usize> = columns
            .iter()
            .copied()
            .filter(|&c| added.is_none_or(|a| c < a))
            .collect();
        if table.is_empty() && !columns.is_empty() {
            Err(NotCopied::KeyOnly)
        } else {
            Ok(table)
        }
    }

    /// How many of `rows_taken` hold a change in `columns` that isn't
    /// applied: a value changed or being typed, a new row, a row to be
    /// deleted (#250).
    fn unapplied(
        &self,
        rows: &[Vec<Cell>],
        rows_taken: impl IntoIterator<Item = RowRef>,
        columns: &[usize],
    ) -> usize {
        let Some(edit) = &self.edit else {
            return 0;
        };
        let typed = edit
            .active
            .as_ref()
            .filter(|(r, c, text)| {
                columns.contains(c) && self.value(rows, *r, *c).unwrap_or_default() != text
            })
            .map(|(r, ..)| self.row_ref(*r));
        rows_taken
            .into_iter()
            .filter(|&row| {
                typed == Some(row)
                    || match row {
                        RowRef::New(i) => i < edit.changes.inserts.len(),
                        RowRef::Existing(r) => {
                            edit.changes.deletes.contains(&r)
                                || edit
                                    .changes
                                    .updates
                                    .get(&r)
                                    .is_some_and(|u| columns.iter().any(|c| u.contains_key(c)))
                        }
                    }
            })
            .count()
    }

    /// A cell as the grid shows it, for the viewer, and whether that is a
    /// change not applied, which isn't copied (#250).
    fn viewed(&self, rows: &[Vec<Cell>], (view_row, col): (usize, usize)) -> (Cell, bool) {
        let value = self.value(rows, view_row, col).map(str::to_owned);
        let row = self.row_ref(view_row);
        (value, self.unapplied(rows, [row], &[col]) > 0)
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
        self.changes_revision += 1;
    }

    /// Keeps `columns` of the editable grid read-only, e.g. those that
    /// aren't the table's column of their name (#218).
    pub fn lock_columns(&mut self, columns: impl IntoIterator<Item = usize>) {
        if let Some(edit) = &mut self.edit {
            edit.read_only.extend(columns);
        }
    }

    /// Changes a cell programmatically (as if edited by the user).
    pub fn edit_cell(&mut self, rows: &[Vec<Cell>], view_row: usize, col: usize, value: Cell) {
        self.sync(rows, &[]);
        self.undoable(|s| s.set_value(rows, view_row, col, value));
    }

    /// Adds a new row programmatically (as if by the user).
    pub fn add_row(&mut self, rows: &[Vec<Cell>], columns: usize) {
        self.sync(rows, &[]);
        self.undoable(|s| s.insert_row(columns));
    }

    /// Marks a fetched row for deletion programmatically (as if by the user).
    pub fn delete_row(&mut self, rows: &[Vec<Cell>], view_row: usize) {
        self.sync(rows, &[]);
        if let RowRef::Existing(r) = self.row_ref(view_row) {
            self.undoable(|s| {
                if let Some(edit) = &mut s.edit {
                    edit.changes.deletes.insert(r);
                }
            });
        }
    }

    pub fn set_read_only(&mut self) {
        self.edit = None;
        self.changes_revision += 1;
    }

    pub fn is_editable(&self) -> bool {
        self.edit.is_some()
    }

    pub fn changes(&self) -> Option<&Changes> {
        self.edit.as_ref().map(|e| &e.changes)
    }

    /// Edits not applied yet: changes waiting for Apply, or a value being
    /// typed into a cell.
    pub fn has_unapplied(&self, rows: &[Vec<Cell>]) -> bool {
        let Some(edit) = &self.edit else {
            return false;
        };
        !edit.changes.is_empty()
            || edit.active.as_ref().is_some_and(|(row, col, text)| {
                self.value(rows, *row, *col).unwrap_or_default() != text
            })
    }

    /// Replaces the pending changes, e.g. with those kept for the rows
    /// fetched again. The changes before them were made to other rows:
    /// there's nothing to take back.
    pub fn set_changes(&mut self, changes: Changes) {
        if let Some(e) = &mut self.edit {
            e.changes = changes;
            e.undo.clear();
            self.changes_revision += 1;
        }
    }

    pub fn clear_changes(&mut self) {
        if let Some(e) = &mut self.edit {
            e.changes = Changes::default();
            e.undo.clear();
            e.active = None;
            self.changes_revision += 1;
        }
    }

    /// Makes `change` to the pending changes a step Ctrl+Z takes back
    /// (#257).
    fn undoable(&mut self, change: impl FnOnce(&mut Self)) {
        let before = self.edit.as_ref().map(|e| e.changes.clone());
        change(self);
        let (Some(before), Some(edit)) = (before, &mut self.edit) else {
            return;
        };
        if edit.changes == before {
            return;
        }
        self.changes_revision += 1;
        edit.undo.push(before);
        let mut cells = 0;
        let kept = edit
            .undo
            .iter()
            .rev()
            .take(UNDO_STEPS)
            .take_while(|c| {
                cells += c.cells();
                cells <= UNDO_CELLS
            })
            .count()
            .max(1);
        edit.undo.drain(..edit.undo.len() - kept);
    }

    /// Whether Ctrl+Z has a change to take back.
    fn can_undo(&self) -> bool {
        self.edit.as_ref().is_some_and(|e| !e.undo.is_empty())
    }

    /// Takes back the last change not applied (#257).
    fn undo(&mut self, rows: &[Vec<Cell>]) {
        let Some(edit) = &mut self.edit else { return };
        let Some(before) = edit.undo.pop() else {
            return;
        };
        edit.changes = before;
        self.changes_revision += 1;
        // Rows inserted may be gone again.
        let last = self.view_len(rows).saturating_sub(1);
        for (row, _) in [&mut self.anchor, &mut self.cursor].into_iter().flatten() {
            *row = (*row).min(last);
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

    /// Whether a view row is a fetched row marked for deletion.
    fn to_delete(&self, view_row: usize) -> bool {
        match (&self.edit, self.row_ref(view_row)) {
            (Some(edit), RowRef::Existing(r)) => edit.changes.deletes.contains(&r),
            _ => false,
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
        self.undoable(|s| s.delete_refs(&refs));
    }

    fn delete_refs(&mut self, refs: &[RowRef]) {
        let Some(edit) = &mut self.edit else { return };
        let mut new_rows: Vec<usize> = Vec::new();
        for &r in refs {
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
        self.undoable(|s| {
            for row in r {
                for &col in &c {
                    s.set_value(rows, row, col, None);
                }
            }
        });
    }

    /// Pastes tab separated text (e.g. an Excel range) at the cursor, adding
    /// new rows past the end. Empty cells become NULL.
    pub fn paste(&mut self, rows: &[Vec<Cell>], columns: usize, text: &str) {
        self.paste_cells(rows, columns, &read_tsv(text));
    }

    /// Pastes rows of cells at the cursor, adding new rows past the end.
    fn paste_cells(&mut self, rows: &[Vec<Cell>], columns: usize, cells: &[Vec<Cell>]) {
        if self.edit.is_none() {
            return;
        }
        self.sync(rows, &[]);
        self.undoable(|s| s.paste_rows(rows, columns, cells));
    }

    fn paste_rows(&mut self, rows: &[Vec<Cell>], columns: usize, cells: &[Vec<Cell>]) {
        let visible = self.visible_columns(columns);
        let (r0, c0) = self
            .cursor
            .unwrap_or((0, visible.first().copied().unwrap_or(0)));
        // From the cursor's column on, in the order shown.
        let from = visible.iter().position(|&c| c == c0).unwrap_or(0);
        let targets: Vec<usize> = visible[from..]
            .iter()
            .copied()
            .filter(|&c| self.edit.as_ref().is_some_and(|e| !e.is_locked(c)))
            .collect();
        let mut last = None;
        for (i, line) in cells.iter().enumerate() {
            let view_row = r0 + i;
            while view_row >= self.view_len(rows) {
                self.insert_row(columns);
            }
            for (cell, &col) in line.iter().zip(&targets) {
                self.set_value(rows, view_row, col, cell.clone());
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
        if !fits_inline(value.as_deref()) && initial.is_none() {
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
        // A NULL cell's editor opens empty: left so, the cell stays NULL
        // rather than becoming '', which a date or a number refuses (#262).
        let unchanged = text.is_empty() && self.value(rows, r, c).is_none();
        if commit && !unchanged {
            self.undoable(|s| s.set_value(rows, r, c, Some(text)));
        }
    }

    /// Ends an edit as `end` says.
    fn end_edit(&mut self, rows: &[Vec<Cell>], columns: usize, end: EditEnd) {
        match end {
            EditEnd::Drop => self.finish_edit(rows, false),
            EditEnd::Keep => self.finish_edit(rows, true),
            EditEnd::Next(back) => self.edit_next(rows, columns, back),
        }
    }

    /// Keeps the value being typed and edits the next cell that can be
    /// edited, or the one before it (`back`): along the row, then on to
    /// the next one (#254). Past the last cell, the cursor stays.
    fn edit_next(&mut self, rows: &[Vec<Cell>], columns: usize, back: bool) {
        let Some((r, c)) = self
            .edit
            .as_ref()
            .and_then(|e| e.active.as_ref())
            .map(|(r, c, _)| (*r, *c))
        else {
            return;
        };
        self.finish_edit(rows, true);
        let visible = self.visible_columns(columns);
        let Some(edit) = &self.edit else { return };
        let editable: Vec<usize> = visible
            .into_iter()
            .filter(|&col| !edit.is_locked(col))
            .collect();
        let Some(at) = editable.iter().position(|&col| col == c) else {
            return;
        };
        // Cells counted along the rows.
        let at = r * editable.len() + at;
        let next = if back {
            at.checked_sub(1)
        } else {
            Some(at + 1).filter(|&n| n < self.view_len(rows) * editable.len())
        };
        let Some(next) = next else { return };
        let cell = (next / editable.len(), editable[next % editable.len()]);
        self.select(cell, false);
        self.scroll_to_cursor = true;
        // A value the one-line editor can't hold waits for Enter or F2,
        // which open the dialog.
        if fits_inline(self.value(rows, cell.0, cell.1)) {
            self.start_edit(rows, cell, None);
        }
    }
}

/// How the edit of a cell ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditEnd {
    /// Escape: what was typed is dropped.
    Drop,
    /// Enter, or the focus went elsewhere: the value is kept.
    Keep,
    /// Tab (`true`: Shift+Tab): the value is kept and the next cell (the
    /// one before) is edited.
    Next(bool),
}

/// How a key ends the edit of a cell this frame, if one does.
fn edit_end(i: &egui::InputState, lost_focus: bool) -> Option<EditEnd> {
    // The key's own modifiers: Shift may be let go of in the same frame.
    let tab = i.events.iter().find_map(|e| match e {
        egui::Event::Key {
            key: Key::Tab,
            pressed: true,
            modifiers,
            ..
        } => Some(modifiers.shift),
        _ => None,
    });
    if i.key_pressed(Key::Escape) {
        Some(EditEnd::Drop)
    } else if let Some(back) = tab {
        Some(EditEnd::Next(back))
    } else if i.key_pressed(Key::Enter) || lost_focus {
        Some(EditEnd::Keep)
    } else {
        None
    }
}

/// Whether the one-line editor can hold a value: it would lose line
/// breaks, and long text goes to the dialog.
fn fits_inline(value: Option<&str>) -> bool {
    value.is_none_or(|v| !v.contains('\n') && v.chars().count() <= 200)
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
            (Some(x), Some(y)) if numeric => compare_numbers(x, y).unwrap_or_else(|| x.cmp(y)),
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
        out.selection_summary = state.selection_summary(columns, rows);
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
    let mut undo = false;
    let mut paste = None;
    let editable = state.is_editable();
    ui.input(|i| {
        extend = i.modifiers.shift;
        let ctrl = i.modifiers.command;
        for e in &i.events {
            match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
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
                    Key::Z if modifiers.command && !modifiers.shift && editable => undo = true,
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
    if undo {
        state.undo(rows);
    }
    if let Some(text) = paste {
        let cells = pasted_cells(ui.ctx(), &text);
        state.paste_cells(rows, columns.len(), &cells);
    }
    if let Some(with_header) = copy {
        copy_selection(ui, columns, rows, state, with_header);
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
    edit_done: Option<EditEnd>,
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
        const MAX_CHARS: usize = 48;
        let content = self
            .rows
            .iter()
            .take(200)
            .filter_map(|r| r.get(c)?.as_deref())
            // No further than the widest column (#212).
            .map(|v| {
                v.lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(MAX_CHARS)
                    .count()
            })
            .max()
            .unwrap_or(4);
        let chars = content.max(self.columns[c].name.chars().count() + 2).max(4) as f32;
        (chars * self.look.char_width + 16.0).min(self.look.char_width * MAX_CHARS as f32)
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
    if let Some(end) = act.edit_done {
        state.end_edit(rows, columns.len(), end);
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
    // While editing, the columns that can't be edited are weak (#218).
    let locked = state.edit.as_ref().is_some_and(|e| e.is_locked(c));
    let text = RichText::new(format!("{}{arrow}", col.name));
    let text = if locked { text.weak() } else { text.strong() };
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
        let read_only = if locked {
            "\nRead-only: the key, an expression or a renamed column"
        } else {
            ""
        };
        r.on_hover_text(format!(
            "{}{read_only}\nClick to sort, drag to move, right-click for more",
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
    // What the cell shows of the value, not a copy of all of it (#212).
    let binary = is_binary(state.dialect, col);
    let shown = state.value(d.rows, view_row, c).map(|text| {
        if binary {
            binary_summary(text)
        } else {
            cell_text(text)
        }
    });
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
            let lost_focus = r.lost_focus();
            edit_done = ui.input(|i| edit_end(i, lost_focus));
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
        ui.with_layout(layout, |ui| match shown {
            Some(summary) if binary => {
                ui.add(
                    egui::Label::new(look.text(summary).color(ui.visuals().weak_text_color()))
                        .truncate()
                        .selectable(false),
                );
            }
            Some(text) => {
                let mut rich = look.text(text);
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
            state.undoable(|s| s.insert_row(columns.len()));
            ui.close();
        }
        if ui.button("Delete / undelete rows").clicked() {
            state.delete_rows(columns.len());
            ui.close();
        }
        if ui
            .add_enabled(state.can_undo(), egui::Button::new("Undo"))
            .on_hover_text("Take back the last change (Ctrl+Z)")
            .clicked()
        {
            state.undo(rows);
            ui.close();
        }
        ui.separator();
    }
    if ui.button("Copy").clicked() {
        copy_selection(ui, columns, rows, state, false);
        ui.close();
    }
    if ui.button("Copy with header").clicked() {
        copy_selection(ui, columns, rows, state, true);
        ui.close();
    }
    ui.menu_button("Copy as", |ui| {
        for format in Format::ALL
            .iter()
            .filter(|f| f.is_text() && **f != Format::Tsv)
        {
            if ui.button(format.label()).clicked() {
                let copied = selection_data(columns, rows, state)
                    .map_err(|e| e.to_string())
                    .and_then(|(cols, data)| {
                        pythia_data::export_to_vec(*format, &cols, &data, &state.export_options())
                            .map_err(|e| e.to_string())
                    });
                match copied {
                    Ok(bytes) => ui
                        .ctx()
                        .copy_text(String::from_utf8_lossy(&bytes).into_owned()),
                    Err(e) => state.failed = Some(format!("Copy as {}: {e}", format.label())),
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

/// Why a copy or an export of a grid's rows takes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotCopied {
    /// Only the columns editing added to find the rows are selected or
    /// shown (#249).
    KeyOnly,
    /// This many of the rows have changes that aren't applied (#250).
    Unapplied(usize),
}

impl std::fmt::Display for NotCopied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotCopied::KeyOnly => f.write_str(
                "the selection has only the key columns Pythia added to find the rows, which \
                 the table hasn't",
            ),
            NotCopied::Unapplied(1) => f.write_str(
                "a row has changes that aren't applied: apply or discard them, then copy it",
            ),
            NotCopied::Unapplied(n) => write!(
                f,
                "{n} rows have changes that aren't applied: apply or discard them, then copy \
                 them"
            ),
        }
    }
}

/// The selected block as columns + rows (in view order); everything if
/// nothing is selected. Not while those rows hold changes that aren't
/// applied (#250).
pub fn selection_data(
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &GridState,
) -> Result<(Vec<Column>, Vec<Vec<Cell>>), NotCopied> {
    let Some((r, keep)) = state.selection(columns.len()) else {
        return shown_data(columns, rows, state);
    };
    let keep = state.copied(keep)?;
    let pending = state.unapplied(rows, r.clone().map(|vr| state.row_ref(vr)), &keep);
    if pending > 0 {
        return Err(NotCopied::Unapplied(pending));
    }
    let cols = keep.iter().map(|&i| columns[i].clone()).collect();
    let data = r
        .filter_map(|vr| rows.get(state.row_index(vr)))
        .map(|row| {
            keep.iter()
                .map(|&i| row.get(i).cloned().flatten())
                .collect()
        })
        .collect();
    Ok((cols, data))
}

/// The fetched rows as shown, those the filter passes in the order shown
/// (#243), with the columns shown, in the order shown, but those editing
/// added (#249).
pub fn shown_data(
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &GridState,
) -> Result<(Vec<Column>, Vec<Vec<Cell>>), NotCopied> {
    let keep = state.copied(state.visible_columns(columns.len()))?;
    // Also the rows fetched since the grid was drawn.
    let order = state.order_of(rows, columns);
    let inserted = state.edit.as_ref().map_or(0, |e| e.changes.inserts.len());
    let shown = order
        .iter()
        .map(|&r| RowRef::Existing(r))
        .chain((0..inserted).map(RowRef::New));
    let pending = state.unapplied(rows, shown, &keep);
    if pending > 0 {
        return Err(NotCopied::Unapplied(pending));
    }
    let cols = keep.iter().map(|&i| columns[i].clone()).collect();
    let data = order
        .iter()
        .map(|&r| {
            keep.iter()
                .map(|&i| rows[r].get(i).cloned().flatten())
                .collect()
        })
        .collect();
    Ok((cols, data))
}

/// Copies the selection as tab separated text, or says why it can't.
fn copy_selection(
    ui: &Ui,
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &mut GridState,
    header: bool,
) {
    match selection_cells(columns, rows, state, header) {
        Ok(cells) => copy_cells(ui.ctx(), cells),
        Err(e) => state.failed = Some(format!("Copy: {e}")),
    }
}

/// The selection's cells as Copy takes them, the names of its columns
/// first if `header`.
fn selection_cells(
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &GridState,
    header: bool,
) -> Result<Vec<Vec<Cell>>, NotCopied> {
    let (cols, data) = selection_data(columns, rows, state)?;
    Ok(with_header(header.then_some(&cols[..]), data))
}

#[cfg(test)]
fn selection_text(
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &GridState,
    header: bool,
) -> Result<String, NotCopied> {
    selection_cells(columns, rows, state, header).map(|cells| tsv(&cells))
}

/// `rows`, after a row of the names of `columns` if there are any.
fn with_header(columns: Option<&[Column]>, rows: Vec<Vec<Cell>>) -> Vec<Vec<Cell>> {
    let Some(columns) = columns else {
        return rows;
    };
    let names = columns.iter().map(|c| Some(c.name.clone())).collect();
    std::iter::once(names).chain(rows).collect()
}

/// Copies `rows` as tab separated text with a header line, as *Copy all*
/// does, kept for a paste like [`copy_selection`]'s.
pub fn copy_rows(ctx: &egui::Context, columns: &[Column], rows: Vec<Vec<Cell>>) {
    copy_cells(ctx, with_header(Some(columns), rows));
}

/// What Copy put on the clipboard last: the cells it copied, NULLs as
/// NULL. The text writes a NULL as an empty cell, as other programs read
/// it, and an empty text the same; a paste of that text takes the cells
/// as they were copied (#275).
#[derive(Clone)]
struct Copied {
    /// The text, as [`clipboard_key`] has it.
    key: (u64, usize),
    cells: std::sync::Arc<Vec<Vec<Cell>>>,
}

fn copied_id() -> egui::Id {
    egui::Id::new("pythia-grid-copied")
}

/// Tells a text pasted back from others: its hash and length, without the
/// CRLF line breaks and the line break at the end a clipboard may add.
fn clipboard_key(text: &str) -> (u64, usize) {
    let text = text.replace("\r\n", "\n");
    let text = text.strip_suffix('\n').unwrap_or(&text);
    (egui::util::hash(text), text.len())
}

/// Copies `cells` as tab separated text, and keeps them for a paste of it.
fn copy_cells(ctx: &egui::Context, cells: Vec<Vec<Cell>>) {
    let text = tsv(&cells);
    let copied = Copied {
        key: clipboard_key(&text),
        cells: std::sync::Arc::new(cells),
    };
    ctx.data_mut(|d| d.insert_temp(copied_id(), copied));
    ctx.copy_text(text);
}

/// The cells of a pasted text: those Copy kept for it, or the text read
/// as tab separated, an empty cell as NULL.
fn pasted_cells(ctx: &egui::Context, text: &str) -> std::sync::Arc<Vec<Vec<Cell>>> {
    match ctx.data(|d| d.get_temp::<Copied>(copied_id())) {
        Some(copied) if copied.key == clipboard_key(text) => copied.cells,
        _ => std::sync::Arc::new(read_tsv(text)),
    }
}

/// Rows of cells as tab separated text: a NULL is an empty cell.
fn tsv(cells: &[Vec<Cell>]) -> String {
    let lines: Vec<String> = cells
        .iter()
        .map(|row| tsv_line(row.iter().map(Option::as_deref)))
        .collect();
    lines.join("\n")
}

/// A line of tab separated text: a NULL is an empty cell.
fn tsv_line<'c>(cells: impl IntoIterator<Item = Option<&'c str>>) -> String {
    cells
        .into_iter()
        .map(|c| tsv_cell(c.unwrap_or("")))
        .collect::<Vec<_>>()
        .join("\t")
}

/// A value as a cell of tab separated text: in double quotes, its quotes
/// doubled, when it has a tab or a line break or starts with a quote, as
/// spreadsheets write it (#276).
fn tsv_cell(value: &str) -> std::borrow::Cow<'_, str> {
    if value.contains(['\t', '\n', '\r']) || value.starts_with('"') {
        format!("\"{}\"", value.replace('"', "\"\"")).into()
    } else {
        value.into()
    }
}

/// Tab separated text, e.g. a spreadsheet's range, as rows of cells: an
/// empty cell is NULL. A cell in double quotes is the text in them, with
/// its tabs and line breaks and its doubled quotes single (#276), and
/// `""` an empty text; a quote that doesn't end the cell is text.
fn read_tsv(text: &str) -> Vec<Vec<Cell>> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    let mut rows = vec![Vec::new()];
    let mut rest = text;
    loop {
        let (cell, after) = match quoted_cell(rest) {
            Some((cell, after)) => (Some(cell), after),
            None => {
                let end = rest.find(['\t', '\n']).unwrap_or(rest.len());
                let (mut cell, after) = rest.split_at(end);
                if !after.starts_with('\t') {
                    cell = cell.trim_end_matches('\r');
                }
                ((!cell.is_empty()).then(|| cell.to_owned()), after)
            }
        };
        rows.last_mut().expect("a row").push(cell);
        match after.as_bytes().first() {
            Some(b'\t') => rest = &after[1..],
            Some(b'\n') => {
                rows.push(Vec::new());
                rest = &after[1..];
            }
            _ => return rows,
        }
    }
}

/// The cell in double quotes `text` starts with, and what follows it: a
/// tab, a line break or nothing. `None` where it doesn't start with one.
fn quoted_cell(text: &str) -> Option<(String, &str)> {
    let mut rest = text.strip_prefix('"')?;
    let mut cell = String::new();
    loop {
        let quote = rest.find('"')?;
        cell.push_str(&rest[..quote]);
        rest = &rest[quote + 1..];
        match rest.strip_prefix('"') {
            Some(after) => {
                cell.push('"');
                rest = after;
            }
            None => {
                let after = rest
                    .strip_prefix('\r')
                    .filter(|a| a.starts_with('\n'))
                    .unwrap_or(rest);
                let ends = after.is_empty() || after.starts_with(['\t', '\n']);
                return ends.then_some((cell, after));
            }
        }
    }
}

/// The count, sum, average, minimum and maximum of the selected cells as
/// the grid shows them, with the changes waiting for Apply; rows to be
/// deleted are left out, as Apply leaves them (#277).
fn summary(columns: &[Column], rows: &[Vec<Cell>], state: &GridState) -> Option<String> {
    let (r, c) = state.selection(columns.len())?;
    let shown: Vec<usize> = r.filter(|&vr| !state.to_delete(vr)).collect();
    let cells = shown.len() * c.len();
    if cells < 2 {
        return None;
    }
    let mut numbers = Vec::new();
    let mut non_null = 0;
    for vr in shown {
        for &ci in &c {
            if let Some(v) = state.value(rows, vr, ci) {
                non_null += 1;
                if columns.get(ci).is_some_and(|col| col.numeric) && v.parse::<f64>().is_ok() {
                    numbers.push(v);
                }
            }
        }
    }
    if numbers.is_empty() {
        return Some(format!("{cells} cells · {non_null} not null"));
    }
    // The smallest and largest as they are, not as doubles (#238).
    let pick = |keep: Ordering| {
        numbers
            .iter()
            .copied()
            .reduce(|a, b| {
                if compare_numbers(b, a) == Some(keep) {
                    b
                } else {
                    a
                }
            })
            .unwrap_or_default()
    };
    let (min, max) = (pick(Ordering::Less), pick(Ordering::Greater));
    let (sum, avg) = exact_sum(&numbers).unwrap_or_else(|| {
        let sum: f64 = numbers.iter().filter_map(|n| n.parse::<f64>().ok()).sum();
        (fmt_num(sum), fmt_num(sum / numbers.len() as f64))
    });
    Some(format!(
        "Count {non_null} · Sum {sum} · Avg {avg} · Min {min} · Max {max}"
    ))
}

/// Two numbers in order: plain decimals exactly, whatever their size
/// (#238), others (`1e300`) as doubles; `None` if one isn't a number.
fn compare_numbers(x: &str, y: &str) -> Option<Ordering> {
    pythia_data::compare_decimals(x, y)
        .or_else(|| x.parse::<f64>().ok()?.partial_cmp(&y.parse::<f64>().ok()?))
}

/// The sum and the average (with four more decimals) of plain decimals,
/// exact while the sum fits an `i128` of their smallest unit (#238).
fn exact_sum(numbers: &[&str]) -> Option<(String, String)> {
    let units: Vec<(i128, u32)> = numbers
        .iter()
        .map(|n| decimal_units(n))
        .collect::<Option<_>>()?;
    let scale = units.iter().map(|&(_, s)| s).max()?;
    let mut sum: i128 = 0;
    for (value, s) in units {
        sum = sum.checked_add(value.checked_mul(10i128.checked_pow(scale - s)?)?)?;
    }
    let n = numbers.len() as i128;
    // Rounded half away from zero.
    let scaled = sum.checked_mul(10_000)?;
    let half = if scaled < 0 { -(n / 2) } else { n / 2 };
    let avg = scaled.checked_add(half)? / n;
    Some((decimal_text(sum, scale), decimal_text(avg, scale + 4)))
}

/// A plain decimal as a count of its smallest unit and the digits after
/// the point: `12.50` is (125, 1). `None` for other numbers (`1e300`) and
/// past what an `i128` holds.
fn decimal_units(text: &str) -> Option<(i128, u32)> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let decimal = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && fraction.is_empty()) || !decimal(whole) || !decimal(fraction) {
        return None;
    }
    let fraction = fraction.trim_end_matches('0');
    let all = format!("{whole}{fraction}");
    let value: i128 = if all.is_empty() { 0 } else { all.parse().ok()? };
    Some((if negative { -value } else { value }, fraction.len() as u32))
}

/// `units` of 10^-`scale` as a decimal without trailing zeros: (125, 1) is
/// `12.5`.
fn decimal_text(units: i128, scale: u32) -> String {
    let scale = scale as usize;
    let digits = format!("{:0>width$}", units.unsigned_abs(), width = scale + 1);
    let (whole, fraction) = digits.split_at(digits.len() - scale);
    let fraction = fraction.trim_end_matches('0');
    let sign = if units < 0 { "-" } else { "" };
    if fraction.is_empty() {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{fraction}")
    }
}

/// A sum or an average of doubles as floats are shown (#278): with an
/// exponent below 1e-4 and from 1e16, plainly in between (#261), to 15
/// significant digits, past which a double's digits aren't the numbers'.
/// Rounded to four decimals, 2e-20 read 0.
fn fmt_num(n: f64) -> String {
    let rounded = format!("{n:.14e}").parse().unwrap_or(n);
    pythia_db::float_text(rounded)
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
                            let lost_focus = response.lost_focus();
                            edit_done = ui.input(|i| edit_end(i, lost_focus));
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
                            Some(v) => look.text(record_text(v)),
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
    if let Some(end) = edit_done {
        state.end_edit(rows, columns.len(), end);
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
        state.undoable(|s| s.set_value(rows, view_row, col, value));
    } else if !open {
        state.value_editor = None;
    }
}

/// What the viewer shows of a cell, worked out when it opens on the cell
/// and when the value changes, not on every frame (#284).
struct Viewed {
    key: ViewedKey,
    value: Cell,
    /// The value is a change not applied, which isn't copied (#250).
    unapplied: bool,
    chars: usize,
    /// The value as formatted JSON, if it is an object or an array.
    pretty: Option<String>,
    /// Where the rows of a text over [`VIEWER_CHARS`] start, which are
    /// laid out as they come into view, and whether a line was cut.
    rows: Option<(Vec<usize>, bool)>,
    binary: Option<ViewedBinary>,
}

/// What the viewer was worked out for: the cell, the rows (where they
/// are, how many), their order and the pending changes.
#[derive(PartialEq)]
struct ViewedKey {
    cell: (usize, usize),
    rows: (usize, usize),
    order: u64,
    changes: u64,
}

/// A binary value as the viewer shows it.
struct ViewedBinary {
    bytes: Vec<u8>,
    /// The size of a value held cut short (#232).
    cut: Option<u64>,
    /// Named by its bytes: egui keeps an image by its URI, and the next
    /// value in this cell (of another result, or edited) would show this
    /// one (#231).
    image: Option<String>,
    dump: String,
}

impl Viewed {
    fn new(key: ViewedKey, columns: &[Column], rows: &[Vec<Cell>], state: &GridState) -> Self {
        let (value, unapplied) = state.viewed(rows, key.cell);
        let binary = columns
            .get(key.cell.1)
            .is_some_and(|col| is_binary(state.dialect, col));
        let binary = value.as_deref().filter(|_| binary).and_then(|v| {
            let bytes = decode_hex(held_hex(v))?;
            let cut = pythia_db::cut_size(v);
            let image = image_kind(&bytes).filter(|_| cut.is_none()).map(|kind| {
                format!(
                    "bytes://pythia-grid/{:016x}.{kind}",
                    egui::util::hash(&bytes)
                )
            });
            let mut dump = hex_dump(&bytes, 4096);
            if let Some(size) = cut {
                let more = (size as usize).saturating_sub(bytes.len());
                dump.push_str(&format!(
                    "… {} more, not held by the grid",
                    human_bytes(more)
                ));
            }
            Some(ViewedBinary {
                bytes,
                cut,
                image,
                dump,
            })
        });
        let pretty = value
            .as_deref()
            .filter(|_| binary.is_none())
            .and_then(|v| serde_json::from_str::<serde_json::Value>(v.trim()).ok())
            .filter(|j| j.is_object() || j.is_array())
            .and_then(|j| serde_json::to_string_pretty(&j).ok());
        let text = pretty.as_deref().or(value.as_deref()).unwrap_or_default();
        let rows =
            (binary.is_none() && text.chars().nth(VIEWER_CHARS).is_some()).then(|| text_rows(text));
        Self {
            key,
            chars: value.as_deref().map_or(0, |v| v.chars().count()),
            value,
            unapplied,
            pretty,
            rows,
            binary,
        }
    }

    /// The text shown: the formatted JSON, or the value.
    fn text(&self) -> &str {
        self.pretty
            .as_deref()
            .or(self.value.as_deref())
            .unwrap_or_default()
    }
}

/// Where the rows of a long text start: at each of its lines, and every
/// [`VIEWER_ROW_CHARS`] characters of a longer line; and whether a line
/// was cut so.
fn text_rows(text: &str) -> (Vec<usize>, bool) {
    let mut starts = vec![0];
    let mut chars = 0;
    let mut cut = false;
    for (i, c) in text.char_indices() {
        if c == '\n' {
            starts.push(i + 1);
            chars = 0;
        } else {
            if chars == VIEWER_ROW_CHARS {
                starts.push(i);
                chars = 0;
                cut = true;
            }
            chars += 1;
        }
    }
    (starts, cut)
}

fn viewer(
    ctx: &egui::Context,
    columns: &[Column],
    rows: &[Vec<Cell>],
    state: &mut GridState,
    id: egui::Id,
) {
    let Some((r, c)) = state.viewer else {
        state.viewed = None;
        return;
    };
    let key = ViewedKey {
        cell: (r, c),
        rows: (rows.as_ptr().addr(), rows.len()),
        order: state.order_revision,
        changes: state.changes_revision,
    };
    if state.viewed.as_ref().is_none_or(|v| v.key != key) {
        state.viewed = Some(Viewed::new(key, columns, rows, state));
    }
    let Some(viewed) = &state.viewed else { return };
    let name = columns.get(c).map_or("", |col| col.name.as_str());
    let mut open = true;
    egui::Window::new(format!("{name} — row {}", r + 1))
        .id(id.with("viewer"))
        .open(&mut open)
        .default_size([560.0, 380.0])
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                match (&viewed.value, &viewed.binary) {
                    (None, _) => ui.weak("NULL"),
                    (_, Some(b)) => {
                        let size = b.cut.map_or(b.bytes.len(), |n| n as usize);
                        let mut header = format!(
                            "{} · {}",
                            human_bytes(size),
                            image_kind(&b.bytes).unwrap_or("binary")
                        );
                        if b.cut.is_some() {
                            header.push_str(&format!(
                                " · the grid holds its first {} bytes",
                                b.bytes.len()
                            ));
                        }
                        ui.weak(header)
                    }
                    (Some(_), None) => ui.weak(format!("{} characters", viewed.chars)),
                };
                if viewed.pretty.is_some() {
                    ui.weak("· JSON, formatted");
                }
                if viewed.rows.as_ref().is_some_and(|(_, cut)| *cut) {
                    ui.weak(format!("· lines cut every {VIEWER_ROW_CHARS} characters"));
                }
                if viewed.unapplied {
                    ui.weak("· changed, not applied");
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let copy = ui
                        .add_enabled(!viewed.unapplied, egui::Button::new("Copy"))
                        .on_disabled_hover_text("Apply or discard the change first");
                    if copy.clicked() {
                        ctx.copy_text(viewed.value.clone().unwrap_or_default());
                    }
                });
            });
            ui.separator();
            if let Some(b) = &viewed.binary {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    if let Some(uri) = &b.image {
                        ui.add(
                            egui::Image::from_bytes(uri.clone(), b.bytes.clone())
                                .max_width(ui.available_width())
                                .max_height(480.0),
                        );
                        ui.separator();
                    } else if b.cut.is_some() && image_kind(&b.bytes).is_some() {
                        ui.weak(format!(
                            "The image can't be shown: the grid holds the first {} bytes of it. \
                             Execute to file writes such values whole.",
                            b.bytes.len()
                        ));
                        ui.separator();
                    }
                    ui.label(RichText::new(&b.dump).monospace());
                });
                return;
            }
            let text = viewed.text();
            let Some((starts, _)) = &viewed.rows else {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut &*text)
                            .code_editor()
                            .desired_width(f32::INFINITY)
                            .frame(egui::Frame::NONE),
                    );
                });
                return;
            };
            // A row at a time, those in view (#284).
            let height = ui.text_style_height(&egui::TextStyle::Monospace);
            egui::ScrollArea::both().auto_shrink(false).show_rows(
                ui,
                height,
                starts.len(),
                |ui, shown| {
                    for row in shown {
                        let end = starts.get(row + 1).copied().unwrap_or(text.len());
                        let line = text[starts[row]..end].trim_end_matches(['\n', '\r']);
                        ui.add(egui::Label::new(RichText::new(line).monospace()).extend());
                    }
                },
            );
        });
    if !open {
        if let Some(uri) = viewed.binary.as_ref().and_then(|b| b.image.as_ref()) {
            ctx.forget_image(uri);
        }
        state.viewer = None;
        state.viewed = None;
    }
}

/// What a cell shows of a value: its first line, at most [`SHOWN_CHARS`] of
/// it, and ` …` when there is more.
fn cell_text(text: &str) -> String {
    let first = text.lines().next().unwrap_or_default();
    let end = first
        .char_indices()
        .nth(SHOWN_CHARS)
        .map_or(first.len(), |(i, _)| i);
    let more = if end < text.len() { " …" } else { "" };
    format!("{}{more}", &first[..end])
}

/// What a field of the record view shows of a value: at most
/// [`RECORD_CHARS`] of it, and ` …` when there is more.
fn record_text(text: &str) -> String {
    match text.char_indices().nth(RECORD_CHARS) {
        Some((end, _)) => format!("{} …", &text[..end]),
        None => text.to_owned(),
    }
}

/// Columns whose values arrive as hex text, by the database's type names
/// (#224): Oracle's BLOB, RAW, LONG RAW and BFILE; MySQL's BLOB, BINARY and
/// VARBINARY; SQL Server's binary, varbinary and image (as `0x…`);
/// PostgreSQL's bytea (as `\x…`); SQLite's columns of blobs (#209).
fn is_binary(dialect: Dialect, col: &Column) -> bool {
    let t = col.type_name.as_str();
    match dialect.kind() {
        DbKind::Oracle => {
            let t = t.to_ascii_uppercase();
            t.starts_with("BLOB")
                || t.starts_with("RAW")
                || t.starts_with("LONG RAW")
                || t == "BFILE"
        }
        DbKind::MySql => matches!(t, "BLOB" | "BINARY" | "VARBINARY"),
        DbKind::SqlServer => matches!(t, "binary" | "varbinary" | "image"),
        DbKind::Postgres => t == "bytea",
        DbKind::Sqlite => t.to_ascii_uppercase().contains("BLOB"),
    }
}

/// The hex digits of a binary value, after SQL Server's `0x` or
/// PostgreSQL's `\x`.
fn hex_digits(text: &str) -> &str {
    text.strip_prefix("0x")
        .or_else(|| text.strip_prefix("\\x"))
        .unwrap_or(text)
}

/// The hex digits the grid holds of a binary value: all of them, or the
/// first [`pythia_db::SHOWN_BYTES`]' of one held cut short (#204).
fn held_hex(text: &str) -> &str {
    let hex = hex_digits(text);
    match pythia_db::cut_size(text) {
        Some(_) => &hex[..pythia_db::SHOWN_BYTES * 2],
        None => hex,
    }
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

/// `0x89504E47… · 12.3 KB` for a hex-encoded value; one held cut short has
/// its size after the hex (#204).
fn binary_summary(text: &str) -> String {
    let hex = hex_digits(text);
    let head: String = hex.chars().take(16).collect();
    let more = if hex.len() > 16 { "…" } else { "" };
    let size = pythia_db::cut_size(hex).map_or(hex.len() / 2, |n| n as usize);
    format!("0x{head}{more} · {}", human_bytes(size))
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
    let mut out = tsv_line(columns.iter().map(|c| Some(c.name.as_str())));
    for row in rows {
        out.push('\n');
        out.push_str(&tsv_line(row.iter().map(Option::as_deref)));
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
        // Fetched cut short, a value has its size after the hex.
        let cut = format!("{}… (10485760 bytes)", "89504E47".repeat(1000));
        assert_eq!(binary_summary(&cut), "0x89504E4789504E47… · 10.0 MB");
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
        assert_eq!(
            selection_text(&c, &r, &s, true).unwrap(),
            "A\tB\n10\tx\n\ta\n9\t"
        );
        assert_eq!(
            summary(&c, &r, &s).unwrap(),
            "Count 4 · Sum 19 · Avg 9.5 · Min 9 · Max 10"
        );
    }

    /// Numbers sort and add up as the numbers they are, also past the 15
    /// to 17 digits a double holds (#238): 9223372036854775806 sorted after
    /// 9223372036854775807, and the summary of both read "Min
    /// 9223372036854775808." and "Sum 18446744073709551616.".
    #[test]
    fn big_numbers_sort_and_add_up_exactly() {
        let columns = vec![Column {
            name: "N".into(),
            type_name: "NUMBER".into(),
            numeric: true,
        }];
        let values = [
            "9223372036854775807",
            "9223372036854775806",
            "-9223372036854775808",
            "9007199254740993",
            "9007199254740992",
            "12345678901234567890.5",
            "12345678901234567890.25",
            "1e300",
            "0.1",
            "0.2",
        ];
        let rows: Vec<Vec<Cell>> = values.iter().map(|v| vec![Some(v.to_string())]).collect();
        let mut s = GridState::default();
        s.sync(&rows, &columns);
        s.set_sort(0, &rows, &columns);
        let sorted: Vec<&str> = s.order.iter().map(|&i| values[i]).collect();
        assert_eq!(
            sorted,
            [
                "-9223372036854775808",
                "0.1",
                "0.2",
                "9007199254740992",
                "9007199254740993",
                "9223372036854775806",
                "9223372036854775807",
                "12345678901234567890.25",
                "12345678901234567890.5",
                "1e300",
            ]
        );
        let summary_of = |from: usize, to: usize| {
            let mut s = GridState::default();
            s.sync(&rows, &columns);
            s.select((from, 0), false);
            s.select((to, 0), true);
            summary(&columns, &rows, &s).unwrap()
        };
        assert_eq!(
            summary_of(0, 1),
            "Count 2 · Sum 18446744073709551613 · Avg 9223372036854775806.5 · \
             Min 9223372036854775806 · Max 9223372036854775807"
        );
        assert_eq!(
            summary_of(3, 4),
            "Count 2 · Sum 18014398509481985 · Avg 9007199254740992.5 · \
             Min 9007199254740992 · Max 9007199254740993"
        );
        assert_eq!(
            summary_of(5, 6),
            "Count 2 · Sum 24691357802469135780.75 · Avg 12345678901234567890.375 · \
             Min 12345678901234567890.25 · Max 12345678901234567890.5"
        );
        assert_eq!(
            summary_of(8, 9),
            "Count 2 · Sum 0.3 · Avg 0.15 · Min 0.1 · Max 0.2"
        );
        // Past what an exact sum holds, a double's, without a stray dot.
        assert_eq!(
            summary_of(6, 7),
            "Count 2 · Sum 1e300 · Avg 5e299 · Min 12345678901234567890.25 · Max 1e300"
        );
    }

    #[test]
    fn hidden_columns_are_left_out() {
        let columns = cols();
        let rows = vec![vec![Some("1".into()), Some("x".into())]];
        let mut state = GridState::default();
        state.hidden.insert(0);
        let (c, d) = selection_data(&columns, &rows, &state).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "B");
        assert_eq!(d, vec![vec![Some("x".to_owned())]]);
        assert_eq!(state.visible_columns(2), vec![1]);
    }

    /// Without a selection, Export and Copy all take the rows as shown
    /// (#243): those the filter passes, in the order shown, as they leave the
    /// hidden columns out. They took every fetched row, also those the
    /// filter hid, in the order fetched.
    #[test]
    fn exports_take_the_rows_shown() {
        let columns = cols();
        let rows: Vec<Vec<Cell>> = [
            ("1", "apple"),
            ("2", "banana"),
            ("3", "cherry"),
            ("4", "grape"),
        ]
        .iter()
        .map(|(a, b)| vec![Some(a.to_string()), Some(b.to_string())])
        .collect();
        let mut state = GridState::default();
        state.set_filter("ap");
        state.set_sort(0, &rows, &columns);
        state.set_sort(0, &rows, &columns);
        let shown = vec![
            vec![Some("4".to_owned()), Some("grape".to_owned())],
            vec![Some("1".to_owned()), Some("apple".to_owned())],
        ];
        assert_eq!(shown_data(&columns, &rows, &state).unwrap().1, shown);
        assert_eq!(selection_data(&columns, &rows, &state).unwrap().1, shown);
        // Rows fetched since the grid was last drawn are filtered too.
        let mut more = rows.clone();
        more.push(vec![Some("5".into()), Some("papaya".into())]);
        assert_eq!(shown_data(&columns, &more, &state).unwrap().1.len(), 3);
        state.set_filter("");
        assert_eq!(shown_data(&columns, &rows, &state).unwrap().1.len(), 4);
    }

    /// The columns editing adds to find the rows (`pythia$id`, the ROWID)
    /// aren't the table's: copies and exports leave them out (#249).
    #[test]
    fn copies_leave_out_the_key_editing_added() {
        let mut columns = cols();
        columns.push(Column {
            name: "pythia$a".into(),
            type_name: "NUMBER".into(),
            numeric: true,
        });
        let rows = vec![vec![Some("1".into()), Some("x".into()), Some("1".into())]];
        let mut state = GridState::default();
        state.set_editable(Some(2));
        let names = |c: &[Column]| c.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        let (c, d) = shown_data(&columns, &rows, &state).unwrap();
        assert_eq!(names(&c), ["A", "B"]);
        assert_eq!(d, vec![vec![Some("1".to_owned()), Some("x".to_owned())]]);
        let (c, _) = selection_data(&columns, &rows, &state).unwrap();
        assert_eq!(names(&c), ["A", "B"]);
        state.select((0, 1), false);
        state.select((0, 2), true);
        let (c, _) = selection_data(&columns, &rows, &state).unwrap();
        assert_eq!(names(&c), ["B"]);
        assert_eq!(
            selection_text(&columns, &rows, &state, true).unwrap(),
            "B\nx"
        );
        // Nothing but the key: nothing to copy, and why.
        state.select((0, 2), false);
        assert_eq!(
            selection_data(&columns, &rows, &state).unwrap_err(),
            NotCopied::KeyOnly
        );
    }

    /// Changes not applied are in neither the database nor surely what it
    /// will hold: a copy or export of rows that have one waits until they
    /// are applied or discarded, and says so; other rows copy as before
    /// (#250). The viewer shows a cell as the grid does.
    #[test]
    fn copies_wait_for_changes_to_be_applied() {
        let columns = cols();
        let rows = rows();
        let mut state = GridState::default();
        state.set_editable(None);
        state.edit_cell(&rows, 0, 1, Some("new".into()));
        let unapplied = |state: &GridState| shown_data(&columns, &rows, state);
        assert_eq!(unapplied(&state).unwrap_err(), NotCopied::Unapplied(1));
        // Row 1 and row 0's other column hold no change.
        state.select((1, 0), false);
        state.select((1, 1), true);
        assert_eq!(
            selection_text(&columns, &rows, &state, false).unwrap(),
            "\ta"
        );
        state.select((0, 0), false);
        assert_eq!(
            selection_text(&columns, &rows, &state, false).unwrap(),
            "10"
        );
        state.select((0, 1), false);
        assert_eq!(
            selection_data(&columns, &rows, &state).unwrap_err(),
            NotCopied::Unapplied(1)
        );
        assert_eq!(state.viewed(&rows, (0, 1)), (Some("new".to_owned()), true));
        // A row to be deleted, a new one, a value being typed.
        state.delete_row(&rows, 2);
        state.add_row(&rows, 2);
        state.anchor = None;
        state.cursor = None;
        assert_eq!(unapplied(&state).unwrap_err(), NotCopied::Unapplied(3));
        state.start_edit(&rows, (1, 0), Some("7".into()));
        assert_eq!(unapplied(&state).unwrap_err(), NotCopied::Unapplied(4));
        assert_eq!(state.viewed(&rows, (3, 0)), (None, true));
        state.clear_changes();
        assert_eq!(unapplied(&state).unwrap().1.len(), 3);
        assert_eq!(state.viewed(&rows, (0, 1)), (Some("x".to_owned()), false));
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
            selection_text(&columns, &rows, &s, true).unwrap(),
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
        assert_eq!(
            selection_text(&columns, &rows, &s, true).unwrap(),
            "C\tB\np\tx"
        );
        // Without a selection: all rows, the columns shown, in order.
        s.anchor = None;
        s.cursor = None;
        let (c, d) = selection_data(&columns, &rows, &s).unwrap();
        let names: Vec<&str> = c.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["C", "B"]);
        assert_eq!(d[1], vec![Some("q".to_owned()), Some("y".to_owned())]);
        let (shown, all) = shown_data(&columns, &rows, &s).unwrap();
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

    /// A small PNG (2×2 pixels).
    const PNG: &str = "89504E470D0A1A0A0000000D4948445200000002000000020802000000FDD49A73000000\
                       1049444154789C63F8CFC000440C100A001FEE03FD8B5F14D40000000049454E44AE426082";

    /// Binary values show as binary on every database: their start and
    /// size in the cell, the image and a hex dump in the viewer. SQL
    /// Server's, MySQL's VARBINARY and PostgreSQL's bytea showed as text.
    #[test]
    fn binary_values_of_every_database() {
        use pythia_db::Dialect;
        let cases = [
            (Dialect::ORACLE, "BLOB", PNG.to_owned()),
            (Dialect::ORACLE, "RAW(73)", PNG.to_owned()),
            (Dialect::MYSQL, "BLOB", PNG.to_owned()),
            (Dialect::MYSQL, "VARBINARY", PNG.to_owned()),
            (Dialect::MYSQL, "BINARY", PNG.to_owned()),
            (Dialect::SQLSERVER, "varbinary", format!("0x{PNG}")),
            (Dialect::SQLSERVER, "binary", format!("0x{PNG}")),
            (Dialect::SQLSERVER, "image", format!("0x{PNG}")),
            (
                Dialect::POSTGRES,
                "bytea",
                format!("\\x{}", PNG.to_lowercase()),
            ),
            (Dialect::SQLITE, "BLOB", PNG.to_owned()),
            (Dialect::SQLITE, "MEDIUMBLOB", PNG.to_owned()),
        ];
        for (dialect, type_name, value) in cases {
            let columns = vec![Column {
                name: "IMG".into(),
                type_name: type_name.into(),
                numeric: false,
            }];
            let rows = vec![vec![Some(value)]];
            let ctx = egui::Context::default();
            let mut state = GridState {
                dialect,
                ..Default::default()
            };
            let drawn = frame(&ctx, &columns, &rows, &mut state, vec![]);
            let summary = if dialect == Dialect::POSTGRES {
                "0x89504e470d0a1a0a… · 73 bytes"
            } else {
                "0x89504E470D0A1A0A… · 73 bytes"
            };
            assert!(
                texts(&drawn).contains(&summary),
                "{type_name}: {:?}",
                texts(&drawn)
            );
            state.viewer = Some((0, 0));
            // A window is laid out unseen first.
            frame(&ctx, &columns, &rows, &mut state, vec![]);
            let drawn = frame(&ctx, &columns, &rows, &mut state, vec![]);
            assert!(
                texts(&drawn).contains(&"73 bytes · png"),
                "{type_name}: {:?}",
                texts(&drawn)
            );
        }
        // Text in a column SQLite declares VARBINARY is text.
        let columns = vec![Column {
            name: "T".into(),
            type_name: "VARBINARY".into(),
            numeric: false,
        }];
        let rows = vec![vec![Some("CAFE".to_owned())]];
        let mut state = GridState {
            dialect: Dialect::SQLITE,
            ..Default::default()
        };
        let drawn = frame(
            &egui::Context::default(),
            &columns,
            &rows,
            &mut state,
            vec![],
        );
        assert!(texts(&drawn).contains(&"CAFE"), "{:?}", texts(&drawn));
    }

    /// The textures the viewer of cell (0, 0) paints once its image is
    /// loaded, and the texts it draws.
    fn viewer_image(
        ctx: &egui::Context,
        rows: &[Vec<Cell>],
    ) -> (Vec<egui::TextureId>, Vec<String>) {
        let columns = vec![Column {
            name: "IMG".into(),
            type_name: "BLOB".into(),
            numeric: false,
        }];
        let mut state = GridState {
            viewer: Some((0, 0)),
            ..Default::default()
        };
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut painted = (Vec::new(), Vec::new());
        // Images are decoded while the frames go on.
        for _ in 0..100 {
            let mut output = ctx.run_ui(input(), |ui| {
                ResultGrid::new(&columns, rows, &mut state, "grid").show(ui);
            });
            output.textures_delta.clear();
            let mut textures = Vec::new();
            let mut texts = Vec::new();
            for shape in &output.shapes {
                match &shape.shape {
                    egui::Shape::Text(t) => texts.push(t.galley.text().to_owned()),
                    egui::Shape::Rect(r) if r.fill_texture_id() != egui::TextureId::default() => {
                        textures.push(r.fill_texture_id());
                    }
                    egui::Shape::Mesh(m) if m.texture_id != egui::TextureId::default() => {
                        textures.push(m.texture_id);
                    }
                    _ => {}
                }
            }
            painted = (textures, texts);
            if !painted.0.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        painted
    }

    /// The viewer shows an image as the image (#231): without a decoder for
    /// PNG, JPEG, GIF, WebP and BMP, it showed ⚠ in its place.
    #[test]
    fn the_viewer_shows_images() {
        const BLUE: &str = "89504E470D0A1A0A0000000D4948445200000002000000020802000000FDD49A73\
                            0000000F49444154789C636060F80F46600A0017F603FD7EEB37F300000000\
                            49454E44AE426082";
        let ctx = egui::Context::default();
        crate::icons::install(&ctx);
        let (red, texts) = viewer_image(&ctx, &[vec![Some(PNG.to_owned())]]);
        assert!(!red.is_empty(), "no image in {texts:?}");
        assert!(!texts.iter().any(|t| t == "⚠"), "{texts:?}");
        // Another image in the same cell (of another result, or edited) is
        // that one, not the one shown before.
        let (blue, _) = viewer_image(&ctx, &[vec![Some(BLUE.to_owned())]]);
        assert!(!blue.is_empty());
        assert_ne!(red, blue);
    }

    /// The viewer of a binary value the grid holds cut short (#204) shows
    /// its size, a hex dump of what it holds, and why there is no image
    /// (#232); it showed the hex and the size as 8015 characters of text.
    #[test]
    fn the_viewer_of_a_cut_value_says_so() {
        use pythia_db::{Dialect, SHOWN_BYTES};
        let held = format!("{PNG}{}", "00".repeat(SHOWN_BYTES - PNG.len() / 2));
        for (dialect, type_name, value) in [
            (Dialect::MYSQL, "BLOB", format!("{held}… (51200 bytes)")),
            (
                Dialect::SQLSERVER,
                "varbinary",
                format!("0x{held}… (51200 bytes)"),
            ),
        ] {
            let columns = vec![Column {
                name: "IMG".into(),
                type_name: type_name.into(),
                numeric: false,
            }];
            let rows = vec![vec![Some(value)]];
            let ctx = egui::Context::default();
            let mut state = GridState {
                dialect,
                viewer: Some((0, 0)),
                ..Default::default()
            };
            frame(&ctx, &columns, &rows, &mut state, vec![]);
            let drawn = frame(&ctx, &columns, &rows, &mut state, vec![]);
            let texts = texts(&drawn);
            assert!(
                texts.contains(&"50.0 KB · png · the grid holds its first 4000 bytes"),
                "{type_name}: {texts:?}"
            );
            assert!(
                texts.iter().any(|t| t.starts_with(
                    "The image can't be shown: the grid holds the first 4000 bytes of it."
                )),
                "{type_name}: {texts:?}"
            );
            let dump = texts
                .iter()
                .find(|t| t.starts_with("00000000  89 50 4E 47"))
                .unwrap_or_else(|| panic!("{type_name}: no hex dump in {texts:?}"));
            assert!(
                dump.ends_with("… 46.1 KB more, not held by the grid"),
                "{type_name}: {dump}"
            );
        }
    }

    /// A cell lays out no more of a value than it can show (#212): 10 MB on
    /// one line took 1.6 s to lay out and kept 470 MB per cell.
    #[test]
    fn huge_values_are_cut_to_the_cell() {
        let columns = cols();
        let huge = format!("{}\nsecond line", "x".repeat(10 << 20));
        let rows = vec![vec![Some("1".into()), Some(huge)]];
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        let started = std::time::Instant::now();
        for _ in 0..3 {
            let drawn = frame(&ctx, &columns, &rows, &mut state, vec![]);
            let cell = drawn
                .iter()
                .map(|(t, _)| t.as_str())
                .find(|t| t.starts_with("xxx"))
                .expect("the value");
            assert_eq!(cell, format!("{} …", "x".repeat(SHOWN_CHARS)));
        }
        assert!(
            started.elapsed().as_secs_f64() < 1.0,
            "{:?}",
            started.elapsed()
        );
    }

    /// The summary of a selection is worked out when the selection or the
    /// rows change, not on every frame (#241): with a million cells selected,
    /// each frame took a quarter of a second.
    #[test]
    fn summary_of_a_large_selection_is_kept() {
        let columns: Vec<Column> = (0..4)
            .map(|i| Column {
                name: format!("N{i}"),
                type_name: "NUMBER".into(),
                numeric: true,
            })
            .collect();
        let mut rows: Vec<Vec<Cell>> = (0..250_000)
            .map(|r| (0..4).map(|c| Some((r * 4 + c).to_string())).collect())
            .collect();
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        let summary = |rows: &[Vec<Cell>], state: &mut GridState| {
            let mut summary = None;
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                summary = ResultGrid::new(&columns, rows, state, "grid")
                    .show(ui)
                    .selection_summary;
            });
            output.textures_delta.clear();
            summary
        };
        summary(&rows, &mut state);
        state.anchor = Some((0, 0));
        state.cursor = Some((rows.len() - 1, 3));
        let started = std::time::Instant::now();
        let all = summary(&rows, &mut state);
        let first = started.elapsed();
        assert_eq!(
            all.as_deref(),
            Some("Count 1000000 · Sum 499999500000 · Avg 499999.5 · Min 0 · Max 999999")
        );
        let started = std::time::Instant::now();
        for _ in 0..10 {
            assert_eq!(summary(&rows, &mut state), all);
        }
        assert!(
            started.elapsed() < first * 3,
            "10 frames took {:?}, the first one {first:?}",
            started.elapsed()
        );
        // Another selection, more rows, a filter: worked out again.
        state.cursor = Some((1, 1));
        assert_eq!(
            summary(&rows, &mut state).as_deref(),
            Some("Count 4 · Sum 10 · Avg 2.5 · Min 0 · Max 5")
        );
        rows[0][0] = Some("100".into());
        rows.push(vec![Some("1".into()); 4]);
        assert_eq!(
            summary(&rows, &mut state).as_deref(),
            Some("Count 4 · Sum 110 · Avg 27.5 · Min 1 · Max 100")
        );
        // The rows with 99999 and 199999, once the filter shows them.
        state.set_filter("99999");
        state.view_order(&rows, &columns);
        state.anchor = Some((0, 0));
        state.cursor = Some((1, 0));
        assert_eq!(
            summary(&rows, &mut state).as_deref(),
            Some("Count 2 · Sum 299992 · Avg 149996 · Min 99996 · Max 199996")
        );
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

    /// A NULL cell's editor opens empty; left so, the cell stays NULL
    /// (#262). It became `''`, which a date or number column refuses on
    /// Apply ("Incorrect datetime value: ''") and a text column took as a
    /// change: a new row's TIMESTAMP opened and left lost its default.
    #[test]
    fn null_cells_left_empty_stay_null() {
        let (c, r) = (cols(), rows());
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        state.set_editable(None);
        // Row 2's A, the first NULL shown.
        click(&ctx, &c, &r, &mut state, "NULL", 2);
        assert_eq!(
            state.edit.as_ref().unwrap().active,
            Some((1, 0, String::new()))
        );
        frame(&ctx, &c, &r, &mut state, vec![]);
        frame(&ctx, &c, &r, &mut state, vec![key(Key::Enter)]);
        assert!(state.edit.as_ref().unwrap().active.is_none());
        assert!(state.changes().unwrap().is_empty(), "{:?}", state.changes());
        // Typed and taken back, too.
        state.start_edit(&r, (2, 1), Some("a".into()));
        state
            .edit
            .as_mut()
            .unwrap()
            .active
            .as_mut()
            .unwrap()
            .2
            .clear();
        frame(&ctx, &c, &r, &mut state, vec![key(Key::Enter)]);
        assert!(state.changes().unwrap().is_empty(), "{:?}", state.changes());

        // A new row's cells.
        state.add_row(&r, 2);
        state.start_edit(&r, (3, 1), None);
        frame(&ctx, &c, &r, &mut state, vec![]);
        frame(&ctx, &c, &r, &mut state, vec![key(Key::Enter)]);
        assert_eq!(state.changes().unwrap().inserts, [vec![None, None]]);

        // A value typed is kept, and an empty text where there was one.
        state.start_edit(&r, (0, 1), None);
        state
            .edit
            .as_mut()
            .unwrap()
            .active
            .as_mut()
            .unwrap()
            .2
            .clear();
        frame(&ctx, &c, &r, &mut state, vec![key(Key::Enter)]);
        state.start_edit(&r, (3, 1), Some("b".into()));
        frame(&ctx, &c, &r, &mut state, vec![key(Key::Enter)]);
        let changes = state.changes().unwrap();
        assert_eq!(changes.updates[&0][&1].as_deref(), Some(""));
        assert_eq!(changes.inserts, [vec![None, Some("b".into())]]);
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

    /// Tab keeps the value typed and edits the next cell, Shift+Tab the
    /// one before (#254): the value typed next went into the cell just
    /// edited, in place of what Tab had kept.
    #[test]
    fn tab_edits_the_next_cell() {
        let (columns, rows) = three();
        let tab = |shift: bool| egui::Event::Key {
            key: Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: if shift {
                egui::Modifiers::SHIFT
            } else {
                egui::Modifiers::NONE
            },
        };
        let text = |s: &str| egui::Event::Text(s.into());
        let active = |state: &GridState| state.edit.as_ref().unwrap().active.clone();
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        state.set_editable(None);
        // The middle column is read-only: Tab goes past it.
        state.lock_columns([1]);
        // The row's number reads 1 too.
        click(&ctx, &columns, &rows, &mut state, "x", 1);
        frame(&ctx, &columns, &rows, &mut state, vec![key(Key::ArrowLeft)]);
        frame(&ctx, &columns, &rows, &mut state, vec![key(Key::F2)]);
        frame(&ctx, &columns, &rows, &mut state, vec![text("0")]);
        frame(&ctx, &columns, &rows, &mut state, vec![tab(false)]);
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        assert_eq!(active(&state), Some((0, 2, "p".into())));
        assert_eq!(state.cursor, Some((0, 2)));
        frame(&ctx, &columns, &rows, &mut state, vec![text("2")]);
        // At the end of the row, Tab goes on to the next one.
        frame(&ctx, &columns, &rows, &mut state, vec![tab(false)]);
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        assert_eq!(active(&state), Some((1, 0, "2".into())));
        let updates = &state.changes().unwrap().updates;
        assert_eq!(updates[&0][&0].as_deref(), Some("10"));
        assert_eq!(updates[&0][&2].as_deref(), Some("p2"));
        // Shift+Tab goes back, past the read-only column again.
        frame(&ctx, &columns, &rows, &mut state, vec![tab(true)]);
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        assert_eq!(active(&state), Some((0, 2, "p2".into())));
        frame(&ctx, &columns, &rows, &mut state, vec![tab(true)]);
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        assert_eq!(active(&state), Some((0, 0, "10".into())));
        // Escape drops only what was typed in the cell being edited.
        frame(&ctx, &columns, &rows, &mut state, vec![text("9")]);
        frame(&ctx, &columns, &rows, &mut state, vec![key(Key::Escape)]);
        assert_eq!(active(&state), None);
        assert_eq!(
            state.changes().unwrap().updates[&0][&0].as_deref(),
            Some("10")
        );
        // Past the last cell, Tab keeps the value and stays.
        state.cursor = Some((1, 2));
        frame(&ctx, &columns, &rows, &mut state, vec![key(Key::F2)]);
        frame(&ctx, &columns, &rows, &mut state, vec![text("!")]);
        frame(&ctx, &columns, &rows, &mut state, vec![tab(false)]);
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        assert_eq!(active(&state), None);
        assert_eq!(state.cursor, Some((1, 2)));
        assert_eq!(
            state.changes().unwrap().updates[&1][&2].as_deref(),
            Some("q!")
        );
    }

    /// Ctrl+Z takes back the last change not applied, one at a time
    /// (#257): a cell set to NULL by mistake could only be taken back
    /// with Discard, and every other change with it.
    #[test]
    fn changes_are_undone_one_at_a_time() {
        let (c, r) = (cols(), rows());
        let undo = egui::Event::Key {
            key: Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        };
        let updates = |state: &GridState| state.changes().unwrap().updates.clone();
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        state.set_editable(None);
        // A value typed, then a cell set to NULL with Delete.
        click(&ctx, &c, &r, &mut state, "a", 1);
        frame(
            &ctx,
            &c,
            &r,
            &mut state,
            vec![egui::Event::Text("b".into())],
        );
        frame(&ctx, &c, &r, &mut state, vec![key(Key::Enter)]);
        frame(&ctx, &c, &r, &mut state, vec![]);
        frame(&ctx, &c, &r, &mut state, vec![key(Key::ArrowUp)]);
        frame(&ctx, &c, &r, &mut state, vec![key(Key::Delete)]);
        assert_eq!(updates(&state)[&0][&1], None);
        assert_eq!(updates(&state)[&1][&1].as_deref(), Some("b"));
        frame(&ctx, &c, &r, &mut state, vec![undo.clone()]);
        assert!(!updates(&state).contains_key(&0), "{:?}", updates(&state));
        assert_eq!(updates(&state)[&1][&1].as_deref(), Some("b"));
        // A paste that added a row, a row marked for deletion.
        state.cursor = Some((2, 0));
        state.anchor = state.cursor;
        state.paste(&r, 2, "7\tp\n8\tq");
        state.delete_row(&r, 0);
        assert_eq!(state.changes().unwrap().inserts.len(), 1);
        frame(&ctx, &c, &r, &mut state, vec![undo.clone()]);
        assert!(state.changes().unwrap().deletes.is_empty());
        frame(&ctx, &c, &r, &mut state, vec![undo.clone()]);
        let changes = state.changes().unwrap();
        assert!(changes.inserts.is_empty());
        assert!(!changes.updates.contains_key(&2), "{changes:?}");
        frame(&ctx, &c, &r, &mut state, vec![undo.clone()]);
        assert!(state.changes().unwrap().is_empty());
        frame(&ctx, &c, &r, &mut state, vec![undo.clone()]);
        assert!(state.changes().unwrap().is_empty());
        // In the cell's editor, Ctrl+Z is the editor's.
        state.edit_cell(&r, 0, 0, None);
        state.start_edit(&r, (1, 1), Some("typed".into()));
        frame(&ctx, &c, &r, &mut state, vec![]);
        frame(&ctx, &c, &r, &mut state, vec![undo.clone()]);
        assert_eq!(updates(&state)[&0][&0], None);
        frame(&ctx, &c, &r, &mut state, vec![key(Key::Escape)]);
        // Discard leaves nothing to take back.
        state.clear_changes();
        frame(&ctx, &c, &r, &mut state, vec![undo]);
        assert!(state.changes().unwrap().is_empty());
    }

    /// In the record view, Tab edits the next field (#254).
    #[test]
    fn tab_edits_the_next_field_of_the_record() {
        let (c, r) = (cols(), rows());
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        state.set_editable(None);
        state.cursor = Some((0, 0));
        state.set_record_view(&r, true);
        frame(&ctx, &c, &r, &mut state, vec![]);
        click(&ctx, &c, &r, &mut state, "10", 1);
        frame(
            &ctx,
            &c,
            &r,
            &mut state,
            vec![egui::Event::Text("1".into())],
        );
        frame(&ctx, &c, &r, &mut state, vec![key(Key::Tab)]);
        frame(&ctx, &c, &r, &mut state, vec![]);
        assert_eq!(
            state.edit.as_ref().unwrap().active,
            Some((0, 1, "x".into()))
        );
        assert_eq!(
            state.changes().unwrap().updates[&0][&0].as_deref(),
            Some("1")
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

    /// A field of the record view lays out no more of a value than a few
    /// screens show (#220): 10 MB on one line, wrapped, took 30 s a frame.
    #[test]
    fn record_view_cuts_huge_values() {
        let columns = cols();
        let huge = "x".repeat(10 << 20);
        let lines = "line\n".repeat(1 << 20);
        let ctx = egui::Context::default();
        for value in [huge, lines] {
            let rows = vec![vec![Some("1".into()), Some(value.clone())]];
            let mut state = GridState {
                record_view: true,
                ..Default::default()
            };
            let started = std::time::Instant::now();
            for _ in 0..3 {
                let drawn = frame(&ctx, &columns, &rows, &mut state, vec![]);
                let field = drawn
                    .iter()
                    .map(|(t, _)| t.as_str())
                    .find(|t| t.starts_with(&value[..4]))
                    .expect("the value");
                let shown = &value[..value.char_indices().nth(RECORD_CHARS).unwrap().0];
                assert_eq!(field, format!("{shown} …"));
            }
            let elapsed = started.elapsed().as_secs_f64();
            assert!(elapsed < 2.0, "{elapsed} s");
        }
    }

    /// A frame with `events`, and the text it copied, if any.
    fn copied(
        ctx: &egui::Context,
        columns: &[Column],
        rows: &[Vec<Cell>],
        state: &mut GridState,
        events: Vec<egui::Event>,
    ) -> Option<String> {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            time: Some(ctx.input(|i| i.time) + 1.0 / 60.0),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            ResultGrid::new(columns, rows, state, "grid").show(ui);
        });
        output.textures_delta.clear();
        output
            .platform_output
            .commands
            .iter()
            .find_map(|c| match c {
                egui::OutputCommand::CopyText(text) => Some(text.clone()),
                _ => None,
            })
    }

    /// Rows copied and pasted back keep their NULLs and their empty texts
    /// (#275): Copy writes both as an empty cell, and Paste took every
    /// empty cell for NULL, so '' went back as NULL. Text from elsewhere
    /// keeps that rule.
    #[test]
    fn copied_rows_paste_back_as_they_were() {
        let (columns, mut rows) = three();
        rows[0][1] = Some(String::new());
        rows[1][1] = None;
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        state.set_editable(None);
        click(&ctx, &columns, &rows, &mut state, "p", 1);
        state.anchor = Some((0, 1));
        state.cursor = Some((1, 2));
        let text = copied(&ctx, &columns, &rows, &mut state, vec![egui::Event::Copy]);
        assert_eq!(text.as_deref(), Some("\tp\n\tq"));
        let text = text.unwrap();
        state.add_row(&rows, 3);
        state.add_row(&rows, 3);
        state.anchor = Some((2, 1));
        state.cursor = Some((2, 1));
        let paste = |text: &str| vec![egui::Event::Paste(text.into())];
        frame(&ctx, &columns, &rows, &mut state, paste(&text));
        assert_eq!(
            state.changes().unwrap().inserts,
            [
                vec![None, Some(String::new()), Some("p".into())],
                vec![None, None, Some("q".into())],
            ]
        );
        // As the clipboard may give it back, with CRLF and a line break
        // at the end.
        state.cursor = Some((2, 1));
        frame(&ctx, &columns, &rows, &mut state, paste("\tP\r\n\tq\r\n"));
        assert_eq!(state.changes().unwrap().inserts[0][1], None);
        state.cursor = Some((2, 1));
        frame(&ctx, &columns, &rows, &mut state, paste("\tp\r\n\tq\r\n"));
        assert_eq!(state.changes().unwrap().inserts[0][1].as_deref(), Some(""));
        // Other text: an empty cell is NULL.
        state.cursor = Some((0, 1));
        frame(&ctx, &columns, &rows, &mut state, paste("\tz"));
        assert_eq!(state.changes().unwrap().updates[&0][&1], None);
        // Copy all, with its header.
        state.clear_changes();
        copy_rows(&ctx, &columns, rows.clone());
        state.add_row(&rows, 3);
        frame(
            &ctx,
            &columns,
            &rows,
            &mut state,
            paste("A\tB\tC\n1\t\tp\n2\t\tq"),
        );
        let inserts = &state.changes().unwrap().inserts;
        assert_eq!(inserts[1][1].as_deref(), Some(""));
        assert_eq!(inserts[2][1], None);
    }

    /// A value with a line break or a tab copies in quotes, as spreadsheets
    /// write it, and pastes back whole (#276): it was copied as it is, and
    /// Paste split it into new rows and columns. Text from a spreadsheet
    /// pastes as before.
    #[test]
    fn line_breaks_and_tabs_copy_in_quotes() {
        let (columns, _) = three();
        let row = |a: &str, b: &str, c: &str| vec![Some(a.into()), Some(b.into()), Some(c.into())];
        let rows = vec![
            row("1", "two\nlines", "a\tb"),
            row("2", "\"quoted\"", "say \"hi\""),
        ];
        let copy = "1\t\"two\nlines\"\t\"a\tb\"\n2\t\"\"\"quoted\"\"\"\tsay \"hi\"";
        let mut state = GridState::default();
        state.sync(&rows, &columns);
        state.select((0, 0), false);
        state.select((1, 2), true);
        assert_eq!(
            selection_text(&columns, &rows, &state, false).unwrap(),
            copy
        );
        assert_eq!(to_tsv(&columns, &rows), format!("A\tB\tC\n{copy}"));
        // Pasted, the values are whole again.
        state.set_editable(None);
        state.add_row(&rows, 3);
        state.paste(&rows, 3, copy);
        assert_eq!(
            state.changes().unwrap().inserts,
            [rows[0].clone(), rows[1].clone()]
        );
        // From a spreadsheet: CRLF between rows, a quoted cell with a line
        // break, plain cells with quotes in them.
        state.clear_changes();
        state.add_row(&rows, 3);
        state.paste(
            &rows,
            3,
            "7\t\"multi\nline\"\tplain \"x\"\r\n8\t\t\"a\"\"b\" c\r\n9\t\"\"\t\"\"\"\"\r\n",
        );
        assert_eq!(
            state.changes().unwrap().inserts,
            [
                row("7", "multi\nline", "plain \"x\""),
                vec![Some("8".into()), None, Some("\"a\"\"b\" c".into())],
                row("9", "", "\""),
            ]
        );
    }

    /// The summary of a selection adds up the values the grid shows,
    /// with the changes waiting for Apply (#277): it read the values
    /// fetched, and with a filter on, a new row's cells were those of a
    /// fetched row the filter hid.
    #[test]
    fn summary_reads_the_changes() {
        let columns = cols();
        let row = |a: &str, b: &str| vec![Some(a.into()), Some(b.into())];
        let rows = vec![row("1", "x"), row("2", "y"), row("3", "z")];
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        state.set_editable(None);
        let summary = |state: &mut GridState| {
            let mut summary = None;
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                summary = ResultGrid::new(&columns, &rows, state, "grid")
                    .show(ui)
                    .selection_summary;
            });
            output.textures_delta.clear();
            summary.unwrap_or_default()
        };
        state.anchor = Some((0, 0));
        state.cursor = Some((2, 0));
        assert_eq!(
            summary(&mut state),
            "Count 3 · Sum 6 · Avg 2 · Min 1 · Max 3"
        );
        state.edit_cell(&rows, 1, 0, Some("20".into()));
        assert_eq!(
            summary(&mut state),
            "Count 3 · Sum 24 · Avg 8 · Min 1 · Max 20"
        );
        state.edit_cell(&rows, 2, 0, None);
        assert_eq!(
            summary(&mut state),
            "Count 2 · Sum 21 · Avg 10.5 · Min 1 · Max 20"
        );
        // A row to be deleted is left out, as Apply leaves it.
        state.delete_row(&rows, 0);
        assert_eq!(
            summary(&mut state),
            "Count 1 · Sum 20 · Avg 20 · Min 20 · Max 20"
        );
        state.clear_changes();
        assert_eq!(
            summary(&mut state),
            "Count 3 · Sum 6 · Avg 2 · Min 1 · Max 3"
        );
        // With a filter, a new row's own cells.
        state.set_filter("x");
        summary(&mut state);
        state.add_row(&rows, 2);
        state.edit_cell(&rows, 1, 0, Some("5".into()));
        state.anchor = Some((0, 0));
        state.cursor = Some((1, 0));
        assert_eq!(
            summary(&mut state),
            "Count 2 · Sum 6 · Avg 3 · Min 1 · Max 5"
        );
    }

    /// The sum and the average of very small or very large numbers keep
    /// them (#278): 2e-20 and 3e-20 added up to "Sum 0 · Avg 0", and 1 and
    /// 1e-5 to 1, rounded to four decimals.
    #[test]
    fn summary_keeps_small_and_large_numbers() {
        let columns = vec![Column {
            name: "N".into(),
            type_name: "FLOAT".into(),
            numeric: true,
        }];
        let summary_of = |values: &[&str]| {
            let rows: Vec<Vec<Cell>> = values.iter().map(|v| vec![Some((*v).into())]).collect();
            let mut state = GridState::default();
            state.sync(&rows, &columns);
            state.select((0, 0), false);
            state.select((rows.len() - 1, 0), true);
            summary(&columns, &rows, &state).unwrap()
        };
        assert_eq!(
            summary_of(&["2e-20", "3e-20"]),
            "Count 2 · Sum 5e-20 · Avg 2.5e-20 · Min 2e-20 · Max 3e-20"
        );
        assert_eq!(
            summary_of(&["1", "1e-5"]),
            "Count 2 · Sum 1.00001 · Avg 0.500005 · Min 1e-5 · Max 1"
        );
        assert_eq!(
            summary_of(&["1e300", "1e300", "1e299"]),
            "Count 3 · Sum 2.1e300 · Avg 7e299 · Min 1e299 · Max 1e300"
        );
        // A double's last digits aren't the numbers'.
        assert_eq!(
            summary_of(&["1e-1", "2e-1"]),
            "Count 2 · Sum 0.3 · Avg 0.15 · Min 1e-1 · Max 2e-1"
        );
        // Plain decimals add up exactly, as before (#238).
        assert_eq!(
            summary_of(&["0.1", "0.2", "2.5"]),
            "Count 3 · Sum 2.8 · Avg 0.93333 · Min 0.1 · Max 2.5"
        );
    }

    /// Rows fetched later are sorted in among those shown: the selection,
    /// the cursor, the cell being edited and the viewer stay on their rows
    /// (#279). They stayed at their places in the view, on other rows, and a
    /// value typed went into another row.
    #[test]
    fn rows_fetched_later_keep_the_selection() {
        let columns = cols();
        let row = |a: &str, b: &str| vec![Some(a.into()), Some(b.into())];
        let mut rows = vec![row("2", "b"), row("4", "d")];
        let ctx = egui::Context::default();
        let mut state = GridState::default();
        state.set_editable(None);
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        state.set_sort(1, &rows, &columns);
        state.add_row(&rows, 2);
        state.edit_cell(&rows, 2, 0, Some("9".into()));
        // b and d selected, d's B being edited, d's A in the viewer.
        state.select((0, 0), false);
        state.select((1, 1), true);
        state.start_edit(&rows, (1, 1), Some("typed".into()));
        state.viewer = Some((1, 0));
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        rows.extend([row("1", "a"), row("3", "c"), row("5", "e")]);
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        assert_eq!(state.order, [2, 0, 3, 1, 4]);
        // From b to d, with c that came in between them.
        assert_eq!(state.anchor, Some((1, 0)));
        assert_eq!(state.cursor, Some((3, 1)));
        assert_eq!(state.viewer, Some((3, 0)));
        assert_eq!(state.value(&rows, 5, 0), Some("9"));
        assert_eq!(
            state.edit.as_ref().unwrap().active,
            Some((3, 1, "typed".into()))
        );
        frame(&ctx, &columns, &rows, &mut state, vec![key(Key::Enter)]);
        let updates = &state.changes().unwrap().updates;
        assert_eq!(updates.len(), 1, "{updates:?}");
        assert_eq!(updates[&1][&1].as_deref(), Some("typed"));
        // The value editor's row too.
        state.open_value_editor(&rows, (0, 1));
        rows.push(row("0", "0"));
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        assert_eq!(state.value_editor.as_ref().map(|e| e.view_row), Some(1));
        // Unsorted, the rows fetched come before the new rows.
        let mut rows = vec![row("1", "a")];
        let mut state = GridState::default();
        state.set_editable(None);
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        state.add_row(&rows, 2);
        assert_eq!(state.cursor, Some((1, 0)));
        rows.push(row("2", "b"));
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        assert_eq!(state.cursor, Some((2, 0)));
    }

    /// The viewer of a value of megabytes keeps up, shows all of it and
    /// copies all of it (#284): it worked the value out again on every
    /// frame and laid it out whole, 1.9 s a frame for 10 MB on one line
    /// in a test build.
    #[test]
    fn the_viewer_of_a_huge_value_keeps_up() {
        let columns = vec![
            Column {
                name: "T".into(),
                type_name: "text".into(),
                numeric: false,
            },
            Column {
                name: "B".into(),
                type_name: "bytea".into(),
                numeric: false,
            },
        ];
        let json = format!("[{}{{\"end\": 1}}]", "{\"a\": 1}, ".repeat(400_000));
        let cases = [
            (format!("{}the end", "word ".repeat(2 << 20)), 0, "the end"),
            (format!("{}the end", "line\n".repeat(2 << 20)), 0, "the end"),
            (json, 0, "]"),
            (
                format!("\\x{}", "ab".repeat(10 << 20)),
                1,
                "10.0 MB · binary",
            ),
        ];
        let ctx = egui::Context::default();
        for (value, col, last) in cases {
            let mut row = vec![None, None];
            row[col] = Some(value.clone());
            let rows = vec![row];
            let mut state = GridState {
                dialect: Dialect::POSTGRES,
                viewer: Some((0, col)),
                ..Default::default()
            };
            // A window is laid out unseen first.
            frame(&ctx, &columns, &rows, &mut state, vec![]);
            let drawn = frame(&ctx, &columns, &rows, &mut state, vec![]);
            let started = std::time::Instant::now();
            for _ in 0..5 {
                frame(&ctx, &columns, &rows, &mut state, vec![]);
            }
            let elapsed = started.elapsed().as_secs_f64();
            assert!(elapsed < 2.0, "{last}: 5 frames took {elapsed} s");
            let at = |drawn: &[(String, egui::Pos2)], text: &str| {
                drawn
                    .iter()
                    .find(|(t, _)| t == text)
                    .unwrap_or_else(|| panic!("{text} not in {:?}", texts(drawn)))
                    .1
            };
            let copy = at(&drawn, "Copy");
            if col == 0 {
                // Scrolled to the end, the end shows.
                let below = copy + egui::vec2(-100.0, 120.0);
                let wheel = egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -1e9),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                };
                frame(
                    &ctx,
                    &columns,
                    &rows,
                    &mut state,
                    vec![egui::Event::PointerMoved(below)],
                );
                frame(&ctx, &columns, &rows, &mut state, vec![wheel]);
                let mut drawn = Vec::new();
                for _ in 0..3 {
                    drawn = frame(&ctx, &columns, &rows, &mut state, vec![]);
                }
                assert!(
                    drawn.iter().any(|(t, _)| t.ends_with(last)),
                    "{last}: {:?}",
                    texts(&drawn)
                );
            } else {
                assert!(texts(&drawn).contains(&last), "{:?}", texts(&drawn));
            }
            // Copy copies all of it.
            let button = |pressed| egui::Event::PointerButton {
                pos: copy,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            frame(
                &ctx,
                &columns,
                &rows,
                &mut state,
                vec![egui::Event::PointerMoved(copy)],
            );
            frame(&ctx, &columns, &rows, &mut state, vec![button(true)]);
            let copied = copied(&ctx, &columns, &rows, &mut state, vec![button(false)]);
            assert!(copied == Some(value), "{last}: not copied whole");
        }
    }

    /// A sort or a filter keeps the selection, the cursor, the cell being
    /// edited, the viewer and the dialog editing a cell on their rows
    /// (#296). They stayed at their places in the view, on other rows: Set
    /// NULL, Delete, the value typed and the dialog's Apply went into rows
    /// that weren't selected.
    #[test]
    fn a_sort_or_a_filter_keeps_the_selection_on_its_rows() {
        let columns = cols();
        let row = |a: &str, b: &str| vec![Some(a.into()), Some(b.into())];
        let rows = vec![row("1", "c"), row("2", "a"), row("3", "b")];
        let ctx = egui::Context::default();
        let new = || {
            let mut state = GridState::default();
            state.set_editable(None);
            state
        };
        let mut state = new();
        // c's B selected, in the viewer and in the dialog; a's being
        // edited. Sorted by B: a, b, c.
        click(&ctx, &columns, &rows, &mut state, "c", 1);
        state.viewer = Some((0, 1));
        state.open_value_editor(&rows, (0, 1));
        state.start_edit(&rows, (1, 1), Some("typed".into()));
        state.set_sort(1, &rows, &columns);
        assert_eq!(state.order, [1, 2, 0]);
        assert_eq!((state.anchor, state.cursor), (Some((2, 1)), Some((2, 1))));
        assert_eq!(state.viewer, Some((2, 1)));
        assert_eq!(state.value_editor.as_ref().map(|e| e.view_row), Some(2));
        frame(&ctx, &columns, &rows, &mut state, vec![key(Key::Enter)]);
        let updates = |state: &GridState| state.changes().unwrap().updates.clone();
        assert_eq!(updates(&state)[&1][&1].as_deref(), Some("typed"));
        // Set NULL and Delete take c (before: b), and so does the dialog.
        state.set_null(&rows, 2);
        state.delete_rows(2);
        assert_eq!(updates(&state).keys().collect::<Vec<_>>(), [&0, &1]);
        assert_eq!(updates(&state)[&0][&1], None);
        let deletes = &state.changes().unwrap().deletes;
        assert_eq!(deletes.iter().collect::<Vec<_>>(), [&0]);
        state.viewer = None;
        state.value_editor.as_mut().unwrap().text = "dialog".into();
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        click(&ctx, &columns, &rows, &mut state, "Apply", 1);
        assert_eq!(updates(&state)[&0][&1].as_deref(), Some("dialog"));
        assert_eq!(updates(&state)[&1][&1].as_deref(), Some("typed"));

        // Rows a and b selected, sorted the other way: c, b, a. They no
        // longer stand together: the cursor's row b stays selected.
        let mut state = new();
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        state.set_sort(1, &rows, &columns);
        state.select((0, 0), false);
        state.select((1, 1), true);
        state.set_sort(1, &rows, &columns);
        assert_eq!(state.order, [0, 2, 1]);
        assert_eq!((state.anchor, state.cursor), (Some((1, 0)), Some((1, 1))));
        state.set_null(&rows, 2);
        assert_eq!(updates(&state).keys().collect::<Vec<_>>(), [&2]);

        // A filter: the viewer stays on b, the dialog on c, which it
        // hides, closes, and the edit of a's cell keeps its value.
        let mut state = new();
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        state.viewer = Some((2, 1));
        state.open_value_editor(&rows, (0, 1));
        state.start_edit(&rows, (1, 1), Some("kept".into()));
        state.set_filter("b");
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        assert_eq!(state.order, [2]);
        assert_eq!(state.viewer, Some((0, 1)));
        assert!(state.value_editor.is_none());
        assert!(state.edit.as_ref().unwrap().active.is_none());
        assert_eq!(updates(&state)[&1][&1].as_deref(), Some("kept"));
        state.set_filter("");
        frame(&ctx, &columns, &rows, &mut state, vec![]);
        assert_eq!(state.viewer, Some((2, 1)));
    }
}
