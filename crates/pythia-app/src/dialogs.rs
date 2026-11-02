//! Modal dialogs and popups: connect, command palette, object finder,
//! statement history, confirmations.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use egui::{Key, RichText, Sense, Ui};
use pythia_config::{ConnectionProfile, Connections as SavedConnections, DbKind, Privilege};
use pythia_db::ssh::HostKey;
use pythia_db::{DbObject, Dialect, Feature, ObjectType};
use pythia_ui::icons::Icon;

use crate::commands::{Command, Keymap, fuzzy_score};
use crate::connections::Passwords;
use crate::documents::{DocId, ObjectRef};
use crate::history::History;
use crate::secrets;

#[derive(Default)]
pub struct ConnectDialog {
    pub open: bool,
    pub connecting: bool,
    pub error: Option<String>,
    /// The Instant Client couldn't be loaded: `error` comes with the steps
    /// to install it.
    missing_client: bool,
    form: ConnectionProfile,
    password: String,
    /// The SSH password, or the key file's passphrase.
    ssh_password: String,
    /// The SSH server's host key isn't known: offered for trust.
    host_key: Option<HostKey>,
    /// Why passwords aren't saved: there's no keychain, or it refused the
    /// last one.
    not_saved: Option<String>,
    /// The key file being chosen in a file dialog.
    picking_key: Option<Receiver<Option<PathBuf>>>,
    /// A SQLite database file being chosen: an existing one, or (`true`) a
    /// new one.
    picking_file: Option<Receiver<(bool, Option<PathBuf>)>>,
    /// Display name of the saved profile being edited, if any.
    editing: Option<String>,
    search: String,
    confirm_delete: bool,
    focus_requested: bool,
    /// Net service names from tnsnames.ora (read when the dialog opens).
    tns_aliases: Vec<String>,
}

pub struct ConnectRequest {
    pub profile: ConnectionProfile,
    pub passwords: Passwords,
    pub save_profile: bool,
    /// The saved profile the form showed, which `profile` replaces when
    /// it's saved (renamed, if its name changed).
    pub previous: Option<String>,
}

pub enum ConnectAction {
    Connect(ConnectRequest),
    /// Trust the SSH server's host key, then connect.
    Trust {
        key: Box<HostKey>,
        then: ConnectRequest,
    },
    /// Save the profile, replacing the one named `previous` (if renamed).
    Save {
        profile: ConnectionProfile,
        previous: Option<String>,
        passwords: Option<Passwords>,
    },
    Delete(String),
}

/// Says that saving would replace the saved connection `name`.
pub fn name_taken(name: &str) -> String {
    format!(
        "There's a saved connection named {name} already. Give this one another name, \
         or pick {name} in the list to change it."
    )
}

/// Accent colors offered for connections.
const COLORS: [[u8; 3]; 8] = [
    [0x3b, 0x82, 0xf6],
    [0x14, 0xb8, 0xa6],
    [0x22, 0xc5, 0x5e],
    [0xea, 0xb3, 0x08],
    [0xf9, 0x73, 0x16],
    [0xa8, 0x55, 0xf7],
    [0xec, 0x48, 0x99],
    [0x64, 0x74, 0x8b],
];

/// What the connect form's fields take on each database.
struct Hints {
    database: &'static str,
    /// The address through an SSH tunnel; `None` without tunnels.
    tunnel: Option<&'static str>,
    /// `None`: the database has no default schema to set.
    schema: Option<&'static str>,
    login_script: &'static str,
}

impl Hints {
    fn of(kind: DbKind) -> Hints {
        match kind {
            DbKind::Oracle => Hints {
                database: "TNS alias or host:1521/service",
                tunnel: None,
                schema: Some("work in another schema after connecting"),
                login_script: "alter session set nls_date_format = 'YYYY-MM-DD HH24:MI:SS';",
            },
            DbKind::MySql => Hints {
                database: "host:3306/database, ?tls for TLS (?tls=skip-verify for a self-signed certificate)",
                tunnel: Some("localhost:3306/database, as the SSH server sees it"),
                schema: Some("database to use after connecting"),
                login_script: "set session time_zone = '+00:00';",
            },
            DbKind::Postgres => Hints {
                database: "host:5432/database, ?sslmode=require for TLS",
                tunnel: Some("localhost:5432/database, as the SSH server sees it"),
                schema: Some("schema to search first after connecting"),
                login_script: "set timezone = 'UTC';",
            },
            DbKind::SqlServer => Hints {
                database: "host:1433/database or host\\instance, ?trust for a self-signed certificate",
                tunnel: Some("localhost:1433/database, as the SSH server sees it (no \\instance)"),
                schema: None,
                login_script: "set dateformat ymd;",
            },
            DbKind::Sqlite => Hints {
                database: "path of the database file, ?create to make it, ?ro to only read",
                tunnel: None,
                schema: None,
                login_script: "attach database 'archive.db' as archive;",
            },
        }
    }
}

impl ConnectDialog {
    pub fn show(&mut self, last: Option<&ConnectionProfile>) {
        self.open = true;
        self.tns_aliases = pythia_config::tns::aliases();
        self.error = None;
        self.missing_client = false;
        self.host_key = None;
        self.focus_requested = false;
        self.confirm_delete = false;
        if self.not_saved.is_none() {
            self.not_saved = secrets::unavailable();
        }
        if let Some(last) = last {
            self.pick(last.clone());
        }
    }

    /// Whether the keychain took the passwords last saved: why not is shown
    /// by *Save password* (#214).
    pub fn passwords_saved(&mut self, saved: Result<(), String>) {
        self.not_saved = saved.err();
    }

    fn pick(&mut self, profile: ConnectionProfile) {
        (self.password, self.ssh_password) = if profile.save_password {
            let name = profile.display_name();
            (
                secrets::load(&name).unwrap_or_default(),
                secrets::load_ssh(&name).unwrap_or_default(),
            )
        } else {
            Default::default()
        };
        self.editing = Some(profile.display_name());
        self.form = profile;
        self.confirm_delete = false;
        self.focus_requested = false;
    }

