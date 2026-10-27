//! Teams, invitations and signed membership records.
//!
//! Trust model for M1:
//!
//! * Every machine has an ed25519 identity (see [`crate::identity`]).
//! * A team is created by one machine, which becomes its **admin**.
//! * The admin signs a [`MembershipRecord`] for every member. Records travel
//!   between peers and are verified against the admin key that is pinned
//!   locally when the team is created or joined.
//! * An [`Invitation`] is an admin-signed, single-use, expiring token. Holding
//!   it lets exactly one new machine ask the admin for a membership record.
//!
//! An authenticated encrypted connection is *not* enough to join a team: the
//! peer must present either a valid membership record or a valid invitation.
//! Only the admin key can mint records, so a peer cannot grant itself
//! membership by editing the roster it sends.

use std::time::{SystemTime, UNIX_EPOCH};

use data_encoding::BASE64URL_NOPAD;
use iroh::{EndpointAddr, EndpointId, SecretKey, Signature};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Signature domain for membership records. Changing it invalidates old records.
pub const MEMBERSHIP_DOMAIN: &str = "reason-network/membership/v1";
/// Signature domain for revocations.
pub const REVOCATION_DOMAIN: &str = "reason-network/revocation/v1";
/// Signature domain for invitations.
pub const INVITE_DOMAIN: &str = "reason-network/invite/v1";
/// Prefix of an encoded invitation token.
pub const INVITE_TOKEN_PREFIX: &str = "rn1";

/// A membership record: proof, signed by the team admin, that a peer belongs
/// to a team.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MembershipRecord {
    /// Team identifier (random, stable).
    pub team_id: String,
    /// Human readable team name.
    pub team_name: String,
    /// Peer id of the member.
    pub peer_id: String,
    /// Display name of the member machine.
    pub member_name: String,
    /// Issue time (unix seconds).
    pub issued_at: i64,
    /// Unique id of this record.
    pub nonce: String,
    /// Hex encoded ed25519 signature by the team admin.
    pub sig: String,
}

impl MembershipRecord {
    /// Create and sign a record. Only the admin can produce a valid one.
    pub fn issue(
        admin: &SecretKey,
        team_id: &str,
        team_name: &str,
        peer_id: &str,
        member_name: &str,
    ) -> Self {
        let mut record = Self {
            team_id: team_id.to_string(),
            team_name: team_name.to_string(),
            peer_id: peer_id.to_string(),
            member_name: member_name.to_string(),
            issued_at: now_seconds(),
            nonce: random_hex(16),
            sig: String::new(),
        };
        record.sig = hex_encode(&admin.sign(&record.signing_bytes()).to_bytes());
        record
    }

    /// The exact bytes covered by the signature.
    pub fn signing_bytes(&self) -> Vec<u8> {
        canonical(&[
            MEMBERSHIP_DOMAIN,
            &self.team_id,
            &self.team_name,
            &self.peer_id,
            &self.member_name,
            &self.issued_at.to_string(),
            &self.nonce,
        ])
    }

    /// Verify the record against the pinned admin key.
    pub fn verify(&self, admin: &EndpointId) -> bool {
        let Some(signature) = hex_decode_64(&self.sig) else {
            return false;
        };
        let signature = Signature::from_bytes(&signature);
        admin.verify(&self.signing_bytes(), &signature).is_ok()
    }

    /// Parse a peer id from the record.
    pub fn peer(&self) -> Result<EndpointId> {
        self.peer_id
            .parse()
            .map_err(|_| Error::InvalidRequest(format!("invalid peer id {}", self.peer_id)))
    }
}

/// A revocation record: proof, signed by the team admin, that a peer's
/// membership record for a team must no longer be accepted.
///
/// Revocation is authoritative against membership: once a machine holds and
/// verifies this record it refuses connections from the revoked peer, even if
/// the peer still presents its (validly signed) membership record. Re-admitting
/// a revoked peer requires recreating the team; M1 has no `unrevoke`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevocationRecord {
    /// Team the revocation applies to.
    pub team_id: String,
    /// Revoked peer id.
    pub peer_id: String,
    /// Issue time (unix seconds).
    pub issued_at: i64,
    /// Unique id of this revocation.
    pub nonce: String,
    /// Hex encoded ed25519 signature by the team admin.
    pub sig: String,
}

impl RevocationRecord {
    /// Create and sign a revocation. Only the admin can produce a valid one.
    pub fn issue(admin: &SecretKey, team_id: &str, peer_id: &str) -> Self {
        let mut record = Self {
            team_id: team_id.to_string(),
            peer_id: peer_id.to_string(),
            issued_at: now_seconds(),
            nonce: random_hex(16),
            sig: String::new(),
        };
        record.sig = hex_encode(&admin.sign(&record.signing_bytes()).to_bytes());
        record
    }

    /// The exact bytes covered by the signature.
    pub fn signing_bytes(&self) -> Vec<u8> {
        canonical(&[
            REVOCATION_DOMAIN,
            &self.team_id,
            &self.peer_id,
            &self.issued_at.to_string(),
            &self.nonce,
        ])
    }

