//! Users and roles of MySQL/MariaDB, PostgreSQL and SQL Server (Oracle's
//! are `account.rs`'s): how a user logs in, the roles granted and the
//! privileges, read through the session's [`Client`] and changed with each
//! database's own statements (#142). What the session can't read is left
//! as it is: what's added is granted, and nothing unseen is revoked (#89).

mod mysql;
mod postgres;
mod sqlserver;

use std::collections::{BTreeMap, BTreeSet};

use egui::{RichText, Ui};
use pythia_db::{Cell, Client, DbError, DbKind, MetaQuery, MetaResult, ObjectType};

pub use super::account::{RoleGrant, Unread};
use super::{EditTarget, Lists, form_grid, form_row, picker};

/// What a privilege is granted on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// The server (MySQL's `*.*`), or the database (SQL Server).
    Global,
    /// MySQL's `db.*`, PostgreSQL's `database d`.
    Database(String),
    /// PostgreSQL's `schema s`, SQL Server's `schema::s`.
    Schema(String),
    /// A table, view or sequence: in a MySQL database, or in a schema.
    Object(String, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ScopeKind {
    #[default]
    Global,
    Database,
    Schema,
    Object,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    Plain,
    /// With grant option: may grant it on.
    WithGrantOption,
    /// SQL Server's `DENY`.
    Deny,
}

/// How a SQL Server user logs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Login {
    /// With a server login (`FOR LOGIN`).
    #[default]
    Mapped,
    /// With a new login and its password, created first.
    New,
    /// Nobody logs in as the user (`WITHOUT LOGIN`).
    None,
    /// A contained database's user with a password of its own.
    Password,
    /// Windows or external authentication: shown, not changed.
    Other,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UserDef {
    pub kind: DbKind,
    pub mariadb: bool,
    /// MySQL: the session's `sql_mode` has `NO_BACKSLASH_ESCAPES`, so a
    /// `\` in a string stands for itself.
    pub no_backslash_escapes: bool,
    pub role: bool,
    pub name: String,
    /// MySQL: the host of `'user'@'host'`.
    pub host: String,
    /// A new password; empty keeps the current one.
    pub password: String,
    /// MySQL's account, SQL Server's login. `None`: as it is.
    pub locked: Option<bool>,
    /// MySQL: the password must be changed at the next login.
    pub expire: bool,
    /// PostgreSQL: the role's options that are on (`CREATEDB`, `INHERIT`, …).
    pub options: BTreeSet<String>,
    /// PostgreSQL: when the password stops working; empty for never.
    pub valid_until: String,
    /// PostgreSQL: connections at most; empty for no limit.
    pub connection_limit: String,
    pub login: Login,
    /// SQL Server: the login the user is for.
    pub login_name: String,
    /// SQL Server.
    pub default_schema: String,
    pub roles: BTreeMap<String, RoleGrant>,
    pub privileges: BTreeMap<(Scope, String), Grant>,
    /// Shown, not edited, e.g. MySQL's authentication plugin.
    pub info: Vec<(String, String)>,
    pub unread: Unread,
}

impl UserDef {
    pub fn new(kind: DbKind, role: bool, mariadb: bool) -> Self {
        let mut def = Self {
            kind,
            mariadb,
            role,
            ..Self::default()
        };
        match kind {
            DbKind::MySql => {
                def.host = if role && mariadb { "" } else { "%" }.to_owned();
                def.locked = Some(false);
            }
            DbKind::Postgres => {
                def.options.insert("INHERIT".to_owned());
            }
            DbKind::SqlServer if !role => def.default_schema = "dbo".to_owned(),
            _ => {}
        }
        def
    }

    /// The name the object browser lists it by: MySQL's as `user@host`, a
    /// role without its usual host.
    pub fn browser_name(&self) -> String {
        let name = self.name.trim();
        match self.kind {
            DbKind::MySql if self.role && matches!(self.host.trim(), "%" | "") => name.to_owned(),
            DbKind::MySql => format!("{name}@{}", self.host.trim()),
            _ => name.to_owned(),
        }
    }

    /// The scopes privileges are granted on here, with their labels.
    fn scope_kinds(&self) -> &'static [(ScopeKind, &'static str)] {
        match self.kind {
            DbKind::MySql => &[
                (ScopeKind::Global, "server"),
                (ScopeKind::Database, "database"),
                (ScopeKind::Object, "table"),
            ],
            DbKind::Postgres => &[
                (ScopeKind::Database, "database"),
                (ScopeKind::Schema, "schema"),
                (ScopeKind::Object, "table"),
            ],
            _ => &[
                (ScopeKind::Global, "database"),
                (ScopeKind::Schema, "schema"),
                (ScopeKind::Object, "object"),
            ],
        }
    }

    /// A scope as shown: `*.*`, `db.*`, `schema s`, `s.t`.
    fn scope_label(&self, scope: &Scope) -> String {
        match (self.kind, scope) {
            (DbKind::MySql, Scope::Global) => "*.*".to_owned(),
            (DbKind::MySql, Scope::Database(d)) => format!("{d}.*"),
            (_, Scope::Global) => "database".to_owned(),
            (_, Scope::Database(d)) => format!("database {d}"),
            (_, Scope::Schema(s)) => format!("schema {s}"),
            (_, Scope::Object(c, o)) => format!("{c}.{o}"),
        }
    }

    /// Whether a grant names whether it may be granted on (PostgreSQL,
    /// SQL Server; MySQL's grant option is a privilege of the scope).
    fn per_privilege_option(&self) -> bool {
        self.kind != DbKind::MySql
    }
}

