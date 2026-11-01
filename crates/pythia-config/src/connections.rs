use serde::{Deserialize, Serialize};

/// The kind of database a connection talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    #[default]
    Oracle,
    MySql,
    Postgres,
    SqlServer,
    Sqlite,
}

impl DbKind {
    pub const ALL: [DbKind; 5] = [
        DbKind::Oracle,
        DbKind::MySql,
        DbKind::Postgres,
        DbKind::SqlServer,
        DbKind::Sqlite,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DbKind::Oracle => "Oracle",
            DbKind::MySql => "MySQL",
            DbKind::Postgres => "PostgreSQL",
            DbKind::SqlServer => "SQL Server",
            DbKind::Sqlite => "SQLite",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Privilege {
    #[default]
    Normal,
    Sysdba,
    Sysoper,
    Sysbackup,
}

impl Privilege {
    pub const ALL: [Privilege; 4] = [
        Privilege::Normal,
        Privilege::Sysdba,
        Privilege::Sysoper,
        Privilege::Sysbackup,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Privilege::Normal => "Normal",
            Privilege::Sysdba => "SYSDBA",
            Privilege::Sysoper => "SYSOPER",
            Privilege::Sysbackup => "SYSBACKUP",
        }
    }
}

/// Reaching the database through an SSH server, like `ssh -L`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SshTunnel {
    pub enabled: bool,
    /// `host[:port]` of the SSH server.
    pub host: String,
    pub username: String,
    /// A private key (OpenSSH or PuTTY `.ppk`). Without one: the SSH
    /// password, or the SSH agent's keys when there is none either.
    pub key_file: String,
}

impl SshTunnel {
    /// Never set up: not written to `connections.toml`.
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

/// A saved connection. Never contains the password.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionProfile {
    pub name: String,
    /// Slash separated folder path, e.g. `customer/prod`.
    pub folder: String,
    /// Oracle unless the profile says otherwise.
    pub kind: DbKind,
    pub username: String,
    /// Oracle: TNS alias, EZConnect (`host:port/service`) or a full
    /// descriptor. MySQL: `host[:port][/database]`. SQLite: the database
    /// file.
    pub connect_string: String,
    pub privilege: Privilege,
    /// Production connections get a red marker and extra confirmations.
    pub production: bool,
    pub read_only: bool,
    /// Optional RGB accent shown in tabs and the status bar.
    pub color: Option<[u8; 3]>,
    /// The password is stored in the OS keychain.
    pub save_password: bool,
    /// Schema to work in after connecting (`ALTER SESSION SET CURRENT_SCHEMA`).
    pub default_schema: String,
    /// SQL run on every new session, e.g. NLS settings.
    pub login_script: String,
    /// Kept while switched off, so it can be switched on again. The SSH
    /// password goes to the keychain too.
    #[serde(skip_serializing_if = "SshTunnel::is_unset")]
    pub ssh: SshTunnel,
}

