//! Object view: describe a table, view or code object (columns, details, DDL).

use std::collections::HashMap;

use egui::{RichText, Ui};
use egui_extras::{Column as TableColumn, TableBuilder};
use pythia_db::{
    DbEvent, DbRequest, Detail, Dialect, Feature, MetaQuery, MetaResult, ObjectType,
    TableColumn as Col,
};
use pythia_ui::ResultGrid;
use serde::{Deserialize, Serialize};

use crate::documents::{
    DocContext, Effects, ObjectRef, OpenRequest, TableData, Target, request_id,
};
use crate::editor::Editor;

#[derive(Serialize, Deserialize)]
pub struct ObjectView {
    pub object: ObjectRef,
    #[serde(skip)]
    state: State,
    /// The connection's database, which decides the tabs; follows the
    /// connection while the view is shown.
    #[serde(skip)]
    dialect: Dialect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Tab {
    Columns,
    Detail(Detail),
    Ddl,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Columns => "Columns",
            Tab::Detail(d) => d.label(),
            Tab::Ddl => "DDL",
        }
    }
}

/// Where the tab last chosen for objects of type `t` is remembered.
fn remembered_tab(t: ObjectType) -> egui::Id {
    egui::Id::new(("object-view-tab", t.dictionary_name()))
}

pub enum Loaded<T> {
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Default)]
struct State {
    tab: Option<Tab>,
    columns: Option<Loaded<Vec<Col>>>,
    details: HashMap<Detail, Loaded<TableData>>,
    grids: HashMap<Detail, pythia_ui::GridState>,
    ddl: Option<Loaded<Editor>>,
    pending: HashMap<u64, Tab>,
    /// Testing a database link: the request, then the remote global name.
    link_test: Option<(u64, Loaded<String>)>,
}

impl ObjectView {
    /// Opens on the tab last chosen for this type of object.
    pub fn new(object: ObjectRef, dialect: Dialect, ctx: &egui::Context) -> Self {
        let mut view = Self {
            object,
            state: State::default(),
            dialect,
        };
        let label =
            ctx.data_mut(|d| d.get_persisted::<String>(remembered_tab(view.object.object_type)));
        view.state.tab = label.and_then(|l| view.tabs().into_iter().find(|t| t.label() == l));
        view
    }

    pub fn title(&self) -> String {
        crate::browser::shown_name(self.dialect, &self.object.name)
    }