/// Names for the form's pickers.
#[derive(Debug, Clone, Default)]
pub struct Choices {
    /// Roles that may be granted.
    pub roles: Vec<String>,
    /// The privileges per kind of scope.
    pub privileges: BTreeMap<ScopeKind, Vec<String>>,
    pub databases: Vec<String>,
    pub schemas: Vec<String>,
    /// SQL Server's logins.
    pub logins: Vec<String>,
    /// SQL Server: the database is contained, so users may have passwords.
    pub contained: bool,
}

/// The account as loaded (none for a new one), the form to start from, and
/// the pickers' names.
pub type Loaded = (Option<UserDef>, UserDef, Choices);

pub fn load(client: &mut dyn Client, target: &EditTarget) -> Result<Loaded, DbError> {
    let role = target.object_type() == ObjectType::Role;
    let name = target.object().map(|o| o.name.as_str());
    match client.dialect().kind() {
        DbKind::MySql => mysql::load(client, name, role),
        DbKind::Postgres => postgres::load(client, name, role),
        DbKind::SqlServer => sqlserver::load(client, name, role),
        DbKind::Oracle => Err(DbError::new("Oracle's users have a form of their own")),
        DbKind::Sqlite => Err(DbError::unsupported(
            "Users and roles",
            pythia_db::Dialect::SQLITE,
        )),
    }
}

/// Tables and views of a MySQL database or a schema, for the picker.
pub fn objects(client: &mut dyn Client, container: &str) -> Result<Vec<String>, DbError> {
    let sql = match client.dialect().kind() {
        DbKind::MySql => {
            "select table_name from information_schema.tables where table_schema = ? \
              order by table_name"
        }
        DbKind::Postgres => {
            "select c.relname from pg_class c join pg_namespace n on n.oid = c.relnamespace \
              where n.nspname = $1 and c.relkind in ('r', 'p', 'v', 'm', 'f', 'S') \
              order by c.relname"
        }
        DbKind::SqlServer => {
            "select o.name from sys.objects o where o.schema_id = schema_id(@P1) \
               and o.type in ('U', 'V', 'P', 'FN', 'IF', 'TF', 'SO', 'SN') order by o.name"
        }
        DbKind::Oracle | DbKind::Sqlite => return Ok(Vec::new()),
    };
    texts(client, sql, &[container])
}

pub fn statements(original: Option<&UserDef>, def: &UserDef) -> Result<Vec<String>, String> {
    if def.name.trim().is_empty() {
        return Err("Enter a name".to_owned());
    }
    match def.kind {
        DbKind::MySql => mysql::statements(original, def),
        DbKind::Postgres => postgres::statements(original, def),
        DbKind::SqlServer => sqlserver::statements(original, def),
        DbKind::Oracle | DbKind::Sqlite => Ok(Vec::new()),
    }
}

