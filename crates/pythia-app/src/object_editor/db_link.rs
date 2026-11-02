//! Database link: private or public, the remote user and password, and the
//! connect string. A change recreates the link.

use egui::Ui;
use pythia_db::oracle::Connection;
use pythia_db::{DbError, Dialect};

use super::{form_grid, form_row, name_field, qualified};
use crate::browser::written;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LinkDef {
    pub public: bool,
    pub name: String,
    /// Empty: connect as the local user (no `CONNECT TO`).
    pub user: String,
    /// Empty for an existing link: unchanged.
    pub password: String,
    /// TNS alias or `host:port/service`.
    pub using: String,
}

pub fn load(conn: &Connection, owner: &str, name: &str) -> Result<LinkDef, DbError> {
    // A name without a domain gets the database's (`REMOTE` becomes
    // `REMOTE.EXAMPLE.COM`): the shortest match is the link.
    let (user, host) = conn
        .query_as::<(Option<String>, Option<String>)>(
            "select username, host from all_db_links
              where owner = :1 and (db_link = :2 or db_link like :2 || '.%')
              order by length(db_link)",
            &[&owner, &name],
        )?
        .next()
        .transpose()?
        .ok_or_else(|| DbError::new(format!("Database link {name} not found")))?;
    Ok(LinkDef {
        public: owner == "PUBLIC",
        name: name.to_lowercase(),
        // As SQL reads it: a user "hr_ro" isn't HR_RO (#365).
        user: user
            .map(|u| written(Dialect::ORACLE, &u))
            .unwrap_or_default(),
        password: String::new(),
        using: host.unwrap_or_default(),
    })
}

/// A link's name: dots belong to it (`remote.example.com`), so it is never
/// qualified with a schema.
fn link_name(name: &str) -> String {
    let name = name.trim();
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_$#.@".contains(c))
    {
        name.to_lowercase()
    } else {
        format!("\"{}\"", name.trim_matches('"').replace('"', "\"\""))
    }
}

fn connect_to(def: &LinkDef) -> String {
    format!(
        " connect to {} identified by \"{}\"",
        def.user.trim(),
        def.password
    )
}

fn create(def: &LinkDef) -> String {
    let public = if def.public { "public " } else { "" };
    let login = if def.user.trim().is_empty() {
        String::new()
    } else {
        connect_to(def)
    };
    format!(
        "create {public}database link {}{login}\n  using '{}'",
        link_name(&def.name),
        def.using.trim().replace('\'', "''")
    )
}

pub fn statements(original: Option<&LinkDef>, def: &LinkDef) -> Result<Vec<String>, String> {
    if def.name.trim().is_empty() {
        return Err("Enter a name".into());
    }
    if def.using.trim().is_empty() {
        return Err("Enter the connect string".into());
    }
    if def.password.contains('"') {
        return Err("The password can't contain double quotes".into());
    }
    let changed = original.is_none_or(|o| {
        def.using.trim() != o.using.trim()
            || def.user.trim() != o.user.trim()
            || !def.password.is_empty()
    });
    if !changed {
        return Ok(Vec::new());
    }
    if !def.user.trim().is_empty() && def.password.is_empty() {
        return Err(if original.is_some() {
            "Enter the password to change the user or connect string".into()
        } else {
            "Enter the remote user's password".into()
        });
    }
    let Some(_) = original else {
        return Ok(vec![create(def)]);
    };
    // ALTER DATABASE LINK needs a privilege of its own; dropping and
    // creating it again only needs what created it.
    let public = if def.public { "public " } else { "" };
    Ok(vec![
        format!("drop {public}database link {}", link_name(&def.name)),
        create(def),
    ])
}

/// The statements with the password hidden, for showing them.
pub fn masked(statements: &[String], def: &LinkDef) -> Vec<String> {
    if def.password.is_empty() {
        return statements.to_vec();
    }
    let secret = format!("identified by \"{}\"", def.password);
    statements
        .iter()
        .map(|s| s.replace(&secret, "identified by \"********\""))
        .collect()
}

pub fn ui(ui: &mut Ui, def: &mut LinkDef, existing: bool, owner: &str) {
    form_grid(ui, "db-link", |ui| {
        form_row(ui, "Visibility", |ui| {
            ui.add_enabled_ui(!existing && owner != "PUBLIC", |ui| {
                ui.radio_value(&mut def.public, false, "private")
                    .on_hover_text(format!("Only {owner} uses it"));
                ui.radio_value(&mut def.public, true, "public")
                    .on_hover_text("Every user can use it; needs CREATE PUBLIC DATABASE LINK");
            });
        });
        name_field(ui, &mut def.name, existing);
        form_row(ui, "Connect string", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut def.using)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("TNS alias or host:1521/service")
                    .desired_width(320.0),
            );
        });
        form_row(ui, "Remote user", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut def.user)
                    .hint_text("the local user")
                    .desired_width(200.0),
            )
            .on_hover_text("Empty: connects as whoever uses the link, with their password");
        });
        form_row(ui, "Password", |ui| {
            ui.add_enabled(
                !def.user.trim().is_empty(),
                egui::TextEdit::singleline(&mut def.password)
                    .password(true)
                    .hint_text(if existing { "unchanged" } else { "" })
                    .desired_width(200.0),
            );
        });
    });
    ui.add_space(6.0);
    ui.weak(format!(
        "The link is created as {}; Test link in its context menu checks it.",
        if def.public {
            "a public link".to_owned()
        } else {
            qualified(owner, &def.name).to_lowercase()
        }
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_alters_links() {
        let mut def = LinkDef {
            public: false,
            name: "Remote.Example.Com".into(),
            user: "scott".into(),
            password: "tiger".into(),
            using: "db2:1521/orcl".into(),
        };
        let s = statements(None, &def).unwrap();
        assert_eq!(
            s,
            [
                "create database link remote.example.com connect to scott identified by \"tiger\"\n  using 'db2:1521/orcl'"
            ]
        );
        assert_eq!(
            masked(&s, &def),
            [
                "create database link remote.example.com connect to scott identified by \"********\"\n  using 'db2:1521/orcl'"
            ]
        );
        def.password.clear();
        assert!(statements(None, &def).is_err());
        def.user.clear();
        def.public = true;
        assert_eq!(
            statements(None, &def).unwrap(),
            ["create public database link remote.example.com\n  using 'db2:1521/orcl'"]
        );

        let original = LinkDef {
            public: false,
            name: "remote".into(),
            user: "scott".into(),
            password: String::new(),
            using: "db2".into(),
        };
        let mut edited = original.clone();
        assert!(statements(Some(&original), &edited).unwrap().is_empty());
        edited.password = "new".into();
        assert_eq!(
            statements(Some(&original), &edited).unwrap(),
            [
                "drop database link remote",
                "create database link remote connect to scott identified by \"new\"\n  using 'db2'"
            ]
        );
        edited.using = "db3".into();
        let s = statements(Some(&original), &edited).unwrap();
        assert!(s[1].ends_with("using 'db3'"));
        edited.password.clear();
        assert!(statements(Some(&original), &edited).is_err());
    }
}