    fn new_profile(&mut self) {
        self.form = ConnectionProfile::default();
        self.password.clear();
        self.ssh_password.clear();
        self.editing = None;
        self.confirm_delete = false;
        self.focus_requested = false;
    }

    pub fn ui(&mut self, ctx: &egui::Context, saved: &SavedConnections) -> Option<ConnectAction> {
        if !self.open {
            return None;
        }
        let mut action = None;
        let modal = egui::Modal::new(egui::Id::new("connect-dialog")).show(ctx, |ui| {
            ui.set_width(if saved.profiles.is_empty() {
                460.0
            } else {
                720.0
            });
            ui.horizontal_top(|ui| {
                if !saved.profiles.is_empty() {
                    ui.vertical(|ui| {
                        ui.set_width(240.0);
                        self.list_ui(ui, saved, &mut action);
                    });
                    ui.separator();
                }
                ui.vertical(|ui| self.form_ui(ui, saved, &mut action));
            });
        });
        if modal.should_close() && !self.connecting {
            self.open = false;
        }
        action
    }

    fn list_ui(
        &mut self,
        ui: &mut Ui,
        saved: &SavedConnections,
        action: &mut Option<ConnectAction>,
    ) {
        ui.horizontal(|ui| {
            ui.strong("Connections");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if crate::icons_ext::small_icon_button(ui, Icon::Plus, "New connection").clicked() {
                    self.new_profile();
                }
            });
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text("Search…")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(4.0);
        let needle = self.search.to_lowercase();
        let mut folders: Vec<&str> = saved.profiles.iter().map(|p| p.folder.as_str()).collect();
        folders.sort_unstable();
        folders.dedup();
        let mut picked = None;
        let mut connect = None;
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for folder in folders {
                    let profiles: Vec<&ConnectionProfile> = saved
                        .profiles
                        .iter()
                        .filter(|p| p.folder == folder)
                        .filter(|p| {
                            needle.is_empty()
                                || p.display_name().to_lowercase().contains(&needle)
                                || p.connect_string.to_lowercase().contains(&needle)
                        })
                        .collect();
                    if profiles.is_empty() {
                        continue;
                    }
                    if !folder.is_empty() {
                        ui.horizontal(|ui| {
                            ui.add(Icon::Folder.sized(ui, pythia_ui::icons::IconSize::Small));
                            ui.label(RichText::new(folder).small().strong());
                        });
                    }
                    for p in profiles {
                        let name = p.display_name();
                        let selected = self.editing.as_deref() == Some(name.as_str());
                        let r = ui.horizontal(|ui| {
                            if !folder.is_empty() {
                                ui.add_space(10.0);
                            }
                            let color = if p.production {
                                ui.visuals().error_fg_color
                            } else {
                                p.color.map_or(ui.visuals().weak_text_color(), |[r, g, b]| {
                                    egui::Color32::from_rgb(r, g, b)
                                })
                            };
                            let (dot, _) =
                                ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
                            ui.painter().circle_filled(dot.center(), 4.0, color);
                            let mut text = RichText::new(&name);
                            if p.production {
                                text = text.color(ui.visuals().error_fg_color);
                            }
                            let r = ui.add(egui::Button::selectable(selected, text));
                            r.on_hover_text(format!(
                                "{} {}@{}{}",
                                p.kind.label(),
                                p.username,
                                p.connect_string,
                                if p.production { "\nProduction" } else { "" }
                            ))
                        });
                        let r = r.inner;
                        if r.clicked() {
                            picked = Some(p.clone());
                        }
                        if r.double_clicked() {
                            connect = Some(p.clone());
                        }
                    }
                }
            });
        if let Some(p) = picked {
            self.pick(p);
        }
        if let Some(p) = connect {
            self.pick(p);
            *action = self.connect_request(saved);
        }
    }

    fn form_ui(
        &mut self,
        ui: &mut Ui,
        saved: &SavedConnections,
        action: &mut Option<ConnectAction>,
    ) {
        ui.heading(if self.editing.is_some() {
            "Connection"
        } else {
            "New connection"
        });
        ui.add_space(6.0);
        let mut submit = false;
        let oracle = self.form.kind == DbKind::Oracle;
        // A database file: no user, no password.
        let file = self.form.kind == DbKind::Sqlite;
        if let Some((new, picked)) = self.picking_file.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.picking_file = None;
            if let Some(path) = picked {
                let path = path.display().to_string();
                self.form.connect_string = if new { format!("{path}?create") } else { path };
            }
        }
        egui::Grid::new("connect-form")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                ui.label("Type");
                egui::ComboBox::from_id_salt("db-kind")
                    .selected_text(self.form.kind.label())
                    .show_ui(ui, |ui| {
                        for kind in DbKind::ALL {
                            ui.selectable_value(&mut self.form.kind, kind, kind.label());
                        }
                    });
                ui.end_row();

                if !file {
                    ui.label("Username");
                    let user = ui.add(
                        egui::TextEdit::singleline(&mut self.form.username)
                            .hint_text(if oracle {
                                "user, or proxy[target] for proxy authentication"
                            } else {
                                "user"
                            })
                            .desired_width(280.0),
                    );
                    if !self.focus_requested {
                        if self.form.username.is_empty() {
                            user.request_focus();
                        }
                        self.focus_requested = true;
                    }
                    ui.end_row();

                    ui.label("Password");
                    let pw = ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .desired_width(280.0),
                    );
                    if self.focus_requested
                        && !self.form.username.is_empty()
                        && self.password.is_empty()
                        && !pw.has_focus()
                        && ui.memory(|m| m.focused().is_none())
                    {
                        pw.request_focus();
                    }
                    submit |= pw.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.end_row();
                }

                ui.label(if file { "File" } else { "Database" });
                ui.horizontal(|ui| {
                    let aliases = oracle && !self.tns_aliases.is_empty();
                    let width = if aliases {
                        250.0
                    } else if file {
                        200.0
                    } else {
                        280.0
                    };
                    let db = ui.add(
                        egui::TextEdit::singleline(&mut self.form.connect_string)
                            .hint_text({
                                let hints = Hints::of(self.form.kind);
                                match hints.tunnel {
                                    Some(tunnel)
                                        if self.form.ssh.enabled
                                            && Dialect::from(self.form.kind)
                                                .supports(Feature::SshTunnel) =>
                                    {
                                        tunnel
                                    }
                                    _ => hints.database,
                                }
                            })
                            .desired_width(width),
                    );
                    if file && !self.focus_requested {
                        db.request_focus();
                        self.focus_requested = true;
                    }
                    submit |= db.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if file {
                        let idle = self.picking_file.is_none();
                        let open = ui
                            .add_enabled(idle, egui::Button::new("…"))
                            .on_hover_text("Choose a database file");
                        let new = ui
                            .add_enabled(idle, egui::Button::new("New…"))
                            .on_hover_text("Make a new database file");
                        if open.clicked() || new.clicked() {
                            let new = new.clicked();
                            let (tx, rx) = std::sync::mpsc::channel();
                            std::thread::spawn(move || {
                                let dialog = rfd::FileDialog::new()
                                    .add_filter(
                                        "SQLite databases",
                                        &["db", "sqlite", "sqlite3", "db3"],
                                    )
                                    .add_filter("All files", &["*"]);
                                let picked = if new {
                                    dialog.set_file_name("database.db").save_file()
                                } else {
                                    dialog.pick_file()
                                };
                                let _ = tx.send((new, picked));
                            });
                            self.picking_file = Some(rx);
                        }
                    }
                    if aliases {
                        ui.menu_button("▾", |ui| {
                            ui.set_max_height(320.0);
                            let typed = self.form.connect_string.to_lowercase();
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for alias in &self.tns_aliases {
                                    if !typed.is_empty()
                                        && !alias.to_lowercase().contains(&typed)
                                        && self
                                            .tns_aliases
                                            .iter()
                                            .any(|a| a.to_lowercase().contains(&typed))
                                    {
                                        continue;
                                    }
                                    if ui.button(alias).clicked() {
                                        self.form.connect_string = alias.clone();
                                        ui.close();
                                    }
                                }
                            });
                        })
                        .response
                        .on_hover_text("Net service names from tnsnames.ora");
                    }
                });
                ui.end_row();

                if oracle {
                    ui.label("Connect as");
                    egui::ComboBox::from_id_salt("privilege")
                        .selected_text(self.form.privilege.label())
                        .show_ui(ui, |ui| {
                            for p in Privilege::ALL {
                                ui.selectable_value(&mut self.form.privilege, p, p.label());
                            }
                        });
                    ui.end_row();
                }

                ui.label("Name");
                ui.add(
                    egui::TextEdit::singleline(&mut self.form.name)
                        .hint_text("optional, e.g. HR dev")
                        .desired_width(280.0),
                );
                ui.end_row();

                ui.label("Folder");
                ui.add(
                    egui::TextEdit::singleline(&mut self.form.folder)
                        .hint_text("optional, e.g. customer/prod")
                        .desired_width(280.0),
                );
                ui.end_row();

                ui.label("Color");
                ui.horizontal(|ui| {
                    for c in COLORS {
                        let color = egui::Color32::from_rgb(c[0], c[1], c[2]);
                        let (rect, r) =
                            ui.allocate_exact_size(egui::vec2(18.0, 18.0), Sense::click());
                        let selected = self.form.color == Some(c);
                        ui.painter().circle_filled(
                            rect.center(),
                            if selected { 8.0 } else { 6.5 },
                            color,
                        );
                        if selected {
                            ui.painter().circle_stroke(
                                rect.center(),
                                9.0,
                                egui::Stroke::new(1.5, ui.visuals().text_color()),
                            );
                        }
                        if r.clicked() {
                            self.form.color = if selected { None } else { Some(c) };
                        }
                    }
                });
                ui.end_row();
            });

        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.form.production, "Production")
                .on_hover_text("Red marker everywhere and confirmation before commit");
            ui.checkbox(&mut self.form.read_only, "Read-only")
                .on_hover_text("Only queries are allowed");
            if !file {
                ui.checkbox(&mut self.form.save_password, "Save password")
                    .on_hover_text("Stored in the operating system's keychain");
            }
        });
        if let Some(why) = self.not_saved.as_deref()
            && self.form.save_password
            && !file
        {
            ui.label(
                RichText::new(format!("Passwords aren't saved: {why}"))
                    .small()
                    .color(crate::theme::WARN),
            );
        }
        if Dialect::from(self.form.kind).supports(Feature::SshTunnel) {
            submit |= self.ssh_ui(ui);
        }
        egui::CollapsingHeader::new("Advanced")
            .id_salt("connect-advanced")
            .show(ui, |ui| {
                egui::Grid::new("connect-advanced-grid")
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        let hints = Hints::of(self.form.kind);
                        // SQL Server's default schema belongs to the user.
                        if let Some(hint) = hints.schema {
                            ui.label("Default schema");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.form.default_schema)
                                    .hint_text(hint)
                                    .desired_width(280.0),
                            );
                            ui.end_row();
                        }
                        ui.label("Login script");
                        ui.add(
                            egui::TextEdit::multiline(&mut self.form.login_script)
                                .code_editor()
                                .hint_text(hints.login_script)
                                .desired_rows(3)
                                .desired_width(280.0),
                        );
                        ui.end_row();
                    });
                ui.label(
                    RichText::new("Runs on every new session of this connection.")
                        .small()
                        .weak(),
                );
            });

        if let Some(error) = self.error.clone() {
            let error = error.as_str();
            ui.add_space(8.0);
            egui::Frame::new()
                .fill(ui.visuals().error_fg_color.gamma_multiply(0.12))
                .corner_radius(6.0)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    if self.missing_client {
                        client_setup_ui(ui, error);
                    } else {
                        ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
                    }
                    if let Some(key) = &self.host_key
                        && !self.connecting
                        && ui
                            .button("Trust and connect")
                            .on_hover_text("Remember this host key for the server")
                            .clicked()
                    {
                        let key = Box::new(key.clone());
                        if let Some(ConnectAction::Connect(then)) = self.connect_request(saved) {
                            *action = Some(ConnectAction::Trust { key, then });
                        }
                    }
                });
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let valid = self.valid();
            if self.connecting {
                ui.spinner();
                ui.label("Connecting…");
            }
            if let Some(name) = self.editing.clone() {
                if self.confirm_delete {
                    ui.label("Delete this connection?");
                    if ui
                        .button(RichText::new("Delete").color(ui.visuals().error_fg_color))
                        .clicked()
                    {
                        *action = Some(ConnectAction::Delete(name));
                        self.new_profile();
                    }
                    if ui.button("Keep").clicked() {
                        self.confirm_delete = false;
                    }
                } else if crate::icons_ext::small_icon_button(ui, Icon::Trash, "Delete connection")
                    .clicked()
                {
                    self.confirm_delete = true;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let connect = ui.add_enabled(
                    valid && !self.connecting,
                    egui::Button::new(RichText::new("Connect").strong()),
                );
                // A profile with its saved password: nothing to type, Enter
                // connects.
                if connect.enabled()
                    && !self.form.username.is_empty()
                    && !self.password.is_empty()
                    && ui.memory(|m| m.focused().is_none())
                {
                    connect.request_focus();
                }
                if connect.clicked() || (submit && valid && !self.connecting) {
                    *action = self.connect_request(saved);
                }
                if ui.add_enabled(valid, egui::Button::new("Save")).clicked() {
                    let profile = self.cleaned();
                    self.error = self.name_taken(&profile, saved);
                    if self.error.is_none() {
                        let passwords = profile.save_password.then(|| self.passwords());
                        *action = Some(ConnectAction::Save {
                            previous: self.editing.clone(),
                            profile,
                            passwords,
                        });
                    }
                }
                if ui.button("Cancel").clicked() {
                    self.open = false;
                    self.connecting = false;
                }
            });
        });
    }

    /// The SSH tunnel: whether to use one and how to log in to the server.
    /// Returns whether Enter was pressed in the password field.
    fn ssh_ui(&mut self, ui: &mut Ui) -> bool {
        let mut submit = false;
        ui.checkbox(&mut self.form.ssh.enabled, "Connect through SSH")
            .on_hover_text("Reach the database through an SSH server (port forwarding)");
        if let Some(picked) = self.picking_key.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.picking_key = None;
            if let Some(path) = picked {
                self.form.ssh.key_file = path.display().to_string();
            }
        }
        if !self.form.ssh.enabled {
            return false;
        }
        let ssh = &mut self.form.ssh;
        egui::Grid::new("connect-ssh-grid")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                ui.label("SSH server");
                ui.add(
                    egui::TextEdit::singleline(&mut ssh.host)
                        .hint_text("host, or host:port")
                        .desired_width(280.0),
                );
                ui.end_row();

                ui.label("SSH user");
                ui.add(egui::TextEdit::singleline(&mut ssh.username).desired_width(280.0));
                ui.end_row();

                ui.label("SSH password");
                let pw = ui.add(
                    egui::TextEdit::singleline(&mut self.ssh_password)
                        .password(true)
                        .hint_text(if ssh.key_file.trim().is_empty() {
                            "empty: the SSH agent's keys"
                        } else {
                            "the key's passphrase, if it has one"
                        })
                        .desired_width(280.0),
                );
                submit |= pw.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.end_row();

                ui.label("Key file");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut ssh.key_file)
                            .hint_text("optional: OpenSSH or PuTTY key")
                            .desired_width(250.0),
                    );
                    let choose = ui
                        .add_enabled(self.picking_key.is_none(), egui::Button::new("…"))
                        .on_hover_text("Choose a key file");
                    if choose.clicked() {
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            let _ = tx.send(rfd::FileDialog::new().pick_file());
                        });
                        self.picking_key = Some(rx);
                    }
                });
                ui.end_row();
            });
        ui.label(
            RichText::new("The database address is resolved on the SSH server.")
                .small()
                .weak(),
        );
        submit
    }

    /// SSH is ticked and offered for the type: the section is hidden for
    /// a type without tunnels, and connecting ignores it (#95).
    fn ssh_on(&self) -> bool {
        self.form.ssh.enabled && Dialect::from(self.form.kind).supports(Feature::SshTunnel)
    }

    fn valid(&self) -> bool {
        let ssh = &self.form.ssh;
        !self.form.connect_string.trim().is_empty()
            && (!self.ssh_on() || !(ssh.host.trim().is_empty() || ssh.username.trim().is_empty()))
    }

    fn passwords(&self) -> Passwords {
        Passwords {
            database: self.password.clone(),
            ssh: if self.ssh_on() {
                self.ssh_password.clone()
            } else {
                String::new()
            },
        }
    }

    fn cleaned(&self) -> ConnectionProfile {
        let mut profile = self.form.clone();
        profile.username = profile.username.trim().to_owned();
        profile.connect_string = profile.connect_string.trim().to_owned();
        profile.name = profile.name.trim().to_owned();
        profile.folder = profile.folder.trim().trim_matches('/').to_owned();
        if profile.kind != DbKind::Oracle {
            // Hidden for other databases: don't keep one chosen before.
            profile.privilege = Privilege::Normal;
        }
        if profile.kind == DbKind::Sqlite {
            // A file has no users: the fields are hidden.
            profile.username.clear();
            profile.save_password = false;
        }
        let ssh = &mut profile.ssh;
        ssh.host = ssh.host.trim().to_owned();
        ssh.username = ssh.username.trim().to_owned();
        ssh.key_file = ssh.key_file.trim().to_owned();
        if !Dialect::from(profile.kind).supports(Feature::SshTunnel) {
            // Hidden for this database: connect directly, as the dialog shows.
            ssh.enabled = false;
        }
        profile
    }

    /// Why `profile` can't be saved: another saved connection has its name,
    /// but for case, and would be replaced (#246).
    fn name_taken(&self, profile: &ConnectionProfile, saved: &SavedConnections) -> Option<String> {
        saved
            .taken_by(&profile.display_name(), self.editing.as_deref())
            .map(|other| name_taken(&other.display_name()))
    }

    fn connect_request(&mut self, saved: &SavedConnections) -> Option<ConnectAction> {
        if !self.valid() {
            return None;
        }
        let profile = self.cleaned();
        self.error = self.name_taken(&profile, saved);
        self.missing_client = false;
        self.host_key = None;
        if self.error.is_some() {
            return None;
        }
        self.connecting = true;
        Some(ConnectAction::Connect(ConnectRequest {
            profile,
            passwords: self.passwords(),
            save_profile: true,
            previous: self.editing.clone(),
        }))
    }

    /// The profile was saved as `name`: the form shows that one now.
    pub fn saved(&mut self, name: String) {
        self.editing = Some(name);
    }

    /// Fills the form and presses Connect.
    #[cfg(test)]
    pub fn test_submit(
        &mut self,
        profile: ConnectionProfile,
        passwords: Passwords,
    ) -> Option<ConnectAction> {
        self.form = profile;
        self.password = passwords.database;
        self.ssh_password = passwords.ssh;
        self.connect_request(&SavedConnections::default())
    }

    /// As if *…* (or *New…*, `new`) picked `path`.
    #[cfg(test)]
    pub fn test_pick_file(&mut self, new: bool, path: PathBuf) {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send((new, Some(path))).expect("receiver kept");
        self.picking_file = Some(rx);
    }

    #[cfg(test)]
    pub fn test_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The saved profile the form shows, if any.
    #[cfg(test)]
    pub fn test_editing(&self) -> Option<&str> {
        self.editing.as_deref()
    }

    /// Called once the connection is established.
    pub fn connected(&mut self) {
        self.open = false;
        self.connecting = false;
        self.password.clear();
        self.ssh_password.clear();
    }

    pub fn failed(&mut self, error: String) {
        self.open = true;
        self.connecting = false;
        self.error = Some(error);
        self.missing_client = false;
        self.host_key = None;
    }

    /// The SSH server's host key isn't known yet: offers to trust it.
    pub fn failed_host_key(&mut self, error: String, key: HostKey) {
        self.failed(error);
        self.host_key = Some(key);
    }

    /// The Oracle Instant Client couldn't be loaded (DPI-1047): shows how to
    /// install it, with `error` as the details.
    pub fn failed_missing_client(&mut self, error: String) {
        self.failed(error);
        self.missing_client = true;
    }
}

