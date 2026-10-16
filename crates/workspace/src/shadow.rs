//! Shadow mode, for an agent's first real week: everything it would say
//! goes to one private channel instead, except in the conversations its
//! owner marks live (their direct message with it, say). Its owner reads
//! what it would have said and reacts there.
//!
//! A reply or release meant for anywhere else is posted to the channel, with
//! where it was going, and the request is left alone in Workspace. The
//! runtime remembers it, so it doesn't wake the agent again or show up in
//! its briefing. That memory lasts until the daemon restarts.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde_json::Value;

/// Where one claimed request's answer goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// To the conversation it came from.
    Live,
    /// To the shadow channel instead.
    Shadow(Claimed),
    /// Not claimed since the daemon started, so where it came from is
    /// unknown.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Claimed {
    pub conversation_name: String,
    pub required: bool,
}

#[derive(Debug)]
pub struct Shadow {
    channel: String,
    live: HashSet<String>,
    claimed: Mutex<HashMap<String, (String, Claimed)>>,
    shadowed: Mutex<HashSet<String>>,
}

impl Shadow {
    /// `channel` and each of `live` are Workspace conversation ids.
    pub fn new(channel: &str, live: &[String]) -> Result<Self, String> {
        for id in std::iter::once(channel).chain(live.iter().map(String::as_str)) {
            if !is_id(id) {
                return Err("shadow conversations must be Workspace conversation ids".into());
            }
        }
        Ok(Self {
            channel: channel.to_string(),
            live: live.iter().cloned().collect(),
            claimed: Mutex::default(),
            shadowed: Mutex::default(),
        })
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// Whether the agent's answer to `request_id` went to the shadow channel.
    pub fn is_shadowed(&self, request_id: &str) -> bool {
        lock(&self.shadowed).contains(request_id)
    }

    /// Notes where a claimed request came from, from `reply_claim`'s value.
    pub(crate) fn remember(&self, request_id: &str, claim: &Value) {
        let request = &claim["request"];
        let Some(conversation) = request["conversationId"].as_str() else {
            return;
        };
        let claimed = Claimed {
            conversation_name: request["conversationName"]
                .as_str()
                .unwrap_or("a conversation")
                .to_string(),
            // Unsure means required: it can't then be passed quietly.
            required: request["required"].as_bool().unwrap_or(true),
        };
        lock(&self.claimed).insert(request_id.to_string(), (conversation.to_string(), claimed));
    }

    pub(crate) fn route(&self, request_id: &str) -> Route {
        match lock(&self.claimed).get(request_id) {
            None => Route::Unknown,
            Some((conversation, _))
                if *conversation == self.channel || self.live.contains(conversation) =>
            {
                Route::Live
            }
            Some((_, claimed)) => Route::Shadow(claimed.clone()),
        }
    }

    pub(crate) fn mark(&self, request_id: &str) {
        lock(&self.shadowed).insert(request_id.to_string());
    }

    /// Drops requests already answered in the shadow channel from a
    /// briefing, wherever they're listed.
    pub(crate) fn hide_answered(&self, value: &mut Value) {
        let shadowed = lock(&self.shadowed);
        if !shadowed.is_empty() {
            hide(value, &shadowed);
        }
    }
}

fn hide(value: &mut Value, shadowed: &HashSet<String>) {
    match value {
        Value::Array(items) => {
            items.retain(|item| {
                !item["requestId"]
                    .as_str()
                    .is_some_and(|id| shadowed.contains(id))
            });
            items.iter_mut().for_each(|item| hide(item, shadowed));
        }
        Value::Object(fields) => fields.values_mut().for_each(|v| hide(v, shadowed)),
        _ => {}
    }
}

/// What the shadow channel shows for an answer meant for elsewhere. An `@`
/// is followed by a zero-width space so handles stay text: a mention would
/// reach the person it names.
pub(crate) fn post(heading: &str, text: &str, limit: usize) -> String {
    let text = text.replace('@', "@\u{200B}");
    let mut post = format!("{heading}\n\n{text}");
    if post.chars().count() > limit {
        post = post.chars().take(limit - 1).collect::<String>() + "…";
    }
    post
}

fn is_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claim(conversation: &str, required: bool) -> Value {
        json!({"request": {
            "requestId": "r", "conversationId": conversation,
            "conversationName": "General", "required": required
        }})
    }

    #[test]
    fn answers_go_live_only_where_the_owner_said() {
        let shadow = Shadow::new("shadow-1", &["dm-owner".into()]).unwrap();
        assert_eq!(shadow.route("r-1"), Route::Unknown);
        shadow.remember("r-1", &claim("dm-owner", true));
        shadow.remember("r-2", &claim("shadow-1", false));
        shadow.remember("r-3", &claim("general", false));
        assert_eq!(shadow.route("r-1"), Route::Live);
        assert_eq!(shadow.route("r-2"), Route::Live);
        assert_eq!(
            shadow.route("r-3"),
            Route::Shadow(Claimed {
                conversation_name: "General".into(),
                required: false
            })
        );
        // A claim that doesn't say is treated as required.
        shadow.remember("r-4", &json!({"request": {"conversationId": "x"}}));
        assert!(matches!(shadow.route("r-4"), Route::Shadow(c) if c.required));
        assert!(Shadow::new("../x", &[]).is_err());
        assert!(Shadow::new("ok", &["".into()]).is_err());
    }

    #[test]
    fn answered_requests_leave_the_briefing() {
        let shadow = Shadow::new("shadow-1", &[]).unwrap();
        shadow.mark("r-1");
        let mut briefing = json!({
            "replyRequests": [{"requestId": "r-1"}, {"requestId": "r-2"}],
            "mentions": [{"messageId": "m", "requestId": "r-1"}],
            "tasks": [{"taskId": "t"}]
        });
        shadow.hide_answered(&mut briefing);
        assert_eq!(
            briefing,
            json!({
                "replyRequests": [{"requestId": "r-2"}],
                "mentions": [],
                "tasks": [{"taskId": "t"}]
            })
        );
        assert!(shadow.is_shadowed("r-1"));
        assert!(!shadow.is_shadowed("r-2"));
    }

    #[test]
    fn shadow_posts_mention_no_one_and_fit() {
        let shown = post("Would reply in General:", "thanks @mara!", 100);
        assert_eq!(shown, "Would reply in General:\n\nthanks @\u{200B}mara!");
        let long = post("Heading", &"x".repeat(50), 20);
        assert_eq!(long.chars().count(), 20);
        assert!(long.ends_with('…'));
    }
}
