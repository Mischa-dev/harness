//! The forwarding proxy against a fake provider on this machine.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::any;
use harness_daemon::proxy::{Auth, Route, router};

#[derive(Debug, Clone, Default)]
struct Seen {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

type Log = Arc<Mutex<Vec<Seen>>>;

async fn record(
    State(log): State<Log>,
    method: Method,
    uri: Uri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    log.lock().unwrap().push(Seen {
        method: method.to_string(),
        path: uri.path().into(),
        query,
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body: body.to_vec(),
    });
    if uri.path() == "/redirect" {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "http://127.0.0.1:1/elsewhere")],
            "",
        )
            .into_response();
    }
    (
        StatusCode::CREATED,
        [("x-request-id", "req_1"), ("set-cookie", "a=b")],
        r#"{"ok":true}"#,
    )
        .into_response()
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A fake provider and a proxy in front of it with two routes.
async fn setup() -> (SocketAddr, Log) {
    let log: Log = Arc::default();
    let upstream = serve(Router::new().fallback(any(record)).with_state(log.clone())).await;
    let base = format!("http://127.0.0.1:{}", upstream.port());
    let routes = vec![
        Route::new("anthropic", &base, Auth::XApiKey, "sk-ant-secret\n").unwrap(),
        Route::new(
            "openrouter",
            &format!("{base}/api/"),
            Auth::Bearer,
            "sk-or-secret",
        )
        .unwrap(),
    ];
    let proxy = serve(router(routes, Duration::from_secs(5)).unwrap()).await;
    (proxy, log)
}

fn header<'a>(seen: &'a Seen, name: &str) -> Vec<&'a str> {
    seen.headers
        .iter()
        .filter(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .collect()
}

