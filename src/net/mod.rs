//! The peer-to-peer network node.
//!
//! Owns the iroh endpoint and every peer connection. Messaging, membership
//! verification and persistence all go through the daemon's [`Store`], so a
//! network frame can never bypass local validation.
//!
//! Responsibilities are deliberately narrow:
//!
//! * bind one encrypted QUIC endpoint per machine identity,
//! * run the `Hello`/`Welcome` handshake and enforce team membership,
//! * keep one live link per peer and clean up after disconnects,
//! * carry `Presence`, `Msg` and `Ack` frames.

pub mod discovery;
pub mod proto;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::identity::MachineIdentity;
use crate::store::{RemoteSession, Store, StoredMember, StoredRevocation, Team};
use crate::team::{Invitation, MembershipRecord, RevocationRecord};

use proto::{
    ALPN, AckFrame, Body, Frame, HelloFrame, MAX_FRAME_BYTES, PROTOCOL_VERSION, PresenceFrame,
    RateLimiter, RefusedFrame, SessionInfo, StateFrame, WelcomeFrame,
};

/// How long a handshake may take before it is abandoned.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Incoming messages allowed per second, per connection.
const RATE_LIMIT_PER_SECOND: f64 = 20.0;
/// Burst allowance for the rate limiter.
const RATE_LIMIT_BURST: f64 = 40.0;

/// Network configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetOptions {
    /// Enable the peer-to-peer endpoint at all.
    pub enabled: bool,
    /// Use iroh's default relay servers for connectivity.
    pub relay: bool,
    /// Use LAN discovery (mDNS) in addition to explicit addresses.
    pub discovery: bool,
}

impl Default for NetOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            relay: true,
            discovery: true,
        }
    }
}

impl NetOptions {
    /// Read the options from the environment.
    ///
    /// `REASON_NETWORK_NET=0` disables networking entirely (M0 behaviour),
    /// `REASON_NETWORK_RELAY=0` keeps traffic on direct connections only,
    /// `REASON_NETWORK_DISCOVERY=0` disables LAN discovery.
    pub fn from_env() -> Self {
        Self {
            enabled: env_flag("REASON_NETWORK_NET", true),
            relay: env_flag("REASON_NETWORK_RELAY", true),
            discovery: env_flag("REASON_NETWORK_DISCOVERY", true),
        }
    }

    /// Options for tests: direct connections only, no relay, no discovery.
    pub fn offline() -> Self {
        Self {
            enabled: true,
            relay: false,
            discovery: false,
        }
    }

    /// Options that disable networking entirely.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            relay: false,
            discovery: false,
        }
    }
}

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => default,
    }
}

/// Something the network layer wants the daemon to know about.
#[derive(Debug, Clone)]
pub enum NetworkEvent {
    /// A peer connection completed its handshake.
    PeerUp {
        /// Peer id.
        peer_id: String,
        /// Machine name reported by the peer.
        machine: String,
    },
    /// A peer connection ended.
    PeerDown {
        /// Peer id.
        peer_id: String,
    },
    /// The peer's session directory.
    Presence {
        /// Peer id.
        peer_id: String,
        /// Sessions hosted by the peer.
        sessions: Vec<SessionInfo>,
    },
    /// A message arrived and was stored (or was a duplicate).
    Message {
        /// Peer that sent it.
        peer_id: String,
        /// Remote message id.
        message_id: String,
        /// Recipient session on this machine.
        recipient_session: String,
        /// True when it had already been stored.
        duplicate: bool,
    },
    /// A peer acknowledged one of our messages.
    Ack {
        /// Peer id.
        peer_id: String,
        /// Our message id.
        message_id: String,
        /// Whether it was accepted.
        accepted: bool,
        /// Rejection reason.
        error: Option<String>,
    },
    /// A peer connection failed after being established.
    Error {
        /// Peer id, when known.
        peer_id: Option<String>,
        /// Human readable detail.
        message: String,
    },
}

/// Shared dependencies of the network node.
pub struct NetworkDeps {
    /// The daemon's store. Shared, never duplicated.
    pub store: Arc<Mutex<Store>>,
    /// This machine's identity.
    pub identity: Arc<MachineIdentity>,
    /// Human readable machine name.
    pub machine_name: String,
}

/// Which side opened a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// This machine dialled the peer.
    Outbound,
    /// The peer dialled us.
    Inbound,
}

struct PeerLink {
    id: u64,
    direction: Direction,
    outbound: mpsc::UnboundedSender<Frame>,
}

/// The network node.
pub struct Network {
    endpoint: Endpoint,
    peer_id_string: String,
    deps: Arc<NetworkDeps>,
    links: Arc<Mutex<HashMap<String, PeerLink>>>,
    events: mpsc::UnboundedSender<NetworkEvent>,
    next_link_id: AtomicU64,
    /// How many times a duplicate connection replaced an existing one.
    replacements: AtomicU64,
}

