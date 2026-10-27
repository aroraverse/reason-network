//! Local IPC transport.
//!
//! * Windows: named pipes (`\\.\pipe\reason-network-<hash>`).
//! * Unix: domain sockets inside the data directory.
//!
//! The wire format is newline-delimited JSON: one request object per line,
//! one response object per line. No network port is ever opened.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

use crate::config::Endpoint;
use crate::error::{Error, Result};

/// Maximum size of a single protocol line (1 MiB).
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

type DynStream = Box<dyn Stream>;

trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> Stream for T {}

/// A bidirectional local connection.
pub struct Connection {
    reader: BufReader<ReadHalf<DynStream>>,
    writer: WriteHalf<DynStream>,
}

impl Connection {
    fn new(inner: DynStream) -> Self {
        let (reader, writer) = tokio::io::split(inner);
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }

    /// Read one JSON frame. Returns `None` when the peer closed the connection.
    pub async fn read_frame<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(None);
        }
        if line.len() > MAX_LINE_BYTES {
            return Err(Error::Ipc(
                "protocol line exceeds the maximum length".into(),
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(trimmed)?))
    }

    /// Write one JSON frame followed by a newline.
    pub async fn write_frame<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Write raw bytes. Used by tests that exercise malformed input.
    pub async fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

/// Number of named pipe instances kept ready for clients.
#[cfg(windows)]
const PIPE_INSTANCES: usize = 16;

/// Server side listener.
pub struct Listener {
    #[cfg(windows)]
    pipe_name: String,
    #[cfg(windows)]
    incoming: tokio::sync::mpsc::UnboundedReceiver<Connection>,
    #[cfg(windows)]
    incoming_tx: tokio::sync::mpsc::UnboundedSender<Connection>,
    #[cfg(windows)]
    armed: Vec<tokio::net::windows::named_pipe::NamedPipeServer>,
    #[cfg(unix)]
    listener: tokio::net::UnixListener,
}

impl Listener {
    /// Bind the local endpoint.
    ///
    /// On Windows a pool of pipe instances is created and armed, so several
    /// clients can be served at the same time. The first instance is created
    /// with `first_pipe_instance`, which makes a second daemon on the same
    /// data directory fail fast instead of competing for clients.
    pub fn bind(endpoint: &Endpoint) -> Result<Self> {
        match endpoint {
            #[cfg(windows)]
            Endpoint::NamedPipe(name) => {
                let (incoming_tx, incoming) = tokio::sync::mpsc::unbounded_channel();
                let mut armed = Vec::with_capacity(PIPE_INSTANCES);
                for index in 0..PIPE_INSTANCES {
                    let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
                    options.first_pipe_instance(index == 0);
                    let instance = options.create(name).map_err(|error| {
                        Error::Ipc(format!(
                            "cannot create named pipe {name}: {error} (a daemon may already be running)"
                        ))
                    })?;
                    armed.push(instance);
                }
                let mut listener = Listener {
                    pipe_name: name.clone(),
                    incoming,
                    incoming_tx,
                    armed,
                };
                listener.arm();
                Ok(listener)
            }
            #[cfg(unix)]
            Endpoint::UnixSocket(path) => {
                if path.exists() {
                    // Remove a socket left behind by a crashed daemon.
                    let _ = std::fs::remove_file(path);
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let listener = tokio::net::UnixListener::bind(path)?;
                Ok(Listener { listener })
            }
        }
    }

    /// Wait for the next client.
    pub async fn accept(&mut self) -> Result<Connection> {
        match self {
            #[cfg(windows)]
            Listener {
                pipe_name,
                incoming,
                incoming_tx,
                armed,
            } => {
                let connection = incoming
                    .recv()
                    .await
                    .ok_or_else(|| Error::Ipc("named pipe listener stopped".into()))?;

                // Replace the instance that was just consumed so the pool keeps
                // its size.
                let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
                options.first_pipe_instance(false);
                let instance = options.create(pipe_name.as_str()).map_err(|error| {
                    Error::Ipc(format!("cannot create named pipe instance: {error}"))
                })?;
                armed.push(instance);
                let _ = incoming_tx;
                self.arm();
                Ok(connection)
            }
            #[cfg(unix)]
            Listener { listener } => {
                let (stream, _) = listener.accept().await?;
                Ok(Connection::new(Box::new(stream)))
            }
        }
    }

    /// Wait for a client on every not-yet-armed pipe instance.
    #[cfg(windows)]
    fn arm(&mut self) {
        let instances = std::mem::take(&mut self.armed);
        for instance in instances {
            let sender = self.incoming_tx.clone();
            tokio::spawn(async move {
                if instance.connect().await.is_ok() {
                    let _ = sender.send(Connection::new(Box::new(instance)));
                }
            });
        }
    }
}

/// Connect to a running daemon.
pub async fn connect(endpoint: &Endpoint) -> Result<Connection> {
    match endpoint {
        #[cfg(windows)]
        Endpoint::NamedPipe(name) => {
            let client = tokio::net::windows::named_pipe::ClientOptions::new()
                .open(name)
                .map_err(|error| {
                    Error::DaemonUnavailable(format!(
                        "cannot reach daemon at {name}: {error} (is `reason-network daemon-start` running?)"
                    ))
                })?;
            Ok(Connection::new(Box::new(client)))
        }
        #[cfg(unix)]
        Endpoint::UnixSocket(path) => {
            let stream = tokio::net::UnixStream::connect(path).await.map_err(|error| {
                Error::DaemonUnavailable(format!(
                    "cannot reach daemon at {}: {error} (is `reason-network daemon-start` running?)",
                    path.display()
                ))
            })?;
            Ok(Connection::new(Box::new(stream)))
        }
    }
}

/// Send one request over a fresh connection and return the response.
pub async fn request_once<T: Serialize, R: DeserializeOwned>(
    endpoint: &Endpoint,
    request: &T,
) -> Result<R> {
    let mut connection = connect(endpoint).await?;
    connection.write_frame(request).await?;
    match connection.read_frame().await? {
        Some(response) => Ok(response),
        None => Err(Error::Ipc(
            "daemon closed the connection without a reply".into(),
        )),
    }
}
