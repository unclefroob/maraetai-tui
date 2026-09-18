use std::fs;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// On-disk config: server URL + username only. **Never the password** — that
/// lives in the OS keyring (see [`Credentials::load`]), a deliberate
/// improvement over every existing maraetai client, none of which use a
/// hardware/OS-backed secret store on the platforms where one exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub server_url: String,
    pub username: String,
}

/// The OS keyring service name under which the password is stored, keyed by
/// username as the keyring "account" — so switching accounts on the same
/// machine keeps each password separate.
const KEYRING_SERVICE: &str = "maraetai";

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "maraetai").ok_or(Error::NoConfigDir)
}

/// Path to `~/.config/maraetai/config.toml` (or the platform equivalent).
pub fn config_path() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().join("config.toml"))
}

impl Config {
    /// Loads the config file. Returns [`Error::ConfigMissing`] if it doesn't
    /// exist yet — callers should treat that as "run `maraetai login`", not a
    /// fatal startup error, so the daemon can still come up and expose MPRIS
    /// in a clearly-unconfigured state rather than crash-looping.
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(Error::ConfigMissing(path.to_path_buf()));
        }
        let raw = fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| Error::ConfigParse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Writes the config file, creating its parent directory if needed. Only
    /// server URL + username are ever written here — see the module doc.
    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|source| Error::ConfigWrite {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let raw = toml::to_string_pretty(self)?;
        fs::write(path, raw).map_err(|source| Error::ConfigWrite {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Config + the password fetched from the OS keyring — what a caller actually
/// needs to authenticate against `maraetai-service`. Kept separate from
/// [`Config`] so the password is never accidentally serialized to disk (it
/// simply isn't a field on the type that gets written).
#[derive(Clone)]
pub struct Credentials {
    pub server_url: String,
    pub username: String,
    pub password: String,
}

impl Credentials {
    /// Loads the config file, then looks up the matching password in the OS
    /// keyring. Fails closed — [`Error::PasswordMissing`] — rather than
    /// falling back to any plaintext storage if the keyring entry is absent.
    pub fn load() -> Result<Self> {
        let config = Config::load()?;
        let password = keyring_entry(&config.username)?
            .get_password()
            .map_err(|e| match e {
                keyring::Error::NoEntry => Error::PasswordMissing {
                    username: config.username.clone(),
                },
                other => Error::Keyring(other),
            })?;
        Ok(Self {
            server_url: config.server_url,
            username: config.username,
            password,
        })
    }

    /// Saves the config file and stores the password in the OS keyring. This
    /// is the `maraetai login` flow.
    pub fn save(server_url: String, username: String, password: &str) -> Result<()> {
        Config {
            server_url,
            username: username.clone(),
        }
        .save()?;
        keyring_entry(&username)?.set_password(password)?;
        Ok(())
    }
}

fn keyring_entry(username: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(KEYRING_SERVICE, username)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config {
            server_url: "https://music.example.com".into(),
            username: "alice".into(),
        };
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(cfg, loaded);
    }

    #[test]
    fn missing_file_is_a_distinct_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        assert!(matches!(
            Config::load_from(&path),
            Err(Error::ConfigMissing(_))
        ));
    }

    #[test]
    fn never_serializes_a_password_field() {
        // Guards the security property in the module doc: Config has no
        // password field at all, so there is no way for one to leak into the
        // on-disk TOML even by future accident — this test fails to compile
        // (not just fails at runtime) if a `password` field is ever added
        // without updating this guard.
        let cfg = Config {
            server_url: "https://music.example.com".into(),
            username: "alice".into(),
        };
        let raw = toml::to_string(&cfg).unwrap();
        assert!(!raw.contains("password"));
    }
}
