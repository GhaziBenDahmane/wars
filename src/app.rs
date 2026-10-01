//! The jobs behind both the CLI and the web page. Every setting comes from a
//! flag or its environment variable.

use crate::{browser, llm::Llm, race, say};
use anyhow::{Result, bail};
use clap::Args;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Used when no browser is involved (bench, or a token passed in).
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                          Chrome/149.0.0.0 Safari/537.36";

#[derive(Args, Clone)]
pub struct Common {
    #[arg(long, env = "QUIZ_SC_URL", default_value = "https://superchallenge.io/play/SUPERCHALLENGE-JAWVUX")]
    pub url: String,
    #[arg(long, env = "QUIZ_SC_CHROME", default_value = "chromium")]
    pub chrome: String,
    /// Attach to a running Chrome instead of launching one.
    #[arg(long, env = "QUIZ_SC_CDP_URL")]
    pub cdp_url: Option<String>,
    /// Chrome profile directory to reuse (default: a throwaway one).
    #[arg(long, env = "QUIZ_SC_PROFILE")]
    pub profile: Option<PathBuf>,
    #[arg(long, env = "QUIZ_SC_TURNSTILE_TIMEOUT_S", default_value_t = 60)]
    pub turnstile_timeout_s: u64,
}

#[derive(Args, Clone)]
pub struct RaceArgs {
    #[command(flatten)]
    pub common: Common,
    #[arg(long, env = "QUIZ_SC_EMAIL")]
    pub email: Option<String>,
    /// Leaderboard name; without it the score is not submitted.
    #[arg(long, env = "QUIZ_SC_NICKNAME")]
    pub nickname: Option<String>,
    #[arg(long, env = "QUIZ_SC_LOCALE", default_value = "fr")]
    pub locale: String,
    /// Send a duplicate answer on the other connection after this long.
    #[arg(long, env = "QUIZ_SC_HEDGE_MS", default_value_t = 150)]
    pub hedge_ms: u64,
    #[arg(long, env = "QUIZ_SC_MAX_REQUESTS", default_value_t = 4)]
    pub max_requests: usize,
    #[arg(long, env = "QUIZ_SC_RUNS_DIR", default_value = "runs")]
    pub runs_dir: PathBuf,
    /// Use this Turnstile token instead of getting one from Chrome.
    #[arg(long, env = "QUIZ_SC_TURNSTILE_TOKEN")]
    pub turnstile_token: Option<String>,
}

async fn credentials(common: &Common) -> Result<browser::Credentials> {
    let started = Instant::now();
    let credentials = browser::credentials(
        &common.chrome,
        common.cdp_url.as_deref(),
        common.profile.as_deref(),
        &common.url,
        Duration::from_secs(common.turnstile_timeout_s),
    )
    .await?;
    say!(
        "turnstile token in {:.1}s ({} chars, {} cookie bytes)",
        started.elapsed().as_secs_f64(),
        credentials.turnstile_token.len(),
        credentials.cookie.len()
    );
    Ok(credentials)
}

fn report_competition(competition: &serde_json::Value) {
    say!(
        "{} | plays left: {} | agentWars: {}",
        competition["title"].as_str().unwrap_or("?"),
        competition["playsLeft"],
        competition["agentWars"]
    );
}

pub async fn bench(common: &Common, rounds: usize) -> Result<()> {
    let session = race::Session::new(&race::competition_code(&common.url), USER_AGENT, None)?;
    report_competition(&session.warm().await?);
    for (index, mut timings) in session.bench(rounds.max(1)).await?.into_iter().enumerate() {
        if timings.is_empty() {
            continue;
        }
        timings.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        say!(
            "route {index}: min {:.1} p50 {:.1} p90 {:.1} max {:.1} ms",
            ms(timings[0]),
            ms(timings[timings.len() / 2]),
            ms(timings[timings.len() * 9 / 10]),
            ms(timings[timings.len() - 1])
        );
    }
    Ok(())
}

pub async fn dry_run(common: &Common) -> Result<()> {
    let credentials = credentials(common).await?;
    let session = race::Session::new(
        &race::competition_code(&common.url),
        &credentials.user_agent,
        Some(&credentials.cookie),
    )?;
    report_competition(&session.warm().await?);
    say!("dry run OK: user agent {}", credentials.user_agent);
    Ok(())
}

pub async fn run_race(args: &RaceArgs) -> Result<()> {
    let Some(email) = args.email.clone().filter(|e| !e.is_empty()) else {
        bail!("QUIZ_SC_EMAIL is not set");
    };
    let (token, user_agent, cookie) = match args.turnstile_token.clone().filter(|t| !t.is_empty()) {
        Some(token) => (token, USER_AGENT.to_string(), String::new()),
        None => {
            let c = credentials(&args.common).await?;
            (c.turnstile_token, c.user_agent, c.cookie)
        }
    };
    let code = race::competition_code(&args.common.url);
    let session = race::Session::new(&code, &user_agent, Some(&cookie))?;
    report_competition(&session.warm().await?);
    let llm = Llm::from_env()?;
    let config = race::Config {
        code,
        email,
        nickname: args.nickname.clone(),
        locale: args.locale.clone(),
        hedge_after: Duration::from_millis(args.hedge_ms),
        max_requests: args.max_requests.max(1),
        runs_dir: args.runs_dir.clone(),
    };
    race::race(&session, &config, &token, llm.as_ref()).await
}
