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
    let response = reqwest::Client::new()
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
    reqwest::Client::new()
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(proxy).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn unknown_routes_and_dot_dot_go_nowhere() {
    let (proxy, log) = setup().await;
    let unknown = reqwest::Client::new()
        .post(format!("http://{proxy}/openai/v1/chat/completions"))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
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
async fn redirects_go_back_to_the_caller_without_the_key() {
    let (proxy, log) = setup().await;
    let client = reqwest::Client::builder()
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
    let routes = vec![Route::new("dead", "http://127.0.0.1:1", Auth::Bearer, "k").unwrap()];
    let proxy = serve(router(routes, Duration::from_secs(5)).unwrap()).await;
    let response = reqwest::Client::new()
        .post(format!("http://{proxy}/dead/v1/x"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
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
