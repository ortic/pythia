//! *Save to file…* of a result's cells (#293). The file dialog and the
//! file's bytes are on helper threads, as an export's are, so the window
//! keeps drawing. A value of the result being edited is fetched again
//! whole, by its row's key: the grid holds a binary value cut short on
//! most databases.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use pythia_db::{DbEvent, DbKind, DbRequest};
use pythia_ui::{FileRequest, SaveSource, SaveValue};

use super::{BottomTab, Level, Message, SqlWindow};
use crate::documents::{DocContext, Effects, Target, request_id};

/// The files of a window's results in work.
#[derive(Default)]
pub struct ValueFiles {
    /// A file dialog open for a cell.
    pick: Option<Pick>,
    /// A value fetched whole for the file chosen.
    fetch: Option<Fetch>,
    /// Files written on helper threads.
    io: Vec<Receiver<Done>>,
    /// The files tests choose in place of the dialog, in order (`None`:
    /// cancelled).
    #[cfg(test)]
    pub chosen: std::collections::VecDeque<Option<PathBuf>>,
    /// The file names the dialogs suggested.
    #[cfg(test)]
    pub suggested: Vec<String>,
}

impl ValueFiles {
    /// A dialog is open, a value is fetched, or a file written.
    pub fn is_busy(&self) -> bool {
        self.pick.is_some() || self.fetch.is_some() || !self.io.is_empty()
    }

    /// A value is being fetched on the window's session.
    pub fn fetching(&self) -> bool {
        self.fetch.is_some()
    }

    /// Whether the session's event `id` is for the value being fetched.
    fn fetches(&self, id: u64) -> bool {
        self.fetch.as_ref().is_some_and(|f| f.id == id)
    }

    /// Opens the dialog for `request` of result `result`.
    pub(super) fn ask(&mut self, result: u64, request: FileRequest) {
        if self.pick.is_some() {
            return;
        }
        let (tx, path) = mpsc::channel();
        let FileRequest::Save(save) = &request;
        #[cfg(test)]
        self.suggested.push(save.file_name.clone());
        #[cfg(test)]
        if let Some(chosen) = self.chosen.pop_front() {
            let _ = tx.send(chosen);
            self.pick = Some(Pick {
                result,
                request,
                path,
            });
            return;
        }
        let name = save.file_name.clone();
        std::thread::spawn(move || {
            let _ = tx.send(rfd::FileDialog::new().set_file_name(name).save_file());
        });
        self.pick = Some(Pick {
            result,
            request,
            path,
        });
    }
}

struct Pick {
    result: u64,
    request: FileRequest,
    path: Receiver<Option<PathBuf>>,
}

/// A value fetched for a file, by the request's id.
struct Fetch {
    id: u64,
    path: PathBuf,
    binary: bool,
}

/// What a helper thread did.
enum Done {
    Saved { path: PathBuf, bytes: usize },
    Failed(String),
}

/// Writes `bytes` (made on the helper thread) into `path`.
fn write_file(
    path: PathBuf,
    bytes: impl FnOnce() -> Result<Vec<u8>, String> + Send + 'static,
) -> Receiver<Done> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let done = bytes()
            .and_then(|bytes| {
                std::fs::write(&path, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(bytes.len())
            })
            .map_or_else(
                |e| Done::Failed(format!("Save to file failed: {e}")),
                |bytes| Done::Saved { path, bytes },
            );
        let _ = tx.send(done);
    });
    rx
}

