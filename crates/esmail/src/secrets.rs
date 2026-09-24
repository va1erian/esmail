//! Passwords, held in the OS keyring rather than in [`crate::config`].
//!
//! Keyed by `(account_id, kind)` where `kind` is `"imap"` or `"smtp"`, so one
//! account can hold two different passwords (or the same one twice) without
//! the two services colliding.

use secrecy::{ExposeSecret, SecretString};

const SERVICE: &str = "esmail";
/// Names another keyring service, so a test run never touches the real entries.
const SERVICE_VAR: &str = "ESMAIL_KEYRING_SERVICE";

fn entry(account_id: &str, kind: &str) -> keyring::Result<keyring::Entry> {
    let service = std::env::var(SERVICE_VAR).unwrap_or_else(|_| SERVICE.to_string());
    keyring::Entry::new(&service, &format!("{account_id}:{kind}"))
}

/// Store `password` for `account_id`'s `kind` connection (`"imap"`/`"smtp"`).
pub fn set_password(account_id: &str, kind: &str, password: &SecretString) -> keyring::Result<()> {
    entry(account_id, kind)?.set_password(password.expose_secret())
}

/// Look up the stored password, if any. Absent (`NoEntry`) and platform
/// keyring errors are both treated as "no password on file" — the caller
/// falls back to prompting rather than failing to start.
pub fn get_password(account_id: &str, kind: &str) -> Option<SecretString> {
    entry(account_id, kind)
        .ok()?
        .get_password()
        .ok()
        .map(SecretString::from)
}

/// Remove a stored password, e.g. when an account is deleted.
pub fn delete_password(account_id: &str, kind: &str) {
    if let Ok(e) = entry(account_id, kind) {
        let _ = e.delete_credential();
    }
}
