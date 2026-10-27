use std::path::{Path, PathBuf};

/// Environment variable that overrides the application data directory.
pub const ENV_DATA_DIR: &str = "REASON_NETWORK_DATA_DIR";
/// Environment variable carrying the stable session key of the calling agent.
pub const ENV_SESSION_KEY: &str = "REASON_NETWORK_SESSION_KEY";
/// Environment variable overriding the `claude` binary used for push delivery.
pub const ENV_CLAUDE_BIN: &str = "REASON_NETWORK_CLAUDE_BIN";
/// Environment variable overriding the session name used for auto registration.
pub const ENV_SESSION_NAME: &str = "REASON_NETWORK_SESSION_NAME";

/// Name of the SQLite database file inside the data directory.
pub const DB_FILE_NAME: &str = "reason-network.db";

/// The local endpoint clients use to reach the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A Windows named pipe, e.g. `\\.\pipe\reason-network-1a2b3c4d`.
    #[cfg(windows)]
    NamedPipe(String),
    /// A Unix domain socket path.
    #[cfg(unix)]
    UnixSocket(PathBuf),
}

impl Endpoint {
    /// Human readable form used in diagnostics and error messages.
    pub fn display(&self) -> String {
        match self {
            #[cfg(windows)]
            Endpoint::NamedPipe(name) => name.clone(),
            #[cfg(unix)]
            Endpoint::UnixSocket(path) => path.display().to_string(),
        }
    }
}

/// Resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory holding the database and any local runtime files.
    pub data_dir: PathBuf,
    /// Full path of the SQLite database.
    pub db_path: PathBuf,
    /// Local endpoint the daemon listens on.
    pub endpoint: Endpoint,
    /// Peer-to-peer networking options.
    pub net: crate::net::NetOptions,
}

impl Config {
    /// Build the configuration from the environment.
    ///
    /// `REASON_NETWORK_DATA_DIR` wins when set, which is what the test suite
    /// uses to stay away from real user data.
    pub fn from_env() -> Self {
        let data_dir = match std::env::var_os(ENV_DATA_DIR) {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => default_data_dir(),
        };
        Self::with_data_dir(data_dir)
    }

    /// Build the configuration for an explicit data directory.
    pub fn with_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        let db_path = data_dir.join(DB_FILE_NAME);
        let endpoint = endpoint_for(&data_dir);
        Self {
            data_dir,
            db_path,
            endpoint,
            net: crate::net::NetOptions::from_env(),
        }
    }

    /// Override the networking options, used by tests.
    pub fn with_net_options(mut self, net: crate::net::NetOptions) -> Self {
        self.net = net;
        self
    }

    /// Create the data directory if it does not exist yet.
    pub fn ensure_data_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data_dir)
    }
}

/// Platform specific default location for application data.
fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("reason-network");
        }
        let profile = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        profile
            .join("AppData")
            .join("Roaming")
            .join("reason-network")
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(xdg).join("reason-network");
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".local").join("share").join("reason-network")
    }
}

/// Derive a per-data-directory endpoint so parallel test runs never collide.
fn endpoint_for(data_dir: &Path) -> Endpoint {
    #[cfg(windows)]
    {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let canonical = data_dir.to_string_lossy().to_lowercase();
        let mut hasher = DefaultHasher::new();
        canonical.hash(&mut hasher);
        let digest = hasher.finish();
        Endpoint::NamedPipe(format!(r"\\.\pipe\reason-network-{digest:016x}"))
    }
    #[cfg(not(windows))]
    {
        Endpoint::UnixSocket(data_dir.join("daemon.sock"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_override_changes_database_and_endpoint() {
        let config = Config::with_data_dir("/tmp/rn-test-dir");
        assert!(config.db_path.ends_with(DB_FILE_NAME));
        assert_ne!(
            config.endpoint,
            Config::with_data_dir("/tmp/rn-other").endpoint
        );
    }

    #[test]
    fn same_data_dir_yields_same_endpoint() {
        assert_eq!(
            Config::with_data_dir("/tmp/rn-same").endpoint,
            Config::with_data_dir("/tmp/rn-same").endpoint
        );
    }
}
