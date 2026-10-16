//! The forwarding proxy: adapters call `http://127.0.0.1:<port>/<route>/...`
//! without credentials, and the proxy forwards each call to that route's
//! provider with the provider key added. Keys live only here, read from
//! files (systemd credentials) when the daemon starts, so the agent, its
//! transcripts and its workers never see them. It stands in until the Agent
//! OS broker covers each provider.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde::Deserialize;

/// Model requests carry whole conversations; this is well above a full
/// context window.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("couldn't read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid proxy config: {0}")]
    Invalid(String),
}

/// `[proxy]` in the daemon's config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// Must be a loopback address: the proxy adds keys to anything it is
    /// sent, so nothing off the machine may reach it.
    pub listen: SocketAddr,
    #[serde(rename = "route")]
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// The first path segment, e.g. `anthropic` for `/anthropic/v1/messages`.
    pub name: String,
    /// The provider's base URL, e.g. `https://api.anthropic.com`.
    pub upstream: String,
    pub auth: Auth,
    /// A file holding only the key, e.g. under `$CREDENTIALS_DIRECTORY`.
    pub key_file: PathBuf,
}

/// How a provider expects its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Auth {
    /// `Authorization: Bearer <key>` (OpenAI-compatible providers).
    Bearer,
    /// `x-api-key: <key>` (Anthropic).
    XApiKey,
}

/// A provider key. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// A route ready to serve, with its key loaded.
#[derive(Debug, Clone)]
pub struct Route {
    pub name: String,
    pub upstream: String,
    pub auth: Auth,
    pub key: Secret,
}

impl Route {
    /// `key` is used as given; for tests and for keys from other sources.
    pub fn new(name: &str, upstream: &str, auth: Auth, key: &str) -> Result<Self, ConfigError> {
        validate_name(name)?;
        validate_upstream(name, upstream)?;
        let key = key.trim();
        if key.is_empty() || HeaderValue::from_str(key).is_err() {
            return Err(ConfigError::Invalid(format!(
                "route {name:?}: the key is empty or not a valid header value"
            )));
        }
        Ok(Self {
            name: name.into(),
            upstream: upstream.trim_end_matches('/').into(),
            auth,
            key: Secret(key.into()),
        })
    }

    fn auth_header(&self) -> (HeaderName, HeaderValue) {
        let (name, value) = match self.auth {
            Auth::Bearer => (header::AUTHORIZATION, format!("Bearer {}", self.key.0)),
            Auth::XApiKey => (HeaderName::from_static("x-api-key"), self.key.0.clone()),
        };
        let mut value = HeaderValue::from_str(&value).expect("checked in Route::new");
        value.set_sensitive(true);
        (name, value)
    }
}

impl ProxyConfig {
    /// Checks the config and reads every key file.
    pub fn load_routes(&self) -> Result<Vec<Route>, ConfigError> {
        if !self.listen.ip().is_loopback() {
            return Err(ConfigError::Invalid(format!(
                "listen {} is not a loopback address",
                self.listen
            )));
        }
        let mut routes: Vec<Route> = Vec::new();
        for r in &self.routes {
            if routes.iter().any(|seen| seen.name == r.name) {
                return Err(ConfigError::Invalid(format!(
                    "route {:?} appears twice",
                    r.name
                )));
            }
            routes.push(Route::new(
                &r.name,
                &r.upstream,
                r.auth,
                &read_key(&r.key_file)?,
            )?);
        }
        Ok(routes)
    }
}

fn read_key(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })
}

fn validate_name(name: &str) -> Result<(), ConfigError> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(ConfigError::Invalid(format!(
            "route name {name:?}: use letters, digits, '-' and '_'"
        )))
    }
}

/// Keys only travel over HTTPS, except to a server on this machine. The
/// error names the route, not the URL, which may hold a credential.
fn validate_upstream(name: &str, upstream: &str) -> Result<(), ConfigError> {
    let invalid = || {
        ConfigError::Invalid(format!(
            "route {name:?}: the upstream must be https://, or http:// only for localhost, \
             with no credentials, query or fragment"
        ))
    };
    let url = reqwest::Url::parse(upstream).map_err(|_| invalid())?;
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid());
    }
    let local = matches!(
        url.host(),
        Some(url::Host::Domain("localhost"))
            | Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST))
            | Some(url::Host::Ipv6(std::net::Ipv6Addr::LOCALHOST))
    );
    match url.scheme() {
        "https" => Ok(()),
        "http" if local => Ok(()),
        _ => Err(invalid()),
    }
}

