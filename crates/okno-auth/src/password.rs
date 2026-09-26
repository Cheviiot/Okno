use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub const MIN_PASSWORD_LEN: usize = 8;
const MAX_USERNAME_LEN: usize = 64;
const MAX_PASSWORD_LEN: usize = 256;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CredentialsError {
    #[error("username must be 1–{MAX_USERNAME_LEN} characters without spaces or control characters")]
    BadUsername,
    #[error("password must be {MIN_PASSWORD_LEN}–{MAX_PASSWORD_LEN} characters")]
    BadPassword,
    #[error("stored password hash is invalid")]
    BadHash,
}

/// Host login: a username and an Argon2id hash in PHC format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password_hash: String,
}

/// Argon2id with 64 MiB, 3 passes, one lane: slow enough to make offline
/// guessing expensive, fast enough (~0.2 s) for an interactive login.
fn argon2() -> Argon2<'static> {
    let params = Params::new(64 * 1024, 3, 1, None).expect("valid Argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

impl Credentials {
    pub fn new(username: &str, password: &str) -> Result<Self, CredentialsError> {
        let username = username.trim();
        if username.is_empty()
            || username.chars().count() > MAX_USERNAME_LEN
            || username.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(CredentialsError::BadUsername);
        }
        let len = password.chars().count();
        if !(MIN_PASSWORD_LEN..=MAX_PASSWORD_LEN).contains(&len) {
            return Err(CredentialsError::BadPassword);
        }
        let salt = SaltString::generate(&mut OsRng);
        let hash =
            argon2().hash_password(password.as_bytes(), &salt).map_err(|_| CredentialsError::BadPassword)?.to_string();
        Ok(Self { username: username.to_owned(), password_hash: hash })
    }

    /// Checks a login attempt. CPU- and memory-heavy: call from a blocking
    /// thread. The password copy is wiped afterwards.
    pub fn verify(&self, username: &str, password: Zeroizing<String>) -> bool {
        let Ok(hash) = PasswordHash::new(&self.password_hash) else {
            return false;
        };
        // Always run the hash so a wrong username costs as much as a wrong
        // password and cannot be told apart by timing.
        let password_ok =
            password.len() <= MAX_PASSWORD_LEN * 4 && argon2().verify_password(password.as_bytes(), &hash).is_ok();
        let user_ok = constant_time_eq(username.trim().as_bytes(), self.username.as_bytes());
        password_ok & user_ok
    }

    pub fn validate(&self) -> Result<(), CredentialsError> {
        PasswordHash::new(&self.password_hash).map(|_| ()).map_err(|_| CredentialsError::BadHash)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pw(s: &str) -> Zeroizing<String> {
        Zeroizing::new(s.to_owned())
    }

    #[test]
    fn verifies_only_matching_login() {
        let creds = Credentials::new("admin", "correct horse").unwrap();
        assert!(creds.password_hash.starts_with("$argon2id$v=19$m=65536,t=3,p=1$"));
        assert!(creds.verify("admin", pw("correct horse")));
        assert!(creds.verify(" admin ", pw("correct horse")));
        assert!(!creds.verify("admin", pw("wrong horse")));
        assert!(!creds.verify("root", pw("correct horse")));
    }

    #[test]
    fn rejects_weak_input() {
        assert_eq!(Credentials::new("admin", "short").unwrap_err(), CredentialsError::BadPassword);
        assert_eq!(Credentials::new("", "long enough").unwrap_err(), CredentialsError::BadUsername);
        assert_eq!(Credentials::new("a b", "long enough").unwrap_err(), CredentialsError::BadUsername);
    }

    #[test]
    fn corrupt_hash_never_verifies() {
        let creds = Credentials { username: "a".into(), password_hash: "garbage".into() };
        assert!(creds.validate().is_err());
        assert!(!creds.verify("a", pw("anything at all")));
    }
}
