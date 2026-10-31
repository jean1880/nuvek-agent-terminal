//! Session-owned bounded stdio MCP clients and process supervision.
mod client;
mod process;
mod transport;

pub use client::{ConnectionLease, Discovery, ServerDiscovery, Session};
pub use process::Scrubber as CredentialMasks;
