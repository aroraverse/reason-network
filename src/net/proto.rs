//! The peer-to-peer wire protocol.
//!
//! **These identifiers are frozen for M1.** Changing `ALPN`,
//! `PROTOCOL_VERSION` or a frame shape is a breaking change and requires a new
//! ALPN string.
//!
//! * ALPN: `reason-network/1`
//! * Protocol version: `1`
//! * Framing: one JSON object per line (`\n`), maximum 64 KiB per frame.
//! * Envelope: `{"v":1,"body":{"type":"<frame>",...}}`
//!
//! Handshake, identical from both sides:
//!
//! ```text
//! connector                          acceptor
//!   open_bi()  ------------------>
//!   Hello      ------------------>   verify peer identity + membership
//!              <------------------   Welcome (roster) | Refused
//!              <----------------->   Presence, Msg, Ack, Ping, Pong
//! ```

use serde::{Deserialize, Serialize};

use crate::team::{Invitation, MembershipRecord, RevocationRecord};

/// ALPN protocol identifier for M1. Frozen.
pub const ALPN: &[u8] = b"reason-network/1";

/// Wire protocol version carried in every frame. Frozen.
pub const PROTOCOL_VERSION: u16 = 1;

/// Maximum size of a single frame, in bytes.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Maximum length of a session key or message id on the wire.
pub const MAX_ID_BYTES: usize = 256;

/// Maximum number of sessions in one presence frame.
pub const MAX_PRESENCE_SESSIONS: usize = 512;

/// Maximum number of membership records in one hello or welcome frame.
pub const MAX_ROSTER_RECORDS: usize = 256;

/// One protocol frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    /// Protocol version.
    pub v: u16,
    /// Frame body.
    pub body: Body,
}

impl Frame {
    /// Wrap a body in the current protocol version.
    pub fn new(body: Body) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            body,
        }
    }
}

/// Frame bodies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Body {
    /// Opening frame: identity, roster and any invitations being redeemed.
    Hello(HelloFrame),
    /// Accepting frame: the roster the acceptor holds for the shared teams.
    Welcome(WelcomeFrame),
    /// Refusing frame, sent before closing the connection.
    Refused(RefusedFrame),
    /// Sessions hosted by the sender.
    Presence(PresenceFrame),
    /// Team membership state pushed after the handshake: roster and revocations.
    State(StateFrame),
    /// A message for a session on the receiving machine.
    Msg(MsgFrame),
    /// Acknowledgement of a message.
    Ack(AckFrame),
    /// Keep-alive.
    Ping,
    /// Keep-alive reply.
    Pong,
}

/// `hello` frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloFrame {
    /// Sender peer id, as a string.
    pub peer_id: String,
    /// Human readable machine name.
    pub machine: String,
    /// The sender's current dialable address, so the peer can reconnect later.
    pub addr: iroh::EndpointAddr,
    /// Membership records the sender holds, for every team it knows.
    pub teams: Vec<MembershipRecord>,
    /// Invitations the sender wants to redeem.
    pub invites: Vec<Invitation>,
}

/// `welcome` frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WelcomeFrame {
    /// Membership records the acceptor holds for the shared teams.
    pub teams: Vec<MembershipRecord>,
    /// Machine name of the acceptor.
    pub machine: String,
    /// The acceptor's current dialable address.
    pub addr: iroh::EndpointAddr,
}

/// `refused` frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefusedFrame {
    /// Machine readable reason: `not_a_member`, `version`, `identity`, ...
    pub code: String,
    /// Human readable detail.
    pub reason: String,
}

/// One session in a presence frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Session key.
    pub session_key: String,
    /// Session name.
    pub name: String,
    /// Agent type.
    pub agent_type: Option<String>,
    /// Status.
    pub status: String,
    /// Activity description.
    pub activity: String,
    /// Last seen time on the owner's clock.
    pub last_seen: i64,
}

/// `presence` frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceFrame {
    /// Sessions hosted by the sender.
    pub sessions: Vec<SessionInfo>,
}

/// `state` frame: the membership state a peer is prepared to vouch for.
///
/// Sent after the handshake and again whenever the roster or the revocation
/// list changes. Every record must verify against the team admin before it is
/// stored; an invalid record is ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFrame {
    /// Membership records for the shared teams.
    pub teams: Vec<MembershipRecord>,
    /// Revocations for the shared teams.
    #[serde(default)]
    pub revocations: Vec<RevocationRecord>,
}

/// `msg` frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MsgFrame {
    /// Message id; reused on retries so the receiver can deduplicate.
    pub id: String,
    /// Sending session key on the sender's machine.
    pub sender_session: String,
    /// Receiving session key on the receiving machine.
    pub recipient_session: String,
    /// Verbatim body.
    pub body: String,
    /// Creation time on the sender's clock.
    pub created_at: i64,
}