    #[cfg(test)]
    pub fn tab_for_test(&self) -> Option<&'static str> {
        self.state.tab.map(Tab::label)
    }

    #[cfg(test)]
    pub fn loaded_for_test(&self) -> (Option<String>, usize) {
        let ddl = match &self.state.ddl {
            Some(Loaded::Ready(editor)) => Some(editor.text().to_owned()),
            _ => None,
        };
        let columns = match &self.state.columns {
            Some(Loaded::Ready(cols)) => cols.len(),
            _ => 0,
        };
        (ddl, columns)
    }

    pub fn is_loading(&self) -> bool {
        !self.state.pending.is_empty() || matches!(self.state.link_test, Some((_, Loaded::Loading)))
    }

    fn has_columns(&self) -> bool {
        self.object.object_type.has_data()
    }

    fn is_table(&self) -> bool {
        self.object.object_type == ObjectType::Table
    }

    /// Tables open on their columns, everything else on its DDL.
    fn tabs(&self) -> Vec<Tab> {
        let mut tabs = Vec::new();
        if !self.is_table() {
            tabs.push(Tab::Ddl);
        }
        if self.has_columns() {
            tabs.push(Tab::Columns);
        }
        let details = self.dialect.details(self.object.object_type);
        tabs.extend(details.iter().map(|d| Tab::Detail(*d)));
        if self.is_table() {
            tabs.push(Tab::Ddl);
        }
        tabs
    }

    /// Clears everything and loads the current tab again.
    pub fn load(&mut self) -> Effects {
        let tab = self.state.tab.unwrap_or_else(|| self.tabs()[0]);
        self.state = State {
            tab: Some(tab),
            ..State::default()
        };
        self.ensure(tab)
    }

    fn ensure(&mut self, tab: Tab) -> Effects {
        let mut effects = Effects::default();
        let o = &self.object;
        let query = match tab {
            Tab::Columns if self.state.columns.is_none() => {
                self.state.columns = Some(Loaded::Loading);
                MetaQuery::Columns {
                    owner: o.owner.clone(),
                    table: o.name.clone(),
                    keys: o.object_type == ObjectType::Table,
                }
            }
            Tab::Detail(detail) if !self.state.details.contains_key(&detail) => {
                self.state.details.insert(detail, Loaded::Loading);
                MetaQuery::Details {
                    owner: o.owner.clone(),
                    name: o.name.clone(),
                    detail,
                }
            }
            Tab::Ddl if self.state.ddl.is_none() => {
                self.state.ddl = Some(Loaded::Loading);
                MetaQuery::Ddl {
                    owner: o.owner.clone(),
                    name: o.name.clone(),
                    object_type: o.object_type,
                }
            }
            _ => return effects,
        };
        let id = request_id();
        self.state.pending.insert(id, tab);
        effects.request(Target::Meta, DbRequest::Meta { id, query });
        effects
    }

    pub fn on_event(&mut self, event: DbEvent) -> Effects {
        let DbEvent::Meta { id, result } = event else {
            return Effects::default();
        };
        if let Some((test, outcome)) = &mut self.state.link_test
            && *test == id
        {
            *outcome = match result {
                Ok(MetaResult::Text(remote)) => Loaded::Ready(remote),
                Ok(_) => Loaded::Failed("unexpected result".into()),
                Err(e) => Loaded::Failed(e.to_string()),
            };
            return Effects::default();
        }
        let Some(tab) = self.state.pending.remove(&id) else {
            return Effects::default();
        };
        let failed = |e: pythia_db::DbError| e.to_string();
        match (tab, result) {
            // The dictionaries list no columns for a table that isn't
            // there (any more): say so rather than show an empty list (#200).
            (Tab::Columns, Ok(MetaResult::Columns(cols))) if cols.is_empty() => {
                self.state.columns = Some(Loaded::Failed(format!(
                    "There is no {} {}.{}, or it has no columns",
                    self.object.object_type.dictionary_name().to_lowercase(),
                    self.object.owner,
                    self.object.name
                )))
            }
            (Tab::Columns, Ok(MetaResult::Columns(cols))) => {
                self.state.columns = Some(Loaded::Ready(cols))
            }
            (Tab::Columns, Err(e)) => self.state.columns = Some(Loaded::Failed(failed(e))),
            (Tab::Detail(d), Ok(MetaResult::Table { columns, rows })) => {
                self.state.details.insert(d, Loaded::Ready((columns, rows)));
            }
            (Tab::Detail(d), Err(e)) => {
                self.state.details.insert(d, Loaded::Failed(failed(e)));
            }
            (Tab::Ddl, Ok(MetaResult::Text(text))) => {
                self.state.ddl = Some(Loaded::Ready(Editor::new(text)))
            }
            (Tab::Ddl, Err(e)) => self.state.ddl = Some(Loaded::Failed(failed(e))),
            _ => {}
        }
        Effects::default()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext, connection: Option<&str>) -> Effects {
        let mut effects = Effects::default();
        if self.dialect != cx.dialect {
            // Restored with the workspace before its connection said what it is.
            self.dialect = cx.dialect;
            self.state.tab = None;
        }
        let tabs = self.tabs();
        let mut tab = self.state.tab.unwrap_or(tabs[0]);

        ui.horizontal_wrapped(|ui| {
            if let Some(icon) =
                pythia_ui::icons::Icon::for_object_type(self.object.object_type.dictionary_name())
            {
                ui.add(icon.image(ui));
            }
            // The names as stored, a blank one quoted (#281), as is one with
            // spaces around it (#298).
            let name = if crate::browser::quoted_to_show(&self.object.name) {
                self.dialect.quote_ident(&self.object.name)
            } else {
                self.object.name.clone()
            };
            ui.label(RichText::new(format!("{}.{name}", self.object.owner)).strong());
            ui.weak(self.object.object_type.dictionary_name().to_lowercase());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let Some(connection) = connection else {
                    return;
                };
                if ui.button("Refresh").clicked() {
                    effects.extend(self.load());
                    effects.catalog_changed = true;
                }
                if self.object.object_type.has_source() {
                    if self.dialect.supports(Feature::PlSql) {
                        if ui.button("Edit").clicked() {
                            effects.open.push(OpenRequest::Program {
                                connection: connection.to_owned(),
                                object: self.object.clone(),
                            });
                        }
                    } else if let Some(Loaded::Ready(ddl)) = &self.state.ddl
                        && ui
                            .button("Edit")
                            .on_hover_text("Open a script that creates it again")
                            .clicked()
                    {
                        effects.open.push(OpenRequest::Sql {
                            connection: connection.to_owned(),
                            text: crate::browser::replace_script(
                                self.dialect,
                                &self.object,
                                ddl.text(),
                                cx.mariadb,
                            ),
                            run: false,
                        });
                    }
                }
                if self.object.object_type == ObjectType::DatabaseLink
                    && ui.button("Test link").clicked()
                {
                    let id = request_id();
                    let query = MetaQuery::TestLink {
                        owner: self.object.owner.clone(),
                        name: self.object.name.clone(),
                    };
                    self.state.link_test = Some((id, Loaded::Loading));
                    effects.request(Target::Meta, DbRequest::Meta { id, query });
                }
                if self.object.object_type.has_data() && ui.button("Query data").clicked() {
                    effects.open.push(OpenRequest::Sql {
                        connection: connection.to_owned(),
                        text: format!(
                            "select * from {}",
                            self.dialect
                                .qualified(&self.object.owner, &self.object.name)
                        ),
                        run: true,
                    });
                }
            });
        });
        match self.state.link_test.as_ref().map(|(_, outcome)| outcome) {
            Some(Loaded::Loading) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak("Testing the link…");
                });
            }
            Some(Loaded::Ready(remote)) => {
                ui.label(format!("The link works: connected to {remote}"));
            }
            Some(Loaded::Failed(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, format!("The link failed: {e}"));
            }
            None => {}
        }
        ui.horizontal(|ui| {
            for t in &tabs {
                if ui.selectable_value(&mut tab, *t, t.label()).clicked() {
                    let key = remembered_tab(self.object.object_type);
                    ui.data_mut(|d| d.insert_persisted(key, t.label().to_owned()));
                }
            }
        });
        ui.separator();
        if self.state.tab != Some(tab) {
            self.state.tab = Some(tab);
        }
        if cx.connected {
            effects.extend(self.ensure(tab));
        }

        match tab {
            Tab::Columns => match &self.state.columns {
                Some(Loaded::Ready(cols)) => columns_table(ui, cols),
                other => loading_ui(ui, other.as_ref()),
            },
            Tab::Detail(d) => match self.state.details.get(&d) {
                Some(Loaded::Ready((_, rows))) if rows.is_empty() => {
                    ui.weak(format!("No {}", d.label().to_lowercase()));
                }
                Some(Loaded::Ready((columns, rows))) => {
                    let grid = self.state.grids.entry(d).or_default();
                    ResultGrid::new(columns, rows, grid, ("object-detail", cx.id, d.label()))
                        .show(ui);
                }
                other => loading_ui(ui, other),
            },
            Tab::Ddl => match &mut self.state.ddl {
                Some(Loaded::Ready(editor)) => {
                    if let Some(connection) = connection
                        && ui.button("Open in SQL window").clicked()
                    {
                        effects.open.push(OpenRequest::Sql {
                            connection: connection.to_owned(),
                            text: editor.text().to_owned(),
                            run: false,
                        });
                    }
                    editor.ui_read_only(ui, ("object-ddl", cx.id), &cx.settings.editor);
                }
                other => loading_ui(ui, other.as_ref()),
            },
        }
        effects
    }
}

