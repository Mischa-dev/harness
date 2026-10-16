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
}

#[cfg(feature = "http")]
impl ReqwestTransport {
    /// `timeout` bounds one whole request, including a slow model's full answer.
    pub fn new(timeout: std::time::Duration) -> Result<Self, TransportError> {
        Self::build(timeout, reqwest::header::HeaderMap::new())
    }

    /// For the runtime's calls through the daemon's forwarding proxy: every
    /// request carries `token` in [`CALLER_TOKEN_HEADER`]. Only the runtime
    /// is built this way; workers never get the token.
    pub fn with_caller_token(
        timeout: std::time::Duration,
        token: &str,
    ) -> Result<Self, TransportError> {
        let mut value = reqwest::header::HeaderValue::from_str(token.trim())
            .map_err(|_| TransportError("the caller token is not a valid header value".into()))?;
        // Kept out of any debug output of the client.
        value.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(CALLER_TOKEN_HEADER, value);
        Self::build(timeout, headers)
    }

    /// [`Self::with_caller_token`], reading the token from the file the
    /// daemon also reads. The error never quotes the path.
    pub fn with_caller_token_file(
        timeout: std::time::Duration,
        path: &std::path::Path,
    ) -> Result<Self, TransportError> {
        let token = std::fs::read_to_string(path)
            .map_err(|e| TransportError(format!("couldn't read the caller token file: {e}")))?;
        Self::with_caller_token(timeout, &token)
    }

    fn build(
        timeout: std::time::Duration,
        headers: reqwest::header::HeaderMap,
    ) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .default_headers(headers)
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
        Ok(Self { client })
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
        let mut request = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(bytes);
        for (name, value) in headers {
            request = request.header(*name, *value);
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
