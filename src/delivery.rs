//! Push delivery into a live agent context.
//!
//! M0 supports exactly one provider integration: the [CC] CLI. When a session
//! is registered with `agent_type = "claude"` the daemon starts a headless
//! [CC] session in streaming JSON mode and writes inbound messages straight
//! into that session's context. This is genuine delivery into the agent's
//! conversation, not an MCP notification.
//!
//! Every other session type uses the polling path (`check_inbox`).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use crate::config::ENV_CLAUDE_BIN;
use crate::error::{Error, Result};
use crate::store::{Message, Session};

/// An event produced by a live agent session.
#[derive(Debug, Clone)]
pub enum ClaudeEvent {
    /// The agent finished a turn. `reply_to` is the session the answer
    /// belongs to, when the turn was triggered by an inbound message.
    TurnComplete {
        /// Session key of the agent.
        session_key: String,
        /// Session key of the original sender, if any.
        reply_to: Option<String>,
        /// The agent's answer.
        text: String,
    },
    /// The agent session failed.
    Failed {
        /// Session key of the agent.
        session_key: String,
        /// Reason of the failure.
        error: String,
    },
    /// The agent session reported its underlying conversation id.
    SessionId {
        /// Session key of the agent.
        session_key: String,
        /// The [CC] conversation id.
        conversation_id: String,
    },
}

/// A running [CC] session owned by the daemon.
struct ClaudeSession {
    child: Child,
    stdin: ChildStdin,
}

/// Owns the live [CC] sessions and routes messages into them.
pub struct ClaudeBridge {
    binary: PathBuf,
    sessions: HashMap<String, ClaudeSession>,
    events_tx: mpsc::UnboundedSender<ClaudeEvent>,
    /// Senders waiting for an answer, per agent session.
    pending: Arc<Mutex<HashMap<String, VecDeque<String>>>>,
    /// Number of live agent sessions, shared with the daemon for diagnostics.
    live: Arc<AtomicUsize>,
}