/// One step of installing the Instant Client, with a link where it has one.
struct SetupStep {
    text: &'static str,
    link: Option<&'static str>,
}

const fn step(text: &'static str) -> SetupStep {
    SetupStep { text, link: None }
}

const fn link_step(text: &'static str, link: &'static str) -> SetupStep {
    SetupStep {
        text,
        link: Some(link),
    }
}

const RESTART: SetupStep = step("Restart Pythia and connect again.");

const WINDOWS_STEPS: &[SetupStep] = &[
    link_step(
        "Download the Basic package:",
        "https://www.oracle.com/database/technologies/instant-client/winx64-64-downloads.html",
    ),
    step(r"Unpack it, e.g. to C:\oracle\instantclient_23."),
    step("Add that directory to PATH (System Properties → Environment Variables)."),
    link_step(
        "Install the Microsoft Visual C++ Redistributable (x64), which the Instant Client needs:",
        "https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist",
    ),
    RESTART,
];

const MACOS_STEPS: &[SetupStep] = &[
    link_step(
        "Download the Basic package (DMG):",
        if cfg!(target_arch = "aarch64") {
            "https://www.oracle.com/database/technologies/instant-client/macos-arm64-downloads.html"
        } else {
            "https://www.oracle.com/database/technologies/instant-client/macos-intel-x86-downloads.html"
        },
    ),
    step("Open the DMG and run install_ic.sh from it; it copies the client to Downloads."),
    step(
        "Link the library where Pythia finds it: mkdir -p ~/lib && ln -s ~/Downloads/instantclient_23_*/libclntsh.dylib ~/lib/",
    ),
    RESTART,
];