/// The statements as shown: the new password hidden.
pub fn masked(statements: &[String], def: &UserDef) -> Vec<String> {
    if def.password.is_empty() {
        return statements.to_vec();
    }
    let (secret, hidden) = match def.kind {
        DbKind::MySql => (
            mysql::literal(&def.password, def.no_backslash_escapes),
            "'********'",
        ),
        DbKind::Postgres => (postgres::literal(&def.password), "'********'"),
        _ => (sqlserver::literal(&def.password), "N'********'"),
    };
    statements
        .iter()
        .map(|s| s.replace(&secret, hidden))
        .collect()
}

/// The first column of each row, NULLs left out.
fn texts(client: &mut dyn Client, sql: &str, params: &[&str]) -> Result<Vec<String>, DbError> {
    Ok(client
        .query(sql, params)?
        .into_iter()
        .filter_map(|row| row.into_iter().next().flatten())
        .collect())
}

/// Text of a row's column; empty for NULL or a missing one.
fn text(row: &[Cell], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

/// A boolean as the databases write it: `t`, `1`, `Y`, `YES`.
fn yes(row: &[Cell], i: usize) -> bool {
    matches!(
        text(row, i).to_ascii_uppercase().as_str(),
        "T" | "TRUE" | "1" | "Y" | "YES"
    )
}

/// The database's users or roles as the object browser lists them.
fn accounts(client: &mut dyn Client, object_type: ObjectType) -> Result<Vec<String>, DbError> {
    match client.meta(&MetaQuery::Accounts(object_type))? {
        MetaResult::Accounts(names) => Ok(names.into_iter().map(|(name, _)| name).collect()),
        _ => Ok(Vec::new()),
    }
}

/// `revoke` and `grant` turning the roles `before` into `after`.
/// `grant(roles, admin)` and `revoke(roles)` write a statement for a list.
/// Taking the admin option away revokes the role and grants it again.
fn role_changes(
    before: &BTreeMap<String, RoleGrant>,
    after: &BTreeMap<String, RoleGrant>,
    revoke: impl Fn(&[&String]) -> String,
    grant: impl Fn(&[&String], bool) -> String,
    out: &mut Vec<String>,
) {
    let revoked: Vec<&String> = before
        .iter()
        .filter(|(r, g)| after.get(*r).is_none_or(|now| g.admin && !now.admin))
        .map(|(r, _)| r)
        .collect();
    if !revoked.is_empty() {
        out.push(revoke(&revoked));
    }
    let mut granted: [Vec<&String>; 2] = Default::default();
    for (r, g) in after {
        match before.get(r) {
            // New, or revoked above to take the admin option away.
            None => granted[usize::from(g.admin)].push(r),
            Some(was) if was.admin && !g.admin => granted[0].push(r),
            Some(was) if !was.admin && g.admin => granted[1].push(r),
            Some(_) => {}
        }
    }
    for (admin, roles) in [false, true].into_iter().zip(granted) {
        if !roles.is_empty() {
            out.push(grant(&roles, admin));
        }
    }
}

/// Privileges of a scope, and how they're granted.
type Privileges = BTreeMap<String, Grant>;

/// The privileges of each scope, before and after, for the statements.
fn by_scope(
    before: &BTreeMap<(Scope, String), Grant>,
    after: &BTreeMap<(Scope, String), Grant>,
) -> BTreeMap<Scope, (Privileges, Privileges)> {
    let mut out: BTreeMap<Scope, (Privileges, Privileges)> = BTreeMap::new();
    for ((scope, privilege), grant) in before {
        out.entry(scope.clone())
            .or_default()
            .0
            .insert(privilege.clone(), *grant);
    }
    for ((scope, privilege), grant) in after {
        out.entry(scope.clone())
            .or_default()
            .1
            .insert(privilege.clone(), *grant);
    }
    out.retain(|_, (b, a)| b != a);
    out
}

fn list(names: &[&String]) -> String {
    names
        .iter()
        .map(|n| n.to_lowercase())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The privilege being added (kept between frames).
#[derive(Clone, Default)]
struct NewPrivilege {
    kind: ScopeKind,
    /// The database or schema.
    container: String,
    object: String,
    privilege: String,
    grant: Option<Grant>,
}

pub fn ui(ui: &mut Ui, def: &mut UserDef, original: Option<&UserDef>, lists: &mut Lists) {
    let existing = original.is_some();
    if def.unread.any() {
        let what = if def.role { "role" } else { "user" };
        let needs = match def.kind {
            DbKind::MySql => "SELECT on the mysql database",
            DbKind::SqlServer => "VIEW DEFINITION",
            _ => "more privileges",
        };
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!(
                "Without {needs}, not all of this {what} can be read. What's left empty stays \
                 as it is, what's added is granted, and nothing that isn't shown is revoked."
            ),
        );
        ui.add_space(4.0);
    }
    let choices = lists.accounts.clone();
    form_grid(ui, "accounts", |ui| {
        form_row(ui, "Name", |ui| {
            if existing {
                ui.label(RichText::new(def.name.as_str()).monospace().strong());
            } else {
                ui.add(
                    egui::TextEdit::singleline(&mut def.name)
                        .hint_text("name")
                        .desired_width(240.0),
                );
            }
        });
        match def.kind {
            DbKind::MySql => mysql::ui(ui, def, original),
            DbKind::Postgres => postgres::ui(ui, def, original),
            DbKind::SqlServer => sqlserver::ui(ui, def, original, &choices),
            DbKind::Oracle | DbKind::Sqlite => {}
        }
        for (label, value) in &def.info {
            form_row(ui, label, |ui| {
                ui.label(RichText::new(value).monospace());
            });
        }
    });
    ui.add_space(8.0);
    roles_ui(ui, def, &choices.roles);
    ui.add_space(8.0);
    privileges_ui(ui, def, &choices, lists);
}

/// Granted roles: admin option (MySQL, PostgreSQL) and, for MySQL's users,
/// whether it's enabled at login.
fn roles_ui(ui: &mut Ui, def: &mut UserDef, choices: &[String]) {
    let own = def.browser_name();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(if def.kind == DbKind::Postgres {
                "Member of"
            } else {
                "Roles"
            })
            .strong(),
        );
        let open: Vec<String> = choices
            .iter()
            .filter(|c| !def.roles.contains_key(*c) && **c != own)
            .cloned()
            .collect();
        if let Some(role) = picker(ui, "add-role", &open) {
            def.roles.insert(role, RoleGrant::default());
        }
    });
    let unread = def.unread.roles;
    if unread {
        ui.weak("The granted roles can't be read; roles added here are granted.");
    } else if def.roles.is_empty() {
        ui.weak("none");
    }
    if def.roles.is_empty() {
        return;
    }
    let admin = def.kind != DbKind::SqlServer;
    let default = def.kind == DbKind::MySql && !def.role && !unread;
    let mut remove = None;
    egui::Grid::new("account-roles")
        .num_columns(4)
        .spacing([10.0, 2.0])
        .show(ui, |ui| {
            ui.label("");
            if admin {
                ui.weak("admin")
                    .on_hover_text("With admin option: may grant it on");
            } else {
                ui.label("");
            }
            if default {
                ui.weak("default")
                    .on_hover_text("Enabled at login; others need SET ROLE");
            } else {
                ui.label("");
            }
            ui.label("");
            ui.end_row();
            let mut enabled = None;
            for (role, grant) in def.roles.iter_mut() {
                ui.label(RichText::new(role.as_str()).monospace());
                if admin {
                    ui.checkbox(&mut grant.admin, "");
                } else {
                    ui.label("");
                }
                if default {
                    if ui.checkbox(&mut grant.default, "").changed() && grant.default {
                        enabled = Some(role.clone());
                    }
                } else {
                    ui.label("");
                }
                if ui.small_button("×").on_hover_text("Revoke").clicked() {
                    remove = Some(role.clone());
                }
                ui.end_row();
            }
            // MariaDB enables one role at login.
            if let Some(enabled) = enabled.filter(|_| def.mariadb) {
                for (role, grant) in def.roles.iter_mut() {
                    grant.default &= *role == enabled;
                }
            }
        });
    if let Some(role) = remove {
        def.roles.remove(&role);
    }
}