impl SqlWindow {
    /// Goes on with the files in work: the file chosen, then its value
    /// fetched and written; says what was done.
    pub(super) fn poll_value_files(
        &mut self,
        ctx: &egui::Context,
        cx: &DocContext,
        effects: &mut Effects,
    ) {
        let files = &mut self.state.value_files;
        if let Some(pick) = &files.pick {
            match pick.path.try_recv() {
                Ok(Some(path)) => {
                    let pick = files.pick.take().expect("the dialog");
                    self.file_chosen(pick, path, cx, effects);
                }
                Ok(None) | Err(TryRecvError::Disconnected) => files.pick = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let mut done = Vec::new();
        self.state.value_files.io.retain(|rx| match rx.try_recv() {
            Ok(d) => {
                done.push(d);
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        });
        for d in done {
            self.file_done(d);
        }
        if self.state.value_files.is_busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn file_chosen(&mut self, pick: Pick, path: PathBuf, cx: &DocContext, effects: &mut Effects) {
        let FileRequest::Save(save) = pick.request;
        let SaveValue {
            col,
            binary,
            source,
            ..
        } = save;
        let rx = match source {
            SaveSource::Held(text) => {
                write_file(path, move || pythia_ui::value_bytes(&text, binary))
            }
            SaveSource::Fetch(row) => {
                self.fetch_value(pick.result, row, col, path, binary, cx, effects);
                return;
            }
        };
        self.state.value_files.io.push(rx);
    }

    /// Fetches the value of `col` in fetched row `row` of result `result`
    /// again whole, by the row's key, for the file at `path`.
    #[allow(clippy::too_many_arguments)]
    fn fetch_value(
        &mut self,
        result: u64,
        row: usize,
        col: usize,
        path: PathBuf,
        binary: bool,
        cx: &DocContext,
        effects: &mut Effects,
    ) {
        let Some(r) = self.state.results.iter().find(|r| r.id == result) else {
            self.file_failed("Save to file: the result was fetched again; save the value again");
            return;
        };
        let query = r.edit.as_ref().and_then(|edit| {
            crate::data_edit::value_query(cx.dialect, edit, &r.columns, r.rows.get(row)?, col)
        });
        let Some(sql) = query else {
            self.file_failed("Save to file: the row can't be found again by its key");
            return;
        };
        if self.session_busy() {
            self.file_failed("Save to file: the session is busy; try again once it is done");
            return;
        }
        let id = request_id();
        let request = if cx.dialect.kind() == DbKind::Oracle {
            // A task leaves an open result's cursor alone; values come as
            // the driver reads them, LOBs whole.
            DbRequest::Task {
                id,
                task: Box::new(move |conn: &pythia_db::oracle::Connection| {
                    // `None`: no row; `Some(None)`: NULL.
                    let value: Option<Option<Vec<u8>>> = match conn.query(&sql, &[])?.next() {
                        None => None,
                        Some(row) if binary => Some(row?.get(0)?),
                        Some(row) => {
                            Some(row?.get::<_, Option<String>>(0)?.map(String::into_bytes))
                        }
                    };
                    Ok(Box::new(value) as Box<dyn std::any::Any + Send>)
                }),
            }
        } else {
            // The session reads one result at a time: this query ends the
            // one with rows left, whose FetchMore would then find none.
            if let Some(open) = self.state.results.last().filter(|r| r.more && !r.ended) {
                let id = open.id;
                self.query_closed(id, "Save to file ended its query");
            }
            DbRequest::ExecuteWhole {
                id,
                sql,
                max_rows: 2,
            }
        };
        self.state.value_files.fetch = Some(Fetch { id, path, binary });
        effects.request(Target::Own, request);
    }

    /// Takes the events of the value fetched for a file; gives back
    /// another statement's.
    pub(super) fn value_event(&mut self, event: DbEvent) -> Option<DbEvent> {
        let id = match &event {
            DbEvent::QueryStarted { id, .. }
            | DbEvent::Rows { id, .. }
            | DbEvent::MoreResults { id }
            | DbEvent::Executed { id, .. }
            | DbEvent::Failed { id, .. }
            | DbEvent::TaskDone { id, .. } => *id,
            _ => return Some(event),
        };
        if !self.state.value_files.fetches(id) {
            return Some(event);
        }
        let fetched: Result<Option<Vec<u8>>, String> = match event {
            DbEvent::QueryStarted { .. } | DbEvent::MoreResults { .. } => return None,
            DbEvent::Rows { rows, .. } => {
                let binary = self
                    .state
                    .value_files
                    .fetch
                    .as_ref()
                    .is_some_and(|f| f.binary);
                match rows.into_iter().next().map(|row| row.into_iter().next()) {
                    Some(Some(Some(text))) => pythia_ui::value_bytes(&text, binary).map(Some),
                    Some(_) => Ok(None),
                    None => Err("the row isn't in the table any more".into()),
                }
            }
            DbEvent::TaskDone { result, .. } => match result {
                Ok(any) => match any.downcast::<Option<Option<Vec<u8>>>>() {
                    Ok(value) => {
                        (*value).ok_or_else(|| "the row isn't in the table any more".into())
                    }
                    Err(_) => Err("unexpected result".into()),
                },
                Err(e) => Err(e.text()),
            },
            DbEvent::Failed { error, .. } => Err(error.text()),
            DbEvent::Executed { .. } => Err("the query returned no rows".into()),
            _ => return None,
        };
        let fetch = self.state.value_files.fetch.take()?;
        match fetched {
            Ok(Some(bytes)) => {
                let rx = write_file(fetch.path, move || Ok(bytes));
                self.state.value_files.io.push(rx);
            }
            Ok(None) => self.file_failed("Save to file: the value is NULL now"),
            Err(e) => self.file_failed(format!("Save to file failed: {e}")),
        }
        None
    }

    fn file_done(&mut self, done: Done) {
        match done {
            Done::Saved { path, bytes } => {
                self.info(format!("Saved {bytes} bytes to {}", path.display()));
            }
            Done::Failed(text) => self.file_failed(text),
        }
    }

    fn file_failed(&mut self, text: impl Into<String>) {
        self.state.messages.push(Message {
            level: Level::Error,
            text: text.into(),
            jump: None,
            stack: Vec::new(),
        });
        self.state.bottom = BottomTab::Messages;
    }
}