impl Network {
    /// Bind the endpoint and start accepting connections.
    pub async fn bind(
        deps: Arc<NetworkDeps>,
        options: NetOptions,
        events: mpsc::UnboundedSender<NetworkEvent>,
    ) -> Result<Arc<Self>> {
        let builder = Endpoint::builder(presets::Minimal)
            .secret_key(deps.identity.secret().clone())
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(if options.relay {
                RelayMode::Default
            } else {
                RelayMode::Disabled
            });

        let endpoint = builder
            .bind()
            .await
            .map_err(|error| Error::Other(format!("cannot bind iroh endpoint: {error}")))?;

        // LAN discovery is attached after binding and never affects routing.
        if options.discovery
            && let Err(error) = crate::net::discovery::enable_lan_discovery(&endpoint)
        {
            eprintln!("[net] LAN discovery disabled: {error}");
        }

        let identity = Arc::clone(&deps);
        let network = Arc::new(Self {
            peer_id_string: endpoint.id().to_string(),
            endpoint,
            deps: identity,
            links: Arc::new(Mutex::new(HashMap::new())),
            events,
            next_link_id: AtomicU64::new(1),
            replacements: AtomicU64::new(0),
        });

        let acceptor = Arc::clone(&network);
        tokio::spawn(async move { acceptor.accept_loop().await });

        Ok(network)
    }

    /// This machine's peer id.
    pub fn peer_id(&self) -> &str {
        &self.peer_id_string
    }

