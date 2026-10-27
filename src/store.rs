//! SQLite persistence. This module is the *only* place that touches the
//! database; the daemon owns a single connection and serialises access to it.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::protocol::{
    AnswerQuestionParams, AskUserParams, InboxParams, MarkReadParams, RegisterParams,
    SendMessageParams, UpdateParams,
};

/// Maximum accepted message body size (64 KiB).
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// A registered agent session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    /// Stable session key used for routing.
    pub session_key: String,
    /// Human readable name, unique among live sessions.
    pub name: String,
    /// Agent type, e.g. `claude`.
    pub agent_type: Option<String>,
    /// Provider specific handle used for push delivery.
    pub agent_ref: Option<String>,
    /// Working directory of the agent.
    pub working_dir: Option<String>,
    /// Repository the agent works in.
    pub repo: Option<String>,
    /// Branch the agent works on.
    pub branch: Option<String>,
    /// Current status.
    pub status: String,
    /// Short activity description.
    pub activity: String,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Last time the session was seen (unix seconds).
    pub last_seen: i64,
}

/// A message between two sessions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Message {
    /// Unique message id.
    pub id: String,
    /// Sending session key.
    pub sender_key: String,
    /// Receiving session key.
    pub recipient_key: String,
    /// Verbatim message body. Never interpreted, never executed.
    pub body: String,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// `unread` or `read`.
    pub state: String,
    /// `1` once the message has been handed to a push delivery target.
    pub delivered: bool,
}

/// A question an agent asked a human.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Question {
    /// Unique question id.
    pub id: String,
    /// Session that asked.
    pub session_key: String,
    /// Question text.
    pub question: String,
    /// `pending` or `answered`.
    pub status: String,
    /// The human's answer, when present.
    pub answer: Option<String>,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Answer time (unix seconds).
    pub answered_at: Option<i64>,
}

/// A team this machine belongs to.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Team {
    /// Stable team identifier.
    pub team_id: String,
    /// Display name.
    pub name: String,
    /// Peer id of the admin that signs membership records.
    pub admin_peer: String,
    /// Creation time (unix seconds).
    pub created_at: i64,
}

/// A stored membership record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredMember {
    /// Team the member belongs to.
    pub team_id: String,
    /// Peer id of the member machine.
    pub peer_id: String,
    /// Display name of the member machine.
    pub member_name: String,
    /// The signed record as JSON.
    pub record_json: String,
}

/// A stored revocation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredRevocation {
    /// Team the revocation applies to.
    pub team_id: String,
    /// Revoked peer id.
    pub peer_id: String,
    /// The signed record as JSON.
    pub record_json: String,
    /// When it was stored locally.
    pub revoked_at: i64,
}

/// A known peer machine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerRecord {
    /// Peer id.
    pub peer_id: String,
    /// Optional label.
    pub label: Option<String>,
    /// Last known dialable address (JSON encoded iroh `EndpointAddr`).
    pub addr_json: Option<String>,
    /// `connected`, `idle` or `unknown`.
    pub status: String,
    /// Last time the peer was seen.
    pub last_seen: Option<i64>,
}

/// A session hosted by another machine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoteSession {
    /// Session key.
    pub session_key: String,
    /// Machine hosting the session.
    pub peer_id: String,
    /// Session name.
    pub name: String,
    /// Agent type.
    pub agent_type: Option<String>,
    /// Current status.
    pub status: String,
    /// Short activity description.
    pub activity: String,
    /// Last seen time reported by the owner.
    pub last_seen: i64,
}

/// An outgoing message waiting for or awaiting acknowledgement.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OutboundMessage {
    /// Local message id, reused on every retry.
    pub id: String,
    /// Sending session.
    pub sender_key: String,
    /// Receiving session.
    pub recipient_key: String,
    /// Body.
    pub body: String,
    /// Creation time.
    pub created_at: i64,
    /// Destination machine.
    pub peer_id: Option<String>,
}

