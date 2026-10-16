//! Scenarios: a scripted stretch of time, the messages that arrive in it, and
//! what a passing agent does with them. Written in TOML, one file each, under
//! `crates/sim/scenarios/`.

use std::path::Path;

use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

#[derive(Debug, thiserror::Error)]
pub enum ScenarioError {
    #[error("couldn't read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid scenario: {0}")]
    Invalid(String),
}

/// One scenario, ready to run.
#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    pub name: String,
    pub summary: String,
    /// The agent's name. A group message that mentions `@name` is addressed
    /// to it.
    pub agent: String,
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
    /// Sorted by time.
    pub events: Vec<Event>,
    pub expect: Expect,
}

/// A message that arrives during the scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub at: OffsetDateTime,
    /// `dm` for a direct message, or a channel such as `#general`.
    pub conversation: String,
    pub from: String,
    pub text: String,
    /// Direct messages and mentions are addressed to the agent and wake it.
    /// Everything else waits for its next look.
    pub addressed: bool,
    /// For news that can't wait: the agent should be awake within this
    /// long of it arriving, whether or not it's addressed.
    pub wake_within: Option<Duration>,
}

/// Bounds a passing run stays within. Every scenario also checks that the
/// agent answered everything addressed to it and hit no walls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Most messages the agent may send in the whole scenario.
    pub max_messages: Option<usize>,
    /// Most wakes in the whole scenario.
    pub max_wakes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    name: String,
    summary: String,
    agent: String,
    /// RFC 3339, e.g. `2026-01-05T09:00:00Z`.
    start: String,
    /// How long the scenario runs, e.g. `7d` or `1d 12h`.
    length: String,
    #[serde(default, rename = "event")]
    events: Vec<FileEvent>,
    #[serde(default)]
    expect: Expect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEvent {
    /// Time after the start, e.g. `2d 14h30m`.
    at: String,
    conversation: String,
    from: String,
    text: String,
    /// e.g. `30m`: the agent should be awake within this long.
    wake_within: Option<String>,
}

