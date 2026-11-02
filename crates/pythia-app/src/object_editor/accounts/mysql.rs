//! MySQL's and MariaDB's accounts: `'user'@'host'` with a password,
//! expiry and lock; roles (MySQL 8, MariaDB 10.0.5) with the admin option
//! and those enabled at login; privileges on the server (`*.*`), a database
//! (`db.*`) or a table. The grant option is a privilege of its scope here.
//!
//! The accounts' attributes and roles are in the `mysql` database; MariaDB
//! keeps the lock in `mysql.global_priv` and enables one default role. The
//! privileges come from `information_schema`, which shows other accounts'
//! only to who may read `mysql`.

use std::collections::{BTreeMap, BTreeSet};

use egui::Ui;
use pythia_db::{Client, DbError, DbKind, Dialect, ObjectType};

use super::{
    Choices, Grant, Loaded, RoleGrant, Scope, ScopeKind, UserDef, accounts, by_scope, list,
    role_changes, text, texts, yes,
};
use crate::object_editor::form_row;

const GRANT_OPTION: &str = "GRANT OPTION";

/// What can be granted on a database (`db.*`).
const DATABASE: &[&str] = &[
    "ALTER",
    "ALTER ROUTINE",
    "CREATE",
    "CREATE ROUTINE",
    "CREATE TEMPORARY TABLES",
    "CREATE VIEW",
    "DELETE",
    "DROP",
    "EVENT",
    "EXECUTE",
    GRANT_OPTION,
    "INDEX",
    "INSERT",
    "LOCK TABLES",
    "REFERENCES",
    "SELECT",
    "SHOW VIEW",
    "TRIGGER",
    "UPDATE",
];

/// What can be granted on a table.
const TABLE: &[&str] = &[
    "ALTER",
    "CREATE",
    "CREATE VIEW",
    "DELETE",
    "DROP",
    GRANT_OPTION,
    "INDEX",
    "INSERT",
    "REFERENCES",
    "SELECT",
    "SHOW VIEW",
    "TRIGGER",
    "UPDATE",
];

/// No access to the `mysql` table (1142), or one MariaDB doesn't have
/// before 10.4 (1146).
fn no_access(e: &DbError) -> bool {
    matches!(e.code, Some(1142 | 1143 | 1146))
}

/// A string literal, also with `\` in it: doubled, unless the session's
/// `sql_mode` has `NO_BACKSLASH_ESCAPES` (`plain`), where it stands for
/// itself. A password can't be a hex literal, which reads the same in
/// both.
pub fn literal(s: &str, plain: bool) -> String {
    let s = if plain {
        s.to_owned()
    } else {
        s.replace('\\', "\\\\")
    };
    format!("'{}'", s.replace('\'', "''"))
}

/// `'user'@'host'`; a role on its usual host (`%` on MySQL, none on
/// MariaDB) by its name alone.
fn account(user: &str, host: &str, role: bool, plain: bool) -> String {
    if role && matches!(host, "%" | "") {
        literal(user, plain)
    } else {
        format!("{}@{}", literal(user, plain), literal(host, plain))
    }
}

/// A role as listed (`role`, or `role@host`) as written in GRANT.
fn role_sql(name: &str, plain: bool) -> String {
    match name.rsplit_once('@') {
        Some((user, host)) => format!("{}@{}", literal(user, plain), literal(host, plain)),
        None => literal(name, plain),
    }
}

