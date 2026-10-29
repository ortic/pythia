//! Interactive demo of the code editor widget.
//!
//! `cargo run -p pythia-ui --example code_editor`
//!
//! With `PYTHIA_SCREENSHOT=<file>` it types into the editor by itself, answers
//! the completion request, writes a raw RGBA screenshot (8-byte width/height
//! header, then pixels) and exits. Used to check rendering without a human.
//! `PYTHIA_DEMO=fold` scripts folding and bookmarks instead, `PYTHIA_DEMO=snippet`
//! expands a snippet and fills its placeholders.
//!
//! Try: hover the gutter and click a chevron, Ctrl+K Ctrl+0 / Ctrl+K Ctrl+J,
//! Ctrl+Shift+[ / ], Ctrl+Shift+1 then Ctrl+1, Ctrl+F2 / F2, or type `sel`,
//! `fori` or `pkg` and press Tab.

use std::sync::Arc;

use pythia_ui::code_editor::{
    CodeEditor, CompletionEntry, CompletionKind, EditorConfig, Marker, MarkerKind,
};

const SAMPLE: &str = "create or replace package body emp_api as
  /* Employee maintenance.
     Second line of a block comment. */
  procedure raise_salary(p_emp_id in employees.employee_id%type, p_pct in number) is
    l_name employees.last_name%type;
  begin
    select last_name into l_name from employees where employee_id = p_emp_id;
    if p_pct > 10 then
      raise_application_error(-20001, 'Raise too high for ' || l_name);
    end if;
    update employees e
       set e.salary = e.salary * (1 + p_pct / 100)
     where e.employee_id = p_emp_id;
    dbms_output.put_line(q'[It's done: ]' || l_name); -- log
  end raise_salary;

  function emp_count return pls_integer is
  begin
    for r in (select /*+ full(e) */ * from employees e) loop
      null;
    end loop;
    return 0;
  end;
end emp_api;
/
select * from\x20
";

struct Demo {
    editor: CodeEditor,
    config: EditorConfig,
    frame: u64,
    screenshot: Option<String>,
    log: Vec<String>,
}

impl Demo {
    fn new(screenshot: Option<String>) -> Self {
        let mut editor = CodeEditor::new(SAMPLE);
        editor.set_markers(vec![
            Marker {
                line: 9,
                kind: MarkerKind::Error,
                message: "PLS-00201: identifier 'L_NAMEX' must be declared".into(),
            },
            Marker {
                line: 7,
                kind: MarkerKind::Breakpoint,
                message: "Breakpoint".into(),
            },
            Marker {
                line: 11,
                kind: MarkerKind::ExecutionPoint,
                message: String::new(),
            },
            Marker {
                line: 17,
                kind: MarkerKind::Bookmark,
                message: "Bookmark 1".into(),
            },
            Marker {
                line: 19,
                kind: MarkerKind::Warning,
                message: "PLW-06009: hint".into(),
            },
        ]);
        editor.request_focus();
        Self {
            editor,
            config: EditorConfig::default(),
            frame: 0,
            screenshot,
            log: Vec::new(),
        }
    }
}

