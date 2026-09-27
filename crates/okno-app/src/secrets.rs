//! Saved logins in the system keyring (Secret Service on Linux, Credential
//! Manager on Windows), keyed by the host's fingerprint.
//!
//! Calls may block on D-Bus, so run them off the UI thread.

const SERVICE: &str = "io.github.cheviiot.okno";

fn entry(fingerprint: &str) -> Option<keyring::Entry> {
    // Test runs must not touch the user's real keyring.
    if std::env::var_os("OKNO_NO_KEYRING").is_some() {
        return None;
    }
    keyring::Entry::new(SERVICE, fingerprint).inspect_err(|e| tracing::debug!("keyring unavailable: {e}")).ok()
}

/// Username and password saved for a host.
pub fn load(fingerprint: &str) -> Option<(String, String)> {
    let secret = entry(fingerprint)?.get_password().ok()?;
    let (user, password) = secret.split_once('\0')?;
    Some((user.to_owned(), password.to_owned()))
}

pub fn save(fingerprint: &str, user: &str, password: &str) {
    if let Some(e) = entry(fingerprint) {
        if let Err(err) = e.set_password(&format!("{user}\0{password}")) {
            tracing::warn!("cannot save the password: {err}");
        }
    }
}

pub fn forget(fingerprint: &str) {
    if let Some(e) = entry(fingerprint) {
        let _ = e.delete_credential();
    }
}
