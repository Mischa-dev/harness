//! The scenario simulator: scripted stretches of time on a fake clock, run
//! through the real loop, store and policy executor, then scored. With the
//! fake teammate it checks the harness; with a real model it is the exam
//! (PLAN.md, "Testing").

pub mod fake;
pub mod report;
pub mod runner;
pub mod scenario;
pub mod world;

use std::path::{Path, PathBuf};

use harness_providers::{CachingStyle, CapabilityProfile, Pricing, ToolCallStyle};

pub use fake::FakeTeammate;
pub use report::{Check, Report};
pub use runner::{RunError, run};
pub use scenario::{Scenario, ScenarioError};

/// Where the bundled scenarios live.
pub fn scenarios_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios")
}

/// Finds a scenario by name in [`scenarios_dir`], or by path.
pub fn find_scenario(name_or_path: &str) -> PathBuf {
    let path = Path::new(name_or_path);
    if path.extension().is_some() || path.components().count() > 1 {
        return path.to_path_buf();
    }
    scenarios_dir().join(format!("{name_or_path}.toml"))
}

/// The capability profile the fake teammate runs under. It costs nothing.
pub fn fake_profile() -> CapabilityProfile {
    CapabilityProfile {
        adapter: "fake".into(),
        model: "fake".into(),
        context_window: 200_000,
        tool_calls: ToolCallStyle::Parallel,
        caching: CachingStyle::None,
        structured_output: false,
        cache_ttl: None,
        pricing: Pricing {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
    }
}