    /// This machine's dialable address.
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// The underlying endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Peer ids with a live, verified connection.
    pub fn connected_peers(&self) -> Vec<String> {
        self.links
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// True when a verified connection to the peer exists.
    pub fn is_connected(&self, peer_id: &str) -> bool {
        self.links
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(peer_id)
    }

    /// Queue a frame for a connected peer.
    pub fn send(&self, peer_id: &str, frame: Frame) -> Result<()> {
        let links = self.links.lock().unwrap_or_else(|error| error.into_inner());
        let link = links
            .get(peer_id)
            .ok_or_else(|| Error::Other(format!("peer {peer_id} is not connected")))?;
        link.outbound
            .send(frame)
            .map_err(|_| Error::Other(format!("peer {peer_id} connection closed")))
    }

    /// Send the local session directory to one peer.
    pub fn send_presence(&self, peer_id: &str) -> Result<()> {
        let sessions = self.with_store(|store| store.presence_snapshot())?;
        let frame = Frame::new(Body::Presence(PresenceFrame {
            sessions: sessions
                .into_iter()
                .map(|session| SessionInfo {
                    session_key: session.session_key,
                    name: session.name,
                    agent_type: session.agent_type,
                    status: session.status,
                    activity: session.activity,
                    last_seen: session.last_seen,
                })
                .collect(),
        }));
        self.send(peer_id, frame)
    }

    /// Send the local session directory to every connected peer.
    pub fn broadcast_presence(&self) {
        let peers = self.connected_peers();
        for peer in peers {
            if let Err(error) = self.send_presence(&peer) {
                eprintln!("[net] cannot send presence to {peer}: {error}");
            }
        }
    }

    /// Connect to a peer and run the handshake.
    pub async fn connect(self: &Arc<Self>, addr: EndpointAddr) -> Result<String> {
        let peer = addr.id.to_string();
        if self.with_store(|store| store.is_revoked_anywhere(&peer))? {
            return Err(Error::InvalidRequest(format!(
                "peer {peer} has been revoked"
            )));
        }
        if self.is_connected(&peer) {
            return Ok(peer);
        }
        match self.handshake(addr, Vec::new()).await {
            Ok((peer, _)) => Ok(peer),
            // Losing the duplicate tie-break closes our dial, but the peer is
            // connected through the preferred link. That is success, not a
            // reconnect failure worth backing off for.
            Err(_error) if self.is_connected(&peer) => {
                eprintln!("[net] dial to {peer} was superseded by the preferred link");
                Ok(peer)
            }
            Err(error) => Err(error),
        }
    }

    /// Redeem an invitation with its admin and return the joined team.
    pub async fn join_team(self: &Arc<Self>, token: &str) -> Result<Team> {
        let invitation = Invitation::from_token(token)?;
        if invitation.is_expired() {
            return Err(Error::InvalidRequest("this invitation has expired".into()));
        }
        let admin = invitation.admin()?;
        let admin_id = admin.to_string();
        if admin_id == self.peer_id_string {
            return Err(Error::InvalidRequest(
                "this invitation belongs to this machine; share it with another machine".into(),
            ));
        }

        // Pin the admin so the roster is verifiable before the handshake.
        let now = crate::store::now_seconds();
        let team = Team {
            team_id: invitation.team_id.clone(),
            name: invitation.team_name.clone(),
            admin_peer: admin_id.clone(),
            created_at: now,
        };
        self.with_store(|store| store.upsert_team(&team))?;
        if let Ok(addr_json) = serde_json::to_string(&invitation.admin_addr) {
            self.with_store(|store| {
                store.upsert_peer(&crate::store::PeerRecord {
                    peer_id: admin_id.clone(),
                    label: Some("admin".into()),
                    addr_json: Some(addr_json),
                    status: "connecting".into(),
                    last_seen: Some(now),
                })
            })?;
        }

        let result = self
            .handshake(invitation.admin_addr.clone(), vec![invitation.clone()])
            .await;

        match result {
            Ok((peer, welcome)) => {
                let mine = welcome.teams.iter().any(|record| {
                    record.team_id == team.team_id && record.peer_id == self.peer_id_string
                });
                if !mine {
                    self.with_store(|store| store.delete_team(&team.team_id))?;
                    return Err(Error::Other(format!(
                        "peer {peer} accepted the connection but did not issue a membership record"
                    )));
                }
                Ok(team)
            }
            Err(error) => {
                self.with_store(|store| store.delete_team(&team.team_id))?;
                Err(error)
            }
        }
    }

    /// Authorities held by this machine, as `(team_id, admin_peer)` pairs.
    pub fn authoritative_teams(&self) -> Result<Vec<Team>> {
        let me = self.peer_id_string.clone();
        Ok(self
            .with_store(|store| store.list_teams())?
            .into_iter()
            .filter(|team| team.admin_peer == me)
            .collect())
    }

    /// Create a new team owned by this machine.
    pub fn create_team(&self, name: &str) -> Result<Team> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::InvalidRequest("team name must not be empty".into()));
        }
        let team = Team {
            team_id: crate::team::new_team_id(),
            name: name.to_string(),
            admin_peer: self.peer_id_string.clone(),
            created_at: crate::store::now_seconds(),
        };
        let record = MembershipRecord::issue(
            self.deps.identity.secret(),
            &team.team_id,
            &team.name,
            &self.peer_id_string,
            &self.deps.machine_name,
        );
        self.with_store(|store| {
            store.upsert_team(&team)?;
            store.put_member(
                &serde_json::to_string(&record)?,
                &StoredMember {
                    team_id: team.team_id.clone(),
                    peer_id: record.peer_id.clone(),
                    member_name: record.member_name.clone(),
                    record_json: String::new(),
                },
            )
        })?;
        Ok(team)
    }

    /// Issue an invitation for a team this machine administers.
    pub fn invite(&self, team_id: &str, ttl_seconds: i64) -> Result<Invitation> {
        let team = self
            .with_store(|store| store.get_team(team_id))?
            .ok_or_else(|| Error::NotFound(format!("team {team_id}")))?;
        if team.admin_peer != self.peer_id_string {
            return Err(Error::InvalidRequest(
                "only the team admin can issue invitations".into(),
            ));
        }
        Ok(Invitation::issue(
            self.deps.identity.secret(),
            &team.team_id,
            &team.name,
            self.endpoint.addr(),
            ttl_seconds.max(60),
        ))
    }

    /// Revoke another member's membership of a team.
    ///
    /// Only the team admin may do this. The signed revocation is persisted,
    /// pushed to connected team peers, and the revoked peer's connection is
    /// closed immediately.
    pub fn revoke(&self, team_id: &str, peer_id: &str) -> Result<RevocationRecord> {
        let team = self
            .with_store(|store| store.get_team(team_id))?
            .ok_or_else(|| Error::NotFound(format!("team {team_id}")))?;
        if team.admin_peer != self.peer_id_string {
            return Err(Error::InvalidRequest(
                "only the team admin can revoke a member".into(),
            ));
        }
        if peer_id == self.peer_id_string {
            return Err(Error::InvalidRequest(
                "the admin cannot revoke itself".into(),
            ));
        }
        if peer_id.trim().is_empty() {
            return Err(Error::InvalidRequest("peer id must not be empty".into()));
        }

        let is_member = self
            .with_store(|store| store.list_members(team_id))?
            .iter()
            .any(|member| member.peer_id == peer_id);
        if !is_member {
            return Err(Error::NotFound(format!(
                "peer {peer_id} is not a member of {team_id}"
            )));
        }

        let record = RevocationRecord::issue(self.deps.identity.secret(), team_id, peer_id);
        self.store_revocation(&record)?;
        self.disconnect(peer_id);
        self.broadcast_state();
        Ok(record)
    }

    /// Verify and persist one revocation. Returns `true` when it was new.
    fn store_revocation(&self, record: &RevocationRecord) -> Result<bool> {
        let is_new =
            !self.with_store(|store| store.is_revoked(&record.team_id, &record.peer_id))?;
        let json = serde_json::to_string(record)?;
        self.with_store(|store| {
            store.put_revocation(&StoredRevocation {
                team_id: record.team_id.clone(),
                peer_id: record.peer_id.clone(),
                record_json: json,
                revoked_at: crate::store::now_seconds(),
            })
        })?;
        Ok(is_new)
    }

    /// Verify revocations against the pinned admin and store the valid ones.
    ///
    /// Returns the peers that became revoked locally, so their connections can
    /// be closed.
    fn merge_revocations(&self, records: &[RevocationRecord]) -> Result<Vec<String>> {
        let teams = self.with_store(|store| store.list_teams())?;
        let mut newly_revoked = Vec::new();
        for record in records {
            let Some(team) = teams.iter().find(|team| team.team_id == record.team_id) else {
                continue;
            };
            let Ok(admin) = team.admin_peer.parse::<EndpointId>() else {
                continue;
            };
            if !record.verify(&admin) {
                eprintln!(
                    "[net] ignoring a revocation for team {} with a bad signature",
                    record.team_id
                );
                continue;
            }
            if self.store_revocation(record)? {
                newly_revoked.push(record.peer_id.clone());
            }
            self.with_store(|store| store.set_peer_status(&record.peer_id, "revoked"))
                .ok();
        }
        Ok(newly_revoked)
    }

    /// Close the connection to a peer, if any.
    pub fn disconnect(&self, peer_id: &str) {
        let removed = self
            .links
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(peer_id);
        if removed.is_some() {
            eprintln!("[net] closing the connection to {peer_id}");
            let _ = self.events.send(NetworkEvent::PeerDown {
                peer_id: peer_id.to_string(),
            });
        }
    }

    /// Push the current membership state to every connected peer.
    pub fn broadcast_state(&self) {
        let state = self.local_state();
        for peer in self.connected_peers() {
            if let Err(error) = self.send(&peer, Frame::new(Body::State(state.clone()))) {
                eprintln!("[net] cannot push membership state to {peer}: {error}");
            }
        }
    }

    /// The membership records and revocations this machine vouches for.
    fn local_state(&self) -> StateFrame {
        let teams = self
            .with_store(|store| store.all_members())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|member| serde_json::from_str::<MembershipRecord>(&member.record_json).ok())
            .collect();
        let revocations = self
            .with_store(|store| store.all_revocations())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|record| serde_json::from_str::<RevocationRecord>(&record.record_json).ok())
            .collect();
        StateFrame { teams, revocations }
    }

    /// Store a dialable address for a member so reconnects work after restart.
    pub fn remember_peer_address(&self, peer_id: &str, addr: &EndpointAddr) -> Result<()> {
        let addr_json = serde_json::to_string(addr)?;
        self.with_store(|store| {
            store.upsert_peer(&crate::store::PeerRecord {
                peer_id: peer_id.to_string(),
                label: None,
                addr_json: Some(addr_json),
                status: "known".into(),
                last_seen: Some(crate::store::now_seconds()),
            })
        })
    }

    /// Addresses of every dialable peer, for reconnection.
    ///
    /// Peers this machine has left are skipped so we stop dialling them.
    pub fn known_peer_addresses(&self) -> Vec<(String, EndpointAddr)> {
        let peers = match self.with_store(|store| store.list_peers()) {
            Ok(peers) => peers,
            Err(error) => {
                eprintln!("[net] cannot list peers: {error}");
                return Vec::new();
            }
        };
        peers
            .into_iter()
            .filter(|peer| peer.status != "left" && peer.status != "revoked")
            .filter_map(|peer| {
                let addr_json = peer.addr_json?;
                let addr: EndpointAddr = serde_json::from_str(&addr_json).ok()?;
                Some((peer.peer_id, addr))
            })
            .collect()
    }

    /// Close the endpoint.
    pub fn shutdown(&self) {
        let endpoint = self.endpoint.clone();
        tokio::spawn(async move {
            endpoint.close().await;
        });
    }

    // ---------------------------------------------------------------------
    // Handshake
    // ---------------------------------------------------------------------

    /// Connect + Hello/Welcome exchange. On success the link is registered.
    async fn handshake(
        self: &Arc<Self>,
        addr: EndpointAddr,
        invites: Vec<Invitation>,
    ) -> Result<(String, WelcomeFrame)> {
        let expected = addr.id;
        let remote = expected.to_string();

        if self.is_connected(&remote) {
            return Err(Error::Other(format!("already connected to {remote}")));
        }

        let exchange = async {
            let connection = self
                .endpoint
                .connect(addr.clone(), ALPN)
                .await
                .map_err(|error| Error::Other(format!("cannot reach peer {remote}: {error}")))?;

            let (mut send, recv) = connection
                .open_bi()
                .await
                .map_err(|error| Error::Other(format!("cannot open stream: {error}")))?;

            let hello = self.build_hello(invites);
            write_frame(&mut send, &Frame::new(Body::Hello(hello))).await?;

            let mut reader = BufReader::new(recv);
            let frame = read_frame(&mut reader).await?.ok_or_else(|| {
                Error::Other("peer closed the stream during the handshake".into())
            })?;

            match frame.body {
                Body::Welcome(welcome) => {
                    self.merge_roster(&remote, &welcome.teams)?;
                    // Prefer the address the peer just reported; it survives
                    // restarts on either side.
                    let fresh = if welcome.addr.id.to_string() == remote {
                        welcome.addr.clone()
                    } else {
                        addr.clone()
                    };
                    self.remember_peer_address(&remote, &fresh).ok();
                    // If this dial lost the deterministic tie-break, the peer
                    // is still connected through the preferred link.
                    self.register_peer(
                        remote.clone(),
                        connection.clone(),
                        send,
                        reader,
                        welcome.machine.clone(),
                        Direction::Outbound,
                    )
                    .await;
                    Ok((remote.clone(), welcome))
                }
                Body::Refused(refused) => {
                    connection.close(1u32.into(), b"refused");
                    Err(Error::Other(format!(
                        "peer refused the connection: {} ({})",
                        refused.reason, refused.code
                    )))
                }
                _ => {
                    connection.close(1u32.into(), b"protocol");
                    Err(Error::Other("unexpected frame during the handshake".into()))
                }
            }
        };

        match tokio::time::timeout(HANDSHAKE_TIMEOUT, exchange).await {
            Ok(result) => result,
            Err(_) => Err(Error::Other(format!("handshake with {remote} timed out"))),
        }
    }

    fn build_hello(&self, invites: Vec<Invitation>) -> HelloFrame {
        let teams = self
            .with_store(|store| store.all_members())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|member| serde_json::from_str::<MembershipRecord>(&member.record_json).ok())
            .collect();
        HelloFrame {
            peer_id: self.peer_id_string.clone(),
            machine: self.deps.machine_name.clone(),
            addr: self.endpoint.addr(),
            teams,
            invites,
        }
    }

    async fn accept_loop(self: Arc<Self>) {
        loop {
            let Some(incoming) = self.endpoint.accept().await else {
                return;
            };
            let node = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(error) = node.handle_incoming(incoming).await {
                    eprintln!("[net] incoming connection closed: {error}");
                }
            });
        }
    }

    async fn handle_incoming(self: &Arc<Self>, incoming: iroh::endpoint::Incoming) -> Result<()> {
        let connection = incoming
            .await
            .map_err(|error| Error::Other(format!("incoming connection failed: {error}")))?;
        let remote = connection.remote_id().to_string();

        let (mut send, recv) = connection
            .accept_bi()
            .await
            .map_err(|error| Error::Other(format!("cannot accept stream: {error}")))?;

        let mut reader = BufReader::new(recv);
        let frame = match read_frame(&mut reader).await? {
            Some(frame) => frame,
            None => return Ok(()),
        };

        let Body::Hello(hello) = frame.body else {
            let _ = write_frame(
                &mut send,
                &Frame::new(Body::Refused(RefusedFrame {
                    code: "protocol".into(),
                    reason: "expected a hello frame".into(),
                })),
            )
            .await;
            connection.close(1u32.into(), b"protocol");
            return Ok(());
        };

        match self.authorize(&remote, &hello) {
            Ok((roster, machine)) => {
                write_frame(
                    &mut send,
                    &Frame::new(Body::Welcome(WelcomeFrame {
                        teams: roster,
                        machine: self.deps.machine_name.clone(),
                        addr: self.endpoint.addr(),
                    })),
                )
                .await?;
                self.remember_peer_address(&remote, &hello.addr).ok();
                self.register_peer(
                    remote.clone(),
                    connection,
                    send,
                    reader,
                    machine,
                    Direction::Inbound,
                )
                .await;
                Ok(())
            }
            Err((code, reason)) => {
                eprintln!("[net] refusing {remote}: {reason}");
                let _ = write_frame(
                    &mut send,
                    &Frame::new(Body::Refused(RefusedFrame {
                        code,
                        reason: reason.clone(),
                    })),
                )
                .await;
                // Finish the stream and give the peer a moment to read the
                // refusal before the connection is closed; otherwise the QUIC
                // close can overtake the frame.
                let _ = send.finish();
                tokio::time::sleep(Duration::from_millis(250)).await;
                connection.close(2u32.into(), b"refused");
                Err(Error::Other(format!("refused peer {remote}: {reason}")))
            }
        }
    }

    /// Decide whether a hello may join, and which roster to answer with.
    ///
    /// Returns `Err((code, reason))` when the peer must be refused.
    fn authorize(
        &self,
        remote: &str,
        hello: &HelloFrame,
    ) -> std::result::Result<(Vec<MembershipRecord>, String), (String, String)> {
        hello
            .validate()
            .map_err(|reason| ("protocol".to_string(), reason))?;

        if hello.peer_id != remote {
            return Err((
                "identity".into(),
                "hello peer id does not match the authenticated connection".into(),
            ));
        }
        if hello.addr.id.to_string() != remote {
            return Err((
                "identity".into(),
                "hello address does not belong to the authenticated peer".into(),
            ));
        }

        let teams = self
            .with_store(|store| store.list_teams())
            .map_err(|error| ("internal".to_string(), error.to_string()))?;
        if teams.is_empty() {
            return Err((
                "not_a_member".into(),
                "this machine does not belong to any team".into(),
            ));
        }

        // A revocation we hold is authoritative: the peer's membership record
        // may still be validly signed, but it must no longer be accepted.
        for team in &teams {
            let revoked = self
                .with_store(|store| store.is_revoked(&team.team_id, remote))
                .map_err(|error| ("internal".to_string(), error.to_string()))?;
            if revoked {
                return Err((
                    "revoked".into(),
                    "membership of this peer was revoked by the team admin".into(),
                ));
            }
        }

        let mut accepted: Vec<String> = Vec::new();

        // 1. Valid membership records for teams we know about.
        for record in &hello.teams {
            let Some(team) = teams.iter().find(|team| team.team_id == record.team_id) else {
                continue;
            };
            let Ok(admin) = team.admin_peer.parse::<EndpointId>() else {
                continue;
            };
            if !record.verify(&admin) {
                eprintln!(
                    "[net] dropping forged membership record for team {}",
                    record.team_id
                );
                continue;
            }
            if record.peer_id == remote && !accepted.contains(&record.team_id) {
                accepted.push(record.team_id.clone());
            }
            let _ = self.store_member(record);
        }

        // 2. Single-use invitations, redeemable only by the admin.
        for invitation in &hello.invites {
            if !invitation.verify() || invitation.is_expired() {
                continue;
            }
            let Some(team) = teams.iter().find(|team| team.team_id == invitation.team_id) else {
                continue;
            };
            if team.admin_peer != self.peer_id_string
                || invitation.admin_peer_id != self.peer_id_string
            {
                // Only the admin may mint records; other members must not
                // honour another member's invitation.
                continue;
            }
            match self.with_store(|store| store.consume_invite(&invitation.nonce, &team.team_id)) {
                Ok(true) => {
                    let record = MembershipRecord::issue(
                        self.deps.identity.secret(),
                        &team.team_id,
                        &team.name,
                        remote,
                        &hello.machine,
                    );
                    if let Err(error) = self.store_member(&record) {
                        eprintln!("[net] cannot store issued membership record: {error}");
                        continue;
                    }
                    if !accepted.contains(&team.team_id) {
                        accepted.push(team.team_id.clone());
                    }
                }
                Ok(false) => {
                    eprintln!("[net] invitation {} was already used", invitation.nonce);
                }
                Err(error) => {
                    eprintln!("[net] cannot consume invitation: {error}");
                }
            }
        }

        if accepted.is_empty() {
            return Err((
                "not_a_member".into(),
                "no valid membership record or invitation for a shared team".into(),
            ));
        }

        let roster = self
            .with_store(|store| store.all_members())
            .map_err(|error| ("internal".to_string(), error.to_string()))?
            .into_iter()
            .filter(|member| accepted.contains(&member.team_id))
            .filter_map(|member| serde_json::from_str::<MembershipRecord>(&member.record_json).ok())
            .collect();

        Ok((roster, hello.machine.clone()))
    }

    fn store_member(&self, record: &MembershipRecord) -> Result<()> {
        let json = serde_json::to_string(record)?;
        self.with_store(|store| {
            store.put_member(
                &json,
                &StoredMember {
                    team_id: record.team_id.clone(),
                    peer_id: record.peer_id.clone(),
                    member_name: record.member_name.clone(),
                    record_json: json.clone(),
                },
            )
        })
    }

    /// Verify and store a roster received from a peer.
    fn merge_roster(&self, peer: &str, records: &[MembershipRecord]) -> Result<()> {
        let teams = self.with_store(|store| store.list_teams())?;
        for record in records {
            let Some(team) = teams.iter().find(|team| team.team_id == record.team_id) else {
                continue;
            };
            let Ok(admin) = team.admin_peer.parse::<EndpointId>() else {
                continue;
            };
            if !record.verify(&admin) {
                eprintln!("[net] ignoring a membership record from {peer} with a bad signature");
                continue;
            }
            // A stale roster must not resurrect a revoked member.
            let revoked =
                self.with_store(|store| store.is_revoked(&record.team_id, &record.peer_id))?;
            if revoked {
                continue;
            }
            self.store_member(record)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Connections
    // ---------------------------------------------------------------------

    /// Register a verified connection.
    ///
    /// Simultaneous dials can produce two connections to the same peer. The
    /// rule is deterministic and identical on both sides: the machine with the
    /// smaller peer id prefers its **outbound** connection, the larger id
    /// prefers its **inbound** one. Once a link in the preferred direction
    /// exists, every further duplicate is rejected, so the peer pair converges
    /// on one connection and cannot enter a replacement loop.
    ///
    /// Returns `false` when the new connection was the redundant one and has
    /// been closed.
    async fn register_peer(
        self: &Arc<Self>,
        peer_id: String,
        connection: iroh::endpoint::Connection,
        mut send: iroh::endpoint::SendStream,
        reader: BufReader<iroh::endpoint::RecvStream>,
        machine: String,
        direction: Direction,
    ) -> bool {
        let link_id = self.next_link_id.fetch_add(1, Ordering::Relaxed);
        let (outbound, receiver) = mpsc::unbounded_channel::<Frame>();

        // Decide and register while holding the lock, then release it before
        // any await: the guard must never cross an await point.
        let rejected = {
            let mut links = self.links.lock().unwrap_or_else(|error| error.into_inner());
            let preferred = if self.peer_id_string.as_str() < peer_id.as_str() {
                Direction::Outbound
            } else {
                Direction::Inbound
            };

            let reject = match links.get(&peer_id) {
                // A preferred link already exists: never disturb it.
                Some(existing) if existing.direction == preferred => true,
                // A non-preferred link exists and this is not preferred either.
                Some(_) if direction != preferred => true,
                Some(_) => {
                    self.replacements.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "[net] replacing the existing connection to {peer_id} with the preferred {} one",
                        describe(direction)
                    );
                    false
                }
                None => false,
            };

            if !reject {
                links.insert(
                    peer_id.clone(),
                    PeerLink {
                        id: link_id,
                        direction,
                        outbound,
                    },
                );
            }
            reject
        };

        if rejected {
            eprintln!(
                "[net] ignoring the redundant {} connection to {peer_id}",
                describe(direction)
            );
            settle_before_close(&mut send).await;
            connection.close(3u32.into(), b"duplicate");
            return false;
        }

        let _ = self.events.send(NetworkEvent::PeerUp {
            peer_id: peer_id.clone(),
            machine: machine.clone(),
        });
        self.with_store(|store| {
            store.upsert_peer(&crate::store::PeerRecord {
                peer_id: peer_id.clone(),
                label: Some(machine),
                addr_json: None,
                status: "connected".into(),
                last_seen: Some(crate::store::now_seconds()),
            })
        })
        .ok();

        // Writer: owns the send half of the QUIC stream.
        let writer_owner = Arc::clone(self);
        let writer_peer = peer_id.clone();
        tokio::spawn(async move {
            let _ = writer_owner
                .writer_loop(writer_peer, send, receiver, connection)
                .await;
        });

        // Reader: owns the receive half.
        let reader_owner = Arc::clone(self);
        let reader_peer = peer_id.clone();
        tokio::spawn(async move {
            reader_owner.reader_loop(reader_peer, link_id, reader).await;
        });

        if let Err(error) = self.send_presence(&peer_id) {
            eprintln!("[net] cannot push presence to {peer_id}: {error}");
        }
        // Share the membership state (roster + revocations) with the new link
        // so a revocation propagates on the next handshake even if the
        // handshake roster was stale.
        let state = Frame::new(Body::State(self.local_state()));
        if let Err(error) = self.send(&peer_id, state) {
            eprintln!("[net] cannot push membership state to {peer_id}: {error}");
        }
        true
    }

    /// True when `link_id` is still the live link for `peer_id`.
    fn is_current_link(&self, peer_id: &str, link_id: u64) -> bool {
        self.links
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(peer_id)
            .map(|link| link.id == link_id)
            .unwrap_or(false)
    }

    /// Number of times a duplicate connection replaced an existing one.
    pub fn replacements(&self) -> u64 {
        self.replacements.load(Ordering::Relaxed)
    }

    async fn writer_loop(
        &self,
        peer_id: String,
        mut send: iroh::endpoint::SendStream,
        mut receiver: mpsc::UnboundedReceiver<Frame>,
        connection: iroh::endpoint::Connection,
    ) {
        while let Some(frame) = receiver.recv().await {
            if let Err(error) = write_frame(&mut send, &frame).await {
                eprintln!("[net] cannot write to {peer_id}: {error}");
                break;
            }
        }
        let _ = send.finish();
        connection.close(0u32.into(), b"bye");
    }

    async fn reader_loop(
        self: Arc<Self>,
        peer_id: String,
        link_id: u64,
        mut reader: BufReader<iroh::endpoint::RecvStream>,
    ) {
        let mut limiter = RateLimiter::new(RATE_LIMIT_PER_SECOND, RATE_LIMIT_BURST);
        loop {
            match read_frame(&mut reader).await {
                Ok(Some(frame)) => match self.handle_frame(&peer_id, frame, &mut limiter) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => {
                        eprintln!("[net] protocol error from {peer_id}: {error}");
                        break;
                    }
                },
                Ok(None) => break,
                Err(error) => {
                    // A superseded link is expected to end; only real faults
                    // are worth a log line.
                    if self.is_current_link(&peer_id, link_id) {
                        eprintln!("[net] connection to {peer_id} ended: {error}");
                    }
                    break;
                }
            }
        }
        self.link_down(&peer_id, link_id);
    }

    /// Handle one frame. Returns `false` when the connection must be closed.
    fn handle_frame(&self, peer_id: &str, frame: Frame, limiter: &mut RateLimiter) -> Result<bool> {
        match frame.body {
            Body::Presence(presence) => {
                if let Err(reason) = presence.validate() {
                    return Err(Error::Ipc(format!("invalid presence frame: {reason}")));
                }
                let sessions: Vec<RemoteSession> = presence
                    .sessions
                    .iter()
                    .map(|session| RemoteSession {
                        session_key: session.session_key.clone(),
                        peer_id: peer_id.to_string(),
                        name: session.name.clone(),
                        agent_type: session.agent_type.clone(),
                        status: session.status.clone(),
                        activity: session.activity.clone(),
                        last_seen: session.last_seen,
                    })
                    .collect();
                self.with_store(|store| store.replace_remote_sessions(peer_id, &sessions))?;
                let _ = self.events.send(NetworkEvent::Presence {
                    peer_id: peer_id.to_string(),
                    sessions: presence.sessions,
                });
                Ok(true)
            }
            Body::State(state) => {
                if let Err(reason) = state.validate() {
                    return Err(Error::Ipc(format!("invalid state frame: {reason}")));
                }
                let mut revoked = self.merge_revocations(&state.revocations)?;
                self.merge_roster(peer_id, &state.teams)?;
                // Drop peers whose membership was just revoked.
                revoked.retain(|peer| peer != peer_id);
                for peer in revoked {
                    self.disconnect(&peer);
                }
                Ok(true)
            }
            Body::Msg(message) => {
                if let Err(reason) = message.validate() {
                    self.send_ack(
                        peer_id,
                        AckFrame {
                            message_id: message.id.clone(),
                            accepted: false,
                            error: Some(reason),
                        },
                    );
                    return Ok(true);
                }
                if !limiter.allow() {
                    self.send_ack(
                        peer_id,
                        AckFrame {
                            message_id: message.id.clone(),
                            accepted: false,
                            error: Some("rate limited".into()),
                        },
                    );
                    return Ok(true);
                }

                let stored = self.with_store(|store| {
                    store.store_incoming_remote(
                        peer_id,
                        &message.id,
                        &message.sender_session,
                        &message.recipient_session,
                        &message.body,
                        message.created_at,
                    )
                });

                match stored {
                    Ok(Some(_)) => {
                        self.send_ack(
                            peer_id,
                            AckFrame {
                                message_id: message.id.clone(),
                                accepted: true,
                                error: None,
                            },
                        );
                        let _ = self.events.send(NetworkEvent::Message {
                            peer_id: peer_id.to_string(),
                            message_id: message.id,
                            recipient_session: message.recipient_session,
                            duplicate: false,
                        });
                    }
                    Ok(None) => {
                        // Already stored: acknowledge again so the sender stops
                        // retrying, but do not deliver twice.
                        self.send_ack(
                            peer_id,
                            AckFrame {
                                message_id: message.id.clone(),
                                accepted: true,
                                error: None,
                            },
                        );
                        let _ = self.events.send(NetworkEvent::Message {
                            peer_id: peer_id.to_string(),
                            message_id: message.id,
                            recipient_session: message.recipient_session,
                            duplicate: true,
                        });
                    }
                    Err(error) => {
                        self.send_ack(
                            peer_id,
                            AckFrame {
                                message_id: message.id.clone(),
                                accepted: false,
                                error: Some(error.to_string()),
                            },
                        );
                    }
                }
                Ok(true)
            }
            Body::Ack(ack) => {
                let _ = self.events.send(NetworkEvent::Ack {
                    peer_id: peer_id.to_string(),
                    message_id: ack.message_id,
                    accepted: ack.accepted,
                    error: ack.error,
                });
                Ok(true)
            }
            Body::Ping => {
                let _ = self.send(peer_id, Frame::new(Body::Pong));
                Ok(true)
            }
            Body::Pong => Ok(true),
            Body::Hello(_) | Body::Welcome(_) | Body::Refused(_) => Err(Error::Ipc(
                "handshake frame received after the handshake".into(),
            )),
        }
    }

    fn send_ack(&self, peer_id: &str, ack: AckFrame) {
        if let Err(error) = self.send(peer_id, Frame::new(Body::Ack(ack))) {
            eprintln!("[net] cannot send acknowledgement to {peer_id}: {error}");
        }
    }

    fn link_down(&self, peer_id: &str, link_id: u64) {
        let removed = {
            let mut links = self.links.lock().unwrap_or_else(|error| error.into_inner());
            match links.get(peer_id) {
                Some(link) if link.id == link_id => links.remove(peer_id).is_some(),
                // A newer connection replaced this one; leave it alone.
                _ => false,
            }
        };
        if !removed {
            return;
        }
        // The sessions stay in the directory so messages can still be
        // addressed to them while the peer is away; reachability is derived
        // from the connection state.
        self.with_store(|store| store.set_peer_status(peer_id, "idle"))
            .ok();
        let _ = self.events.send(NetworkEvent::PeerDown {
            peer_id: peer_id.to_string(),
        });
    }

    fn with_store<T>(&self, action: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
        let mut store = self
            .deps
            .store
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        action(&mut store)
    }
}