struct Proxy {
    routes: Vec<Route>,
    /// For HTTPS upstreams elsewhere. It may go through the system's proxy,
    /// which sees only an encrypted tunnel.
    remote: reqwest::Client,
    /// For upstreams on this machine, HTTP or HTTPS. Never through a proxy:
    /// a plain-HTTP call would show it the key, and a local call has no
    /// business leaving the machine.
    local: reqwest::Client,
}

/// The proxy's HTTP routes. `timeout` bounds one upstream call, and should
/// allow for a slow model's whole answer.
pub fn router(routes: Vec<Route>, timeout: Duration) -> Result<Router, reqwest::Error> {
    let builder = || {
        reqwest::Client::builder()
            .timeout(timeout)
            // A redirect would carry the key to wherever it points.
            .redirect(reqwest::redirect::Policy::none())
    };
    let proxy = Proxy {
        routes,
        remote: builder().build()?,
        local: builder().no_proxy().build()?,
    };
    Ok(Router::new()
        .fallback(any(forward))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(Arc::new(proxy)))
}

/// Whether `url` names this machine: `localhost`, 127.0.0.0/8, `::1`, or a
/// 127.x address written as `::ffff:127.0.0.1`.
fn is_loopback(url: &reqwest::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        None => false,
    }
}

async fn forward(
    State(proxy): State<Arc<Proxy>>,
    uri: Uri,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // The raw path, still percent-encoded, so nothing is decoded into a
    // different meaning on the way through.
    let path = uri.path().trim_start_matches('/');
    let (name, rest) = path.split_once('/').unwrap_or((path, ""));
    let Some(route) = proxy.routes.iter().find(|r| r.name == name) else {
        return (StatusCode::NOT_FOUND, format!("no route {name:?}\n")).into_response();
    };
    // Model APIs need only these. Others, TRACE above all (it echoes the
    // request back, key included), never reach a provider.
    if method != Method::GET && method != Method::POST {
        return (StatusCode::METHOD_NOT_ALLOWED, "only GET and POST\n").into_response();
    }
    let Some(url) = upstream_url(route, rest, uri.query()) else {
        return (StatusCode::BAD_REQUEST, "that path leaves the route\n").into_response();
    };

    let mut outgoing = HeaderMap::new();
    for (name, value) in &headers {
        if forwards_request_header(name) {
            outgoing.append(name.clone(), value.clone());
        }
    }
    let (auth_name, auth_value) = route.auth_header();
    outgoing.insert(auth_name, auth_value);

    let client = if is_loopback(&url) {
        &proxy.local
    } else {
        &proxy.remote
    };
    let sent = client
        .request(method, url)
        .headers(outgoing)
        .body(body)
        .send()
        .await;
    let upstream = match sent {
        Ok(upstream) => upstream,
        Err(err) => {
            // reqwest's message names the URL, which holds no key.
            return (
                StatusCode::BAD_GATEWAY,
                format!("couldn't reach {:?}: {err}\n", route.name),
            )
                .into_response();
        }
    };
    let status = upstream.status();
    let mut response_headers = HeaderMap::new();
    for (name, value) in upstream.headers() {
        if forwards_response_header(name) {
            response_headers.append(name.clone(), value.clone());
        }
    }
    // Streamed, not buffered: the daemon holds no more of an answer than is
    // in flight, however large it is, and streamed model output passes
    // through as it arrives.
    let body = Body::from_stream(upstream.bytes_stream());
    (status, response_headers, body).into_response()
}

/// The upstream URL for a call, or `None` if the path could climb out of the
/// route's base or reach another server. Model API paths need no escapes, so
/// any `%` or `\\` is refused: an upstream that decodes `%2e%2e%2f` or
/// `..\\` before routing can't be led outside the route.
fn upstream_url(route: &Route, rest: &str, query: Option<&str>) -> Option<reqwest::Url> {
    if rest.contains(['%', '\\']) || rest.split('/').any(|s| s == "." || s == "..") {
        return None;
    }
    let base = reqwest::Url::parse(&route.upstream).ok()?;
    let mut url = reqwest::Url::parse(&format!("{}/{rest}", route.upstream)).ok()?;
    url.set_query(query);
    let same_server = url.scheme() == base.scheme()
        && url.host() == base.host()
        && url.port_or_known_default() == base.port_or_known_default();
    (same_server && url.path().starts_with(base.path())).then_some(url)
}

