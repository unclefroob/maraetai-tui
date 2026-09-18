//! Subsonic salt+token authentication — the same scheme every existing
//! maraetai client (iOS/macOS, Android, web) already uses against
//! `maraetai-service`: the password never goes on the wire, a fresh random
//! salt is generated per request, and the token is `MD5(password + salt)`.
//! (MD5 here is the Subsonic API's own required scheme, not a security
//! choice of ours — matching it is what makes this a valid Subsonic client.)

use md5::{Digest, Md5};
use rand::RngCore;

pub const CLIENT_NAME: &str = "maraetai-tui";
pub const API_VERSION: &str = "1.16.1";

/// One request's worth of Subsonic auth query parameters. Regenerate a fresh
/// one per request — salts are not meant to be reused.
#[derive(Debug, Clone)]
pub struct AuthParams {
    pub username: String,
    pub token: String,
    pub salt: String,
}

impl AuthParams {
    pub fn new(username: &str, password: &str) -> Self {
        let salt = random_salt();
        let token = token_for(password, &salt);
        Self {
            username: username.to_string(),
            token,
            salt,
        }
    }

    /// Appends `u`, `t`, `s`, `c`, `v` to the given query pairs — the common
    /// tail of every Subsonic request this project makes. `f` (response
    /// format) is deliberately left to the caller, since it depends on
    /// whether the caller wants JSON (used throughout this project) or the
    /// XML default.
    pub fn append_to(&self, pairs: &mut Vec<(String, String)>) {
        pairs.push(("u".into(), self.username.clone()));
        pairs.push(("t".into(), self.token.clone()));
        pairs.push(("s".into(), self.salt.clone()));
        pairs.push(("c".into(), CLIENT_NAME.into()));
        pairs.push(("v".into(), API_VERSION.into()));
    }
}

fn random_salt() -> String {
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn token_for(password: &str, salt: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(password.as_bytes());
    hasher.update(salt.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_matches_known_subsonic_vector() {
        // From the Subsonic API docs' own worked example:
        // password "sesame", salt "c19b2d", token = md5("sesamec19b2d").
        assert_eq!(token_for("sesame", "c19b2d"), "26719a1196d2a940705a59634eb18eab");
    }

    #[test]
    fn each_call_gets_a_fresh_salt() {
        let a = AuthParams::new("alice", "hunter2");
        let b = AuthParams::new("alice", "hunter2");
        assert_ne!(a.salt, b.salt, "salts must not repeat across requests");
        assert_ne!(a.token, b.token, "token must change when the salt does");
    }

    #[test]
    fn append_to_carries_the_right_keys() {
        let auth = AuthParams::new("alice", "hunter2");
        let mut pairs = Vec::new();
        auth.append_to(&mut pairs);
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["u", "t", "s", "c", "v"]);
    }
}
