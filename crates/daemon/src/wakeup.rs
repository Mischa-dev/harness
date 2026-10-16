//! The signed webhook receiver: Agent Workspace posts `inbox.available` when
//! a reply request is waiting for the agent, and a valid wakeup becomes a
//! wake event. A wakeup only says "check your inbox", so a missed or repeated
//! one is harmless; the body never reaches the agent, only that it arrived.
//!
//! Workspace signs `"{timestamp}.{body}"` with HMAC-SHA256 and sends it as
//! `x-workspace-signature: sha256=<hex>`, with the Unix time in
//! `x-workspace-timestamp` (agent-workspace `apps/server/src/replies.rs`).

use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use harness_core::{Clock, WakeReason};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use tokio::sync::mpsc;

use crate::agent::WakeEvent;

/// Workspace's wakeups are a few hundred bytes.
pub const MAX_BODY_BYTES: usize = 4096;

/// How far a wakeup's timestamp may be from our clock, either way.
pub const MAX_SKEW_SECONDS: i64 = 300;

/// Delivery ids remembered to drop repeats. Older repeats fall outside the
/// skew window anyway.
const REMEMBERED_DELIVERIES: usize = 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WakeupConfig {
    /// Where Workspace can reach the daemon, such as a Tailscale address.
    pub listen: SocketAddr,
    /// This agent's id in Workspace. Wakeups for anyone else are refused.
    pub agent_id: String,
    /// The signing secret Workspace showed when the webhook was set.
    pub secret_file: PathBuf,
}

/// Workspace's signing secret. Never printed.
#[derive(Clone)]
pub struct WebhookSecret(Vec<u8>);

impl fmt::Debug for WebhookSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebhookSecret([redacted])")
    }
}

impl WebhookSecret {
    pub fn new(secret: &str) -> Result<Self, &'static str> {
        let secret = secret.trim();
        if secret.len() < 16 {
            return Err("the webhook secret is too short");
        }
        Ok(Self(secret.as_bytes().to_vec()))
    }

    fn verify(&self, timestamp: &str, body: &[u8], signature: &str) -> bool {
        let Some(sent) = signature.strip_prefix("sha256=").and_then(decode_hex) else {
            return false;
        };
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&self.0) else {
            return false;
        };
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(body);
        // Constant time.
        mac.verify_slice(&sent).is_ok()
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wakeup {
    event: String,
    delivery_id: String,
    agent_id: String,
}

struct Receiver {
    agent_id: String,
    secret: WebhookSecret,
    clock: Arc<dyn Clock>,
    events: mpsc::Sender<WakeEvent>,
    seen: Mutex<VecDeque<String>>,
}

/// The wakeup routes: `POST /wake` and nothing else.
pub fn router(
    agent_id: &str,
    secret: WebhookSecret,
    clock: Arc<dyn Clock>,
    events: mpsc::Sender<WakeEvent>,
) -> Router {
    let receiver = Arc::new(Receiver {
        agent_id: agent_id.trim().to_string(),
        secret,
        clock,
        events,
        seen: Mutex::new(VecDeque::new()),
    });
    Router::new()
        .route("/wake", post(wake))
        .with_state(receiver)
}

async fn wake(State(receiver): State<Arc<Receiver>>, request: Request) -> StatusCode {
    let (head, body) = request.into_parts();
    let too_long = head
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_BODY_BYTES as u64);
    if too_long {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }
    let Ok(body) = axum::body::to_bytes(Body::new(body), MAX_BODY_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE;
    };
    // Nothing in the body is read until the signature holds.
    let (timestamp, signature) = (
        header(&head.headers, "x-workspace-timestamp"),
        header(&head.headers, "x-workspace-signature"),
    );
    if !receiver.secret.verify(timestamp, &body, signature)
        || !fresh(timestamp, receiver.clock.as_ref())
    {
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(wakeup) = serde_json::from_slice::<Wakeup>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    if wakeup.event != "inbox.available" || wakeup.agent_id != receiver.agent_id {
        return StatusCode::NOT_FOUND;
    }
    if !receiver.first_delivery(&wakeup.delivery_id) {
        return StatusCode::ACCEPTED;
    }
    // A full queue means wakes are already waiting, and each one checks the
    // inbox, so this one can go.
    let _ = receiver.events.try_send(WakeEvent {
        reason: WakeReason::Workspace {
            summary: "a reply request is waiting in your Workspace inbox".into(),
            urgent: true,
        },
        text: "Workspace: a reply request is waiting in your inbox.".into(),
    });
    StatusCode::ACCEPTED
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

fn fresh(timestamp: &str, clock: &dyn Clock) -> bool {
    if timestamp.is_empty() || !timestamp.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    timestamp
        .parse::<i64>()
        .is_ok_and(|sent| (clock.now().unix_timestamp() - sent).abs() <= MAX_SKEW_SECONDS)
}

impl Receiver {
    /// Whether this delivery is new. Workspace retries until it hears back,
    /// so a repeat is normal and needs no second wake.
    fn first_delivery(&self, id: &str) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return true;
        };
        if seen.iter().any(|known| known == id) {
            return false;
        }
        if seen.len() == REMEMBERED_DELIVERIES {
            seen.pop_front();
        }
        seen.push_back(id.to_string());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_print() {
        let secret = WebhookSecret::new("whsec_abcdefghijklmnopqrstuvwxyz").unwrap();
        assert!(!format!("{secret:?}").contains("whsec"));
        assert!(WebhookSecret::new("short").is_err());
    }

    #[test]
    fn hex_decodes_strictly() {
        assert_eq!(decode_hex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(decode_hex("0"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(decode_hex("é0"), None);
    }
}
