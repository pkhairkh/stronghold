//! Session lifecycle management.
//!
//! Sessions are the unit of approval. One phone tap opens a TTL'd
//! workspace with full PTY. Destructive operations trigger quorum
//! re-approval mid-session.

pub mod manager;
pub mod scopes;