    /// Verify the revocation against the pinned admin key.
    pub fn verify(&self, admin: &EndpointId) -> bool {
        let Some(signature) = hex_decode_64(&self.sig) else {
            return false;
        };
        admin
            .verify(&self.signing_bytes(), &Signature::from_bytes(&signature))
            .is_ok()
    }

    /// Parse the revoked peer id.
    pub fn peer(&self) -> Result<EndpointId> {
        self.peer_id
            .parse()
            .map_err(|_| Error::InvalidRequest(format!("invalid peer id {}", self.peer_id)))
    }
}

/// An admin-signed, single-use invitation to join a team.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Invitation {
    /// Team being joined.
    pub team_id: String,
    /// Display name of the team.
    pub team_name: String,
    /// Peer id of the admin that issued the invitation.
    pub admin_peer_id: String,
    /// How to reach the admin. Removes the need to know any IP address.
    pub admin_addr: EndpointAddr,
    /// Single-use nonce.
    pub nonce: String,
    /// Expiry time (unix seconds).
    pub expires_at: i64,
    /// Hex encoded ed25519 signature by the admin.
    pub sig: String,
}

impl Invitation {
    /// Issue a new invitation that is valid for `ttl_seconds`.
    pub fn issue(
        admin: &SecretKey,
        team_id: &str,
        team_name: &str,
        admin_addr: EndpointAddr,
        ttl_seconds: i64,
    ) -> Self {
        let mut invitation = Self {
            team_id: team_id.to_string(),
            team_name: team_name.to_string(),
            admin_peer_id: admin.public().to_string(),
            admin_addr,
            nonce: random_hex(16),
            expires_at: now_seconds() + ttl_seconds,
            sig: String::new(),
        };
        invitation.sig = hex_encode(&admin.sign(&invitation.signing_bytes()).to_bytes());
        invitation
    }

    /// The exact bytes covered by the signature.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let addr = serde_json::to_string(&self.admin_addr).unwrap_or_default();
        canonical(&[
            INVITE_DOMAIN,
            &self.team_id,
            &self.team_name,
            &self.admin_peer_id,
            &addr,
            &self.nonce,
            &self.expires_at.to_string(),
        ])
    }

    /// Verify the invitation is self-consistent and signed by its admin.
    pub fn verify(&self) -> bool {
        let Ok(admin) = self.admin() else {
            return false;
        };
        let Some(signature) = hex_decode_64(&self.sig) else {
            return false;
        };
        admin
            .verify(&self.signing_bytes(), &Signature::from_bytes(&signature))
            .is_ok()
    }

    /// True when the invitation has not expired.
    pub fn is_expired(&self) -> bool {
        now_seconds() > self.expires_at
    }

    /// The admin peer id.
    pub fn admin(&self) -> Result<EndpointId> {
        self.admin_peer_id.parse().map_err(|_| {
            Error::InvalidRequest(format!("invalid admin peer id {}", self.admin_peer_id))
        })
    }

    /// Encode the invitation as a single copy-pasteable token.
    pub fn to_token(&self) -> Result<String> {
        let json = serde_json::to_vec(self)?;
        Ok(format!(
            "{INVITE_TOKEN_PREFIX}:{}",
            BASE64URL_NOPAD.encode(&json)
        ))
    }

    /// Decode a token produced by [`Invitation::to_token`].
    pub fn from_token(token: &str) -> Result<Self> {
        let token = token.trim();
        let body = token
            .strip_prefix(INVITE_TOKEN_PREFIX)
            .and_then(|rest| rest.strip_prefix(':'))
            .ok_or_else(|| Error::InvalidRequest("not a reason-network invitation".into()))?;
        let bytes = BASE64URL_NOPAD
            .decode(body.as_bytes())
            .map_err(|error| Error::InvalidRequest(format!("damaged invitation token: {error}")))?;
        let invitation: Invitation = serde_json::from_slice(&bytes)
            .map_err(|error| Error::InvalidRequest(format!("invalid invitation token: {error}")))?;
        if !invitation.verify() {
            return Err(Error::InvalidRequest(
                "invitation signature does not verify".into(),
            ));
        }
        Ok(invitation)
    }
}

/// Generate a fresh team identifier.
pub fn new_team_id() -> String {
    format!("team-{}", random_hex(16))
}

/// Build the canonical byte string that gets signed.
///
/// A JSON array of strings is unambiguous (each field is escaped) and
/// deterministic (fixed order), which avoids delimiter-injection tricks.
fn canonical(fields: &[&str]) -> Vec<u8> {
    let encoded: Vec<&str> = fields.to_vec();
    serde_json::to_vec(&encoded).unwrap_or_default()
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or_default()
}

