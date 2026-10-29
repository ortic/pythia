//! Monitoring document: hosts the `pythia_monitor` views on a dedicated session.

use std::collections::HashMap;

use egui::Ui;
use pythia_db::{DbEvent, DbRequest};
use pythia_monitor::{Monitor, MonitorAction, MonitorTab, MonitorTask, Token};
use serde::{Deserialize, Serialize};

use crate::documents::{DocContext, Effects, ObjectRef, OpenRequest, Target, request_id};

/// Which view a monitor document opens with (persisted with the layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MonitorStart {
    #[default]
    Dashboard,
    Sessions,
    Blocking,
    Awr,
    Trace,
}

#[derive(Serialize, Deserialize)]
pub struct MonitorWindow {
    pub start: MonitorStart,
    #[serde(skip)]
    state: Option<State>,
}

struct State {
    monitor: Monitor,
    /// Request id → monitor token.
    tokens: HashMap<u64, Token>,
    /// egui time of the last frame, handed to the monitor with results.
    now: f64,
}

impl MonitorWindow {
    pub fn new(start: MonitorStart) -> Self {
        Self { start, state: None }
    }

    #[cfg(test)]
    pub fn test_monitor(&mut self) -> &mut Monitor {
        &mut self.state().monitor
    }

    #[cfg(test)]
    pub fn test_monitor_ref(&self) -> Option<&Monitor> {
        self.state.as_ref().map(|s| &s.monitor)
    }

    fn state(&mut self) -> &mut State {
        let start = self.start;
        self.state.get_or_insert_with(|| {
            let mut monitor = Monitor::new(match start {
                MonitorStart::Dashboard => MonitorTab::Dashboard,
                MonitorStart::Sessions | MonitorStart::Blocking => MonitorTab::Sessions,
                MonitorStart::Awr => MonitorTab::Awr,
                MonitorStart::Trace => MonitorTab::Trace,
            });
            if start == MonitorStart::Blocking {
                // Oracle's sessions, or the other databases'.
                monitor.sessions.show_blocking();
                monitor.connections.show_blocking();
            }
            State {
                monitor,
                tokens: HashMap::new(),
                now: 0.0,
            }
        })
    }

    pub fn title(&self) -> String {
        self.state
            .as_ref()
            .map_or_else(|| "Monitor".to_owned(), |s| s.monitor.title())
    }

    pub fn is_busy(&self) -> bool {
        self.state.as_ref().is_some_and(|s| !s.tokens.is_empty())
    }

    pub fn on_event(&mut self, event: DbEvent) -> Effects {
        if let DbEvent::TaskDone { id, result } = event {
            let state = self.state();
            if let Some(token) = state.tokens.remove(&id) {
                let now = state.now;
                state.monitor.on_task_done(token, result, now);
            }
        }
        Effects::default()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &DocContext) -> Effects {
        let mut effects = Effects::default();
        if !cx.connected {
            ui.centered_and_justified(|ui| ui.weak("Connect to see the monitor"));
            return effects;
        }
        let state = self.state();
        state.now = ui.input(|i| i.time);
        state
            .monitor
            .set_analyze(!cx.settings.ai.backends.is_empty());
        state.monitor.set_dialect(cx.dialect);
        let out = state.monitor.ui(ui);
        for request in out.requests {
            let id = request_id();
            state.tokens.insert(id, request.token);
            let request = match request.task {
                MonitorTask::Oracle(task) => DbRequest::Task { id, task },
                MonitorTask::Client(task) => DbRequest::Run { id, task },
            };
            effects.request(Target::Own, request);
        }
        for action in out.actions {
            match action {
                MonitorAction::OpenPath(path) => open_path(&path, &mut effects),
                MonitorAction::CopyText(text) => ui.ctx().copy_text(text),
                MonitorAction::Analyze(analysis) => effects.ai.push(ai_request(analysis)),
                MonitorAction::OpenSql(text) => {
                    if let Some(connection) = cx.connection.clone() {
                        effects.open.push(OpenRequest::Sql {
                            connection,
                            text,
                            run: false,
                        });
                    }
                }
                MonitorAction::Maintain(fix) => {
                    if let Some(connection) = cx.connection.clone() {
                        effects.open.push(maintain(connection, fix));
                    }
                }
                MonitorAction::OpenObject {
                    owner,
                    name,
                    object_type,
                    subprogram,
                } => {
                    if let Some(connection) = cx.connection.clone() {
                        effects.open.extend(open_object(
                            connection,
                            owner,
                            name,
                            &object_type,
                            subprogram,
                        ));
                    }
                }
            }
        }
        effects
    }
}

