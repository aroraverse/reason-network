//! LAN discovery.
//!
//! This module is the *only* place that knows about discovery, and discovery
//! never influences message routing: it only helps the endpoint resolve a peer
//! id it already trusts to a network address.
//!
//! M1 uses iroh's mDNS address lookup (`iroh-mdns-address-lookup`, backed by
//! `swarm-discovery`). Relay-based connectivity is handled by iroh itself and
//! configured in [`crate::net`]; discovery and connectivity stay separate.

use iroh::Endpoint;
use iroh_mdns_address_lookup::MdnsAddressLookup;

use crate::error::{Error, Result};

/// Attach LAN discovery to a bound endpoint.
///
/// Failing to start mDNS is not fatal: the daemon keeps working with explicit
/// addresses and relays.
pub fn enable_lan_discovery(endpoint: &Endpoint) -> Result<()> {
    let lookup = MdnsAddressLookup::builder()
        .build(endpoint.id())
        .map_err(|error| Error::Other(format!("cannot start LAN discovery: {error}")))?;

    let services = endpoint
        .address_lookup()
        .map_err(|error| Error::Other(format!("address lookup unavailable: {error}")))?;

    services.add(lookup);
    Ok(())
}
