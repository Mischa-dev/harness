//! The real transport reaches the forwarding proxy directly, even when the
//! environment names a system proxy.
#![cfg(feature = "http")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use harness_providers::{ReqwestTransport, Transport};
use serde_json::json;

/// Runs `loopback_calls_skip_the_system_proxy_inner` in a child process
/// whose `HTTP_PROXY` and `ALL_PROXY` point at a listener here, since
/// changing this process's environment would race the other tests.
#[test]
fn loopback_calls_skip_the_system_proxy() {
    let snoop = TcpListener::bind("127.0.0.1:0").unwrap();
    snoop.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", snoop.local_addr().unwrap());
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "loopback_calls_skip_the_system_proxy_inner",
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
#[ignore = "run by loopback_calls_skip_the_system_proxy"]
async fn loopback_calls_skip_the_system_proxy_inner() {
    // A stand-in for the forwarding proxy that answers one request.
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = server.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let body = r#"{"ok":true}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
    });
    let transport = ReqwestTransport::new(Duration::from_secs(5)).unwrap();
    let response = transport
        .post_json(
            &format!("http://{addr}/anthropic/v1/messages"),
            &[],
            &json!({}),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn redirects_come_back_instead_of_being_followed() {
    // Where the redirect points: nothing may connect here.
    let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
    elsewhere.set_nonblocking(true).unwrap();
    let target = format!("http://{}/stolen", elsewhere.local_addr().unwrap());
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = server.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let _ = write!(
            stream,
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: {target}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
    });
    let transport = ReqwestTransport::new(Duration::from_secs(5)).unwrap();
    let response = transport
        .post_json(
            &format!("http://{addr}/anthropic/v1/messages"),
            &[],
            &json!({}),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 307);
    assert!(elsewhere.accept().is_err(), "the redirect was followed");
}