/// The SQLite store. Not thread safe on its own; the daemon wraps it in a mutex.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (or create) the database at `path` and apply the schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open an in-memory database. Useful for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON").ok();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                session_key TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                agent_type  TEXT,
                agent_ref   TEXT,
                working_dir TEXT,
                repo        TEXT,
                branch      TEXT,
                status      TEXT NOT NULL DEFAULT 'idle',
                activity    TEXT NOT NULL DEFAULT '',
                created_at  INTEGER NOT NULL,
                last_seen   INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS messages (
                id            TEXT PRIMARY KEY,
                sender_key    TEXT NOT NULL,
                recipient_key TEXT NOT NULL,
                body          TEXT NOT NULL,
                created_at    INTEGER NOT NULL,
                state         TEXT NOT NULL DEFAULT 'unread',
                delivered     INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_messages_recipient
                ON messages (recipient_key, state, created_at);

            CREATE TABLE IF NOT EXISTS questions (
                id         TEXT PRIMARY KEY,
                session_key TEXT NOT NULL,
                question   TEXT NOT NULL,
                status     TEXT NOT NULL DEFAULT 'pending',
                answer     TEXT,
                created_at INTEGER NOT NULL,
                answered_at INTEGER
            );
            "#,
        )?;

        // ---- Milestone 1: teams, peers, remote sessions, remote messages ----

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS teams (
                team_id    TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                admin_peer TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS team_members (
                team_id     TEXT NOT NULL,
                peer_id     TEXT NOT NULL,
                member_name TEXT NOT NULL,
                record_json TEXT NOT NULL,
                added_at    INTEGER NOT NULL,
                PRIMARY KEY (team_id, peer_id)
            );

            CREATE TABLE IF NOT EXISTS used_invites (
                nonce   TEXT PRIMARY KEY,
                team_id TEXT NOT NULL,
                used_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS revocations (
                team_id     TEXT NOT NULL,
                peer_id     TEXT NOT NULL,
                record_json TEXT NOT NULL,
                revoked_at  INTEGER NOT NULL,
                PRIMARY KEY (team_id, peer_id)
            );

            CREATE TABLE IF NOT EXISTS remote_sessions (
                session_key TEXT PRIMARY KEY,
                peer_id     TEXT NOT NULL,
                name        TEXT NOT NULL,
                agent_type  TEXT,
                status      TEXT NOT NULL DEFAULT 'idle',
                activity    TEXT NOT NULL DEFAULT '',
                last_seen   INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS peers (
                peer_id   TEXT PRIMARY KEY,
                label     TEXT,
                addr_json TEXT,
                status    TEXT NOT NULL DEFAULT 'unknown',
                last_seen INTEGER
            );

            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )?;

        // Columns added to the M0 messages table. Existing databases are
        // migrated in place; new databases get them here.
        add_column_if_missing(&conn, "messages", "origin", "TEXT NOT NULL DEFAULT 'local'")?;
        add_column_if_missing(&conn, "messages", "peer_id", "TEXT")?;
        add_column_if_missing(&conn, "messages", "remote_id", "TEXT")?;
        add_column_if_missing(
            &conn,
            "messages",
            "delivery_state",
            "TEXT NOT NULL DEFAULT 'local'",
        )?;
        conn.execute_batch(
            r#"
            CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_remote_dedupe
                ON messages (peer_id, remote_id) WHERE remote_id IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_messages_outbound
                ON messages (delivery_state, peer_id);
            "#,
        )?;

        Ok(Store { conn })
    }

    /// Register or refresh a session.
    ///
    /// When a session key is supplied the existing row is updated in place,
    /// which keeps the identity stable across agent restarts. Session names
    /// are made unique by appending a numeric suffix.
    pub fn register(&mut self, params: RegisterParams) -> Result<Session> {
        let name = params.name.trim();
        if name.is_empty() {
            return Err(Error::InvalidRequest(
                "session name must not be empty".into(),
            ));
        }

        let session_key = match params.session_key.as_deref().map(str::trim) {
            Some(key) if !key.is_empty() => key.to_string(),
            _ => new_id("sess"),
        };

        let now = now_seconds();
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT created_at FROM sessions WHERE session_key = ?1",
                params![session_key],
                |row| row.get(0),
            )
            .optional()?;

        let created_at = existing.unwrap_or(now);
        let unique_name = self.unique_name(name, &session_key)?;

        self.conn.execute(
            r#"
            INSERT INTO sessions (
                session_key, name, agent_type, agent_ref, working_dir, repo, branch,
                status, activity, created_at, last_seen
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(session_key) DO UPDATE SET
                name        = excluded.name,
                agent_type  = COALESCE(excluded.agent_type, sessions.agent_type),
                agent_ref   = COALESCE(excluded.agent_ref, sessions.agent_ref),
                working_dir = COALESCE(excluded.working_dir, sessions.working_dir),
                repo        = COALESCE(excluded.repo, sessions.repo),
                branch      = COALESCE(excluded.branch, sessions.branch),
                status      = excluded.status,
                activity    = excluded.activity,
                last_seen   = excluded.last_seen
            "#,
            params![
                session_key,
                unique_name,
                params.agent_type,
                params.agent_ref,
                params.working_dir,
                params.repo,
                params.branch,
                params.status.unwrap_or_else(|| "idle".to_string()),
                params.activity.unwrap_or_default(),
                created_at,
                now,
            ],
        )?;

        self.get_session(&session_key)?
            .ok_or_else(|| Error::NotFound(format!("session {session_key}")))
    }

    /// Make sure no two sessions share a name.
    fn unique_name(&self, name: &str, session_key: &str) -> Result<String> {
        let taken: Vec<String> = self
            .conn
            .prepare("SELECT session_key FROM sessions WHERE name = ?1")
            .and_then(|mut stmt| {
                stmt.query_map(params![name], |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()
            })
            .unwrap_or_default();

        // The name stays as it is when no *other* session holds it.
        if taken.iter().all(|key| key == session_key) {
            return Ok(name.to_string());
        }
        let mut suffix = 2;
        loop {
            let candidate = format!("{name}-{suffix}");
            let conflict: Option<String> = self
                .conn
                .query_row(
                    "SELECT session_key FROM sessions WHERE name = ?1",
                    params![candidate],
                    |row| row.get(0),
                )
                .optional()?;
            if conflict.as_deref() != Some(session_key) {
                return Ok(candidate);
            }
            suffix += 1;
        }
    }

    /// Fetch a single session.
    pub fn get_session(&self, session_key: &str) -> Result<Option<Session>> {
        Ok(self
            .conn
            .query_row(
                "SELECT session_key, name, agent_type, agent_ref, working_dir, repo, branch,
                        status, activity, created_at, last_seen
                 FROM sessions WHERE session_key = ?1",
                params![session_key],
                row_to_session,
            )
            .optional()?)
    }

    /// List every registered session, newest first.
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_key, name, agent_type, agent_ref, working_dir, repo, branch,
                    status, activity, created_at, last_seen
             FROM sessions ORDER BY created_at DESC, name ASC",
        )?;
        let sessions = stmt
            .query_map([], row_to_session)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(sessions)
    }

    /// Update the mutable fields of a session.
    pub fn update(&mut self, params: UpdateParams) -> Result<Session> {
        let key = params.session_key.trim();
        if key.is_empty() {
            return Err(Error::InvalidRequest(
                "session_key must not be empty".into(),
            ));
        }
        if self.get_session(key)?.is_none() {
            return Err(Error::NotFound(format!("session {key}")));
        }

        if let Some(status) = params.status {
            self.conn.execute(
                "UPDATE sessions SET status = ?2, last_seen = ?3 WHERE session_key = ?1",
                params![key, status, now_seconds()],
            )?;
        }
        if let Some(activity) = params.activity {
            self.conn.execute(
                "UPDATE sessions SET activity = ?2, last_seen = ?3 WHERE session_key = ?1",
                params![key, activity, now_seconds()],
            )?;
        }
        if let Some(agent_ref) = params.agent_ref {
            self.conn.execute(
                "UPDATE sessions SET agent_ref = ?2, last_seen = ?3 WHERE session_key = ?1",
                params![key, agent_ref, now_seconds()],
            )?;
        }

        self.get_session(key)?
            .ok_or_else(|| Error::NotFound(format!("session {key}")))
    }

    /// Touch the `last_seen` timestamp of a session.
    pub fn touch(&mut self, session_key: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_seen = ?2 WHERE session_key = ?1",
            params![session_key, now_seconds()],
        )?;
        Ok(())
    }

    /// Remove a session. Messages addressed to it are kept for history.
    pub fn remove_session(&mut self, session_key: &str) -> Result<bool> {
        let affected = self.conn.execute(
            "DELETE FROM sessions WHERE session_key = ?1",
            params![session_key],
        )?;
        Ok(affected > 0)
    }

    /// Store a message after validating both endpoints.
    ///
    /// The body is stored verbatim. It is never parsed, never executed and
    /// never treated as an instruction by the daemon.
    pub fn send_message(&mut self, params: SendMessageParams) -> Result<Message> {
        let sender = params.sender_key.trim();
        let recipient = params.recipient_key.trim();
        if sender.is_empty() || recipient.is_empty() {
            return Err(Error::InvalidRequest(
                "sender_key and recipient_key must not be empty".into(),
            ));
        }
        if sender == recipient {
            return Err(Error::InvalidRequest(
                "a session cannot send messages to itself".into(),
            ));
        }
        if params.body.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "message body must not be empty".into(),
            ));
        }
        if params.body.len() > MAX_BODY_BYTES {
            return Err(Error::InvalidRequest(format!(
                "message body exceeds {MAX_BODY_BYTES} bytes"
            )));
        }
        if self.get_session(sender)?.is_none() {
            return Err(Error::NotFound(format!("sender session {sender}")));
        }
        if self.get_session(recipient)?.is_none() {
            return Err(Error::NotFound(format!("recipient session {recipient}")));
        }

        let message = Message {
            id: new_id("msg"),
            sender_key: sender.to_string(),
            recipient_key: recipient.to_string(),
            body: params.body,
            created_at: now_seconds(),
            state: "unread".to_string(),
            delivered: false,
        };

        self.conn.execute(
            "INSERT INTO messages (id, sender_key, recipient_key, body, created_at, state, delivered)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                message.id,
                message.sender_key,
                message.recipient_key,
                message.body,
                message.created_at,
                message.state,
                i64::from(message.delivered),
            ],
        )?;

        Ok(message)
    }

    /// Fetch a session's inbox, oldest message first.
    pub fn inbox(&mut self, params: InboxParams) -> Result<Vec<Message>> {
        if self.get_session(&params.session_key)?.is_none() {
            return Err(Error::NotFound(format!("session {}", params.session_key)));
        }
        let limit = params.limit.unwrap_or(50).clamp(1, 500) as i64;
        let unread_only = params.unread_only.unwrap_or(false);

        let mut stmt = self.conn.prepare(
            "SELECT id, sender_key, recipient_key, body, created_at, state, delivered
             FROM messages
             WHERE recipient_key = ?1 AND (?2 = 0 OR state = 'unread')
             ORDER BY created_at ASC, id ASC
             LIMIT ?3",
        )?;
        let messages = stmt
            .query_map(
                params![params.session_key, unread_only, limit],
                row_to_message,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    /// Count unread messages of a session.
    pub fn unread_count(&mut self, session_key: &str) -> Result<usize> {
        if self.get_session(session_key)?.is_none() {
            return Err(Error::NotFound(format!("session {session_key}")));
        }
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE recipient_key = ?1 AND state = 'unread'",
            params![session_key],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Mark messages as read. Without ids every message of the session is marked.
    pub fn mark_read(&mut self, params: MarkReadParams) -> Result<usize> {
        if self.get_session(&params.session_key)?.is_none() {
            return Err(Error::NotFound(format!("session {}", params.session_key)));
        }
        match params.message_ids {
            Some(ids) if !ids.is_empty() => {
                let mut updated = 0usize;
                for id in ids {
                    updated += self.conn.execute(
                        "UPDATE messages SET state = 'read'
                         WHERE id = ?1 AND recipient_key = ?2 AND state = 'unread'",
                        params![id, params.session_key],
                    )?;
                }
                Ok(updated)
            }
            _ => {
                let updated = self.conn.execute(
                    "UPDATE messages SET state = 'read'
                     WHERE recipient_key = ?1 AND state = 'unread'",
                    params![params.session_key],
                )?;
                Ok(updated)
            }
        }
    }

    /// Messages that still need to be handed to a push delivery target.
    pub fn undelivered_messages(&self) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, sender_key, recipient_key, body, created_at, state, delivered
             FROM messages WHERE delivered = 0 ORDER BY created_at ASC",
        )?;
        let messages = stmt
            .query_map([], row_to_message)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    /// Flag a message as delivered to its push target.
    pub fn mark_delivered(&mut self, message_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET delivered = 1 WHERE id = ?1",
            params![message_id],
        )?;
        Ok(())
    }

    /// Record a question for a human.
    pub fn ask_user(&mut self, params: AskUserParams) -> Result<Question> {
        if self.get_session(&params.session_key)?.is_none() {
            return Err(Error::NotFound(format!("session {}", params.session_key)));
        }
        if params.question.trim().is_empty() {
            return Err(Error::InvalidRequest("question must not be empty".into()));
        }
        let question = Question {
            id: new_id("q"),
            session_key: params.session_key,
            question: params.question,
            status: "pending".to_string(),
            answer: None,
            created_at: now_seconds(),
            answered_at: None,
        };
        self.conn.execute(
            "INSERT INTO questions (id, session_key, question, status, answer, created_at, answered_at)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, NULL)",
            params![
                question.id,
                question.session_key,
                question.question,
                question.status,
                question.created_at
            ],
        )?;
        Ok(question)
    }

    /// List questions, optionally filtered by status.
    pub fn list_questions(&self, status: Option<&str>) -> Result<Vec<Question>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_key, question, status, answer, created_at, answered_at
             FROM questions
             WHERE (?1 IS NULL OR status = ?1)
             ORDER BY created_at DESC",
        )?;
        let questions = stmt
            .query_map(params![status], row_to_question)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(questions)
    }

    /// Answer a question and deliver the answer to the asking session's inbox.
    pub fn answer_question(&mut self, params: AnswerQuestionParams) -> Result<Question> {
        let question = self
            .conn
            .query_row(
                "SELECT id, session_key, question, status, answer, created_at, answered_at
                 FROM questions WHERE id = ?1",
                params![params.question_id],
                row_to_question,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("question {}", params.question_id)))?;

        if question.status == "answered" {
            return Err(Error::InvalidRequest(format!(
                "question {} is already answered",
                question.id
            )));
        }

        let now = now_seconds();
        self.conn.execute(
            "UPDATE questions SET status = 'answered', answer = ?2, answered_at = ?3 WHERE id = ?1",
            params![question.id, params.answer, now],
        )?;

        // The answer reaches the agent as an ordinary inbox message.
        let message = Message {
            id: new_id("msg"),
            sender_key: "human".to_string(),
            recipient_key: question.session_key.clone(),
            body: params.answer.clone(),
            created_at: now,
            state: "unread".to_string(),
            delivered: false,
        };
        self.conn.execute(
            "INSERT INTO messages (id, sender_key, recipient_key, body, created_at, state, delivered)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                message.id,
                message.sender_key,
                message.recipient_key,
                message.body,
                message.created_at,
                message.state,
                i64::from(message.delivered),
            ],
        )?;

        Ok(Question {
            status: "answered".to_string(),
            answer: Some(params.answer),
            answered_at: Some(now),
            ..question
        })
    }

    /// Count of stored sessions and messages, used by diagnostics.
    pub fn counts(&self) -> Result<(usize, usize)> {
        let sessions: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        let messages: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
        Ok((sessions as usize, messages as usize))
    }

    // -----------------------------------------------------------------------
    // Milestone 1: teams, peers, remote sessions, remote messages
    // -----------------------------------------------------------------------

    /// Create (or refresh) a team this machine belongs to.
    pub fn upsert_team(&mut self, team: &Team) -> Result<()> {
        self.conn.execute(
            "INSERT INTO teams (team_id, name, admin_peer, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(team_id) DO UPDATE SET name = excluded.name, admin_peer = excluded.admin_peer",
            params![team.team_id, team.name, team.admin_peer, team.created_at],
        )?;
        Ok(())
    }

    /// Fetch one team.
    pub fn get_team(&self, team_id: &str) -> Result<Option<Team>> {
        Ok(self
            .conn
            .query_row(
                "SELECT team_id, name, admin_peer, created_at FROM teams WHERE team_id = ?1",
                params![team_id],
                |row| {
                    Ok(Team {
                        team_id: row.get(0)?,
                        name: row.get(1)?,
                        admin_peer: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// List every team this machine belongs to.
    pub fn list_teams(&self) -> Result<Vec<Team>> {
        let mut stmt = self.conn.prepare(
            "SELECT team_id, name, admin_peer, created_at FROM teams ORDER BY created_at",
        )?;
        let teams = stmt
            .query_map([], |row| {
                Ok(Team {
                    team_id: row.get(0)?,
                    name: row.get(1)?,
                    admin_peer: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(teams)
    }

    /// Remove a team and its membership, used by `team-leave`.
    pub fn delete_team(&mut self, team_id: &str) -> Result<bool> {
        let removed = self
            .conn
            .execute("DELETE FROM teams WHERE team_id = ?1", params![team_id])?;
        self.conn.execute(
            "DELETE FROM team_members WHERE team_id = ?1",
            params![team_id],
        )?;
        Ok(removed > 0)
    }

    /// Store a verified membership record.
    pub fn put_member(&mut self, record_json: &str, member: &StoredMember) -> Result<()> {
        self.conn.execute(
            "INSERT INTO team_members (team_id, peer_id, member_name, record_json, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(team_id, peer_id) DO UPDATE SET
                 member_name = excluded.member_name,
                 record_json = excluded.record_json",
            params![
                member.team_id,
                member.peer_id,
                member.member_name,
                record_json,
                now_seconds()
            ],
        )?;
        Ok(())
    }

    /// List the roster of a team.
    pub fn list_members(&self, team_id: &str) -> Result<Vec<StoredMember>> {
        let mut stmt = self.conn.prepare(
            "SELECT team_id, peer_id, member_name, record_json
             FROM team_members WHERE team_id = ?1 ORDER BY member_name",
        )?;
        let members = stmt
            .query_map(params![team_id], |row| {
                Ok(StoredMember {
                    team_id: row.get(0)?,
                    peer_id: row.get(1)?,
                    member_name: row.get(2)?,
                    record_json: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(members)
    }

    /// Every membership record this machine holds, across all teams.
    pub fn all_members(&self) -> Result<Vec<StoredMember>> {
        let mut stmt = self.conn.prepare(
            "SELECT team_id, peer_id, member_name, record_json FROM team_members ORDER BY team_id",
        )?;
        let members = stmt
            .query_map([], |row| {
                Ok(StoredMember {
                    team_id: row.get(0)?,
                    peer_id: row.get(1)?,
                    member_name: row.get(2)?,
                    record_json: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(members)
    }

    /// Remove one member from a team.
    pub fn delete_member(&mut self, team_id: &str, peer_id: &str) -> Result<bool> {
        let removed = self.conn.execute(
            "DELETE FROM team_members WHERE team_id = ?1 AND peer_id = ?2",
            params![team_id, peer_id],
        )?;
        Ok(removed > 0)
    }

    /// Persist a verified revocation and drop the member from the roster.
    ///
    /// Storing and removing the member happen together so a revoked peer is
    /// never advertised as a member once the revocation is known.
    pub fn put_revocation(&mut self, record: &StoredRevocation) -> Result<()> {
        let transaction = self.conn.transaction()?;
        transaction.execute(
            "INSERT INTO revocations (team_id, peer_id, record_json, revoked_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(team_id, peer_id) DO UPDATE SET
                 record_json = excluded.record_json,
                 revoked_at = excluded.revoked_at",
            params![
                record.team_id,
                record.peer_id,
                record.record_json,
                record.revoked_at
            ],
        )?;
        transaction.execute(
            "DELETE FROM team_members WHERE team_id = ?1 AND peer_id = ?2",
            params![record.team_id, record.peer_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// True when `peer_id` has been revoked from `team_id`.
    pub fn is_revoked(&self, team_id: &str, peer_id: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM revocations WHERE team_id = ?1 AND peer_id = ?2",
            params![team_id, peer_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// True when any local team has revoked `peer_id`.
    pub fn is_revoked_anywhere(&self, peer_id: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM revocations WHERE peer_id = ?1",
            params![peer_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Revocations known for one team.
    pub fn list_revocations(&self, team_id: &str) -> Result<Vec<StoredRevocation>> {
        let mut stmt = self.conn.prepare(
            "SELECT team_id, peer_id, record_json, revoked_at
             FROM revocations WHERE team_id = ?1 ORDER BY peer_id",
        )?;
        let records = stmt
            .query_map(params![team_id], row_to_revocation)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Every revocation this machine knows.
    pub fn all_revocations(&self) -> Result<Vec<StoredRevocation>> {
        let mut stmt = self.conn.prepare(
            "SELECT team_id, peer_id, record_json, revoked_at FROM revocations ORDER BY team_id",
        )?;
        let records = stmt
            .query_map([], row_to_revocation)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Consume an invitation nonce. Returns `false` when it was already used.
    pub fn consume_invite(&mut self, nonce: &str, team_id: &str) -> Result<bool> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO used_invites (nonce, team_id, used_at) VALUES (?1, ?2, ?3)",
            params![nonce, team_id, now_seconds()],
        )?;
        Ok(inserted > 0)
    }

    /// True when an invitation nonce has already been consumed.
    pub fn invite_used(&self, nonce: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM used_invites WHERE nonce = ?1",
            params![nonce],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Record a peer and how to reach it.
    pub fn upsert_peer(&mut self, peer: &PeerRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO peers (peer_id, label, addr_json, status, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(peer_id) DO UPDATE SET
                 label = COALESCE(excluded.label, peers.label),
                 addr_json = COALESCE(excluded.addr_json, peers.addr_json),
                 status = excluded.status,
                 last_seen = excluded.last_seen",
            params![
                peer.peer_id,
                peer.label,
                peer.addr_json,
                peer.status,
                peer.last_seen
            ],
        )?;
        Ok(())
    }

    /// Update the connection status of a known peer.
    pub fn set_peer_status(&mut self, peer_id: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE peers SET status = ?2, last_seen = ?3 WHERE peer_id = ?1",
            params![peer_id, status, now_seconds()],
        )?;
        Ok(())
    }

    /// List known peers.
    pub fn list_peers(&self) -> Result<Vec<PeerRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT peer_id, label, addr_json, status, last_seen FROM peers ORDER BY peer_id",
        )?;
        let peers = stmt
            .query_map([], |row| {
                Ok(PeerRecord {
                    peer_id: row.get(0)?,
                    label: row.get(1)?,
                    addr_json: row.get(2)?,
                    status: row.get(3)?,
                    last_seen: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(peers)
    }

    /// Replace the remote session table of one peer with a fresh snapshot.
    pub fn replace_remote_sessions(
        &mut self,
        peer_id: &str,
        sessions: &[RemoteSession],
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM remote_sessions WHERE peer_id = ?1",
            params![peer_id],
        )?;
        for session in sessions {
            self.conn.execute(
                "INSERT OR REPLACE INTO remote_sessions
                     (session_key, peer_id, name, agent_type, status, activity, last_seen, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    session.session_key,
                    peer_id,
                    session.name,
                    session.agent_type,
                    session.status,
                    session.activity,
                    session.last_seen,
                    now_seconds()
                ],
            )?;
        }
        Ok(())
    }

    /// Drop all sessions of a peer that is no longer reachable.
    pub fn clear_remote_sessions(&mut self, peer_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM remote_sessions WHERE peer_id = ?1",
            params![peer_id],
        )?;
        Ok(())
    }

    /// List synchronised sessions from other machines.
    pub fn list_remote_sessions(&self) -> Result<Vec<RemoteSession>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_key, peer_id, name, agent_type, status, activity, last_seen
             FROM remote_sessions ORDER BY name",
        )?;
        let sessions = stmt
            .query_map([], |row| {
                Ok(RemoteSession {
                    session_key: row.get(0)?,
                    peer_id: row.get(1)?,
                    name: row.get(2)?,
                    agent_type: row.get(3)?,
                    status: row.get(4)?,
                    activity: row.get(5)?,
                    last_seen: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(sessions)
    }

    /// Find which peer hosts a remote session.
    pub fn owner_of_session(&self, session_key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT peer_id FROM remote_sessions WHERE session_key = ?1",
                params![session_key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Store an outgoing remote message before it is sent.
    ///
    /// The row is the retry record: `delivery_state` moves from `pending` to
    /// `sent` and finally `acked`. The message id is reused on every retry, so
    /// the receiver can drop duplicates.
    pub fn queue_remote_message(
        &mut self,
        sender_key: &str,
        recipient_key: &str,
        body: &str,
        peer_id: &str,
    ) -> Result<Message> {
        if body.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "message body must not be empty".into(),
            ));
        }
        if body.len() > MAX_BODY_BYTES {
            return Err(Error::InvalidRequest(format!(
                "message body exceeds {MAX_BODY_BYTES} bytes"
            )));
        }
        if self.get_session(sender_key)?.is_none() {
            return Err(Error::NotFound(format!("sender session {sender_key}")));
        }
        let message = Message {
            id: new_id("msg"),
            sender_key: sender_key.to_string(),
            recipient_key: recipient_key.to_string(),
            body: body.to_string(),
            created_at: now_seconds(),
            state: "sent".to_string(),
            delivered: false,
        };
        self.conn.execute(
            "INSERT INTO messages
                 (id, sender_key, recipient_key, body, created_at, state, delivered,
                  origin, peer_id, remote_id, delivery_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 'local', ?7, NULL, 'pending')",
            params![
                message.id,
                message.sender_key,
                message.recipient_key,
                message.body,
                message.created_at,
                message.state,
                peer_id
            ],
        )?;
        Ok(message)
    }

    /// Outgoing messages that still need to reach a peer.
    pub fn pending_outbound(&self) -> Result<Vec<OutboundMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, sender_key, recipient_key, body, created_at, peer_id
             FROM messages
             WHERE delivery_state IN ('pending', 'sent') AND peer_id IS NOT NULL
             ORDER BY created_at ASC",
        )?;
        let messages = stmt
            .query_map([], |row| {
                Ok(OutboundMessage {
                    id: row.get(0)?,
                    sender_key: row.get(1)?,
                    recipient_key: row.get(2)?,
                    body: row.get(3)?,
                    created_at: row.get(4)?,
                    peer_id: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    /// Update the delivery state of an outgoing message.
    pub fn set_delivery_state(&mut self, message_id: &str, state: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET delivery_state = ?2 WHERE id = ?1",
            params![message_id, state],
        )?;
        Ok(())
    }

    /// Recent outbound messages with their delivery state, for diagnostics.
    pub fn list_outbound(&self, limit: u32) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, recipient_key, peer_id, delivery_state, created_at
             FROM messages WHERE peer_id IS NOT NULL
             ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "recipient_key": row.get::<_, String>(1)?,
                    "peer_id": row.get::<_, Option<String>>(2)?,
                    "delivery_state": row.get::<_, String>(3)?,
                    "created_at": row.get::<_, i64>(4)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Store a message received from another machine.
    ///
    /// Returns `Ok(None)` when the message was already stored, which is what
    /// makes retries safe: the receiver deduplicates on `(peer, remote_id)`.
    pub fn store_incoming_remote(
        &mut self,
        peer_id: &str,
        remote_id: &str,
        sender_key: &str,
        recipient_key: &str,
        body: &str,
        created_at: i64,
    ) -> Result<Option<Message>> {
        if body.len() > MAX_BODY_BYTES {
            return Err(Error::InvalidRequest(
                "message exceeds the size limit".into(),
            ));
        }
        if self.get_session(recipient_key)?.is_none() {
            return Err(Error::NotFound(format!(
                "recipient session {recipient_key}"
            )));
        }
        let message = Message {
            id: new_id("msg"),
            sender_key: sender_key.to_string(),
            recipient_key: recipient_key.to_string(),
            body: body.to_string(),
            created_at,
            state: "unread".to_string(),
            delivered: false,
        };
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO messages
                 (id, sender_key, recipient_key, body, created_at, state, delivered,
                  origin, peer_id, remote_id, delivery_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 'remote', ?7, ?8, 'local')",
            params![
                message.id,
                message.sender_key,
                message.recipient_key,
                message.body,
                message.created_at,
                message.state,
                peer_id,
                remote_id
            ],
        )?;
        if inserted == 0 {
            return Ok(None);
        }
        Ok(Some(message))
    }

    /// Sessions that must be advertised to peers.
    pub fn presence_snapshot(&self) -> Result<Vec<RemoteSession>> {
        let sessions = self.list_sessions()?;
        Ok(sessions
            .into_iter()
            .map(|session| RemoteSession {
                session_key: session.session_key,
                peer_id: String::new(),
                name: session.name,
                agent_type: session.agent_type,
                status: session.status,
                activity: session.activity,
                last_seen: session.last_seen,
            })
            .collect())
    }

    /// Read a value from the local metadata table.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Write a value into the local metadata table.
    pub fn meta_set(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

/// Add a column to an existing table when it is missing.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !existing.iter().any(|name| name == column) {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        session_key: row.get(0)?,
        name: row.get(1)?,
        agent_type: row.get(2)?,
        agent_ref: row.get(3)?,
        working_dir: row.get(4)?,
        repo: row.get(5)?,
        branch: row.get(6)?,
        status: row.get(7)?,
        activity: row.get(8)?,
        created_at: row.get(9)?,
        last_seen: row.get(10)?,
    })
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let delivered: i64 = row.get(6)?;
    Ok(Message {
        id: row.get(0)?,
        sender_key: row.get(1)?,
        recipient_key: row.get(2)?,
        body: row.get(3)?,
        created_at: row.get(4)?,
        state: row.get(5)?,
        delivered: delivered != 0,
    })
}

fn row_to_revocation(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRevocation> {
    Ok(StoredRevocation {
        team_id: row.get(0)?,
        peer_id: row.get(1)?,
        record_json: row.get(2)?,
        revoked_at: row.get(3)?,
    })
}

fn row_to_question(row: &rusqlite::Row<'_>) -> rusqlite::Result<Question> {
    Ok(Question {
        id: row.get(0)?,
        session_key: row.get(1)?,
        question: row.get(2)?,
        status: row.get(3)?,
        answer: row.get(4)?,
        created_at: row.get(5)?,
        answered_at: row.get(6)?,
    })
}

/// Current unix timestamp in seconds.
pub fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Generate a unique, sortable id.
fn new_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{prefix}-{nanos:x}-{counter:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register(store: &mut Store, key: &str, name: &str) -> Session {
        store
            .register(RegisterParams {
                session_key: Some(key.to_string()),
                name: name.to_string(),
                agent_type: Some("claude".into()),
                ..Default::default()
            })
            .expect("register")
    }

    #[test]
    fn registration_keeps_identity_and_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        let first = register(&mut store, "alice", "alice");
        assert_eq!(first.session_key, "alice");
        assert_eq!(first.status, "idle");

        let second = register(&mut store, "alice", "alice");
        assert_eq!(second.session_key, "alice");
        assert_eq!(second.created_at, first.created_at);
        assert_eq!(store.list_sessions().unwrap().len(), 1);
    }

    #[test]
    fn duplicate_names_are_disambiguated() {
        let mut store = Store::open_in_memory().unwrap();
        register(&mut store, "a", "worker");
        let second = register(&mut store, "b", "worker");
        assert_eq!(second.name, "worker-2");
    }

    #[test]
    fn messages_require_known_endpoints() {
        let mut store = Store::open_in_memory().unwrap();
        register(&mut store, "a", "a");
        let err = store
            .send_message(SendMessageParams {
                sender_key: "a".into(),
                recipient_key: "ghost".into(),
                body: "hi".into(),
            })
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[test]
    fn inbox_tracks_read_state() {
        let mut store = Store::open_in_memory().unwrap();
        register(&mut store, "a", "a");
        register(&mut store, "b", "b");
        store
            .send_message(SendMessageParams {
                sender_key: "a".into(),
                recipient_key: "b".into(),
                body: "hello".into(),
            })
            .unwrap();

        assert_eq!(store.unread_count("b").unwrap(), 1);
        let inbox = store
            .inbox(InboxParams {
                session_key: "b".into(),
                limit: None,
                unread_only: Some(true),
            })
            .unwrap();
        assert_eq!(inbox.len(), 1);

        let updated = store
            .mark_read(MarkReadParams {
                session_key: "b".into(),
                message_ids: None,
            })
            .unwrap();
        assert_eq!(updated, 1);
        assert_eq!(store.unread_count("b").unwrap(), 0);
    }

    // ---- Milestone 1 -----------------------------------------------------

    #[test]
    fn remote_messages_are_deduplicated_per_peer_and_id() {
        let mut store = Store::open_in_memory().unwrap();
        register(&mut store, "bob", "bob");

        let first = store
            .store_incoming_remote("peer-1", "remote-1", "alice", "bob", "hi", 10)
            .unwrap();
        assert!(first.is_some());

        let again = store
            .store_incoming_remote("peer-1", "remote-1", "alice", "bob", "hi", 10)
            .unwrap();
        assert!(again.is_none(), "a retried message must be dropped");

        let inbox = store
            .inbox(InboxParams {
                session_key: "bob".into(),
                limit: None,
                unread_only: None,
            })
            .unwrap();
        assert_eq!(inbox.len(), 1);
    }

    #[test]
    fn the_same_remote_id_from_a_different_peer_is_a_separate_message() {
        let mut store = Store::open_in_memory().unwrap();
        register(&mut store, "bob", "bob");
        assert!(
            store
                .store_incoming_remote("peer-1", "1", "a", "bob", "one", 10)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .store_incoming_remote("peer-2", "1", "b", "bob", "two", 10)
                .unwrap()
                .is_some()
        );
        assert_eq!(store.unread_count("bob").unwrap(), 2);
    }

    #[test]
    fn an_unknown_remote_recipient_is_rejected() {
        let mut store = Store::open_in_memory().unwrap();
        let error = store
            .store_incoming_remote("peer-1", "1", "a", "nobody", "hi", 10)
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)));
    }

    #[test]
    fn outbound_messages_move_from_pending_to_acked() {
        let mut store = Store::open_in_memory().unwrap();
        register(&mut store, "alice", "alice");
        let message = store
            .queue_remote_message("alice", "bob-agent", "hello", "peer-2")
            .unwrap();
        assert_eq!(store.pending_outbound().unwrap().len(), 1);

        store.set_delivery_state(&message.id, "acked").unwrap();
        assert!(store.pending_outbound().unwrap().is_empty());

        let outbox = store.list_outbound(10).unwrap();
        assert_eq!(outbox[0]["delivery_state"], "acked");
    }

    #[test]
    fn remote_sessions_are_replaced_per_peer() {
        let mut store = Store::open_in_memory().unwrap();
        let session = |key: &str, peer: &str| RemoteSession {
            session_key: key.into(),
            peer_id: peer.into(),
            name: key.into(),
            agent_type: Some("claude".into()),
            status: "idle".into(),
            activity: String::new(),
            last_seen: 1,
        };

        store
            .replace_remote_sessions("peer-1", &[session("a", "peer-1"), session("b", "peer-1")])
            .unwrap();
        store
            .replace_remote_sessions("peer-2", &[session("c", "peer-2")])
            .unwrap();
        assert_eq!(store.list_remote_sessions().unwrap().len(), 3);
        assert_eq!(
            store.owner_of_session("a").unwrap().as_deref(),
            Some("peer-1")
        );

        // A fresh snapshot replaces the previous one for that peer only.
        store
            .replace_remote_sessions("peer-1", &[session("b", "peer-1")])
            .unwrap();
        let sessions = store.list_remote_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(store.owner_of_session("a").unwrap().is_none());
        assert!(store.owner_of_session("b").unwrap().is_some());
    }

    #[test]
    fn an_invitation_nonce_can_only_be_consumed_once() {
        let mut store = Store::open_in_memory().unwrap();
        assert!(store.consume_invite("nonce-1", "team-1").unwrap());
        assert!(!store.consume_invite("nonce-1", "team-1").unwrap());
        assert!(store.invite_used("nonce-1").unwrap());
        assert!(!store.invite_used("nonce-2").unwrap());
    }

    #[test]
    fn a_revocation_removes_the_member_and_is_recorded() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_team(&Team {
                team_id: "team-1".into(),
                name: "core".into(),
                admin_peer: "admin".into(),
                created_at: 1,
            })
            .unwrap();
        store
            .put_member(
                "{}",
                &StoredMember {
                    team_id: "team-1".into(),
                    peer_id: "peer-1".into(),
                    member_name: "laptop".into(),
                    record_json: "{}".into(),
                },
            )
            .unwrap();

        store
            .put_revocation(&StoredRevocation {
                team_id: "team-1".into(),
                peer_id: "peer-1".into(),
                record_json: "{}".into(),
                revoked_at: 2,
            })
            .unwrap();

        assert!(store.is_revoked("team-1", "peer-1").unwrap());
        assert!(store.is_revoked_anywhere("peer-1").unwrap());
        assert!(!store.is_revoked("team-1", "peer-2").unwrap());
        assert_eq!(store.list_revocations("team-1").unwrap().len(), 1);
        assert!(
            store.list_members("team-1").unwrap().is_empty(),
            "a revoked peer must leave the roster"
        );

        // Storing the same revocation twice is idempotent.
        store
            .put_revocation(&StoredRevocation {
                team_id: "team-1".into(),
                peer_id: "peer-1".into(),
                record_json: "{\"again\":true}".into(),
                revoked_at: 3,
            })
            .unwrap();
        assert_eq!(store.all_revocations().unwrap().len(), 1);
    }

    #[test]
    fn teams_and_members_round_trip() {
        let mut store = Store::open_in_memory().unwrap();
        let team = Team {
            team_id: "team-1".into(),
            name: "core".into(),
            admin_peer: "admin".into(),
            created_at: 1,
        };
        store.upsert_team(&team).unwrap();
        store
            .put_member(
                "{}",
                &StoredMember {
                    team_id: "team-1".into(),
                    peer_id: "peer-1".into(),
                    member_name: "laptop".into(),
                    record_json: "{}".into(),
                },
            )
            .unwrap();
        assert_eq!(store.list_teams().unwrap().len(), 1);
        assert_eq!(store.list_members("team-1").unwrap().len(), 1);

        assert!(store.delete_member("team-1", "peer-1").unwrap());
        assert!(store.list_members("team-1").unwrap().is_empty());
        assert!(store.delete_team("team-1").unwrap());
        assert!(store.get_team("team-1").unwrap().is_none());
    }
}
