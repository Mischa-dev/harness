//! Agent basics shared by every other crate: the [`Clock`] all time goes
//! through, and the reasons an agent wakes.

pub mod clock;
pub mod wake;

pub use clock::{Clock, FakeClock, SystemClock};
pub use wake::{Urgency, WakeReason};
