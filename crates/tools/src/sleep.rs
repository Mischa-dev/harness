//! `sleep(minutes)`: how the agent ends a wake. The runtime keeps the choice
//! between a floor of a few minutes and a ceiling of once a day.

use async_trait::async_trait;
use harness_providers::ToolSpec;
use serde_json::{Value, json};
use time::Duration;

use crate::tool::{Control, Effect, Tool, ToolContext, ToolError, ToolOutput};

pub const MIN_SLEEP: Duration = Duration::minutes(5);
pub const MAX_SLEEP: Duration = Duration::hours(24);

#[derive(Debug, Default)]
pub struct SleepTool;

#[async_trait]
impl Tool for SleepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sleep".into(),
            description: "End this wake. Say when you want to look again, in minutes. \
                          Anything important that arrives sooner will still wake you."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "minutes": {"type": "integer", "description": "When to look again, from now"},
                    "watching_for": {"type": "string", "description": "What should wake you sooner"}
                },
                "required": ["minutes"]
            }),
        }
    }

    fn effects(&self, _args: &Value) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![])
    }

    async fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let minutes = args
            .get("minutes")
            .and_then(Value::as_i64)
            .ok_or_else(|| ToolError::InvalidArguments("minutes must be an integer".into()))?;
        let wanted = Duration::minutes(minutes);
        let chosen = wanted.clamp(MIN_SLEEP, MAX_SLEEP);
        let until = ctx.clock.now() + chosen;
        let mut content = format!("Sleeping until {until}.");
        if chosen != wanted {
            content.push_str(&format!(
                " (Adjusted from {minutes} minutes to stay between {} minutes and {} hours.)",
                MIN_SLEEP.whole_minutes(),
                MAX_SLEEP.whole_hours()
            ));
        }
        Ok(ToolOutput {
            content,
            control: Some(Control::Sleep { until }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Clock, FakeClock};

    async fn sleep_for(minutes: i64, clock: &FakeClock) -> ToolOutput {
        SleepTool
            .run(&json!({"minutes": minutes}), &ToolContext { clock })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn sleeps_for_the_chosen_time() {
        let clock = FakeClock::at_default_start();
        let out = sleep_for(180, &clock).await;
        assert_eq!(
            out.control,
            Some(Control::Sleep {
                until: clock.now() + Duration::hours(3)
            })
        );
    }

    #[tokio::test]
    async fn clamps_to_the_floor_and_ceiling() {
        let clock = FakeClock::at_default_start();
        let short = sleep_for(0, &clock).await;
        assert_eq!(
            short.control,
            Some(Control::Sleep {
                until: clock.now() + MIN_SLEEP
            })
        );
        let long = sleep_for(60 * 24 * 7, &clock).await;
        assert_eq!(
            long.control,
            Some(Control::Sleep {
                until: clock.now() + MAX_SLEEP
            })
        );
        assert!(long.content.contains("Adjusted"));
    }
}
