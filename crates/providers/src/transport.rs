//! How adapters reach a model endpoint. Every call goes to the Agent OS
//! broker or the daemon's forwarding proxy, which inject provider keys, so
//! adapters never hold or send credentials.

use async_trait::async_trait;
use serde_json::Value;

/// Model responses are text and should be far smaller than this. Bounding
/// them prevents a broken or hostile proxy from exhausting the daemon's RAM.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// The header that proves to the daemon's forwarding proxy that a call
/// comes from the runtime. The proxy refuses calls without it.
pub const CALLER_TOKEN_HEADER: &str = "x-harness-proxy-token";

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
    caller_token: Option<ScopedToken>,
}

/// The caller token and the one origin it may be sent to.
#[cfg(feature = "http")]
#[derive(Debug, Clone)]
struct ScopedToken {
    origin: reqwest::Url,
    // Marked sensitive, so debug output never shows it.
    value: reqwest::header::HeaderValue,
}

#[cfg(feature = "http")]
impl ReqwestTransport {
    /// `timeout` bounds one whole request, including a slow model's full answer.
    pub fn new(timeout: std::time::Duration) -> Result<Self, TransportError> {
        Self::build(timeout, None)
    }

    /// For the runtime's calls through the daemon's forwarding proxy at
    /// `proxy_url`: requests to that origin carry `token` in
    /// [`CALLER_TOKEN_HEADER`], and requests anywhere else never do. The
    /// proxy must be on loopback, so the token never leaves the machine.
    /// Only the runtime is built this way; workers never get the token.
    pub fn with_caller_token(
        timeout: std::time::Duration,
        proxy_url: &str,
        token: &str,
    ) -> Result<Self, TransportError> {
        let origin = proxy_origin(proxy_url)?;
        let mut value = reqwest::header::HeaderValue::from_str(token.trim())
            .map_err(|_| TransportError("the caller token is not a valid header value".into()))?;
        value.set_sensitive(true);
        Self::build(timeout, Some(ScopedToken { origin, value }))
    }

    /// [`Self::with_caller_token`], reading the token from the file the
    /// daemon also reads. The error never quotes the path.
    pub fn with_caller_token_file(
        timeout: std::time::Duration,
        proxy_url: &str,
        path: &std::path::Path,
    ) -> Result<Self, TransportError> {
        let token = std::fs::read_to_string(path)
            .map_err(|e| TransportError(format!("couldn't read the caller token file: {e}")))?;
        Self::with_caller_token(timeout, proxy_url, &token)
    }

    fn build(
        timeout: std::time::Duration,
        caller_token: Option<ScopedToken>,
    ) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // Never route private agent context through ambient HTTP proxy
            // environment variables. The explicit broker URL is the only
            // endpoint this transport trusts.
            .no_proxy()
            // The configured broker/proxy is the trust boundary. Following a
            // redirect could disclose the complete agent context to another
            // origin chosen by a compromised endpoint.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| TransportError(e.to_string()))?;
        Ok(Self {
            client,
            caller_token,
        })
    }

    /// The token, if `url` is on the proxy's origin.
    fn token_for(&self, url: &str) -> Option<&reqwest::header::HeaderValue> {
        let token = self.caller_token.as_ref()?;
        let url = reqwest::Url::parse(url).ok()?;
        (url.origin() == token.origin.origin()).then_some(&token.value)
    }
}