/// The privileges per scope; a row to add one.
fn privileges_ui(ui: &mut Ui, def: &mut UserDef, choices: &Choices, lists: &mut Lists) {
    ui.label(RichText::new("Privileges").strong());
    if def.unread.privileges {
        ui.weak("The granted privileges can't be read; privileges added here are granted.");
    }
    let option = def.per_privilege_option();
    let deny = def.kind == DbKind::SqlServer;
    let mut remove = None;
    if def.privileges.is_empty() {
        if !def.unread.privileges {
            ui.weak("none");
        }
    } else {
        let labels: Vec<String> = def
            .privileges
            .keys()
            .map(|(scope, _)| def.scope_label(scope))
            .collect();
        egui::ScrollArea::vertical()
            .id_salt("account-privileges")
            .max_height(260.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("account-privileges-grid")
                    .striped(true)
                    .num_columns(4)
                    .spacing([12.0, 2.0])
                    .show(ui, |ui| {
                        ui.weak("On");
                        ui.weak("Privilege");
                        if option {
                            ui.weak(if deny { "state" } else { "grantable" })
                                .on_hover_text("With grant option: may grant it on");
                        } else {
                            ui.label("");
                        }
                        ui.label("");
                        ui.end_row();
                        for ((key, grant), label) in def.privileges.iter_mut().zip(labels) {
                            ui.label(RichText::new(label).monospace());
                            ui.label(key.1.to_lowercase());
                            if deny {
                                grant_combo(ui, (&key.0, &key.1), grant);
                            } else if option {
                                let mut with = *grant == Grant::WithGrantOption;
                                if ui.checkbox(&mut with, "").changed() {
                                    *grant = if with {
                                        Grant::WithGrantOption
                                    } else {
                                        Grant::Plain
                                    };
                                }
                            } else {
                                ui.label("");
                            }
                            if ui.small_button("×").on_hover_text("Revoke").clicked() {
                                remove = Some(key.clone());
                            }
                            ui.end_row();
                        }
                    });
            });
    }
    if let Some(key) = remove {
        def.privileges.remove(&key);
    }

    let id = egui::Id::new("accounts-new-privilege");
    let kinds = def.scope_kinds();
    let mut new: NewPrivilege = ui.data(|d| d.get_temp(id)).unwrap_or_else(|| NewPrivilege {
        kind: kinds[0].0,
        ..NewPrivilege::default()
    });
    if !kinds.iter().any(|(k, _)| *k == new.kind) {
        new.kind = kinds[0].0;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label("Add");
        let label = |k: ScopeKind| {
            kinds
                .iter()
                .find(|(kind, _)| *kind == k)
                .map_or("", |(_, l)| l)
        };
        egui::ComboBox::from_id_salt("privilege-scope")
            .selected_text(label(new.kind))
            .show_ui(ui, |ui| {
                for (kind, label) in kinds {
                    if ui.selectable_value(&mut new.kind, *kind, *label).changed() {
                        new.privilege.clear();
                    }
                }
            });
        // MySQL's tables are in a database, the others' in a schema.
        let container = match (new.kind, def.kind) {
            (ScopeKind::Global, _) => None,
            (ScopeKind::Database, _) | (ScopeKind::Object, DbKind::MySql) => {
                Some(("database", &choices.databases))
            }
            _ => Some(("schema", &choices.schemas)),
        };
        if let Some((hint, names)) = container {
            ui.add(
                egui::TextEdit::singleline(&mut new.container)
                    .hint_text(hint)
                    .desired_width(130.0),
            );
            if let Some(name) = picker(ui, "privilege-container", names) {
                new.container = name;
                new.object.clear();
            }
        }
        if new.kind == ScopeKind::Object {
            ui.label(".");
            ui.add(
                egui::TextEdit::singleline(&mut new.object)
                    .hint_text("table")
                    .desired_width(160.0),
            );
            let container = new.container.trim().to_owned();
            if !container.is_empty() {
                match lists.objects.get(&container) {
                    Some(objects) => {
                        if let Some(o) = picker(ui, "privilege-object", objects) {
                            new.object = o;
                        }
                    }
                    None => lists.want_objects = Some(container),
                }
            }
        }
        let offered = choices
            .privileges
            .get(&new.kind)
            .map(Vec::as_slice)
            .unwrap_or_default();
        egui::ComboBox::from_id_salt("privilege-name")
            .selected_text(if new.privilege.is_empty() {
                "privilege".to_owned()
            } else {
                new.privilege.to_lowercase()
            })
            .height(320.0)
            .show_ui(ui, |ui| {
                for p in offered {
                    ui.selectable_value(&mut new.privilege, p.clone(), p.to_lowercase());
                }
            });
        let mut grant = new.grant.unwrap_or(Grant::Plain);
        if deny {
            grant_combo(ui, "new", &mut grant);
        } else if option {
            let mut with = grant == Grant::WithGrantOption;
            ui.checkbox(&mut with, "with grant option");
            grant = if with {
                Grant::WithGrantOption
            } else {
                Grant::Plain
            };
        }
        new.grant = Some(grant);
        let scope = match new.kind {
            ScopeKind::Global => Some(Scope::Global),
            _ if new.container.trim().is_empty() => None,
            ScopeKind::Database => Some(Scope::Database(new.container.trim().to_owned())),
            ScopeKind::Schema => Some(Scope::Schema(new.container.trim().to_owned())),
            ScopeKind::Object if new.object.trim().is_empty() => None,
            ScopeKind::Object => Some(Scope::Object(
                new.container.trim().to_owned(),
                new.object.trim().to_owned(),
            )),
        };
        let complete = scope.is_some() && !new.privilege.is_empty();
        if ui.add_enabled(complete, egui::Button::new("Add")).clicked()
            && let Some(scope) = scope
        {
            def.privileges.insert((scope, new.privilege.clone()), grant);
            new.privilege.clear();
            new.object.clear();
        }
    });
    ui.data_mut(|d| d.insert_temp(id, new));
}

