//! Persistent machine identity.
//!
//! Each machine owns one ed25519 key, generated on first start and stored in
//! the application data directory. The peer id is the public half of that key,
//! so it is stable across restarts and independent of any agent session.
//!
//! The private key is never printed, logged or sent anywhere: only signatures
//! and the public peer id leave this module.

use std::path::{Path, PathBuf};

use iroh::{EndpointId, SecretKey, Signature};

use crate::error::{Error, Result};

/// File name of the machine key inside the data directory.
pub const KEY_FILE: &str = "machine.key";

/// The identity of this machine.
pub struct MachineIdentity {
    secret: SecretKey,
    path: PathBuf,
}

impl MachineIdentity {
    /// Load the key from `data_dir`, generating and persisting one if needed.
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join(KEY_FILE);
        let secret = if path.exists() {
            let contents = std::fs::read_to_string(&path)?;
            let bytes = decode_hex(contents.trim()).ok_or_else(|| {
                Error::Other(format!(
                    "machine key file {} is corrupt; delete it to generate a new identity",
                    path.display()
                ))
            })?;
            SecretKey::from_bytes(&bytes)
        } else {
            let secret = SecretKey::generate();
            std::fs::write(&path, encode_hex(&secret.to_bytes()))?;
            restrict_permissions(&path)?;
            secret
        };
        Ok(Self { secret, path })
    }

    /// The public peer id of this machine.
    pub fn peer_id(&self) -> EndpointId {
        self.secret.public()
    }

    /// The peer id as the stable string form used in storage and on the wire.
    pub fn peer_id_string(&self) -> String {
        self.peer_id().to_string()
    }

    /// Sign a message with the machine key.
    pub fn sign(&self, message: &[u8]) -> Signature {
        self.secret.sign(message)
    }

    /// The secret key, used to build the iroh endpoint.
    pub fn secret(&self) -> &SecretKey {
        &self.secret
    }

    /// Path of the key file, for diagnostics. Never the key itself.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Restrict the key file to the current user where the platform supports it.
fn restrict_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)?;
    }
    #[cfg(windows)]
    {
        // Files under %APPDATA% already inherit the user's profile ACL.
        let _ = path;
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn decode_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        out[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let first = MachineIdentity::load_or_create(dir.path()).unwrap();
        let second = MachineIdentity::load_or_create(dir.path()).unwrap();
        assert_eq!(first.peer_id(), second.peer_id());
        assert!(dir.path().join(KEY_FILE).exists());
    }

    #[test]
    fn stored_key_is_not_the_peer_id() {
        let dir = tempfile::tempdir().unwrap();
        let identity = MachineIdentity::load_or_create(dir.path()).unwrap();
        let stored = std::fs::read_to_string(dir.path().join(KEY_FILE)).unwrap();
        assert_eq!(stored.trim().len(), 64);
        assert_ne!(stored.trim(), identity.peer_id_string());
    }

    #[test]
    fn signatures_verify_against_the_peer_id() {
        let dir = tempfile::tempdir().unwrap();
        let identity = MachineIdentity::load_or_create(dir.path()).unwrap();
        let signature = identity.sign(b"hello");
        assert!(identity.peer_id().verify(b"hello", &signature).is_ok());
        assert!(identity.peer_id().verify(b"tampered", &signature).is_err());
    }
}
