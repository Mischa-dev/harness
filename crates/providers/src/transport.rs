//! How adapters reach a model endpoint. Every call goes to the Agent OS
//! broker or the daemon's forwarding proxy, which inject provider keys, so
//! adapters never hold or send credentials.

use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

#[derive(Debug, thiserror::Error)]
#[error("transport failed: {0}")]
pub struct TransportError(pub String);

#[async_trait]
pub trait Transport: Send + Sync {
    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<HttpResponse, TransportError>;
}

/// The real transport, over HTTPS with rustls.
#[cfg(feature = "http")]
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
    /// For the daemon's forwarding proxy and anything else on this machine:
    /// never through a system proxy, which would see the whole request in
    /// the clear.
    direct: reqwest::Client,
}

#[cfg(feature = "http")]
impl ReqwestTransport {
    /// `timeout` bounds one whole request, including a slow model's full answer.
    pub fn new(timeout: std::time::Duration) -> Result<Self, TransportError> {
        let builder = || {
            reqwest::Client::builder()
                .timeout(timeout)
                // A redirect would replay the request, conversation and all,
                // to wherever it points, possibly another proxy route that
                // adds a different key. The caller sees the 3xx instead.
                .redirect(reqwest::redirect::Policy::none())
        };
        let build =
            |b: reqwest::ClientBuilder| b.build().map_err(|e| TransportError(e.to_string()));
        Ok(Self {
            client: build(builder())?,
            direct: build(builder().no_proxy())?,
        })
    }
}

/// Whether `url` names this machine: `localhost`, `127.0.0.0/8` (also as
/// an IPv4-mapped IPv6 address) or `::1`.
#[cfg(feature = "http")]
pub fn is_loopback(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        // `::1`, or a 127.x address written as `::ffff:127.0.0.1`.
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        None => false,
    }
}

#[cfg(feature = "http")]
#[async_trait]
impl Transport for ReqwestTransport {
    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<HttpResponse, TransportError> {
        let bytes = serde_json::to_vec(body).map_err(|e| TransportError(e.to_string()))?;
        let client = if is_loopback(url) {
            &self.direct
        } else {
            &self.client
        };
        let mut request = client
            .post(url)
            .header("content-type", "application/json")
            .body(bytes);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request
            .send()
            .await
            .map_err(|e| TransportError(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| TransportError(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

#[cfg(all(test, feature = "http"))]
mod tests {
    use super::is_loopback;

    #[test]
    fn loopback_urls_are_recognised() {
        for yes in [
            "http://127.0.0.1:8787/anthropic/v1/messages",
            "http://localhost:8787/x",
            "http://LOCALHOST/x",
            "http://127.1.2.3/x",
            "http://[::1]:8787/x",
            "http://[::ffff:127.0.0.1]:8787/x",
        ] {
            assert!(is_loopback(yes), "{yes}");
        }
        for no in [
            "https://api.anthropic.com/v1/messages",
            "http://localhost.evil.com/x",
            "http://10.0.0.1/x",
            "http://[::ffff:10.0.0.1]/x",
            "not a url",
        ] {
            assert!(!is_loopback(no), "{no}");
        }
    }
}