/// SQL Server's grant, grant with grant option, or deny.
fn grant_combo(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, grant: &mut Grant) {
    let label = |g: Grant| match g {
        Grant::Plain => "grant",
        Grant::WithGrantOption => "grant, grantable",
        Grant::Deny => "deny",
    };
    egui::ComboBox::from_id_salt(("privilege-state", id))
        .selected_text(label(*grant))
        .show_ui(ui, |ui| {
            for g in [Grant::Plain, Grant::WithGrantOption, Grant::Deny] {
                ui.selectable_value(grant, g, label(g));
            }
        });
}

#[cfg(test)]
pub(crate) mod live;

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(entries: &[(&str, bool)]) -> BTreeMap<String, RoleGrant> {
        entries
            .iter()
            .map(|(r, admin)| {
                (
                    (*r).to_owned(),
                    RoleGrant {
                        admin: *admin,
                        default: true,
                    },
                )
            })
            .collect()
    }

    fn changes(before: &[(&str, bool)], after: &[(&str, bool)]) -> Vec<String> {
        let mut out = Vec::new();
        role_changes(
            &roles(before),
            &roles(after),
            |r| format!("revoke {}", list(r)),
            |r, admin| format!("grant {}{}", list(r), if admin { " admin" } else { "" }),
            &mut out,
        );
        out
    }

    #[test]
    fn role_grants() {
        assert_eq!(
            changes(&[], &[("a", false), ("b", true)]),
            ["grant a", "grant b admin"]
        );
        assert_eq!(changes(&[("a", false)], &[]), ["revoke a"]);
        assert!(changes(&[("a", true)], &[("a", true)]).is_empty());
        // Adding the admin option grants it again with it; taking it away
        // revokes and grants.
        assert_eq!(changes(&[("a", false)], &[("a", true)]), ["grant a admin"]);
        assert_eq!(
            changes(&[("a", true)], &[("a", false)]),
            ["revoke a", "grant a"]
        );
    }

    #[test]
    fn browser_names() {
        let mut def = UserDef::new(DbKind::MySql, false, false);
        def.name = "app".into();
        assert_eq!(def.browser_name(), "app@%");
        def.host = "localhost".into();
        assert_eq!(def.browser_name(), "app@localhost");
        let mut role = UserDef::new(DbKind::MySql, true, true);
        role.name = "reader".into();
        assert_eq!(role.browser_name(), "reader");
        let mut pg = UserDef::new(DbKind::Postgres, false, false);
        pg.name = "App".into();
        assert_eq!(pg.browser_name(), "App");
    }
}