/// The maintenance that fixes a dashboard finding (#145): the table's
/// window on the task, or the database's scan.
fn maintain(connection: String, fix: pythia_monitor::insights::Fix) -> OpenRequest {
    use crate::maintenance::{Options, Task};
    use pythia_monitor::insights::{Fix, FixTask};
    match fix {
        Fix::Table {
            schema,
            table,
            task,
        } => {
            let task = match task {
                FixTask::VacuumAnalyze => (
                    Task::Vacuum,
                    Options {
                        analyze: true,
                        ..Options::default()
                    },
                ),
                FixTask::Optimize => (Task::Optimize, Options::default()),
            };
            OpenRequest::Maintenance {
                connection,
                object: ObjectRef {
                    owner: schema,
                    name: table,
                    object_type: pythia_db::ObjectType::Table,
                },
                task: Some(task),
            }
        }
        Fix::Scan => OpenRequest::MaintenanceScan {
            connection,
            schema: String::new(),
        },
    }
}

/// What opens an object the monitor names: a program window (at the
/// subprogram) for PL/SQL, else its object view.
fn open_object(
    connection: String,
    owner: String,
    name: String,
    object_type: &str,
    subprogram: Option<String>,
) -> Option<OpenRequest> {
    let object_type = pythia_db::ObjectType::from_dictionary(object_type)?;
    let object = ObjectRef {
        owner,
        name,
        object_type,
    };
    Some(match subprogram {
        Some(member) if object_type.has_source() => OpenRequest::ProgramMember {
            connection,
            object,
            member,
            parameter: None,
        },
        _ if object_type.has_source() => OpenRequest::Program { connection, object },
        _ => OpenRequest::Object { connection, object },
    })
}

/// Opens a file with the system's default application.
fn open_path(path: &std::path::Path, effects: &mut Effects) {
    if let Err(e) = crate::desktop::open_path(path) {
        effects
            .log
            .push(format!("Could not open {}: {e}", path.display()));
    }
}

/// An assistant request for something the monitor shows. Captured binds
/// are labeled as bind values, so the data policy applies to them.
fn ai_request(analysis: pythia_monitor::Analysis) -> crate::ai::AiRequest {
    use pythia_ai::{Part, Task};
    let mut parts = Vec::new();
    let mut lookup = crate::ai::Lookup {
        sql_id: analysis.sql_id,
        ..Default::default()
    };
    if let Some(sql) = &analysis.sql {
        parts.push(Part::code("Statement", sql));
        lookup.tables = crate::ai::context::table_refs(sql);
    }
    if !analysis.details.is_empty() {
        parts.push(Part::text("Statistics and details", analysis.details));
    }
    if !analysis.binds.is_empty() {
        parts.push(Part::bind_values(
            "Captured bind values",
            analysis.binds.join("\n"),
        ));
    }
    crate::ai::AiRequest {
        task: Task::AnalyzeSlow(analysis.title),
        parts,
        lookup,
    }
}

#[cfg(test)]
mod tests {
    use pythia_ai::Sensitivity;

    use super::*;

    #[test]
    fn trace_binds_are_bind_values() {
        let request = ai_request(pythia_monitor::Analysis {
            title: "statement 1 from the trace file x.trc".into(),
            sql: Some("select * from emp where empno = :1".into()),
            sql_id: None,
            details: "parse 1 0.00".into(),
            binds: vec!["abc: 7839".into()],
        });
        let kinds: Vec<Sensitivity> = request.parts.iter().map(|p| p.sensitivity).collect();
        assert_eq!(
            kinds,
            [
                Sensitivity::Metadata,
                Sensitivity::Metadata,
                Sensitivity::BindValues
            ]
        );
        assert_eq!(request.lookup.tables, [(None, "EMP".to_owned())]);
        assert_eq!(
            request.task.title(),
            "Analyze statement 1 from the trace file x.trc"
        );
    }
}
