//! The daemon: one per agent. `run` is the wake loop, which calls models
//! through `proxy`, the forwarding proxy that adds provider keys. They run as
//! separate services, so the process running the agent's tools never holds a
//! provider key. The webhook receiver and the scheduler come next.

pub mod agent;
pub mod config;
pub mod proxy;