fn loading_ui<T>(ui: &mut Ui, state: Option<&Loaded<T>>) {
    match state {
        Some(Loaded::Failed(e)) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        _ => {
            ui.spinner();
        }
    }
}

fn columns_table(ui: &mut Ui, cols: &[Col]) {
    let row_height = pythia_ui::row_height(ui);
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .auto_shrink(false)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(TableColumn::initial(28.0))
        .column(TableColumn::initial(200.0).clip(true))
        .column(TableColumn::initial(170.0).clip(true))
        .column(TableColumn::initial(50.0))
        .column(TableColumn::initial(140.0).clip(true))
        .column(TableColumn::remainder().clip(true))
        .header(row_height, |mut h| {
            for title in ["", "Name", "Type", "Null", "Default", "Comment"] {
                h.col(|ui| {
                    ui.strong(title);
                });
            }
        })
        .body(|body| {
            body.rows(row_height, cols.len(), |mut row| {
                let c = &cols[row.index()];
                row.col(|ui| {
                    if c.pk_position.is_some() {
                        ui.add(pythia_ui::icons::Icon::PrimaryKey.image(ui))
                            .on_hover_text("Primary key");
                    }
                });
                row.col(|ui| {
                    ui.label(RichText::new(&c.name).monospace());
                });
                row.col(|ui| {
                    ui.label(RichText::new(&c.data_type).monospace());
                });
                row.col(|ui| {
                    if c.nullable {
                        ui.weak("Y");
                    } else {
                        ui.label("N");
                    }
                });
                row.col(|ui| {
                    ui.label(RichText::new(c.default.as_deref().unwrap_or("")).monospace());
                });
                row.col(|ui| {
                    ui.label(c.comment.as_deref().unwrap_or(""));
                });
            });
        });
}
