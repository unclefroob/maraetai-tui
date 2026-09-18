use std::path::PathBuf;

/// Errors shared across the daemon and TUI — config/credential/IPC-path
/// resolution failures that both binaries need to report the same way.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not determine a config directory for this platform")]
    NoConfigDir,

    #[error("config file not found at {0}; run `maraetai login` first")]
    ConfigMissing(PathBuf),

    #[error("failed to read config at {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write config at {path}: {source}")]
    ConfigWrite {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse config at {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("failed to serialize config: {0}")]
    ConfigSerialize(#[from] toml::ser::Error),

    #[error(
        "no password found in the OS keyring for user {username:?} — run `maraetai login` again"
    )]
    PasswordMissing { username: String },

    #[error("keyring error: {0}")]
    Keyring(#[from] keyring::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