fn roles_sql(roles: &[&String], plain: bool) -> String {
    roles
        .iter()
        .map(|r| role_sql(r, plain))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `*.*`, `db.*` or `db.table`.
fn scope_sql(scope: &Scope) -> String {
    let q = |n: &str| Dialect::MYSQL.quote_ident(n);
    match scope {
        Scope::Global => "*.*".to_owned(),
        Scope::Database(d) | Scope::Schema(d) => format!("{}.*", q(d)),
        Scope::Object(d, t) => format!("{}.{}", q(d), q(t)),
    }
}

/// A user or role as listed: the user and the host.
fn split(name: &str, role: bool, mariadb: bool) -> (String, String) {
    match name.rsplit_once('@') {
        Some((user, host)) => (user.to_owned(), host.to_owned()),
        None if role && mariadb => (name.to_owned(), String::new()),
        None => (name.to_owned(), "%".to_owned()),
    }
}

/// A role granted, as the object browser lists roles.
fn role_name(user: String, host: &str) -> String {
    match host {
        "%" | "" => user,
        host => format!("{user}@{host}"),
    }
}

pub(super) fn load(
    client: &mut dyn Client,
    name: Option<&str>,
    role: bool,
) -> Result<Loaded, DbError> {
    let version = texts(client, "select version()", &[])?
        .into_iter()
        .next()
        .unwrap_or_default();
    let mariadb = version.contains("MariaDB");
    let no_backslash_escapes = texts(client, "select @@session.sql_mode", &[])?
        .first()
        .is_some_and(|mode| mode.to_uppercase().contains("NO_BACKSLASH_ESCAPES"));
    // Privilege, Context, Comment.
    let privileges: Vec<(String, String)> = client
        .query("show privileges", &[])?
        .iter()
        .map(|row| (text(row, 0).to_uppercase(), text(row, 1)))
        .filter(|(p, _)| !matches!(p.as_str(), "USAGE" | "PROXY" | GRANT_OPTION))
        .collect();
    let mut global: Vec<String> = privileges.iter().map(|(p, _)| p.clone()).collect();
    global.push(GRANT_OPTION.to_owned());
    global.sort();
    let all = global.clone();
    // The lists above, and what else the server says is for tables (or
    // databases), such as MariaDB's DELETE HISTORY.
    let scoped = |known: &[&str], contexts: &[&str]| {
        let mut list: BTreeSet<String> = known.iter().map(|p| (*p).to_owned()).collect();
        list.extend(
            privileges
                .iter()
                .filter(|(_, context)| context.split(',').any(|c| contexts.contains(&c.trim())))
                .map(|(p, _)| p.clone()),
        );
        list.into_iter().collect::<Vec<_>>()
    };
    let mut roles = accounts(client, ObjectType::Role)?;
    if !role && let Some(name) = name {
        roles.retain(|r| r != name);
    }
    let choices = Choices {
        roles,
        privileges: BTreeMap::from([
            (ScopeKind::Global, all),
            (
                ScopeKind::Database,
                scoped(DATABASE, &["Databases", "Tables"]),
            ),
            (ScopeKind::Object, scoped(TABLE, &["Tables"])),
        ]),
        databases: texts(client, "show databases", &[])?,
        ..Choices::default()
    };
    let original = match name {
        Some(name) => {
            let (user, host) = split(name, role, mariadb);
            Some(load_account(client, &user, &host, role, mariadb, &global)?)
        }
        None => None,
    };
    let mut start = original
        .clone()
        .unwrap_or_else(|| UserDef::new(DbKind::MySql, role, mariadb));
    let original = original.map(|def| UserDef {
        no_backslash_escapes,
        ..def
    });
    start.no_backslash_escapes = no_backslash_escapes;
    Ok((original, start, choices))
}

/// An account as it is; `global` is what can be granted on the server,
/// what a role's `ALL PRIVILEGES` stands for.
fn load_account(
    client: &mut dyn Client,
    user: &str,
    host: &str,
    role: bool,
    mariadb: bool,
    global: &[String],
) -> Result<UserDef, DbError> {
    let mut def = UserDef {
        name: user.to_owned(),
        host: host.to_owned(),
        ..UserDef::new(DbKind::MySql, role, mariadb)
    };
    def.locked = None;
    let what = if role { "role" } else { "user" };
    let missing = || {
        DbError::new(format!(
            "The {what} {} doesn't exist",
            account(user, host, role, false)
        ))
    };
    let attributes = if mariadb {
        let sql = "select u.password_expired, u.plugin, u.default_role, \
                          json_value(g.priv, '$.account_locked') \
                     from mysql.user u \
                     left join mysql.global_priv g on g.user = u.user and g.host = u.host \
                    where u.user = ? and u.host = ?";
        client.query(sql, &[user, host]).or_else(|e| match e.code {
            // Before 10.4: no global_priv, and no locked accounts.
            Some(1146) => client.query(
                "select password_expired, plugin, default_role, null from mysql.user \
                  where user = ? and host = ?",
                &[user, host],
            ),
            _ => Err(e),
        })
    } else {
        client.query(
            "select password_expired, plugin, null, account_locked from mysql.user \
              where user = ? and host = ?",
            &[user, host],
        )
    };
    let mut mariadb_default = None;
    match attributes {
        Ok(rows) => {
            let row = rows.into_iter().next().ok_or_else(missing)?;
            if !role {
                def.expire = yes(&row, 0);
                def.locked = Some(yes(&row, 3));
                let plugin = text(&row, 1);
                if !plugin.is_empty() {
                    def.info.push(("Authentication".to_owned(), plugin));
                }
            }
            mariadb_default = Some(text(&row, 2)).filter(|r| !r.is_empty());
            if mariadb && !role {
                let shown = texts(
                    client,
                    &format!("show create user {}", account(user, host, false, false)),
                    &[],
                );
                def.plugins = shown
                    .ok()
                    .and_then(|rows| rows.first().map(|s| plugins(s)))
                    .unwrap_or_default();
            }
        }
        Err(e) if no_access(&e) => {
            def.unread.attributes = true;
        }
        Err(e) => return Err(e),
    }

    let edges = if mariadb {
        client.query(
            "select role, '', admin_option from mysql.roles_mapping where user = ? and host = ?",
            &[user, host],
        )
    } else {
        client.query(
            "select from_user, from_host, with_admin_option from mysql.role_edges \
              where to_user = ? and to_host = ?",
            &[user, host],
        )
    };
    match edges {
        Ok(rows) => {
            let defaults: BTreeSet<String> = if mariadb {
                mariadb_default.into_iter().collect()
            } else {
                client
                    .query(
                        "select default_role_user, default_role_host from mysql.default_roles \
                          where user = ? and host = ?",
                        &[user, host],
                    )?
                    .into_iter()
                    .map(|row| role_name(text(&row, 0), &text(&row, 1)))
                    .collect()
            };
            for row in rows {
                let name = role_name(text(&row, 0), &text(&row, 1));
                let grant = RoleGrant {
                    admin: yes(&row, 2),
                    default: defaults.contains(&name),
                };
                def.roles.insert(name, grant);
            }
        }
        Err(e) if no_access(&e) => def.unread.roles = true,
        Err(e) => return Err(e),
    }

    // information_schema shows only the session's own privileges to who
    // can't read `mysql`.
    let me = texts(client, "select current_user()", &[])?;
    let own = me.first().is_some_and(|me| *me == format!("{user}@{host}"));
    if def.unread.attributes && !own {
        def.unread.privileges = true;
        return Ok(def);
    }
    let grantee = format!("'{user}'@'{host}'");
    let mut add = |scope: Scope, privilege: String, grantable: bool| {
        if privilege == "USAGE" {
            return;
        }
        // Dynamic privileges (MySQL 8's `BACKUP_ADMIN`) are grantable each
        // on their own: GRANT OPTION is the static ones'.
        if grantable && !privilege.contains('_') {
            def.privileges
                .insert((scope.clone(), GRANT_OPTION.to_owned()), Grant::Plain);
        }
        def.privileges.insert((scope, privilege), Grant::Plain);
    };
    if mariadb && role {
        // MariaDB's user_privileges has no roles (#445): a role's global
        // privileges are SHOW GRANTS's `… ON *.*`.
        let sql = format!("show grants for {}", Dialect::MYSQL.quote_ident(user));
        for row in client.query(&sql, &[])? {
            let Some((privileges, grantable)) = global_grant(&text(&row, 0)) else {
                continue;
            };
            for privilege in privileges {
                if privilege == "ALL PRIVILEGES" {
                    for p in global.iter().filter(|p| *p != GRANT_OPTION) {
                        add(Scope::Global, p.clone(), grantable);
                    }
                } else {
                    add(Scope::Global, privilege, grantable);
                }
            }
        }
    } else {
        for row in client.query(
            "select privilege_type, is_grantable from information_schema.user_privileges \
              where grantee = ?",
            &[&grantee],
        )? {
            add(Scope::Global, text(&row, 0), yes(&row, 1));
        }
    }
    for row in client.query(
        "select table_schema, privilege_type, is_grantable \
           from information_schema.schema_privileges where grantee = ?",
        &[&grantee],
    )? {
        add(Scope::Database(text(&row, 0)), text(&row, 1), yes(&row, 2));
    }
    for row in client.query(
        "select table_schema, table_name, privilege_type, is_grantable \
           from information_schema.table_privileges where grantee = ?",
        &[&grantee],
    )? {
        add(
            Scope::Object(text(&row, 0), text(&row, 1)),
            text(&row, 2),
            yes(&row, 3),
        );
    }
    Ok(def)
}

/// MariaDB: the plugins of `IDENTIFIED VIA a OR b USING …` in a SHOW
/// CREATE USER, where the account has another than a password alone.
fn plugins(create: &str) -> Vec<String> {
    let Some((_, chain)) = create.split_once(" IDENTIFIED VIA ") else {
        return Vec::new();
    };
    let plugins: Vec<String> = chain
        .split(" OR ")
        .filter_map(|part| part.split_whitespace().next())
        .map(|p| p.trim_matches('`').to_owned())
        .collect();
    if plugins == ["mysql_native_password"] {
        Vec::new()
    } else {
        plugins
    }
}

/// How a new password is set: `identified by …`, or on MariaDB through the
/// account's plugins, which `identified by` replaced with the password
/// alone: root@localhost lost its `unix_socket` login (#463). A plugin that
/// takes a password gets it; without one, the password is added.
fn identified(def: &UserDef, plain: bool) -> String {
    const PASSWORDS: &[&str] = &["mysql_native_password", "ed25519", "parsec"];
    let password = || format!("using password({})", literal(&def.password, plain));
    if !def.mariadb || def.plugins.is_empty() {
        return format!("identified by {}", literal(&def.password, plain));
    }
    let mut parts: Vec<String> = def
        .plugins
        .iter()
        .map(|p| {
            if PASSWORDS.contains(&p.as_str()) {
                format!("{p} {}", password())
            } else {
                p.clone()
            }
        })
        .collect();
    if !def.plugins.iter().any(|p| PASSWORDS.contains(&p.as_str())) {
        parts.push(format!("mysql_native_password {}", password()));
    }
    format!("identified via {}", parts.join(" or "))
}

/// The privileges of a line of SHOW GRANTS that grants on the server
/// (`GRANT RELOAD, PROCESS ON *.* TO `r` WITH GRANT OPTION`), and whether
/// with the grant option.
fn global_grant(line: &str) -> Option<(Vec<String>, bool)> {
    let (privileges, to) = line.strip_prefix("GRANT ")?.split_once(" ON *.* TO ")?;
    let privileges = privileges
        .split(',')
        .map(|p| p.trim().to_uppercase())
        .collect();
    Some((privileges, to.ends_with(" WITH GRANT OPTION")))
}

pub(super) fn statements(original: Option<&UserDef>, def: &UserDef) -> Result<Vec<String>, String> {
    let name = def.name.trim();
    let host = match def.host.trim() {
        "" if !(def.role && def.mariadb) => "%",
        host => host,
    };
    let plain = def.no_backslash_escapes;
    let acct = account(name, host, def.role, plain);
    let mut out = Vec::new();
    match original {
        None if def.role => out.push(format!("create role {acct}")),
        None => {
            if def.password.is_empty() {
                return Err("Enter a password".to_owned());
            }
            let mut create = format!(
                "create user {acct} identified by {}",
                literal(&def.password, plain)
            );
            if def.expire {
                create.push_str(" password expire");
            }
            if def.locked == Some(true) {
                create.push_str(" account lock");
            }
            out.push(create);
        }
        Some(_) if def.role => {}
        Some(o) => {
            let mut clauses = Vec::new();
            if !def.password.is_empty() {
                clauses.push(identified(def, plain));
            }
            if o.expire && !def.expire && def.password.is_empty() {
                return Err("An expired password stays expired until a new one is set".to_owned());
            }
            // A new password isn't expired.
            if def.expire && (!o.expire || !def.password.is_empty()) {
                clauses.push("password expire".to_owned());
            }
            if def.locked != o.locked {
                match def.locked {
                    Some(true) => clauses.push("account lock".to_owned()),
                    Some(false) => clauses.push("account unlock".to_owned()),
                    None => {}
                }
            }
            if !clauses.is_empty() {
                out.push(format!("alter user {acct} {}", clauses.join(" ")));
            }
        }
    }

    let empty = UserDef::default();
    let before = original.unwrap_or(&empty);
    role_changes(
        &before.roles,
        &def.roles,
        |roles| format!("revoke {} from {acct}", roles_sql(roles, plain)),
        |roles, admin| {
            let option = if admin { " with admin option" } else { "" };
            format!("grant {} to {acct}{option}", roles_sql(roles, plain))
        },
        &mut out,
    );
    // Unseen roles: which are enabled at login stays as it is.
    if !def.role && !def.unread.roles {
        let defaults = |d: &UserDef| -> Vec<String> {
            d.roles
                .iter()
                .filter(|(_, g)| g.default)
                .map(|(r, _)| r.clone())
                .collect()
        };
        let after = defaults(def);
        // MySQL's REVOKE, which takes the admin option away, ends a role's
        // being enabled at login, and the GRANT after it doesn't give that
        // back (#462); MariaDB keeps it.
        let regranted = !def.mariadb
            && def.roles.iter().any(|(r, g)| {
                g.default && before.roles.get(r).is_some_and(|was| was.admin && !g.admin)
            });
        if after != defaults(before) || regranted {
            let refs: Vec<&String> = after.iter().collect();
            let roles = if after.is_empty() {
                "none".to_owned()
            } else {
                roles_sql(&refs, plain)
            };
            if def.mariadb {
                if after.len() > 1 {
                    return Err("MariaDB enables one role at login: pick one default".to_owned());
                }
                out.push(format!("set default role {roles} for {acct}"));
            } else {
                out.push(format!("set default role {roles} to {acct}"));
            }
        }
    }

    for (scope, (before, after)) in by_scope(&before.privileges, &def.privileges) {
        let on = scope_sql(&scope);
        let revoked: Vec<&String> = before.keys().filter(|p| !after.contains_key(*p)).collect();
        if !revoked.is_empty() {
            out.push(format!("revoke {} on {on} from {acct}", list(&revoked)));
        }
        let option = after.contains_key(GRANT_OPTION) && !before.contains_key(GRANT_OPTION);
        let option = if option { " with grant option" } else { "" };
        let granted: Vec<&String> = after
            .keys()
            .filter(|p| !before.contains_key(*p) && *p != GRANT_OPTION)
            .collect();
        if !granted.is_empty() {
            out.push(format!(
                "grant {} on {on} to {acct}{option}",
                list(&granted)
            ));
        } else if !option.is_empty() {
            out.push(format!("grant usage on {on} to {acct}{option}"));
        }
    }
    Ok(out)
}

pub(super) fn ui(ui: &mut Ui, def: &mut UserDef, original: Option<&UserDef>) {
    let existing = original.is_some();
    // MariaDB's roles have no host.
    if !(def.role && def.mariadb) {
        form_row(ui, "Host", |ui| {
            if existing {
                ui.monospace(def.host.as_str());
            } else {
                ui.add(
                    egui::TextEdit::singleline(&mut def.host)
                        .hint_text("%")
                        .desired_width(160.0),
                )
                .on_hover_text("Where the user may connect from: % for anywhere");
            }
        });
    }
    if def.role {
        return;
    }
    form_row(ui, "Password", |ui| {
        ui.add(
            egui::TextEdit::singleline(&mut def.password)
                .password(true)
                .hint_text(if existing { "unchanged" } else { "" })
                .desired_width(200.0),
        );
        ui.checkbox(&mut def.expire, "expired")
            .on_hover_text("Must be changed at the next login");
    });
    form_row(ui, "Account", |ui| {
        if def.unread.attributes {
            ui.radio_value(&mut def.locked, None, "as it is");
            ui.radio_value(&mut def.locked, Some(true), "locked");
            ui.radio_value(&mut def.locked, Some(false), "unlocked");
        } else {
            let mut locked = def.locked == Some(true);
            if ui
                .checkbox(&mut locked, "locked")
                .on_hover_text("Nobody can log in until it's unlocked")
                .changed()
            {
                def.locked = Some(locked);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> UserDef {
        UserDef {
            name: "app".into(),
            password: "s3cret".into(),
            ..UserDef::new(DbKind::MySql, false, false)
        }
    }

    #[test]
    fn creates_users_and_roles() {
        let mut def = user();
        def.expire = true;
        def.roles.insert("reader".into(), RoleGrant::default());
        def.roles.insert(
            "admin@localhost".into(),
            RoleGrant {
                admin: true,
                default: false,
            },
        );
        def.privileges
            .insert((Scope::Global, "PROCESS".into()), Grant::Plain);
        def.privileges.insert(
            (Scope::Database("shop".into()), "SELECT".into()),
            Grant::Plain,
        );
        def.privileges.insert(
            (Scope::Database("shop".into()), GRANT_OPTION.into()),
            Grant::Plain,
        );
        def.privileges.insert(
            (
                Scope::Object("shop".into(), "order items".into()),
                "UPDATE".into(),
            ),
            Grant::Plain,
        );
        assert_eq!(
            statements(None, &def).unwrap(),
            [
                "create user 'app'@'%' identified by 's3cret' password expire",
                "grant 'reader' to 'app'@'%'",
                "grant 'admin'@'localhost' to 'app'@'%' with admin option",
                "set default role 'reader' to 'app'@'%'",
                "grant process on *.* to 'app'@'%'",
                "grant select on shop.* to 'app'@'%' with grant option",
                "grant update on shop.`order items` to 'app'@'%'",
            ]
        );
        let role = UserDef {
            name: "o'reader".into(),
            ..UserDef::new(DbKind::MySql, true, false)
        };
        assert_eq!(
            statements(None, &role).unwrap(),
            ["create role 'o''reader'"]
        );
        assert!(
            statements(
                None,
                &UserDef {
                    password: String::new(),
                    ..user()
                }
            )
            .is_err()
        );
    }

    /// Taking a default role's admin option away revokes and grants it
    /// again, and sets the default roles again: MySQL's REVOKE ended its
    /// being enabled at login (#462).
    #[test]
    fn a_default_role_without_admin_stays_default() {
        let mut loaded = user();
        loaded.password.clear();
        loaded.roles.insert(
            "reader".into(),
            RoleGrant {
                admin: true,
                default: true,
            },
        );
        let mut changed = loaded.clone();
        changed.roles.get_mut("reader").unwrap().admin = false;
        assert_eq!(
            statements(Some(&loaded), &changed).unwrap(),
            [
                "revoke 'reader' from 'app'@'%'",
                "grant 'reader' to 'app'@'%'",
                "set default role 'reader' to 'app'@'%'",
            ]
        );
        // MariaDB keeps it.
        loaded.mariadb = true;
        changed.mariadb = true;
        assert_eq!(statements(Some(&loaded), &changed).unwrap().len(), 2);
    }

    /// On MariaDB a new password keeps the account's other plugins (#463).
    #[test]
    fn a_new_password_keeps_mariadbs_plugins() {
        assert_eq!(
            plugins(
                "CREATE USER `root`@`localhost` IDENTIFIED VIA unix_socket OR \
                 mysql_native_password USING '*667F' REQUIRE SSL"
            ),
            ["unix_socket", "mysql_native_password"]
        );
        assert!(plugins("CREATE USER `a`@`%` IDENTIFIED BY PASSWORD '*F33A'").is_empty());
        assert!(
            plugins("CREATE USER `a`@`%` IDENTIFIED VIA mysql_native_password USING '*F'")
                .is_empty()
        );
        let mut loaded = user();
        loaded.password.clear();
        loaded.mariadb = true;
        loaded.plugins = vec!["unix_socket".into(), "mysql_native_password".into()];
        let changed = UserDef {
            password: "n'ew".into(),
            ..loaded.clone()
        };
        assert_eq!(
            statements(Some(&loaded), &changed).unwrap(),
            [
                "alter user 'app'@'%' identified via unix_socket or mysql_native_password \
              using password('n''ew')"
            ]
        );
        let socket_only = UserDef {
            plugins: vec!["unix_socket".into()],
            ..changed
        };
        assert_eq!(
            statements(Some(&loaded), &socket_only).unwrap(),
            [
                "alter user 'app'@'%' identified via unix_socket or mysql_native_password \
              using password('n''ew')"
            ]
        );
    }

    #[test]
    fn alters_users() {
        let mut loaded = user();
        loaded.password.clear();
        loaded.roles.insert("reader".into(), RoleGrant::default());
        loaded
            .privileges
            .insert((Scope::Global, "SELECT".into()), Grant::Plain);
        loaded
            .privileges
            .insert((Scope::Global, GRANT_OPTION.into()), Grant::Plain);
        assert_eq!(statements(Some(&loaded), &loaded), Ok(Vec::new()));

        let mut edited = loaded.clone();
        edited.password = "new".into();
        edited.locked = Some(true);
        edited.roles.get_mut("reader").unwrap().default = false;
        edited
            .privileges
            .remove(&(Scope::Global, GRANT_OPTION.into()));
        edited
            .privileges
            .insert((Scope::Global, "INSERT".into()), Grant::Plain);
        assert_eq!(
            statements(Some(&loaded), &edited).unwrap(),
            [
                "alter user 'app'@'%' identified by 'new' account lock",
                "set default role none to 'app'@'%'",
                "revoke grant option on *.* from 'app'@'%'",
                "grant insert on *.* to 'app'@'%'",
            ]
        );
    }

    #[test]
    fn mariadb_default_role() {
        let mut loaded = UserDef {
            mariadb: true,
            ..user()
        };
        loaded.password.clear();
        let mut edited = loaded.clone();
        edited.roles.insert("a".into(), RoleGrant::default());
        assert_eq!(
            statements(Some(&loaded), &edited).unwrap(),
            [
                "grant 'a' to 'app'@'%'",
                "set default role 'a' for 'app'@'%'",
            ]
        );
        edited.roles.insert("b".into(), RoleGrant::default());
        assert!(statements(Some(&loaded), &edited).is_err());
    }

    #[test]
    fn expiry() {
        let mut loaded = user();
        loaded.password.clear();
        loaded.expire = true;
        let mut edited = loaded.clone();
        edited.expire = false;
        assert!(statements(Some(&loaded), &edited).is_err());
        edited.password = "x".into();
        assert_eq!(
            statements(Some(&loaded), &edited).unwrap(),
            ["alter user 'app'@'%' identified by 'x'"]
        );
        edited.expire = true;
        assert_eq!(
            statements(Some(&loaded), &edited).unwrap(),
            ["alter user 'app'@'%' identified by 'x' password expire"]
        );
    }

    #[test]
    fn names() {
        assert_eq!(
            split("o'neil@x@%", false, false),
            ("o'neil@x".into(), "%".into())
        );
        assert_eq!(split("reader", true, false), ("reader".into(), "%".into()));
        assert_eq!(
            split("reader", true, true),
            ("reader".into(), String::new())
        );
        assert_eq!(account("a\\b", "%", false, false), "'a\\\\b'@'%'");
        // With NO_BACKSLASH_ESCAPES a `\` stands for itself.
        assert_eq!(account("a\\b", "%", false, true), "'a\\b'@'%'");
    }
}
