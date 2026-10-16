//! The runtime's client for Agent Workspace, through its public MCP server
//! only: the briefing, reply requests, and the polling fallback that looks
//! at the inbox when a wakeup may have been missed, and shadow mode.

pub mod api;
pub mod client;
pub mod shadow;
pub mod tools;
pub mod watch;

pub use api::{BriefingCounts, InboxEntry, Outcome, Workspace};
pub use client::{AccessToken, McpClient, McpError};
pub use shadow::Shadow;
pub use tools::{tools, tools_with};
pub use watch::{InboxWatch, NewRequests};