const LINUX_STEPS: &[SetupStep] = &[
    link_step(
        "Download the Basic package (ZIP):",
        if cfg!(target_arch = "aarch64") {
            "https://www.oracle.com/database/technologies/instant-client/linux-arm-aarch64-downloads.html"
        } else {
            "https://www.oracle.com/database/technologies/instant-client/linux-x86-64-downloads.html"
        },
    ),
    step("Unpack it, e.g. to /opt/oracle/instantclient_23."),
    step("Install libaio: sudo apt install libaio1t64 (libaio1 on older distributions)."),
    step(
        "Make it findable: echo /opt/oracle/instantclient_23 | sudo tee /etc/ld.so.conf.d/oracle.conf && sudo ldconfig (or set LD_LIBRARY_PATH).",
    ),
    RESTART,
];

/// How to install the Oracle Instant Client on this platform.
fn client_setup_steps() -> &'static [SetupStep] {
    if cfg!(target_os = "windows") {
        WINDOWS_STEPS
    } else if cfg!(target_os = "macos") {
        MACOS_STEPS
    } else {
        LINUX_STEPS
    }
}

/// The steps to install the Instant Client, then the original error.
fn client_setup_ui(ui: &mut Ui, error: &str) {
    ui.label(
        RichText::new("The Oracle Instant Client is needed to connect, and it isn't installed.")
            .color(ui.visuals().error_fg_color)
            .strong(),
    );
    ui.add_space(4.0);
    for (i, step) in client_setup_steps().iter().enumerate() {
        ui.horizontal_top(|ui| {
            ui.label(format!("{}.", i + 1));
            ui.vertical(|ui| {
                ui.add(egui::Label::new(step.text).wrap());
                if let Some(link) = step.link {
                    ui.hyperlink(link);
                }
            });
        });
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.weak("Details");
        if ui.small_button("Copy").clicked() {
            ui.ctx().copy_text(error.to_owned());
        }
    });
    ui.add(
        egui::Label::new(RichText::new(error).monospace().small())
            .selectable(true)
            .wrap(),
    );
}