impl ConnectionProfile {
    /// `user@connect_string`, the way PL/SQL Developer shows connections;
    /// a database without users (SQLite) by its file.
    pub fn display_name(&self) -> String {
        if !self.name.is_empty() {
            self.name.clone()
        } else if self.username.is_empty() {
            self.connect_string.clone()
        } else {
            format!("{}@{}", self.username, self.connect_string)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Connections {
    #[serde(rename = "connection")]
    pub profiles: Vec<ConnectionProfile>,
}

impl Connections {
    /// Adds the profile, replacing an existing one with the same display name.
    pub fn upsert(&mut self, profile: ConnectionProfile) {
        let name = profile.display_name();
        match self.profiles.iter_mut().find(|p| p.display_name() == name) {
            Some(existing) => *existing = profile,
            None => self.profiles.push(profile),
        }
    }

    /// The profile other than `previous` (the one being changed) whose
    /// display name is `name`, but for case: saving under `name` would
    /// replace it. None when `name` is `previous`'s own.
    pub fn taken_by(&self, name: &str, previous: Option<&str>) -> Option<&ConnectionProfile> {
        if previous == Some(name) {
            return None;
        }
        let name = name.to_lowercase();
        self.profiles.iter().find(|p| {
            let other = p.display_name();
            Some(other.as_str()) != previous && other.to_lowercase() == name
        })
    }

    /// Saves `profile` in place of `previous`, the profile it was (by its
    /// display name), or as a new one. When another profile has its name,
    /// but for case, that one stays and the error is its name (#246).
    pub fn save(
        &mut self,
        profile: ConnectionProfile,
        previous: Option<&str>,
    ) -> Result<(), String> {
        let name = profile.display_name();
        if let Some(other) = self.taken_by(&name, previous) {
            return Err(other.display_name());
        }
        let was = previous.unwrap_or(&name);
        match self.profiles.iter_mut().find(|p| p.display_name() == was) {
            Some(existing) => *existing = profile,
            None => self.upsert(profile),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_without_a_kind_are_oracle() {
        let saved: Connections = toml::from_str(
            "[[connection]]\nusername = \"scott\"\nconnect_string = \"localhost/FREEPDB1\"\n",
        )
        .unwrap();
        assert_eq!(saved.profiles[0].kind, DbKind::Oracle);

        let mysql = ConnectionProfile {
            kind: DbKind::MySql,
            ..Default::default()
        };
        let text = toml::to_string(&mysql).unwrap();
        assert!(text.contains("kind = \"mysql\""), "{text}");
        let back: ConnectionProfile = toml::from_str(&text).unwrap();
        assert_eq!(back.kind, DbKind::MySql);

        for (kind, name) in [
            (DbKind::Postgres, "postgres"),
            (DbKind::SqlServer, "sqlserver"),
            (DbKind::Sqlite, "sqlite"),
        ] {
            let profile = ConnectionProfile {
                kind,
                ..Default::default()
            };
            let text = toml::to_string(&profile).unwrap();
            assert!(text.contains(&format!("kind = \"{name}\"")), "{text}");
        }
    }

    #[test]
    fn saved_under_their_own_name() {
        let profile = |name: &str, user: &str| ConnectionProfile {
            name: name.into(),
            username: user.into(),
            ..Default::default()
        };
        let mut saved = Connections {
            profiles: vec![profile("super", "a"), profile("other", "b")],
        };
        let listed = |saved: &Connections| -> Vec<(String, String)> {
            saved
                .profiles
                .iter()
                .map(|p| (p.name.clone(), p.username.clone()))
                .collect()
        };
        // Another one's name, but for case: refused, it stays.
        for name in ["super", "Super", "SUPER"] {
            let refused = Err("super".to_owned());
            assert_eq!(saved.save(profile(name, "b"), Some("other")), refused);
            assert_eq!(saved.save(profile(name, "new"), None), refused);
        }
        assert_eq!(
            listed(&saved),
            [("super".into(), "a".into()), ("other".into(), "b".into())]
        );
        // Its own name in another case: renamed, in its place.
        assert_eq!(saved.save(profile("Super", "a"), Some("super")), Ok(()));
        assert_eq!(saved.save(profile("Super", "a2"), Some("Super")), Ok(()));
        assert_eq!(saved.save(profile("third", "c"), None), Ok(()));
        assert_eq!(
            listed(&saved),
            [
                ("Super".into(), "a2".into()),
                ("other".into(), "b".into()),
                ("third".into(), "c".into())
            ]
        );
        // Unnamed ones go by user@database.
        let mut saved = Connections {
            profiles: vec![ConnectionProfile {
                username: "hr".into(),
                connect_string: "xe".into(),
                ..Default::default()
            }],
        };
        let other = ConnectionProfile {
            name: "HR@XE".into(),
            ..Default::default()
        };
        assert_eq!(saved.save(other, None), Err("hr@xe".to_owned()));
    }

    #[test]
    fn ssh_tunnel_is_saved_once_set_up() {
        let plain = toml::to_string(&ConnectionProfile::default()).unwrap();
        assert!(!plain.contains("ssh"), "{plain}");

        let mut saved = Connections::default();
        saved.upsert(ConnectionProfile {
            kind: DbKind::MySql,
            username: "app".into(),
            connect_string: "localhost/shop".into(),
            ssh: SshTunnel {
                enabled: true,
                host: "bastion.example.com:2222".into(),
                username: "remo".into(),
                key_file: "~/.ssh/id_ed25519".into(),
            },
            ..Default::default()
        });
        let text = toml::to_string_pretty(&saved).unwrap();
        assert!(text.contains("[connection.ssh]"), "{text}");
        let back: Connections = toml::from_str(&text).unwrap();
        assert_eq!(back, saved);
    }
}
