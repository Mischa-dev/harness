//! `sim run <scenario> --provider fake`: run a scenario and print its report.
//! Exits non-zero when a check fails.

use std::process::ExitCode;

use sim::{FakeTeammate, Scenario, fake_profile, find_scenario, run, scenarios_dir};

const USAGE: &str = "usage: sim run <scenario> --provider fake
       sim list";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["list"] => list(),
        ["run", scenario, rest @ ..] => match rest {
            [] | ["--provider", "fake"] => run_fake(scenario).await,
            ["--model", _] => {
                eprintln!(
                    "Real-model runs need the charter prompt and the forwarding proxy, \
                     which aren't built yet. Use --provider fake for now."
                );
                ExitCode::from(2)
            }
            _ => usage(),
        },
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn list() -> ExitCode {
    let entries = match std::fs::read_dir(scenarios_dir()) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!(
                "couldn't list scenarios in {}: {err}",
                scenarios_dir().display()
            );
            return ExitCode::from(2);
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!(
                    "couldn't read an entry in {}: {err}",
                    scenarios_dir().display()
                );
                return ExitCode::from(2);
            }
        };
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                eprintln!("scenario filename is not valid UTF-8: {}", path.display());
                return ExitCode::from(2);
            };
            names.push(name.to_string());
        }
    }
    names.sort();
    for name in names {
        println!("{name}");
    }
    ExitCode::SUCCESS
}

async fn run_fake(name: &str) -> ExitCode {
    let scenario = match Scenario::load(&find_scenario(name)) {
        Ok(scenario) => scenario,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(2);
        }
    };
    match run(&scenario, &FakeTeammate::default(), &fake_profile(), "fake").await {
        Ok(report) => {
            print!("{report}");
            if report.passed() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            eprintln!("run failed: {err}");
            ExitCode::from(2)
        }
    }
}