#[derive(Default)]
pub struct CommandPalette {
    pub open: bool,
    query: String,
    selected: usize,
}

impl CommandPalette {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.query.clear();
        self.selected = 0;
    }

    /// Lists the commands available for a connection of `dialect`.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        keymap: &Keymap,
        dialect: pythia_db::Dialect,
    ) -> Option<Command> {
        if !self.open {
            return None;
        }
        let mut matches: Vec<(i32, Command)> = Command::ALL
            .iter()
            .filter(|c| **c != Command::ShowPalette && c.available(dialect))
            .filter_map(|c| fuzzy_score(&self.query, c.label()).map(|s| (s, *c)))
            .collect();
        matches.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        let labels: Vec<(String, String)> = matches
            .iter()
            .map(|(_, c)| (c.label().to_owned(), keymap.shortcut_text(ctx, *c)))
            .collect();
        let picked = popup_list(
            ctx,
            "command-palette",
            "Type a command…",
            &mut self.query,
            &mut self.selected,
            &labels,
        );
        match picked {
            PopupResult::Picked(i) => {
                self.open = false;
                matches.get(i).map(|(_, c)| *c)
            }
            PopupResult::Closed => {
                self.open = false;
                None
            }
            PopupResult::Open => None,
        }
    }
}

enum PopupResult {
    Open,
    Picked(usize),
    Closed,
}

