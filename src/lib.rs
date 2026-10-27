//! Reason Network — peer-to-peer messaging for AI coding agents.
//!
//! Milestone 0 built the local foundation (daemon, SQLite, IPC, CLI, MCP).
//! Milestone 1 adds encrypted machine-to-machine team networking on iroh.
//! Milestone 2 adds first-class agent integrations, diagnostics and a
//! background service; milestone 3 adds release packaging and updates.
//!
//! The crate is split into small, independent modules:
//!
//! * [`config`] — where state lives and how clients find the daemon.
//! * [`protocol`] — the newline-delimited JSON request/reply contract.
//! * [`store`] — the SQLite owner of all application state.
//! * [`ipc`] — local transport (Windows named pipes / Unix domain sockets).
//! * [`daemon`] — the single owner of state; dispatches requests.
//! * [`delivery`] — optional push delivery into a live agent context.
//! * [`mcp`] — a thin MCP server that speaks to the daemon over IPC.
//! * [`identity`] — the persistent machine key and peer id.
//! * [`team`] — teams, invitations and signed membership records.
//! * [`net`] — the iroh endpoint, wire protocol and peer connections.
//! * [`agents`] — Claude Code and Codex setup and session identity.
//! * [`hook`] — agent lifecycle hooks (status, context, unread notices).
//! * [`doctor`] — read-only diagnostics.
//! * [`service`] — user-level background service definitions.
//! * [`update`] — release artifacts, checksums and explicit self-update.
//! * [`util`] — hashing and safe file writes.

pub mod agents;
pub mod config;
pub mod daemon;
pub mod delivery;
pub mod doctor;
pub mod error;
pub mod hook;
pub mod identity;
pub mod ipc;
pub mod mcp;
pub mod net;
pub mod protocol;
pub mod service;
pub mod store;
pub mod team;
pub mod update;
pub mod util;

pub use error::{Error, Result};
