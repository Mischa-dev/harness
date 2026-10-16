use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use harness_core::SystemClock;
use harness_daemon::agent::{Agent, WakeEvent};
use harness_daemon::config::Config;
use harness_daemon::wakeup::{self, WebhookSecret};
use harness_daemon::{polling, proxy};

/// Long enough for a slow model's whole answer.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(600);

const USAGE: &str = "usage: harness-daemon proxy --config <path>
       harness-daemon run --config <path>
       harness-daemon resume --config <path>";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, config) = match args.as_slice() {
        [command, flag, path] if flag == "--config" => (command.as_str(), PathBuf::from(path)),
        _ => return usage(),
    };
    let result = match command {
        "proxy" => run_proxy(&config).await,
        "run" => run_agent(&config).await,
        "resume" => resume_agent(&config),
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("harness-daemon: {err}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

async fn run_proxy(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(path)?
        .proxy
        .ok_or("the config has no [proxy] section")?;
    let routes = config.load_routes()?;
    let caller_token = config.load_caller_token()?;
    // A count, not the names: whatever the config holds stays out of logs.
    let count = routes.len();
    let router = proxy::router(routes, caller_token, UPSTREAM_TIMEOUT)?;
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    eprintln!(
        "forwarding proxy on {} with {count} route(s)",
        listener.local_addr()?
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn run_agent(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(path)?
        .agent
        .ok_or("the config has no [agent] section")?;
    // The only place the runtime reads the real clock.
    let clock = Arc::new(SystemClock);
    let mut agent = Agent::from_config(&config, clock.clone())?;
    let (events_in, mut events) = tokio::sync::mpsc::channel::<WakeEvent>(64);
    if let Some(wakeups) = &config.wakeups {
        let secret = std::fs::read_to_string(&wakeups.secret_file)
            .map_err(|e| format!("couldn't read wakeups.secret_file: {e}"))?;
        let secret = WebhookSecret::new(&secret)?;
        let router = wakeup::router(&wakeups.agent_id, secret, clock.clone(), events_in.clone());
        let listener = tokio::net::TcpListener::bind(wakeups.listen).await?;
        eprintln!("wakeups on {}", listener.local_addr()?);
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, router).await {
                eprintln!("harness-daemon: the wakeup receiver stopped: {err}");
            }
        });
    }
    if let (Some(settings), Some(workspace)) = (&config.workspace, agent.workspace.clone()) {
        let every = settings.poll_every()?;
        eprintln!("looking at the Workspace inbox every {}s", every.as_secs());
        tokio::spawn(polling::poll(workspace, clock, every, events_in.clone()));
    }
    // With neither, nothing sends, and the agent wakes only at the looks it
    // chooses.
    drop(events_in);
    eprintln!("agent running; first look at {}", agent.rhythm.next_look());
    agent.run(&mut events, shutdown()).await?;
    Ok(())
}

/// The owner lifting a breaker's pause.
fn resume_agent(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(path)?
        .agent
        .ok_or("the config has no [agent] section")?;
    match harness_daemon::agent::resume(&config)? {
        Some(breaker) => {
            eprintln!("lifted the {breaker} pause; a running agent resumes within a minute")
        }
        None => eprintln!("the agent wasn't paused"),
    }
    Ok(())
}

/// Ctrl-C, or SIGTERM from systemd or Agent OS stopping the service. In-flight
/// work finishes before the process exits.
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