/// A palette-style popup: search field plus a keyboard navigable list of
/// (label, detail) rows.
fn popup_list(
    ctx: &egui::Context,
    id: &str,
    hint: &str,
    query: &mut String,
    selected: &mut usize,
    rows: &[(String, String)],
) -> PopupResult {
    *selected = (*selected).min(rows.len().saturating_sub(1));
    let (up, down, enter, escape) = ctx.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
            i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
            i.consume_key(egui::Modifiers::NONE, Key::Enter),
            i.consume_key(egui::Modifiers::NONE, Key::Escape),
        )
    });
    if up {
        *selected = selected.saturating_sub(1);
    }
    if down && *selected + 1 < rows.len() {
        *selected += 1;
    }
    let mut result = PopupResult::Open;
    let area = egui::Area::new(egui::Id::new(id))
        .anchor(egui::Align2::CENTER_TOP, [0.0, 72.0])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.set_width(560.0);
                    let edit = ui.add(
                        egui::TextEdit::singleline(query)
                            .hint_text(hint)
                            .desired_width(f32::INFINITY)
                            .margin(6.0),
                    );
                    edit.request_focus();
                    if edit.changed() {
                        *selected = 0;
                    }
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .max_height(360.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for (i, (label, detail)) in rows.iter().enumerate() {
                                let button =
                                    egui::Button::selectable(i == *selected, label.as_str())
                                        .right_text(RichText::new(detail.as_str()).weak())
                                        .min_size(egui::vec2(ui.available_width(), 24.0));
                                let response = ui.add(button);
                                if i == *selected && (up || down) {
                                    response.scroll_to_me(None);
                                }
                                if response.clicked() {
                                    result = PopupResult::Picked(i);
                                }
                            }
                            if rows.is_empty() {
                                ui.weak("No matches");
                            }
                        });
                });
        });
    let clicked_outside = ctx.input(|i| i.pointer.any_click())
        && !ctx
            .pointer_interact_pos()
            .is_some_and(|p| area.response.rect.contains(p));
    if enter && !rows.is_empty() {
        result = PopupResult::Picked(*selected);
    } else if escape || clicked_outside {
        result = PopupResult::Closed;
    }
    result
}

/// A declaration to jump to with Go to Symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolItem {
    /// Name, indented by nesting.
    pub label: String,
    /// Kind, and the part ("Spec", "Body") where there are several.
    pub detail: String,
    pub part: usize,
    /// The name in the part's text.
    pub range: std::ops::Range<usize>,
}

/// The declarations of a text (subprograms, variables, cursors, types, …),
/// nested ones indented.
pub fn symbol_items(part: usize, text: &str, part_label: Option<&str>) -> Vec<SymbolItem> {
    use pythia_lsp_core::{OutlineItem, OutlineKind as K};
    fn walk(
        items: &[OutlineItem],
        depth: usize,
        part: usize,
        part_label: Option<&str>,
        out: &mut Vec<SymbolItem>,
    ) {
        for item in items {
            let kind = match item.kind {
                K::Statement | K::Block => continue,
                K::PackageSpec => "package",
                K::PackageBody => "package body",
                K::Procedure => "procedure",
                K::Function => "function",
                K::Trigger => "trigger",
                K::Type | K::TypeDecl => "type",
                K::TypeBody => "type body",
                K::Variable => "variable",
                K::Constant => "constant",
                K::Cursor => "cursor",
                K::Exception => "exception",
            };
            out.push(SymbolItem {
                label: format!("{}{}", "    ".repeat(depth), item.name.to_lowercase()),
                detail: match part_label {
                    Some(p) => format!("{kind} · {p}"),
                    None => kind.to_owned(),
                },
                part,
                range: item.name_span.clone(),
            });
            walk(&item.children, depth + 1, part, part_label, out);
        }
    }
    let mut out = Vec::new();
    walk(
        &pythia_lsp_core::outline(text),
        0,
        part,
        part_label,
        &mut out,
    );
    out
}

/// Go to Symbol: a filterable list of the focused editor's declarations.
#[derive(Default)]
pub struct SymbolPicker {
    pub open: bool,
    query: String,
    selected: usize,
    items: Vec<SymbolItem>,
}

impl SymbolPicker {
    pub fn show(&mut self, items: Vec<SymbolItem>) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.items = items;
    }

    /// The chosen declaration: (part, range).
    pub fn ui(&mut self, ctx: &egui::Context) -> Option<(usize, std::ops::Range<usize>)> {
        if !self.open {
            return None;
        }
        // In source order, unless a query ranks them.
        let mut matches: Vec<(i32, &SymbolItem)> = self
            .items
            .iter()
            .filter_map(|item| fuzzy_score(&self.query, item.label.trim()).map(|s| (s, item)))
            .collect();
        if !self.query.trim().is_empty() {
            matches.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        }
        let rows: Vec<(String, String)> = matches
            .iter()
            .map(|(_, item)| (item.label.clone(), item.detail.clone()))
            .collect();
        let picked = popup_list(
            ctx,
            "symbol-picker",
            "Go to symbol…",
            &mut self.query,
            &mut self.selected,
            &rows,
        );
        match picked {
            PopupResult::Picked(i) => {
                self.open = false;
                matches
                    .get(i)
                    .map(|(_, item)| (item.part, item.range.clone()))
            }
            PopupResult::Closed => {
                self.open = false;
                None
            }
            PopupResult::Open => None,
        }
    }
}

