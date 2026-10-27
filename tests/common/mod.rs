//! Shared helpers for the integration tests.
//!
//! Every test gets its own temporary application data directory, so the suite
//! never touches real user data and never collides with a running daemon.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use reason_network::config::Config;
use reason_network::daemon::Daemon;
use reason_network::error::Error;
use reason_network::ipc;
use reason_network::protocol::{Method, Request, Response};
use tempfile::TempDir;

/// A daemon running on a temporary data directory.
pub struct TestDaemon {
    /// The configuration the daemon runs with.
    pub config: Config,
    /// The daemon itself.
    pub daemon: Arc<Daemon>,
    /// Keeps the temporary directory alive for the duration of the test.
    dir: Arc<TempDir>,
}

impl TestDaemon {
    /// Start a daemon on a fresh temporary directory.
    ///
    /// Networking is enabled but offline: direct connections only, no relay
    /// and no discovery, so tests never touch the network.
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        Self::start_in(dir).await
    }

    /// Start a daemon on an existing temporary directory.
    pub async fn start_in(dir: TempDir) -> Self {
        let dir = Arc::new(dir);
        let config = Config::with_data_dir(dir.path())
            .with_net_options(reason_network::net::NetOptions::offline());
        Self::start_with_config(config, dir).await
    }

    /// Start a daemon with an explicit configuration.
    pub async fn start_with_config(config: Config, dir: Arc<TempDir>) -> Self {
        let daemon = Daemon::new(config.clone()).expect("daemon");
        let runner = Arc::clone(&daemon);
        tokio::spawn(async move {
            let _ = runner.serve().await;
        });
        let test = TestDaemon {
            config,
            daemon,
            dir,
        };
        test.wait_until_ready().await;
        test
    }

    /// Wait until the daemon answers requests.
    pub async fn wait_until_ready(&self) {
        for _ in 0..100 {
            let request = Request {
                id: Some("ping".into()),
                method: Method::Status,
                params: serde_json::json!({}),
            };
            if ipc::request_once::<_, Response>(&self.config.endpoint, &request)
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("daemon did not become ready");
    }

    /// Send a request to the daemon over the real local IPC interface.
    pub async fn call(
        &self,
        method: Method,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Error> {
        let request = Request {
            id: Some("test".into()),
            method,
            params,
        };
        let response: Response = ipc::request_once(&self.config.endpoint, &request).await?;
        match (response.ok, response.result, response.error) {
            (true, Some(result), _) => Ok(result),
            (true, None, _) => Ok(serde_json::Value::Null),
            (false, _, Some(error)) => {
                Err(Error::Other(format!("{}: {}", error.code, error.message)))
            }
            (false, _, None) => Err(Error::Other("daemon returned no result".into())),
        }
    }

    /// Stop the daemon and wait for it to release the endpoint.
    pub async fn stop(&self) {
        self.daemon.stop();
        for _ in 0..100 {
            if ipc::connect(&self.config.endpoint).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Restart the daemon on the very same data directory.
    pub async fn restart(&self) -> TestDaemon {
        self.stop().await;
        let config = Config::with_data_dir(self.config.data_dir.clone())
            .with_net_options(self.config.net.clone());
        let daemon = Daemon::new(config.clone()).expect("daemon");
        let runner = Arc::clone(&daemon);
        tokio::spawn(async move {
            let _ = runner.serve().await;
        });
        let restarted = TestDaemon {
            config,
            daemon,
            dir: Arc::clone(&self.dir),
        };
        restarted.wait_until_ready().await;
        restarted
    }

    /// The data directory of this daemon.
    pub fn data_dir(&self) -> &Path {
        &self.config.data_dir
    }

    /// Machine identity and network status.
    pub async fn net_info(&self) -> serde_json::Value {
        self.call(Method::NetInfo, serde_json::json!({}))
            .await
            .expect("net info")
    }

    /// This machine's peer id.
    pub async fn peer_id(&self) -> String {
        self.net_info().await["peer_id"]
            .as_str()
            .expect("peer id")
            .to_string()
    }

    /// This machine's dialable address as JSON.
    pub async fn addr_json(&self) -> String {
        let info = self.net_info().await;
        info["addr"].to_string()
    }

    /// Poll `check` until it returns true, or panic after `timeout`.
    pub async fn wait_for<F, Fut>(&self, what: &str, timeout: Duration, mut check: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if check().await {
                return;
            }
            if std::time::Instant::now() > deadline {
                panic!("timed out waiting for {what}");
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
}