impl ClaudeBridge {
    /// Create a bridge using the configured `claude` binary.
    ///
    /// The returned receiver yields [`ClaudeEvent`]s produced by live agents.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<ClaudeEvent>) {
        let binary = std::env::var_os(ENV_CLAUDE_BIN)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("claude"));
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        (
            Self {
                binary,
                sessions: HashMap::new(),
                events_tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                live: Arc::new(AtomicUsize::new(0)),
            },
            events_rx,
        )
    }

    /// A counter of live agent sessions, readable from outside the bridge.
    pub fn live_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.live)
    }

    /// Number of live agent sessions owned by the bridge.
    pub fn live_session_count(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// True when the session is eligible for push delivery.
    pub fn supports(session: &Session) -> bool {
        session
            .agent_type
            .as_deref()
            .map(|kind| kind.eq_ignore_ascii_case("claude"))
            .unwrap_or(false)
    }

    /// Deliver a message into the agent's live context.
    pub async fn deliver(&mut self, session: &Session, message: &Message) -> Result<()> {
        let key = session.session_key.clone();
        self.drop_dead_session(&key);
        if !self.sessions.contains_key(&key) {
            self.spawn(session).await?;
        }

        let text = format!("Message from {}:\n\n{}", message.sender_key, message.body);
        let payload = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [ { "type": "text", "text": text } ]
            }
        });

        let mut line = serde_json::to_string(&payload)?;
        line.push('\n');

        let outcome = {
            let session = self
                .sessions
                .get_mut(&key)
                .ok_or_else(|| Error::Delivery(format!("no live agent session for {key}")))?;
            match session.stdin.write_all(line.as_bytes()).await {
                Ok(()) => session.stdin.flush().await,
                Err(error) => Err(error),
            }
        };

        if let Err(error) = outcome {
            // The agent is gone; forget it so the next attempt starts a new one.
            self.remove_session(&key);
            return Err(Error::Delivery(format!(
                "cannot write to the agent session {key}: {error}"
            )));
        }

        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(key.clone())
            .or_default()
            .push_back(message.sender_key.clone());
        eprintln!("[daemon] pushed message {} into agent {key}", message.id);
        Ok(())
    }

    /// Forget an agent session whose process has exited.
    fn drop_dead_session(&mut self, key: &str) {
        let dead = match self.sessions.get_mut(key) {
            Some(session) => matches!(session.child.try_wait(), Ok(Some(_))),
            None => false,
        };
        if dead {
            self.remove_session(key);
        }
    }

    /// Forget an agent session and stop its process.
    fn remove_session(&mut self, key: &str) {
        if let Some(mut session) = self.sessions.remove(key) {
            let _ = session.child.start_kill();
            self.live.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Start a headless [CC] session for the given agent session.
    async fn spawn(&mut self, session: &Session) -> Result<()> {
        let mut command = Command::new(&self.binary);
        command
            .arg("-p")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(conversation) = session.agent_ref.as_deref().filter(|c| !c.is_empty()) {
            command.arg("--resume").arg(conversation);
        }
        if let Some(dir) = session
            .working_dir
            .as_deref()
            .filter(|d| !d.is_empty())
            .filter(|d| std::path::Path::new(d).is_dir())
        {
            command.current_dir(dir);
        }

        let mut child = command.spawn().map_err(|error| {
            Error::Delivery(format!("cannot start `{}`: {error}", self.binary.display()))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Delivery("agent stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Delivery("agent stdout unavailable".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Delivery("agent stderr unavailable".into()))?;

        let key = session.session_key.clone();
        let events_tx = self.events_tx.clone();
        let pending = Arc::clone(&self.pending);
        let stderr_key = key.clone();

        // Forward the agent's stderr so failures are visible in the daemon log.
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    eprintln!("[agent {stderr_key}] {line}");
                }
            }
        });

        // Read the agent's stream and turn it into delivery events.
        let reader_key = key.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut accumulated = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match value.get("type").and_then(Value::as_str) {
                    Some("system") => {
                        // Only the `init` message carries the conversation id we
                        // want to remember. Other system messages (hooks, API
                        // retries) repeat the same id.
                        if value.get("subtype").and_then(Value::as_str) == Some("init")
                            && let Some(conversation_id) =
                                value.get("session_id").and_then(Value::as_str)
                        {
                            let _ = events_tx.send(ClaudeEvent::SessionId {
                                session_key: reader_key.clone(),
                                conversation_id: conversation_id.to_string(),
                            });
                        }
                    }
                    Some("assistant") => {
                        if let Some(text) = extract_text(&value) {
                            accumulated.push_str(&text);
                        }
                    }
                    Some("result") => {
                        let mut text = value
                            .get("result")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_default();
                        if text.trim().is_empty() {
                            text = accumulated.clone();
                        }
                        let reply_to = pending
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .get_mut(&reader_key)
                            .and_then(|queue| queue.pop_front());
                        let _ = events_tx.send(ClaudeEvent::TurnComplete {
                            session_key: reader_key.clone(),
                            reply_to,
                            text,
                        });
                        accumulated.clear();
                    }
                    _ => {}
                }
            }
            let _ = events_tx.send(ClaudeEvent::Failed {
                session_key: reader_key.clone(),
                error: "agent session ended".to_string(),
            });
        });

        self.sessions.insert(key, ClaudeSession { child, stdin });
        self.live.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Stop every live agent session.
    pub async fn shutdown(&mut self) {
        for (_, mut session) in self.sessions.drain() {
            let _ = session.child.start_kill();
        }
        self.live.store(0, Ordering::Relaxed);
    }
}

/// Extract the plain text of an assistant message.
fn extract_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    let mut text = String::new();
    if let Some(blocks) = content.as_array() {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("text")
                && let Some(part) = block.get("text").and_then(Value::as_str)
            {
                text.push_str(part);
            }
        }
    } else if let Some(part) = content.as_str() {
        text.push_str(part);
    }
    if text.is_empty() { None } else { Some(text) }
}