/// Find References: the found places of a document, stepped through with the
/// arrow keys.
#[derive(Default)]
pub struct ReferenceList {
    pub open: bool,
    query: String,
    selected: usize,
    pub doc: Option<DocId>,
    items: Vec<crate::documents::Reference>,
    /// The selection shown last, to show a new one once.
    shown: Option<usize>,
    /// A reference was shown: the place the list was opened from is in the
    /// navigation history.
    pub jumped: bool,
}

pub enum ReferenceAction {
    /// Show this reference (the selection moved, or it was picked).
    Show {
        doc: DocId,
        part: usize,
        range: std::ops::Range<usize>,
    },
    /// Closed with Escape: back to where the list was opened.
    Cancel,
}

impl ReferenceList {
    pub fn show(&mut self, doc: DocId, items: Vec<crate::documents::Reference>) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.shown = None;
        self.jumped = false;
        self.doc = Some(doc);
        self.items = items;
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> Option<ReferenceAction> {
        let doc = self.doc.filter(|_| self.open)?;
        // Before the list consumes it.
        let escape = ctx.input(|i| i.key_pressed(Key::Escape));
        let matches: Vec<&crate::documents::Reference> = self
            .items
            .iter()
            .filter(|r| fuzzy_score(&self.query, &r.text).is_some())
            .collect();
        let rows: Vec<(String, String)> = matches
            .iter()
            .map(|r| {
                let place = match r.part_label {
                    Some(p) => format!("{p} {}", r.line),
                    None => format!("line {}", r.line),
                };
                (r.text.clone(), place)
            })
            .collect();
        let hint = format!(
            "{} reference{} to {}, ↑↓ to step through",
            self.items.len(),
            if self.items.len() == 1 { "" } else { "s" },
            self.items.first().map_or("", |r| r.word.as_str())
        );
        let picked = popup_list(
            ctx,
            "reference-list",
            &hint,
            &mut self.query,
            &mut self.selected,
            &rows,
        );
        let show = |r: &crate::documents::Reference| ReferenceAction::Show {
            doc,
            part: r.part,
            range: r.range.clone(),
        };
        match picked {
            PopupResult::Picked(i) => {
                self.open = false;
                matches.get(i).map(|r| show(r))
            }
            PopupResult::Closed => {
                self.open = false;
                escape.then_some(ReferenceAction::Cancel)
            }
            PopupResult::Open if self.shown != Some(self.selected) && !matches.is_empty() => {
                self.shown = Some(self.selected);
                matches.get(self.selected).map(|r| show(r))
            }
            PopupResult::Open => None,
        }
    }
}

/// Quick object finder across all visible schemas.
#[derive(Default)]
pub struct ObjectFinder {
    pub open: bool,
    query: String,
    selected: usize,
    /// Query text the current results / pending request belong to.
    searched: String,
    pub pending: Option<u64>,
    results: Vec<DbObject>,
    changed_at: Option<f64>,
}

pub enum FinderAction {
    /// Search for this pattern now.
    Search(String),
    Open(ObjectRef),
}

impl ObjectFinder {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.selected = 0;
    }

    pub fn results(&mut self, id: u64, objects: Vec<DbObject>) {
        if self.pending == Some(id) {
            self.pending = None;
            self.results = objects;
            self.selected = 0;
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> Option<FinderAction> {
        if !self.open {
            return None;
        }
        let rows: Vec<(String, String)> = self
            .results
            .iter()
            .map(|o| {
                let invalid = if o.valid { "" } else { "  (invalid)" };
                (
                    o.name.clone(),
                    format!(
                        "{} · {}{invalid}",
                        o.object_type.dictionary_name().to_lowercase(),
                        o.owner
                    ),
                )
            })
            .collect();
        let before = self.query.clone();
        let result = popup_list(
            ctx,
            "object-finder",
            "Find table, view, package…",
            &mut self.query,
            &mut self.selected,
            &rows,
        );
        let now = ctx.input(|i| i.time);
        if self.query != before {
            self.changed_at = Some(now);
        }
        let mut action = None;
        // Debounce typing: search 250 ms after the last keystroke.
        if let Some(t) = self.changed_at {
            if now - t > 0.25 {
                self.changed_at = None;
                if self.query.trim().len() >= 2 && self.query != self.searched {
                    self.searched = self.query.clone();
                    action = Some(FinderAction::Search(self.query.trim().to_owned()));
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(260));
            }
        }
        match result {
            PopupResult::Picked(i) => {
                self.open = false;
                self.results.get(i).map(|o| {
                    FinderAction::Open(ObjectRef {
                        owner: o.owner.clone(),
                        name: o.name.clone(),
                        object_type: o.object_type,
                    })
                })
            }
            PopupResult::Closed => {
                self.open = false;
                None
            }
            PopupResult::Open => action,
        }
    }
}

/// Recall a previously executed statement.
#[derive(Default)]
pub struct HistoryDialog {
    pub open: bool,
    query: String,
    selected: usize,
    pub all_connections: bool,
}

impl HistoryDialog {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.selected = 0;
    }

    /// Returns the picked statement.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        history: &History,
        connection: Option<&str>,
    ) -> Option<String> {
        if !self.open {
            return None;
        }
        let filter = if self.all_connections {
            None
        } else {
            connection
        };
        let query = self.query.clone();
        let entries: Vec<_> = history.search(filter, &query).take(300).collect();
        let rows: Vec<(String, String)> = entries
            .iter()
            .map(|e| {
                let one_line: String = e.sql.split_whitespace().collect::<Vec<_>>().join(" ");
                let label = if one_line.chars().count() > 90 {
                    format!("{}…", one_line.chars().take(90).collect::<String>())
                } else {
                    one_line
                };
                let times = if e.count > 1 {
                    format!("{}× · ", e.count)
                } else {
                    String::new()
                };
                (label, format!("{times}{}", ago(e.at)))
            })
            .collect();
        let picked = popup_list(
            ctx,
            "history",
            "Search executed statements…",
            &mut self.query,
            &mut self.selected,
            &rows,
        );
        match picked {
            PopupResult::Picked(i) => {
                self.open = false;
                entries.get(i).map(|e| e.sql.clone())
            }
            PopupResult::Closed => {
                self.open = false;
                None
            }
            PopupResult::Open => None,
        }
    }
}