/// Hop-by-hop headers, and any credentials the caller sent: the route's key
/// is the only one that goes upstream.
fn forwards_request_header(name: &HeaderName) -> bool {
    !is_hop_by_hop(name)
        && name != header::HOST
        && name != header::CONTENT_LENGTH
        && name != header::AUTHORIZATION
        && name != header::PROXY_AUTHORIZATION
        && name != header::COOKIE
        && name.as_str() != "x-api-key"
        && name.as_str() != "api-key"
        && name.as_str() != "x-goog-api-key"
}

fn forwards_response_header(name: &HeaderName) -> bool {
    !is_hop_by_hop(name) && name != header::CONTENT_LENGTH && name != header::SET_COOKIE
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    name == header::CONNECTION
        || name == header::TRANSFER_ENCODING
        || name == header::TE
        || name == header::TRAILER
        || name == header::UPGRADE
        || name == header::PROXY_AUTHENTICATE
        || name.as_str() == "keep-alive"
        || name.as_str() == "proxy-connection"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_upstreams_of_either_scheme_skip_the_system_proxy() {
        for local in [
            "http://localhost:8080/v1",
            "https://localhost:8443/v1",
            "https://LOCALHOST/v1",
            "https://127.0.0.1/v1",
            "https://127.2.3.4/v1",
            "https://[::1]/v1",
            "https://[::ffff:127.0.0.1]/v1",
        ] {
            assert!(is_loopback(&local.parse().unwrap()), "{local}");
        }
        for remote in [
            "https://api.anthropic.com/v1",
            "https://localhost.example.com/v1",
            "https://10.0.0.1/v1",
            "https://[::ffff:10.0.0.1]/v1",
        ] {
            assert!(!is_loopback(&remote.parse().unwrap()), "{remote}");
        }
    }

    #[test]
    fn upstreams_need_https_except_on_this_machine() {
        for ok in [
            "https://api.anthropic.com",
            "https://openrouter.ai/api/",
            "http://127.0.0.1:9000",
            "http://localhost:9000/v1",
            "http://[::1]:9000",
        ] {
            assert!(validate_upstream("r", ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://api.anthropic.com",
            "http://127.0.0.2",
            "ftp://example.com",
            "https://user@example.com",
            "https://:secret@example.com",
            "https://example.com/?a=b",
            "not a url",
        ] {
            let err = validate_upstream("r", bad).unwrap_err().to_string();
            assert!(!err.contains("secret") && !err.contains("user@"), "{err}");
        }
    }

    #[test]
    fn keys_never_print() {
        let route = Route::new(
            "anthropic",
            "https://api.anthropic.com",
            Auth::XApiKey,
            "sk-live-123",
        )
        .unwrap();
        assert!(!format!("{route:?}").contains("sk-live-123"));
    }

    #[test]
    fn paths_stay_inside_the_route() {
        let route = Route::new("or", "https://openrouter.ai/api", Auth::Bearer, "k").unwrap();
        let url = upstream_url(&route, "v1/chat/completions", Some("a=b")).unwrap();
        assert_eq!(
            url.as_str(),
            "https://openrouter.ai/api/v1/chat/completions?a=b"
        );
        for bad in [
            "../x",
            "v1/%2e%2E/x",
            "v1/.%2e",
            "./x",
            "%2E",
            "/evil.com/x",
            "@evil.com",
        ] {
            assert!(
                upstream_url(&route, bad, None).is_none_or(|u| u.host_str()
                    == Some("openrouter.ai")
                    && u.path().starts_with("/api/")),
                "{bad}"
            );
        }
        for refused in [
            "v1/%2e%2E/x",
            "%2e%2e%2fadmin",
            "v1/..%5cadmin",
            "v1\\..\\admin",
        ] {
            assert!(upstream_url(&route, refused, None).is_none(), "{refused}");
        }
    }

    #[test]
    fn route_names_are_plain() {
        assert!(validate_name("openrouter_2").is_ok());
        for bad in ["", "a/b", "..", "a b"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_listen_address_must_be_loopback() {
        let config: ProxyConfig = toml::from_str(
            r#"
            listen = "0.0.0.0:8787"
            route = []
            "#,
        )
        .unwrap();
        let err = config.load_routes().unwrap_err();
        assert!(err.to_string().contains("not a loopback address"));
    }

    #[test]
    fn caller_credentials_are_dropped() {
        for name in ["authorization", "x-api-key", "cookie", "host", "connection"] {
            assert!(
                !forwards_request_header(&HeaderName::from_static(name)),
                "{name}"
            );
        }
        assert!(forwards_request_header(&HeaderName::from_static(
            "anthropic-version"
        )));
    }
}