/// The proxy's origin, refusing anything not plain HTTP on loopback.
#[cfg(feature = "http")]
fn proxy_origin(proxy_url: &str) -> Result<reqwest::Url, TransportError> {
    let refused = || TransportError("the proxy URL must be http on a loopback address".into());
    let url = reqwest::Url::parse(proxy_url).map_err(|_| refused())?;
    let host = url.host_str().unwrap_or_default();
    let loopback = match host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
    {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback(),
        Ok(std::net::IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => host.eq_ignore_ascii_case("localhost") || host.eq_ignore_ascii_case("localhost."),
    };
    if url.scheme() != "http" || !loopback || !url.username().is_empty() || url.password().is_some()
    {
        return Err(refused());
    }
    Ok(url)
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
        let mut request = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(bytes);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(token) = self.token_for(url) {
            request = request.header(CALLER_TOKEN_HEADER, token.clone());
        }
        let mut response = request
            .send()
            .await
            .map_err(|e| TransportError(e.to_string()))?;
        let status = response.status().as_u16();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(TransportError("response exceeds 8 MiB limit".into()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| TransportError(e.to_string()))?
        {
            let remaining = MAX_RESPONSE_BYTES.saturating_sub(bytes.len());
            if chunk.len() > remaining {
                return Err(TransportError("response exceeds 8 MiB limit".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(bytes)
            .map_err(|_| TransportError("response body is not valid UTF-8".into()))?;
        Ok(HttpResponse { status, body })
    }
}

#[cfg(all(test, feature = "http"))]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    fn serve_once(response: String) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request);
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}"), handle)
    }

    /// Answers one request with 200 and hands back what was sent.
    fn capture_once() -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let read = stream.read(&mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            String::from_utf8_lossy(&request[..read]).to_lowercase()
        });
        (format!("http://{address}"), handle)
    }

    const TOKEN: &str = "a-caller-token-long-enough-for-the-proxy";

    #[tokio::test]
    async fn the_caller_token_goes_only_to_the_proxy() {
        let (proxy, proxy_server) = capture_once();
        let (elsewhere, elsewhere_server) = capture_once();
        let transport =
            ReqwestTransport::with_caller_token(Duration::from_secs(2), &proxy, TOKEN).unwrap();
        assert!(!format!("{transport:?}").contains(TOKEN));

        let path = format!("{elsewhere}/v1/messages");
        transport.post_json(&path, &[], &json!({})).await.unwrap();
        assert!(!elsewhere_server.join().unwrap().contains(TOKEN));

        let path = format!("{proxy}/anthropic/v1/messages");
        transport.post_json(&path, &[], &json!({})).await.unwrap();
        let sent = proxy_server.join().unwrap();
        assert!(sent.contains(&format!("{CALLER_TOKEN_HEADER}: {TOKEN}")));
    }

    #[test]
    fn the_proxy_must_be_plain_http_on_loopback() {
        for url in [
            "http://127.0.0.1:8411",
            "http://127.0.0.2:8411",
            "http://localhost:8411/",
            "http://[::1]:8411",
            "http://[::ffff:127.0.0.1]:8411",
        ] {
            assert!(
                ReqwestTransport::with_caller_token(Duration::from_secs(2), url, TOKEN).is_ok(),
                "{url}"
            );
        }
        for url in [
            "https://127.0.0.1:8411",
            "http://broker.example:8411",
            "http://10.0.0.1:8411",
            "http://localhost.evil.example:8411",
            "http://user:pw@127.0.0.1:8411",
            "not a url",
        ] {
            assert!(
                ReqwestTransport::with_caller_token(Duration::from_secs(2), url, TOKEN).is_err(),
                "{url}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_oversized_responses_before_reading_the_body() {
        let (url, server) = serve_once(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        ));
        let transport = ReqwestTransport::new(Duration::from_secs(2)).unwrap();
        let error = transport
            .post_json(&url, &[], &json!({}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("8 MiB"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn does_not_follow_cross_origin_redirects() {
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let location = format!("http://{}", destination.local_addr().unwrap());
        let (url, server) = serve_once(format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ));
        let transport = ReqwestTransport::new(Duration::from_secs(2)).unwrap();
        let response = transport.post_json(&url, &[], &json!({})).await.unwrap();
        assert_eq!(response.status, 307);
        assert!(
            matches!(destination.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        server.join().unwrap();
    }
}