fn ago(at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let secs = now.saturating_sub(at);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86400),
    }
}

/// An action that needs confirmation first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    Commit,
    /// Run statements that change data on a production connection.
    Execute {
        doc: DocId,
        current_only: bool,
    },
    Disconnect(String),
    Quit,
    /// Close windows with an open transaction: committed (yes) or rolled
    /// back (other) first.
    CloseWindows(Vec<DocId>),
    /// Close windows whose results have changes that weren't applied.
    DiscardWindows(Vec<DocId>),
    /// Make this the active connection, committing (yes) or rolling back
    /// (other) the transactions of the SQL windows that move.
    Activate(String),
    /// Save a SQL window's file, which changed on disk since it was read.
    Overwrite(DocId),
}

pub struct Confirm {
    pub action: ConfirmAction,
    pub title: String,
    pub text: String,
    pub yes: &'static str,
    /// Another way to go ahead, e.g. rolling back instead of committing.
    pub other: Option<&'static str>,
}

/// What the user chose in a confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    Other,
    Cancel,
}

/// Shows the confirmation until the user chooses.
pub fn confirm_ui(ctx: &egui::Context, confirm: &Confirm) -> Option<Answer> {
    let mut answer = None;
    let modal = egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
        ui.set_max_width(400.0);
        ui.heading(&confirm.title);
        ui.add_space(6.0);
        ui.label(&confirm.text);
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button(RichText::new(confirm.yes).strong()).clicked() {
                answer = Some(Answer::Yes);
            }
            if let Some(other) = confirm.other
                && ui.button(other).clicked()
            {
                answer = Some(Answer::Other);
            }
            if ui.button("Cancel").clicked() {
                answer = Some(Answer::Cancel);
            }
        });
    });
    if modal.should_close() && answer.is_none() {
        answer = Some(Answer::Cancel);
    }
    answer
}

/// Object types a finder result opens as program windows.
pub fn opens_as_program(t: ObjectType) -> bool {
    t.has_source()
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    /// The download page is a clickable link of its own, without the
    /// punctuation that made a copied URL a 404; the error stays below.
    #[test]
    fn missing_client_shows_install_steps() {
        let mut dialog = ConnectDialog::default();
        dialog
            .failed_missing_client("DPI-1047: Cannot locate a 64-bit Oracle Client library".into());
        let saved = SavedConnections::default();
        let mut h = Harness::builder()
            .with_size(egui::vec2(900.0, 900.0))
            .build_ui(move |ui| {
                dialog.ui(ui.ctx(), &saved);
            });
        h.run();
        let download = client_setup_steps()[0].link.expect("download link");
        // A real link: clicking it opens exactly that page.
        h.get_by_label(download).click();
        h.step();
        let opened: Vec<String> = h
            .output()
            .platform_output
            .commands
            .iter()
            .filter_map(|c| match c {
                egui::OutputCommand::OpenUrl(open) => Some(open.url.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(opened, [download]);
        h.get_by_label_contains("DPI-1047");
        h.get_by_label("Restart Pythia and connect again.");
    }

    #[test]
    fn ssh_only_where_offered() {
        let profile = |kind| ConnectionProfile {
            kind,
            username: "app".into(),
            connect_string: "localhost/app".into(),
            ssh: pythia_config::SshTunnel {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let passwords = || Passwords {
            database: "pw".into(),
            ssh: "ssh-pw".into(),
        };
        let mut dialog = ConnectDialog::default();
        // MySQL has tunnels: an SSH server is needed.
        assert!(
            dialog
                .test_submit(profile(DbKind::MySql), passwords())
                .is_none()
        );
        // Oracle's section is hidden: the ticked box from before doesn't
        // hold up Connect, and the tunnel isn't used.
        let Some(ConnectAction::Connect(request)) =
            dialog.test_submit(profile(DbKind::Oracle), passwords())
        else {
            panic!("Connect is disabled");
        };
        assert!(!request.profile.ssh.enabled);
        assert!(request.passwords.ssh.is_empty());
    }

    /// Opened on a profile with its saved password, Connect has the focus:
    /// Enter connects (#134). Without the password, Enter in its field does.
    #[test]
    fn enter_connects() {
        for saved_password in [true, false] {
            let mut dialog = ConnectDialog {
                open: true,
                form: ConnectionProfile {
                    username: "hr".into(),
                    connect_string: "localhost/xe".into(),
                    ..Default::default()
                },
                ..Default::default()
            };
            if saved_password {
                dialog.password = "hr".into();
            }
            let saved = SavedConnections::default();
            let mut h = Harness::builder()
                .with_size(egui::vec2(900.0, 900.0))
                .build_ui_state(
                    move |ui, (dialog, action): &mut (ConnectDialog, Option<ConnectAction>)| {
                        if let Some(a) = dialog.ui(ui.ctx(), &saved) {
                            *action = Some(a);
                        }
                    },
                    (dialog, None),
                );
            h.run();
            if !saved_password {
                h.get_by_role(egui::accesskit::Role::PasswordInput)
                    .type_text("hr");
                h.run();
            }
            h.key_press(egui::Key::Enter);
            // Connecting… spins: no run() to a still frame.
            h.step();
            let Some(ConnectAction::Connect(request)) = &h.state().1 else {
                panic!("Enter didn't connect (saved password: {saved_password})");
            };
            assert_eq!(request.passwords.database, "hr");
        }
    }

    #[test]
    fn setup_links_are_clean() {
        for steps in [WINDOWS_STEPS, MACOS_STEPS, LINUX_STEPS] {
            for link in steps.iter().filter_map(|s| s.link) {
                assert!(link.starts_with("https://"), "{link}");
                assert!(!link.ends_with([',', '.', ';', ')']), "{link}");
            }
        }
    }
}