/// Random bytes from the operating system CSPRNG.
///
/// `SecretKey::generate` is the platform CSPRNG that iroh already relies on,
/// so we reuse it rather than pulling in a second randomness crate.
fn random_hex(len: usize) -> String {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        out.extend_from_slice(&SecretKey::generate().to_bytes());
    }
    out.truncate(len);
    hex_encode(&out)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn hex_decode_64(text: &str) -> Option<[u8; 64]> {
    if text.len() != 128 {
        return None;
    }
    let mut out = [0u8; 64];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        out[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin() -> SecretKey {
        SecretKey::generate()
    }

    #[test]
    fn membership_records_verify_only_for_the_admin() {
        let admin = admin();
        let record = MembershipRecord::issue(
            &admin,
            "team-1",
            "core",
            &SecretKey::generate().public().to_string(),
            "alice",
        );
        assert!(record.verify(&admin.public()));
        assert!(!record.verify(&SecretKey::generate().public()));
    }

    #[test]
    fn a_tampered_record_is_rejected() {
        let admin = admin();
        let mut record = MembershipRecord::issue(
            &admin,
            "team-1",
            "core",
            &SecretKey::generate().public().to_string(),
            "alice",
        );
        record.member_name = "mallory".into();
        assert!(!record.verify(&admin.public()));
    }

    #[test]
    fn a_peer_cannot_mint_its_own_membership_record() {
        let admin = admin();
        let attacker = SecretKey::generate();
        // The attacker signs a record claiming to be issued for itself.
        let forged = MembershipRecord::issue(
            &attacker,
            "team-1",
            "core",
            &attacker.public().to_string(),
            "mallory",
        );
        // It only verifies against the attacker's own key, never the team admin.
        assert!(!forged.verify(&admin.public()));
    }

    #[test]
    fn invitations_round_trip_through_tokens() {
        let admin = admin();
        let addr = EndpointAddr::new(admin.public());
        let invitation = Invitation::issue(&admin, "team-1", "core", addr, 600);
        let token = invitation.to_token().unwrap();
        let decoded = Invitation::from_token(&token).unwrap();
        assert_eq!(decoded, invitation);
        assert!(decoded.verify());
        assert!(!decoded.is_expired());
    }

    #[test]
    fn a_tampered_token_is_rejected() {
        let admin = admin();
        let invitation = Invitation::issue(
            &admin,
            "team-1",
            "core",
            EndpointAddr::new(admin.public()),
            600,
        );
        let token = invitation.to_token().unwrap();

        let mut tampered = invitation.clone();
        tampered.team_name = "evil".into();
        let forged = format!(
            "rn1:{}",
            BASE64URL_NOPAD.encode(&serde_json::to_vec(&tampered).unwrap())
        );
        assert!(Invitation::from_token(&forged).is_err());
        // The original still works.
        assert!(Invitation::from_token(&token).is_ok());
    }

    #[test]
    fn expired_invitations_are_flagged() {
        let admin = admin();
        let mut invitation = Invitation::issue(
            &admin,
            "team-1",
            "core",
            EndpointAddr::new(admin.public()),
            600,
        );
        invitation.expires_at = now_seconds() - 1;
        // Re-sign so only expiry, not the signature, is being tested.
        invitation.sig = hex_encode(&admin.sign(&invitation.signing_bytes()).to_bytes());
        assert!(invitation.is_expired());
    }

    #[test]
    fn revocations_verify_only_for_the_admin() {
        let admin = admin();
        let victim = SecretKey::generate().public().to_string();
        let record = RevocationRecord::issue(&admin, "team-1", &victim);

        assert!(record.verify(&admin.public()));
        assert!(!record.verify(&SecretKey::generate().public()));
    }

    #[test]
    fn a_tampered_or_forged_revocation_is_rejected() {
        let admin = admin();
        let victim = SecretKey::generate().public().to_string();
        let mut record = RevocationRecord::issue(&admin, "team-1", &victim);
        record.team_id = "team-2".into();
        assert!(!record.verify(&admin.public()));

        // A member signing their own revocation for someone else is not the admin.
        let attacker = SecretKey::generate();
        let forged = RevocationRecord::issue(&attacker, "team-1", &victim);
        assert!(!forged.verify(&admin.public()));
    }

    #[test]
    fn revocation_and_membership_use_different_domains() {
        let admin = admin();
        let victim = SecretKey::generate().public().to_string();
        let revocation = RevocationRecord::issue(&admin, "team-1", &victim);
        // A revocation signature must not validate as a membership record.
        let membership = MembershipRecord {
            team_id: revocation.team_id.clone(),
            team_name: "core".into(),
            peer_id: revocation.peer_id.clone(),
            member_name: "x".into(),
            issued_at: revocation.issued_at,
            nonce: revocation.nonce.clone(),
            sig: revocation.sig.clone(),
        };
        assert!(!membership.verify(&admin.public()));
    }

    #[test]
    fn garbage_tokens_are_rejected() {
        assert!(Invitation::from_token("nope").is_err());
        assert!(Invitation::from_token("rn1:!!!!").is_err());
        assert!(Invitation::from_token("rn1:eyJhIjoxfQ").is_err());
    }
}
