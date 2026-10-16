//! The runtime's client for Agent Workspace, through its public MCP server
//! only: the briefing, reply requests, and the polling fallback that looks
//! at the inbox when a wakeup may have been missed.

pub mod api;
pub mod client;
pub mod tools;
pub mod watch;

pub use api::{InboxEntry, Outcome, Workspace};
pub use client::{AccessToken, McpClient, McpError};
pub use tools::tools;
pub use watch::{InboxWatch, NewRequests};