/// `ack` frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckFrame {
    /// Id of the message being acknowledged.
    pub message_id: String,
    /// True when the message was stored (or was already stored).
    pub accepted: bool,
    /// Why it was rejected.
    pub error: Option<String>,
}

impl MsgFrame {
    /// Validate the frame before it touches storage.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() || self.id.len() > MAX_ID_BYTES {
            return Err("invalid message id".into());
        }
        if self.sender_session.is_empty() || self.sender_session.len() > MAX_ID_BYTES {
            return Err("invalid sender session".into());
        }
        if self.recipient_session.is_empty() || self.recipient_session.len() > MAX_ID_BYTES {
            return Err("invalid recipient session".into());
        }
        if self.body.trim().is_empty() {
            return Err("empty body".into());
        }
        if self.body.len() > MAX_FRAME_BYTES / 2 {
            return Err("body too large".into());
        }
        Ok(())
    }
}

impl HelloFrame {
    /// Validate the shape of a hello frame before verifying signatures.
    pub fn validate(&self) -> Result<(), String> {
        if self.peer_id.is_empty() || self.peer_id.len() > MAX_ID_BYTES {
            return Err("invalid peer id".into());
        }
        if self.machine.len() > MAX_ID_BYTES {
            return Err("machine name too long".into());
        }
        if self.teams.len() > MAX_ROSTER_RECORDS {
            return Err("roster too large".into());
        }
        if self.invites.len() > 8 {
            return Err("too many invitations".into());
        }
        Ok(())
    }
}

impl PresenceFrame {
    /// Validate a presence frame.
    pub fn validate(&self) -> Result<(), String> {
        if self.sessions.len() > MAX_PRESENCE_SESSIONS {
            return Err("too many sessions".into());
        }
        for session in &self.sessions {
            if session.session_key.is_empty() || session.session_key.len() > MAX_ID_BYTES {
                return Err("invalid session key".into());
            }
            if session.name.len() > MAX_ID_BYTES {
                return Err("session name too long".into());
            }
        }
        Ok(())
    }
}

impl StateFrame {
    /// Validate a state frame before any signature is checked.
    pub fn validate(&self) -> Result<(), String> {
        if self.teams.len() > MAX_ROSTER_RECORDS {
            return Err("roster too large".into());
        }
        if self.revocations.len() > MAX_ROSTER_RECORDS {
            return Err("revocation list too large".into());
        }
        Ok(())
    }
}

/// A simple token bucket used to rate limit incoming messages per connection.
#[derive(Debug)]
pub struct RateLimiter {
    tokens: f64,
    capacity: f64,
    refill_per_second: f64,
    last: std::time::Instant,
}

impl RateLimiter {
    /// Create a limiter allowing `per_second` messages with a `burst` capacity.
    pub fn new(per_second: f64, burst: f64) -> Self {
        Self {
            tokens: burst,
            capacity: burst,
            refill_per_second: per_second,
            last: std::time::Instant::now(),
        }
    }

    /// Consume one token. Returns `false` when the peer is over its budget.
    pub fn allow(&mut self) -> bool {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.refill_per_second).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_through_json() {
        let frame = Frame::new(Body::Msg(MsgFrame {
            id: "m1".into(),
            sender_session: "a".into(),
            recipient_session: "b".into(),
            body: "hi".into(),
            created_at: 1,
        }));
        let line = serde_json::to_string(&frame).unwrap();
        assert!(line.contains("\"type\":\"msg\""));
        let decoded: Frame = serde_json::from_str(&line).unwrap();
        assert!(matches!(decoded.body, Body::Msg(_)));
        assert_eq!(decoded.v, PROTOCOL_VERSION);
    }

    #[test]
    fn oversized_bodies_are_rejected() {
        let frame = MsgFrame {
            id: "m1".into(),
            sender_session: "a".into(),
            recipient_session: "b".into(),
            body: "x".repeat(MAX_FRAME_BYTES),
            created_at: 1,
        };
        assert!(frame.validate().is_err());
    }

    #[test]
    fn empty_bodies_are_rejected() {
        let frame = MsgFrame {
            id: "m1".into(),
            sender_session: "a".into(),
            recipient_session: "b".into(),
            body: "   ".into(),
            created_at: 1,
        };
        assert!(frame.validate().is_err());
    }

    #[test]
    fn the_rate_limiter_bites() {
        let mut limiter = RateLimiter::new(1.0, 2.0);
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(!limiter.allow());
    }
}
