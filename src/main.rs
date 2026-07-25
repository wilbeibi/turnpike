mod check;
mod cli;
mod config;
mod cost;
mod json_usage;
mod parsers;
mod paths;
mod peer;
mod pricing;
mod providers;
mod proxy;
mod record;
mod since;
mod sse;
mod stats;
mod tail;

use clap::Parser;
use cli::{Cli, Command};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let cli = Cli::parse();
    match cli.command {
        Command::Start => proxy::run_all().await?,
        Command::Stats {
            by_model,
            by_client,
            by_day,
            by_exe,
            since,
            json,
        } => stats::run(stats::StatsOpts {
            by_model,
            by_client,
            by_day,
            by_exe,
            since,
            json,
        })?,
        Command::Tail { n, since, json } => tail::run(n, since, json)?,
        Command::Check {
            budget,
            quiet,
            json,
        } => {
            // Distinct exit codes are the whole contract: 0 = under budget,
            // 1 = at/over budget (the branchable signal), 2 = error (bad
            // input, corrupt data), 3 = unknown (nothing broken, just can't
            // vouch for the number). Callers must inspect the exact status
            // rather than treating every nonzero status as over-budget, and
            // must not treat 3 with the same severity as 2 — see check.rs.
            let verdict = check::parse_budget(&budget).and_then(|(budget, period)| {
                check::run(check::CheckOpts {
                    budget,
                    period,
                    json,
                    quiet,
                })
            });
            match verdict {
                Ok(check::Outcome::Under) => {}
                Ok(check::Outcome::Over) => std::process::exit(1),
                Ok(check::Outcome::Unknown) => std::process::exit(3),
                Err(e) => {
                    eprintln!("turnpike: {e:#}");
                    std::process::exit(2);
                }
            }
        }
        Command::Config { format, provider } => config::run(
            match format {
                cli::Format::Shell => config::ConfigFormat::Shell,
                cli::Format::Fish => config::ConfigFormat::Fish,
                cli::Format::Json => config::ConfigFormat::Json,
                cli::Format::Url => config::ConfigFormat::Url,
            },
            provider.as_deref(),
        )?,
        Command::Prices { cmd } => match cmd {
            cli::PricesCmd::Pull => pricing::pull(&paths::prices_json()).await?,
            cli::PricesCmd::Show => pricing::show(&paths::prices_json()),
        },
    }

    Ok(())
}
