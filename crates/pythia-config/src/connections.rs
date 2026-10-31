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