#[tokio::test]
async fn adds_the_routes_key_and_passes_the_call_through() {
    let (proxy, log) = setup().await;
    let response = local_client()
        .post(format!("http://{proxy}/anthropic/v1/messages?beta=true"))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(r#"{"model":"m"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 201);
    assert_eq!(response.headers()["x-request-id"], "req_1");
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(response.text().await.unwrap(), r#"{"ok":true}"#);

    let seen = log.lock().unwrap()[0].clone();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path, "/v1/messages");
    assert_eq!(seen.query.as_deref(), Some("beta=true"));
    assert_eq!(header(&seen, "x-api-key"), ["sk-ant-secret"]);
    assert!(header(&seen, "authorization").is_empty());
    assert_eq!(header(&seen, "anthropic-version"), ["2023-06-01"]);
    assert_eq!(seen.body, br#"{"model":"m"}"#);
}

#[tokio::test]
async fn replaces_any_key_the_caller_sends() {
    let (proxy, log) = setup().await;
    local_client()
        .post(format!("http://{proxy}/openrouter/v1/chat/completions"))
        .header("authorization", "Bearer stolen")
        .header("x-api-key", "stolen")
        .header("cookie", "session=stolen")
        .body("{}")
        .send()
        .await
        .unwrap();

    let seen = log.lock().unwrap()[0].clone();
    assert_eq!(seen.path, "/api/v1/chat/completions");
    assert_eq!(header(&seen, "authorization"), ["Bearer sk-or-secret"]);
    assert!(header(&seen, "x-api-key").is_empty());
    assert!(header(&seen, "cookie").is_empty());
}

/// Sends a request line as written, without a client tidying the path.
async fn raw_get(proxy: SocketAddr, path: &str) -> String {
    raw_get_as(proxy, path, &proxy.to_string(), "").await
}

/// The same, naming `host` and adding `extra` header lines.
async fn raw_get_as(proxy: SocketAddr, path: &str, host: &str, extra: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(proxy).await.unwrap();
    let request =
        format!("GET {path} HTTP/1.1\r\nhost: {host}\r\n{extra}connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn unknown_routes_and_dot_dot_go_nowhere() {
    let (proxy, log) = setup().await;
    let unknown = local_client()
        .post(format!(
            "http://{proxy}/sk-pasted-secret/v1/chat/completions"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    let body = unknown.text().await.unwrap();
    assert!(!body.contains("secret"), "{body}");
    for path in [
        "/openrouter/../anthropic/v1/messages",
        "/openrouter/v1/%2E%2E/%2e%2e/admin",
        "/",
    ] {
        let response = raw_get(proxy, path).await;
        assert!(
            response.starts_with("HTTP/1.1 400") || response.starts_with("HTTP/1.1 404"),
            "{path}: {response}"
        );
    }
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn web_pages_cannot_spend_the_key() {
    let (proxy, log) = setup().await;
    let port = proxy.port();
    // A page posting across origins, as a form or fetch would.
    let cross_origin = local_client()
        .post(format!("http://{proxy}/anthropic/v1/messages"))
        .header("origin", "https://evil.example")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(cross_origin.status(), 403);
    for (host, extra) in [
        // A page that rebound its own name to 127.0.0.1.
        (format!("evil.example:{port}"), ""),
        (format!("localhost.evil.example:{port}"), ""),
        (format!("evil.example@127.0.0.1:{port}"), ""),
        // A browser's own request with no Origin, such as an image.
        (
            format!("127.0.0.1:{port}"),
            "sec-fetch-site: cross-site\r\n",
        ),
        (format!("127.0.0.1:{port}"), "sec-fetch-mode: no-cors\r\n"),
    ] {
        let response = raw_get_as(proxy, "/anthropic/v1/models", &host, extra).await;
        assert!(response.starts_with("HTTP/1.1 403"), "{host}: {response}");
    }
    assert!(log.lock().unwrap().is_empty());

    // The runtime's own calls, by any loopback name, still go through.
    for host in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ] {
        let response = raw_get_as(proxy, "/anthropic/v1/models", &host, "").await;
        assert!(response.starts_with("HTTP/1.1 201"), "{host}: {response}");
    }
}

/// Sends only a request's head, announcing a body that never comes, and
/// returns the first answer if one arrives within two seconds.
async fn head_only(proxy: SocketAddr, extra: &str, length: usize) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(proxy).await.unwrap();
    let request = format!(
        "POST /anthropic/v1/messages HTTP/1.1\r\nhost: {proxy}\r\n{extra}content-length: {length}\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = [0u8; 256];
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;
    match read {
        Ok(Ok(n)) if n > 0 => Some(String::from_utf8_lossy(&buf[..n]).into_owned()),
        _ => None,
    }
}

#[tokio::test]
async fn refused_and_oversized_requests_are_answered_before_their_body() {
    let (proxy, log) = setup().await;
    let refused = head_only(proxy, "origin: https://evil.example\r\n", 1_000_000).await;
    assert!(
        refused
            .as_deref()
            .is_some_and(|r| r.starts_with("HTTP/1.1 403")),
        "{refused:?}"
    );
    let oversized = head_only(proxy, "", 64 * 1024 * 1024).await;
    assert!(
        oversized
            .as_deref()
            .is_some_and(|r| r.starts_with("HTTP/1.1 413")),
        "{oversized:?}"
    );
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn only_get_and_post_reach_the_provider() {
    let (proxy, log) = setup().await;
    let client = local_client();
    for method in ["TRACE", "PUT", "DELETE", "OPTIONS"] {
        let response = client
            .request(
                reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
                format!("http://{proxy}/anthropic/v1/messages"),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 405, "{method}");
    }
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn answers_stream_through_instead_of_being_held() {
    // The provider sends one chunk and then never finishes. A proxy that
    // buffered the whole answer would never pass anything on.
    let upstream = serve(Router::new().fallback(any(|| async {
        axum::body::Body::from_stream(FirstChunkThenNothing(false))
    })))
    .await;
    let base = format!("http://127.0.0.1:{}", upstream.port());
    let routes = vec![Route::new("slow", &base, Auth::Bearer, "k").unwrap()];
    let proxy = serve(router(routes, Duration::from_secs(30)).unwrap()).await;
    let mut response = local_client()
        .post(format!("http://{proxy}/slow/v1/chat/completions"))
        .send()
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), response.chunk())
        .await
        .expect("the first chunk arrived before the answer finished")
        .unwrap();
    assert_eq!(first.as_deref(), Some(&b"data: 1\n\n"[..]));
}

struct FirstChunkThenNothing(bool);

impl futures_core::Stream for FirstChunkThenNothing {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.0 {
            return std::task::Poll::Pending;
        }
        self.0 = true;
        std::task::Poll::Ready(Some(Ok(Bytes::from_static(b"data: 1\n\n"))))
    }
}

/// Runs `local_upstreams_skip_the_system_proxy_inner` in a child process
/// whose `HTTP_PROXY` and `ALL_PROXY` point at a listener here, since
/// changing this process's environment would race the other tests. Any
/// connection to that listener means a key went through the proxy.
#[test]
fn local_upstreams_skip_the_system_proxy() {
    let snoop = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    snoop.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", snoop.local_addr().unwrap());
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "local_upstreams_skip_the_system_proxy_inner",
            "--ignored",
        ])
        .env("HTTP_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        snoop.accept().is_err(),
        "the call went through the system proxy"
    );
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "run by local_upstreams_skip_the_system_proxy"]
async fn local_upstreams_skip_the_system_proxy_inner() {
    let (proxy, log) = setup().await;
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .post(format!("http://{proxy}/anthropic/v1/messages"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn redirects_go_back_to_the_caller_without_the_key() {
    let (proxy, log) = setup().await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = client
        .post(format!("http://{proxy}/anthropic/redirect"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 307);
    assert_eq!(log.lock().unwrap().len(), 1, "the proxy didn't follow it");
}

#[tokio::test]
async fn an_unreachable_provider_is_a_bad_gateway() {
    // A route whose name and path hold something that must not leak.
    let routes = vec![
        Route::new(
            "sk-name-secret",
            "http://127.0.0.1:1/sk-path-secret",
            Auth::Bearer,
            "k",
        )
        .unwrap(),
    ];
    let proxy = serve(router(routes, Duration::from_secs(5)).unwrap()).await;
    let response = local_client()
        .post(format!("http://{proxy}/sk-name-secret/v1/x"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    let body = response.text().await.unwrap();
    assert!(
        !body.contains("secret") && !body.contains("127.0.0.1"),
        "{body}"
    );
}

#[test]
fn config_names_key_files_and_never_holds_keys() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("anthropic.key");
    std::fs::write(&key, "sk-ant-from-file\n").unwrap();
    let config = dir.path().join("daemon.toml");
    std::fs::write(
        &config,
        format!(
            r#"
            [proxy]
            listen = "127.0.0.1:8787"

            [[proxy.route]]
            name = "anthropic"
            upstream = "https://api.anthropic.com"
            auth = "x-api-key"
            key_file = "{}"
            "#,
            key.display()
        ),
    )
    .unwrap();

    let config = harness_daemon::config::Config::load(&config).unwrap();
    let routes = config.proxy.load_routes().unwrap();
    assert_eq!(routes[0].name, "anthropic");
    assert!(!format!("{routes:?}").contains("sk-ant-from-file"));

    std::fs::remove_file(&key).unwrap();
    assert!(
        config.proxy.load_routes().is_err(),
        "a missing key file stops startup"
    );
}

/// A client for calling the proxy under test directly, whatever system proxy
/// the environment names.
fn local_client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}