/// Read one newline-delimited frame.
pub async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Frame>> {
    let mut buffer: Vec<u8> = Vec::new();
    let read = reader
        .read_until(b'\n', &mut buffer)
        .await
        .map_err(|error| Error::Ipc(format!("read failed: {error}")))?;
    if read == 0 {
        return Ok(None);
    }
    if buffer.len() > MAX_FRAME_BYTES {
        return Err(Error::Ipc("frame exceeds the maximum size".into()));
    }
    let text =
        std::str::from_utf8(&buffer).map_err(|_| Error::Ipc("frame is not valid UTF-8".into()))?;
    let text = text.trim_end();
    if text.is_empty() {
        return Ok(None);
    }
    let frame: Frame = serde_json::from_str(text)
        .map_err(|error| Error::Ipc(format!("invalid frame: {error}")))?;
    if frame.v != PROTOCOL_VERSION {
        return Err(Error::Ipc(format!(
            "unsupported protocol version {}",
            frame.v
        )));
    }
    Ok(Some(frame))
}

/// Write one newline-delimited frame.
pub async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> Result<()> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|error| Error::Ipc(format!("write failed: {error}")))?;
    writer
        .flush()
        .await
        .map_err(|error| Error::Ipc(format!("flush failed: {error}")))?;
    Ok(())
}

