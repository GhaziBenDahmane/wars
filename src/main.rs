use agentwars::app::{self, Common, RaceArgs};
use agentwars::{solvers, team, web};
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(about = "SuperChallenge Agents War racer")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the race. Spends one attempt.
    Race(RaceArgs),
    /// Everything up to the race without starting it: Turnstile token, warm
    /// connections, plays left. Spends no attempt.
    DryRun(Common),
    /// Network setup diagnostics and API endpoint timings. Spends no attempt.
    Bench {
        #[command(flatten)]
        common: Common,
        #[arg(long, default_value_t = 20)]
        rounds: usize,
    },
    /// Compare warmed requests with and without Chrome's cookies. Spends no attempt.
    CookieBench {
        #[command(flatten)]
        common: Common,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u8).range(1..=100))]
        rounds: u8,
    },
    /// Every interval, the next member of a CSV of `nickname,email` races
    /// `--attempts` times. Spends those attempts each turn and runs until stopped.
    Team {
        #[command(flatten)]
        race: RaceArgs,
        #[arg(long, env = "QUIZ_SC_TEAM_CSV")]
        csv: PathBuf,
        #[arg(long, env = "QUIZ_SC_TEAM_EVERY_MIN", default_value_t = 60,
              value_parser = clap::value_parser!(u64).range(1..))]
        every_min: u64,
        #[arg(long, env = "QUIZ_SC_TEAM_ATTEMPTS", default_value_t = 10,
              value_parser = clap::value_parser!(u8).range(1..=10))]
        attempts: u8,
        /// Turn to start from (0 = first row), to resume the rotation after a restart.
        #[arg(long, default_value_t = 0)]
        start: usize,
    },
    /// Solve one prompt offline.
    Solve { prompt: String },
    /// Container default: races non-stop, one race at a time, taking turns
    /// between the email prefixes, `--attempts` per generated
    /// `{prefix}{i}@amundi.com` email, and serves a read-only page that streams
    /// the current race. Spends attempts until stopped.
    Serve {
        #[command(flatten)]
        race: RaceArgs,
        #[arg(long, env = "PORT", default_value_t = 3000)]
        port: u16,
        #[arg(long, env = "HOST", default_value = "0.0.0.0")]
        host: String,
        /// Comma-separated; emails are `{prefix}{i}@amundi.com` and progress
        /// is saved per prefix.
        #[arg(long, env = "QUIZ_SC_EMAIL_PREFIXES", default_value = "ghazi_",
              value_delimiter = ',')]
        email_prefixes: Vec<String>,
        /// Every other race uses HTTP/1.1 instead of HTTP/2.
        #[arg(long, env = "QUIZ_SC_ALTERNATE_HTTP", action = clap::ArgAction::SetTrue,
              value_parser = clap::builder::FalseyValueParser::new())]
        alternate_http: bool,
        /// Comma-separated racers to take turns between: `rust` (this
        /// binary), `name=path` for another one run as `<path> race`,
        /// `name=path:http1` when it only speaks HTTP/1.1.
        #[arg(long, env = "QUIZ_SC_ENGINES", value_delimiter = ',')]
        engines: Vec<String>,
        /// Comma-separated hedge delays (ms) to take turns between.
        #[arg(long, env = "QUIZ_SC_HEDGE_MS_LIST", value_delimiter = ',')]
        hedge_ms_list: Vec<u64>,
        /// Comma-separated edge settings to take turns between: `dns` keeps
        /// the address DNS returns, `a+b` puts the primary on a and the backup
        /// on b.
        #[arg(long, env = "QUIZ_SC_EDGE_IPS", value_delimiter = ',')]
        edge_list: Vec<String>,
        /// Comma-separated 0/1: races with and without following the winner.
        #[arg(long, env = "QUIZ_SC_FOLLOW_WINNER_LIST", value_delimiter = ',')]
        follow_winner_list: Vec<String>,
        #[arg(long, env = "QUIZ_SC_ATTEMPTS_PER_EMAIL", default_value_t = 10,
              value_parser = clap::value_parser!(u8).range(1..))]
        attempts: u8,
        /// First email index; a higher index saved in the runs directory wins.
        #[arg(long, env = "QUIZ_SC_EMAIL_START", default_value_t = 1)]
        email_start: u64,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Solve { prompt } => match solvers::solve(&prompt) {
            Some(answer) => println!("{answer}"),
            None => bail!("no exact solver for this prompt"),
        },
        Command::Bench { common, rounds } => app::bench(&common, rounds).await?,
        Command::CookieBench { common, rounds } => {
            app::cookie_bench(&common, rounds as usize).await?
        }
        Command::DryRun(common) => app::dry_run(&common).await?,
        Command::Race(args) => app::run_race(&args).await?,
        Command::Team {
            race,
            csv,
            every_min,
            attempts,
            start,
        } => {
            let every = Duration::from_secs(every_min * 60);
            team::run(&race, &csv, every, attempts, start).await?
        }
        Command::Serve {
            race,
            port,
            host,
            email_prefixes,
            alternate_http,
            engines,
            hedge_ms_list,
            edge_list,
            follow_winner_list,
            attempts,
            email_start,
        } => {
            let prefixes = email_prefixes.into_iter().map(|p| p.trim().to_string()).collect();
            let variants = web::Variants::parse(
                &race,
                &engines,
                alternate_http,
                hedge_ms_list,
                &edge_list,
                &follow_winner_list,
            )?;
            web::serve(race, &host, port, prefixes, attempts, variants, email_start).await?
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_takes_several_prefixes_and_a_falsey_alternate_http() {
        let parse = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Command::Serve { email_prefixes, alternate_http, .. } => (email_prefixes, alternate_http),
            _ => unreachable!(),
        };
        let (prefixes, alternate) =
            parse(&["agentwars", "serve", "--email-prefixes", "ghazi_,studio_", "--alternate-http"]);
        assert_eq!(prefixes, ["ghazi_", "studio_"]);
        assert!(alternate);
        match Cli::try_parse_from([
            "agentwars", "serve", "--edge-list", "dns,216.150.1.1+76.76.21.21",
            "--edge-ips", "64.29.17.1", "--hedge-ms-list", "150,80", "--follow-winner-list", "0,1",
            "--engines", "rust,go=/usr/local/bin/agentwars-go",
        ]).unwrap().command {
            Command::Serve { race, engines, edge_list, hedge_ms_list, follow_winner_list, .. } => {
                assert_eq!(engines, ["rust", "go=/usr/local/bin/agentwars-go"]);
                assert_eq!(edge_list, ["dns", "216.150.1.1+76.76.21.21"]);
                assert_eq!(race.common.edge_ips, ["64.29.17.1".parse::<std::net::IpAddr>().unwrap()]);
                assert_eq!(hedge_ms_list, [150, 80]);
                assert_eq!(follow_winner_list, ["0", "1"]);
            }
            _ => unreachable!(),
        }
        let parser = clap::builder::FalseyValueParser::new();
        let command = clap::Command::new("test");
        for (value, expected) in [("1", true), ("true", true), ("0", false), ("false", false)] {
            let parsed = clap::builder::TypedValueParser::parse_ref(
                &parser, &command, None, std::ffi::OsStr::new(value),
            ).unwrap();
            assert_eq!(parsed, expected, "{value}");
        }
    }
}