impl eframe::App for Demo {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if self.screenshot.is_none() {
            return;
        }
        // A background window never has OS focus; pretend it does.
        raw.focused = true;
        let demo = std::env::var("PYTHIA_DEMO").unwrap_or_default();
        if demo == "fold" {
            match self.frame {
                3 => {
                    self.editor.toggle_fold_at(2);
                    self.editor.toggle_fold_at(4);
                    self.editor.toggle_fold_at(19);
                    self.editor.toggle_bookmark(24, None);
                    self.editor.goto(22, 5);
                    // Ctrl+Shift+1 as a US layout reports it.
                    raw.events.push(egui::Event::Key {
                        key: egui::Key::Exclamationmark,
                        physical_key: Some(egui::Key::Num1),
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                    });
                }
                // Dark: hover the gutter (chevrons); light: hover the pill of
                // the folded procedure (preview of the hidden lines).
                5 => {
                    let at = if std::env::var("PYTHIA_LIGHT").is_ok() {
                        egui::pos2(731.0, 38.0)
                    } else {
                        egui::pos2(12.0, 135.0)
                    };
                    raw.events.push(egui::Event::PointerMoved(at));
                }
                _ => {}
            }
            return;
        }
        if demo == "snippet" {
            match self.frame {
                3 => {
                    raw.events
                        .push(key(egui::Key::End, egui::Modifiers::COMMAND));
                    raw.events
                        .push(key(egui::Key::Enter, egui::Modifiers::NONE));
                    raw.events.push(egui::Event::Text("sel".into()));
                }
                4 => raw.events.push(key(egui::Key::Tab, egui::Modifiers::NONE)),
                5 => raw
                    .events
                    .push(egui::Event::Text("e.last_name, e.salary".into())),
                6 => raw.events.push(key(egui::Key::Tab, egui::Modifiers::NONE)),
                7 => raw.events.push(egui::Event::Text("employees e".into())),
                8 => raw.events.push(key(egui::Key::Tab, egui::Modifiers::NONE)),
                _ => {}
            }
            return;
        }
        // Scripted input: go to the last line's end, then type "em".
        match self.frame {
            3 => {
                raw.events
                    .push(key(egui::Key::End, egui::Modifiers::COMMAND));
                raw.events
                    .push(key(egui::Key::ArrowUp, egui::Modifiers::NONE));
                raw.events.push(key(egui::Key::End, egui::Modifiers::NONE));
            }
            4 => raw.events.push(egui::Event::Text("e".into())),
            5 => raw.events.push(egui::Event::Text("m".into())),
            // Light run: close the popup, select a word, open find for it.
            8 if std::env::var("PYTHIA_LIGHT").is_ok() => {
                raw.events
                    .push(key(egui::Key::Escape, egui::Modifiers::NONE));
                raw.events
                    .push(key(egui::Key::Home, egui::Modifiers::COMMAND));
                for _ in 0..6 {
                    raw.events
                        .push(key(egui::Key::ArrowDown, egui::Modifiers::NONE));
                }
                raw.events.push(key(egui::Key::End, egui::Modifiers::NONE));
                raw.events
                    .push(key(egui::Key::ArrowLeft, egui::Modifiers::COMMAND));
                raw.events
                    .push(key(egui::Key::ArrowLeft, egui::Modifiers::COMMAND));
                raw.events
                    .push(key(egui::Key::ArrowLeft, egui::Modifiers::COMMAND));
                raw.events.push(key(
                    egui::Key::ArrowLeft,
                    egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                ));
            }
            9 if std::env::var("PYTHIA_LIGHT").is_ok() => {
                raw.events.push(key(egui::Key::F, egui::Modifiers::COMMAND));
            }
            _ => {}
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame += 1;
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let out = self.editor.ui(ui, &self.config);
                if let Some(trigger) = out.completion_request {
                    self.log.push(format!("completion {trigger:?}"));
                    let word_start = self.editor.text()[..trigger.offset]
                        .rfind(|c: char| !c.is_alphanumeric() && c != '_')
                        .map_or(0, |i| i + 1);
                    let entry = |label: &str, detail: &str, kind| CompletionEntry {
                        label: label.into(),
                        detail: detail.into(),
                        kind,
                        insert_text: label.into(),
                        documentation: None,
                    };
                    let mut items = vec![
                        entry("EMPLOYEES", "HR · table", CompletionKind::Table),
                        entry("EMP_DETAILS_VIEW", "HR · view", CompletionKind::View),
                        entry("EMP_API", "HR · package", CompletionKind::Package),
                        entry("EMPTY_BLOB", "function", CompletionKind::Function),
                        entry("DEPARTMENTS", "HR · table", CompletionKind::Table),
                    ];
                    // Snippets complete too; accepting one expands it.
                    let prefix = &self.editor.text()[word_start..trigger.offset];
                    items.extend(self.editor.snippet_completions(prefix));
                    self.editor
                        .show_completions(word_start..trigger.offset, items, false);
                }
                if let Some(off) = out.goto_request {
                    self.log.push(format!("goto {off}"));
                }
            });

        if let Some(path) = &self.screenshot {
            // Give tooltips time to appear in the fold demo.
            let shot_frame = if std::env::var("PYTHIA_DEMO").is_ok_and(|d| d == "fold") {
                60
            } else {
                12
            };
            if self.frame == shot_frame {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            }
            let shot = ctx.input(|i| {
                i.raw.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                    _ => None,
                })
            });
            if let Some(image) = shot {
                let mut bytes = Vec::with_capacity(8 + image.pixels.len() * 4);
                bytes.extend((image.size[0] as u32).to_le_bytes());
                bytes.extend((image.size[1] as u32).to_le_bytes());
                for p in &image.pixels {
                    bytes.extend(p.to_array());
                }
                std::fs::write(path, bytes).expect("writing screenshot");
                println!("requests: {:?}", self.log);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            // Keep frames coming so the script advances.
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }
}

fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

fn main() -> eframe::Result {
    let screenshot = std::env::var("PYTHIA_SCREENSHOT").ok();
    let dark = std::env::var("PYTHIA_LIGHT").is_err();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 640.0])
            .with_title("Pythia editor demo"),
        ..Default::default()
    };
    eframe::run_native(
        "pythia-editor-demo",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            });
            Ok(Box::new(Demo::new(screenshot)))
        }),
    )
}