/// Human readable form of a connection direction.
fn describe(direction: Direction) -> &'static str {
    match direction {
        Direction::Outbound => "outbound",
        Direction::Inbound => "inbound",
    }
}

/// Let the peer read what was just written before the connection is closed.
///
/// Rejecting a duplicate closes the QUIC connection. Without this pause the
/// close can overtake the `welcome` frame and the other side sees a failed
/// handshake instead of a clean, redundant connection.
async fn settle_before_close(send: &mut iroh::endpoint::SendStream) {
    let _ = send.finish();
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// Summary of the network for diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStatus {
    /// Whether the endpoint is running.
    pub enabled: bool,
    /// This machine's peer id.
    pub peer_id: String,
    /// Dialable address.
    pub addr: Option<EndpointAddr>,
    /// Connected peers.
    pub connected: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn frames_round_trip_through_the_wire_format() {
        let frame = Frame::new(Body::Ping);
        let mut buffer: Vec<u8> = Vec::new();
        write_frame(&mut buffer, &frame).await.unwrap();
        assert!(buffer.ends_with(b"\n"));

        let mut reader = BufReader::new(buffer.as_slice());
        let decoded = read_frame(&mut reader).await.unwrap().unwrap();
        assert!(matches!(decoded.body, Body::Ping));
        assert_eq!(decoded.v, PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn a_foreign_protocol_version_is_rejected() {
        let mut line = serde_json::to_string(&json_frame(99, Body::Ping)).unwrap();
        line.push('\n');
        let mut reader = BufReader::new(line.as_bytes());
        let error = read_frame(&mut reader).await.unwrap_err();
        assert!(error.to_string().contains("protocol version"), "{error}");
    }

    #[tokio::test]
    async fn an_oversized_frame_is_rejected() {
        let mut line = "x".repeat(MAX_FRAME_BYTES + 16);
        line.push('\n');
        let mut reader = BufReader::new(line.as_bytes());
        let error = read_frame(&mut reader).await.unwrap_err();
        assert!(error.to_string().contains("maximum size"), "{error}");
    }

    #[tokio::test]
    async fn a_closed_stream_reads_as_none() {
        let data: &[u8] = b"";
        let mut reader = BufReader::new(data);
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    fn json_frame(version: u16, body: Body) -> Frame {
        Frame { v: version, body }
    }

    #[test]
    fn offline_options_disable_relay_and_discovery() {
        let options = NetOptions::offline();
        assert!(options.enabled);
        assert!(!options.relay);
        assert!(!options.discovery);
        assert!(!NetOptions::disabled().enabled);
    }
}