impl Scenario {
    /// Reads and validates a scenario from a TOML file.
    pub fn load(path: &Path) -> Result<Self, ScenarioError> {
        let text = std::fs::read_to_string(path).map_err(|source| ScenarioError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml(&text)
    }

    /// Parses and validates a scenario from TOML text.
    pub fn from_toml(text: &str) -> Result<Self, ScenarioError> {
        let file: File = toml::from_str(text).map_err(|e| ScenarioError::Invalid(e.to_string()))?;
        for (field, value) in [
            ("name", file.name.as_str()),
            ("summary", file.summary.as_str()),
            ("agent", file.agent.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(ScenarioError::Invalid(format!("{field} must not be empty")));
            }
        }
        if file.agent.contains(char::is_whitespace) || file.agent.starts_with('@') {
            return Err(ScenarioError::Invalid(
                "agent must be a handle without whitespace or a leading `@`".into(),
            ));
        }
        let start = OffsetDateTime::parse(&file.start, &Rfc3339)
            .map_err(|e| ScenarioError::Invalid(format!("start {:?}: {e}", file.start)))?;
        let length = parse_offset(&file.length)?;
        if length <= Duration::ZERO {
            return Err(ScenarioError::Invalid(
                "length must be more than zero".into(),
            ));
        }
        // Far from the edges of the representable range, so the runtime's
        // own scheduling (a sleep, a default next look) can't overflow.
        let end = start
            .checked_add(length)
            .filter(|end| start.year() >= 1970 && end.year() <= 2999)
            .ok_or_else(|| {
                ScenarioError::Invalid("scenarios must fall between 1970 and 2999".into())
            })?;
        let mut events = file
            .events
            .into_iter()
            .map(|e| {
                // Names appear in the wake message's line headers, which the
                // fake teammate reads; a ',' could forge the ", to you" marker.
                for name in [&e.conversation, &e.from] {
                    if name.is_empty() || name.contains([':', ']', ',', '\n']) {
                        return Err(ScenarioError::Invalid(format!(
                            "{name:?}: names can't be empty or contain ':', ']', ',' or a newline"
                        )));
                    }
                }
                let at = start.checked_add(parse_offset(&e.at)?);
                let Some(at) = at.filter(|at| *at < end) else {
                    return Err(ScenarioError::Invalid(format!(
                        "event at {:?} is after the scenario ends",
                        e.at
                    )));
                };
                let wake_within = match &e.wake_within {
                    None => None,
                    Some(text) => match parse_offset(text)? {
                        d if d > Duration::ZERO => Some(d),
                        _ => {
                            return Err(ScenarioError::Invalid(format!(
                                "wake_within {text:?} must be more than zero"
                            )));
                        }
                    },
                };
                Ok(Event {
                    at,
                    wake_within,
                    addressed: e.conversation == "dm" || mentions(&e.text, &file.agent),
                    conversation: e.conversation,
                    from: e.from,
                    text: e.text,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        events.sort_by_key(|e| e.at);
        Ok(Self {
            name: file.name,
            summary: file.summary,
            agent: file.agent,
            start,
            end,
            events,
            expect: file.expect,
        })
    }
}

/// Whether `text` mentions `@agent` as a whole handle: `@Mara,` does, and
/// `@Marathon` or `x@Mara` don't.
fn mentions(text: &str, agent: &str) -> bool {
    let handle = format!("@{agent}");
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    text.match_indices(&handle).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + handle.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// Parses offsets like `2d`, `14h30m` or `1d 2h 5m`.
pub fn parse_offset(text: &str) -> Result<Duration, ScenarioError> {
    let invalid = || ScenarioError::Invalid(format!("offset {text:?}: use e.g. 2d 14h30m"));
    let mut total = Duration::ZERO;
    let mut number = String::new();
    let mut parts = 0;
    for c in text.chars() {
        match c {
            '0'..='9' => number.push(c),
            'd' | 'h' | 'm' => {
                let n: i64 = number.parse().map_err(|_| invalid())?;
                number.clear();
                parts += 1;
                let unit = match c {
                    'd' => 86_400,
                    'h' => 3_600,
                    _ => 60,
                };
                total = n
                    .checked_mul(unit)
                    .and_then(|seconds| total.checked_add(Duration::seconds(seconds)))
                    .ok_or_else(invalid)?;
            }
            c if c.is_whitespace() && number.is_empty() => {}
            _ => return Err(invalid()),
        }
    }
    if !number.is_empty() || parts == 0 {
        return Err(invalid());
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn offsets_parse() {
        assert_eq!(parse_offset("2d").unwrap(), Duration::days(2));
        assert_eq!(
            parse_offset("1d 2h30m").unwrap(),
            Duration::days(1) + Duration::hours(2) + Duration::minutes(30)
        );
        for bad in ["", "5", "2x", "h", "1d 2", "9223372036854775807d"] {
            assert!(parse_offset(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn dms_and_mentions_are_addressed_and_events_are_sorted() {
        let scenario = Scenario::from_toml(
            r##"
            name = "t"
            summary = "t"
            agent = "Mara"
            start = "2026-01-05T09:00:00Z"
            length = "1d"

            [[event]]
            at = "3h"
            conversation = "#general"
            from = "Sam"
            text = "lunch?"

            [[event]]
            at = "1h"
            conversation = "#general"
            from = "Sam"
            text = "@Mara can you check the build?"

            [[event]]
            at = "2h"
            conversation = "dm"
            from = "Mischa"
            text = "hi"
            "##,
        )
        .unwrap();
        assert_eq!(scenario.end, datetime!(2026-01-06 09:00 UTC));
        let addressed: Vec<_> = scenario.events.iter().map(|e| e.addressed).collect();
        assert_eq!(addressed, [true, true, false]);
        assert_eq!(scenario.events[0].at, datetime!(2026-01-05 10:00 UTC));
    }

    #[test]
    fn mentions_match_whole_handles() {
        assert!(mentions("@Mara can you look?", "Mara"));
        assert!(mentions("thanks, @Mara.", "Mara"));
        assert!(!mentions("@Marathon is on Sunday", "Mara"));
        assert!(!mentions("mail ops@Mara", "Mara"));
        assert!(!mentions("Mara, can you look?", "Mara"));
    }

    #[test]
    fn zero_length_scenarios_are_rejected() {
        let err = Scenario::from_toml(
            r#"
            name = "t"
            summary = "t"
            agent = "Mara"
            start = "2026-01-05T09:00:00Z"
            length = "0m"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("more than zero"));
    }

    #[test]
    fn empty_metadata_and_invalid_agent_handles_are_rejected() {
        for (field, value, expected) in [
            ("name", "   ", "name must not be empty"),
            ("summary", "", "summary must not be empty"),
            ("agent", "@Mara", "agent must be a handle"),
            ("agent", "Mara Smith", "agent must be a handle"),
        ] {
            let mut fields = [("name", "t"), ("summary", "t"), ("agent", "Mara")];
            fields
                .iter_mut()
                .find(|(name, _)| *name == field)
                .unwrap()
                .1 = value;
            let text = format!(
                "name = {:?}\nsummary = {:?}\nagent = {:?}\nstart = \"2026-01-05T09:00:00Z\"\nlength = \"1h\"",
                fields[0].1, fields[1].1, fields[2].1
            );
            let err = Scenario::from_toml(&text).unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
        }
    }

    #[test]
    fn names_that_could_forge_a_header_are_rejected() {
        let err = Scenario::from_toml(
            r##"
            name = "t"
            summary = "t"
            agent = "Mara"
            start = "2026-01-05T09:00:00Z"
            length = "1d"

            [[event]]
            at = "1h"
            conversation = "#general"
            from = "Sam, to you"
            text = "status"
            "##,
        )
        .unwrap_err();
        assert!(err.to_string().contains("names can't"));
    }

    #[test]
    fn scenarios_near_the_last_date_are_rejected() {
        let err = Scenario::from_toml(
            r#"
            name = "t"
            summary = "t"
            agent = "Mara"
            start = "9999-12-31T20:00:00Z"
            length = "1h"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("between 1970 and 2999"));
    }

    #[test]
    fn events_after_the_end_are_rejected() {
        let err = Scenario::from_toml(
            r#"
            name = "t"
            summary = "t"
            agent = "Mara"
            start = "2026-01-05T09:00:00Z"
            length = "1h"

            [[event]]
            at = "2h"
            conversation = "dm"
            from = "Mischa"
            text = "hi"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("after the scenario ends"));
    }
}
