use agentwars::app::{self, Common, RaceArgs};
use agentwars::{solvers, web};
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

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
    /// Round-trip times to the API on both routes. Spends no attempt.
    Bench {
        #[command(flatten)]
        common: Common,
        #[arg(long, default_value_t = 20)]
        rounds: usize,
    },
    /// Solve one prompt offline.
    Solve { prompt: String },
    /// Container default: a web page with the run buttons. Never races on its
    /// own, so restarts never spend attempts.
    Serve {
        #[command(flatten)]
        race: RaceArgs,
        #[arg(long, env = "PORT", default_value_t = 3000)]
        port: u16,
        /// Required to press any button; unset disables them.
        #[arg(long, env = "QUIZ_SC_RUN_TOKEN")]
        run_token: Option<String>,
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
        Command::DryRun(common) => app::dry_run(&common).await?,
        Command::Race(args) => app::run_race(&args).await?,
        Command::Serve { race, port, run_token } => web::serve(race, port, run_token).await?,
    }
    Ok(())
}
