use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use harness_daemon::config::Config;
use harness_daemon::proxy;

/// Long enough for a slow model's whole answer.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(600);

const USAGE: &str = "usage: harness-daemon proxy --config <path>";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match args.as_slice() {
        [command, flag, path] if command == "proxy" && flag == "--config" => PathBuf::from(path),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run_proxy(&config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("harness-daemon: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run_proxy(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(path)?;
    let routes = config.proxy.load_routes()?;
    let caller_token = config.proxy.load_caller_token()?;
    // A count, not the names: whatever the config holds stays out of logs.
    let count = routes.len();
    let router = proxy::router(routes, caller_token, UPSTREAM_TIMEOUT)?;
    let listener = tokio::net::TcpListener::bind(config.proxy.listen).await?;
    eprintln!(
        "forwarding proxy on {} with {count} route(s)",
        listener.local_addr()?
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// Ctrl-C, or SIGTERM from systemd or Agent OS stopping the service. In-flight
/// calls finish before the process exits.
async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
