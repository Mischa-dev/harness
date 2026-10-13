//! The daemon: one per agent. This first slice is the forwarding proxy that
//! adds provider keys to model calls. The webhook receiver, the wake loop
//! and the scheduler come next.

pub mod config;
pub mod proxy;
