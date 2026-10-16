//! The daemon: one per agent. `run` is the wake loop, which calls models
//! through `proxy`, the forwarding proxy that adds provider keys. They run as
//! separate services, so the process running the agent's tools never holds a
//! provider key. `wakeup` is the signed webhook receiver that wakes it when
//! Workspace has a reply request waiting, and `polling` looks at the inbox
//! in case a wakeup was missed. `filter` is the wake filter, which decides
//! on the helper model whether news can wait for the agent's next look.

pub mod agent;
pub mod breakers;
pub mod config;
pub mod filter;
pub mod polling;
pub mod proxy;
pub mod wakeup;
