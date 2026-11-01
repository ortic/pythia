//! Connection passwords, SSH passwords and AI API keys in the OS keychain
//! (never in config files).

use tracing::warn;

const SERVICE: &str = "pythia";
/// The service passwords were stored under before the rename to Pythia.
const LEGACY_SERVICE: &str = "orui";
/// API keys of AI backends, by backend id.
const AI_SERVICE: &str = "pythia-ai";
/// SSH passwords and key passphrases, by connection.
const SSH_SERVICE: &str = "pythia-ssh";

fn entry(service: &str, connection: &str) -> Option<keyring::Entry> {
    keyring::Entry::new(service, connection)
        .inspect_err(|e| warn!("keychain unavailable: {e}"))
        .ok()
}

pub fn load(connection: &str) -> Option<String> {
    if let Ok(password) = entry(SERVICE, connection)?.get_password() {
        return Some(password);
    }
    let legacy = entry(LEGACY_SERVICE, connection)?;
    let password = legacy.get_password().ok()?;
    if entry(SERVICE, connection)?.set_password(&password).is_ok() {
        let _ = legacy.delete_credential();
    }
    Some(password)
}

/// Stores the password; the error says why the keychain doesn't keep it.
/// That was only logged, and the next connect had no password (#214).
pub fn store(connection: &str, password: &str) -> Result<(), String> {
    keyring::Entry::new(SERVICE, connection)
        .and_then(|entry| entry.set_password(password))
        .map_err(why)
}

pub fn forget(connection: &str) {
    for service in [SERVICE, LEGACY_SERVICE] {
        if let Some(entry) = entry(service, connection) {
            let _ = entry.delete_credential();
        }
    }
}

pub fn load_ssh(connection: &str) -> Option<String> {
    entry(SSH_SERVICE, connection)?.get_password().ok()
}

/// Stores the SSH password, or forgets it when it is empty.
pub fn store_ssh(connection: &str, password: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SSH_SERVICE, connection).map_err(why)?;
    if password.is_empty() {
        let _ = entry.delete_credential();
        Ok(())
    } else {
        entry.set_password(password).map_err(why)
    }
}

/// Why passwords can't be saved, if there is no keychain (no Secret
/// Service on Linux, for one).
pub fn unavailable() -> Option<String> {
    keyring::Entry::store_status()
        .as_ref()
        .err()
        .map(|e| format!("there's no keychain ({e})"))
}

/// Why the keychain didn't take a password.
fn why(e: keyring::Error) -> String {
    match (&e, unavailable()) {
        (keyring::Error::NoDefaultStore, Some(none)) => none,
        _ => format!("the keychain refused it ({e})"),
    }
}

pub fn forget_ssh(connection: &str) {
    if let Some(entry) = entry(SSH_SERVICE, connection) {
        let _ = entry.delete_credential();
    }
}

pub fn load_api_key(backend: &str) -> Option<String> {
    entry(AI_SERVICE, backend)?.get_password().ok()
}

pub fn store_api_key(backend: &str, key: &str) -> Result<(), String> {
    keyring::Entry::new(AI_SERVICE, backend)
        .and_then(|entry| entry.set_password(key))
        .map_err(|e| format!("Could not store the key in the keychain: {e}"))
}

pub fn forget_api_key(backend: &str) {
    if let Some(entry) = entry(AI_SERVICE, backend) {
        let _ = entry.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    /// A password the keychain doesn't take says why (#214).
    #[test]
    fn refused_passwords_say_why() {
        let e = keyring::Error::NoStorageAccess("the keychain is locked".into());
        assert_eq!(
            super::why(e),
            "the keychain refused it (Couldn't access platform storage: the keychain is locked)"
        );
        // Without a keychain, the reason it couldn't be opened.
        let none = super::why(keyring::Error::NoDefaultStore);
        match super::unavailable() {
            Some(reason) => assert_eq!(none, reason),
            None => assert!(none.starts_with("the keychain refused it"), "{none}"),
        }
    }
}
