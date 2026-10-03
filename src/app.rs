//! The jobs behind both the CLI and the web page. Every setting comes from a
//! flag or its environment variable.

use crate::{browser, llm::Llm, network, race, rpc, say, solvers};
use anyhow::{Result, bail};
use clap::Args;
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Used when no browser is involved (bench, or a token passed in).
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                          Chrome/149.0.0.0 Safari/537.36";

#[derive(Args, Clone)]
pub struct Common {
    #[arg(
        long,
        env = "QUIZ_SC_URL",
        default_value = "https://superchallenge.io/play/SUPERCHALLENGE-JAWVUX"
    )]
    pub url: String,
    #[arg(long, env = "QUIZ_SC_CHROME", default_value = "chromium")]
    pub chrome: String,
    /// Attach to a running Chrome instead of launching one.
    #[arg(long, env = "QUIZ_SC_CDP_URL")]
    pub cdp_url: Option<String>,
    /// Chrome profile directory to reuse (default: a throwaway one).
    #[arg(long, env = "QUIZ_SC_PROFILE")]
    pub profile: Option<PathBuf>,
    /// Load the real play page instead of a stub with only the Turnstile widget.
    #[arg(long, env = "QUIZ_SC_REAL_PAGE")]
    pub real_page: bool,
    #[arg(long, env = "QUIZ_SC_TURNSTILE_TIMEOUT_S", default_value_t = 60)]
    pub turnstile_timeout_s: u64,
    #[arg(long, env = "QUIZ_SC_CONNECTIONS", default_value_t = 2,
          help = "Independent routes used by bench only; races always use two",
          value_parser = clap::value_parser!(u8).range(2..=16))]
    pub connections: u8,
    #[arg(long, env = "QUIZ_SC_NETWORK_PROBES", default_value_t = 3,
          help = "Fresh curl connection probes for bench (0 disables; requires curl >= 7.83)",
          value_parser = clap::value_parser!(u8).range(0..=10))]
    pub network_probes: u8,
    /// Connect to these Vercel edge addresses instead of the one DNS
    /// returns, `+`-separated, one per route in turn (`a+b`: primary on a,
    /// backup on b).
    #[arg(long, env = "QUIZ_SC_EDGE_IP", value_delimiter = '+')]
    pub edge_ips: Vec<std::net::IpAddr>,
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
    /// Make the connection that won a hedged answer the primary.
    #[arg(long, env = "QUIZ_SC_FOLLOW_WINNER", action = clap::ArgAction::SetTrue,
          value_parser = clap::builder::FalseyValueParser::new())]
    pub follow_winner: bool,
    /// What this race tries, for its log; set by `serve`.
    #[arg(skip)]
    pub variant: Option<serde_json::Value>,
    /// Send the answers without Chrome's user agent and cookie (the run is
    /// still started with them).
    #[arg(long, env = "QUIZ_SC_BARE_ANSWERS", action = clap::ArgAction::SetTrue,
          value_parser = clap::builder::FalseyValueParser::new())]
    pub bare_answers: bool,
    /// Abort a race once this many answers are in if they took longer than
    /// `--abort-ms` (0: never abort).
    #[arg(long, env = "QUIZ_SC_ABORT_AFTER", default_value_t = 0)]
    pub abort_after: usize,
    #[arg(long, env = "QUIZ_SC_ABORT_MS", default_value_t = 1500)]
    pub abort_ms: u64,
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
        !common.real_page,
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

fn pin_edge(common: &Common) {
    *rpc::EDGE_IPS.lock().unwrap() = common.edge_ips.clone();
    if !common.edge_ips.is_empty() {
        say!("edge pinned to {:?}", common.edge_ips);
    }
}

pub async fn bench(common: &Common, rounds: usize) -> Result<()> {
    pin_edge(common);
    let code = race::competition_code(&common.url);
    network::bench(&code, USER_AGENT, common.network_probes as usize).await?;
    let session = race::Session::new(&code, USER_AGENT, None, common.connections as usize)?;
    report_competition(&session.warm().await?);
    say!(
        "warm getCompetition endpoint RTT: includes server work; not network-only or submitAnswerV2 latency"
    );
    for (index, mut timings) in session.bench(rounds.max(1)).await?.into_iter().enumerate() {
        if timings.is_empty() {
            continue;
        }
        timings.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        say!(
            "getCompetition route {index}: min {:.1} p50 {:.1} p90 {:.1} max {:.1} ms",
            ms(timings[0]),
            ms(timings[timings.len() / 2]),
            ms(timings[timings.len() * 9 / 10]),
            ms(timings[timings.len() - 1])
        );
    }
    Ok(())
}

pub async fn cookie_bench(common: &Common, rounds: usize) -> Result<()> {
    let credentials = credentials(common).await?;
    let code = race::competition_code(&common.url);
    let input = json!({ "productId": rpc::PRODUCT_ID, "code": code });
    let with_cookie = rpc::Rpc::new(&credentials.user_agent, Some(&credentials.cookie))?;
    let without_cookie = rpc::Rpc::new(&credentials.user_agent, None)?;
    let (warm_with, warm_without) = tokio::join!(
        with_cookie.call("getCompetition", &input),
        without_cookie.call("getCompetition", &input)
    );
    warm_with?;
    warm_without?;

    let mut with_timings = Vec::with_capacity(rounds);
    let mut without_timings = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let with_request = async {
            let started = Instant::now();
            let result = with_cookie.call("getCompetition", &input).await;
            (result, started.elapsed())
        };
        let without_request = async {
            let started = Instant::now();
            let result = without_cookie.call("getCompetition", &input).await;
            (result, started.elapsed())
        };
        let ((with_result, with_elapsed), (without_result, without_elapsed)) =
            tokio::join!(with_request, without_request);
        with_result?;
        without_result?;
        with_timings.push(with_elapsed);
        without_timings.push(without_elapsed);
    }

    let stats = |mut timings: Vec<Duration>| {
        timings.sort();
        let milliseconds = |duration: Duration| duration.as_secs_f64() * 1000.0;
        (
            milliseconds(timings.iter().copied().sum::<Duration>() / timings.len() as u32),
            milliseconds(timings[timings.len() / 2]),
            milliseconds(timings[timings.len() * 9 / 10]),
        )
    };
    let paired_delta_ms = with_timings
        .iter()
        .zip(&without_timings)
        .map(|(with, without)| with.as_secs_f64() * 1000.0 - without.as_secs_f64() * 1000.0)
        .sum::<f64>()
        / rounds as f64;
    let (with_mean, with_p50, with_p90) = stats(with_timings);
    let (without_mean, without_p50, without_p90) = stats(without_timings);
    say!("cookie A/B: {rounds} paired getCompetition calls; no attempt started");
    say!("with cookie:    mean {with_mean:.1} p50 {with_p50:.1} p90 {with_p90:.1} ms");
    say!("without cookie: mean {without_mean:.1} p50 {without_p50:.1} p90 {without_p90:.1} ms");
    say!("paired mean delta (with - without): {paired_delta_ms:+.1} ms");
    Ok(())
}

fn warm_solvers() {
    let started = Instant::now();
    solvers::warm();
    say!(
        "solvers warmed in {:.2} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
}

pub async fn dry_run(common: &Common) -> Result<()> {
    warm_solvers();
    pin_edge(common);
    let credentials = credentials(common).await?;
    let session = race::Session::new(
        &race::competition_code(&common.url),
        &credentials.user_agent,
        Some(&credentials.cookie),
        2,
    )?;
    report_competition(&session.warm().await?);
    say!("dry run OK: user agent {}", credentials.user_agent);
    Ok(())
}

pub async fn run_race(args: &RaceArgs) -> Result<()> {
    let Some(email) = args.email.clone().filter(|e| !e.is_empty()) else {
        bail!("QUIZ_SC_EMAIL is not set");
    };
    warm_solvers();
    let (token, user_agent, cookie) = match args.turnstile_token.clone().filter(|t| !t.is_empty()) {
        Some(token) => (token, USER_AGENT.to_string(), String::new()),
        None => {
            let c = credentials(&args.common).await?;
            (c.turnstile_token, c.user_agent, c.cookie)
        }
    };
    let code = race::competition_code(&args.common.url);
    pin_edge(&args.common);
    let session = if args.bare_answers {
        let mut session = race::Session::new(&code, "", None, 2)?;
        let starter = rpc::Rpc::new(&user_agent, Some(&cookie))?;
        starter.call("getCompetition", &session.base).await?; // open its connection
        session.starter = Some(starter);
        session
    } else {
        race::Session::new(&code, &user_agent, Some(&cookie), 2)?
    };
    report_competition(&session.warm().await?);
    let llm = Llm::from_env()?;
    let config = race::Config {
        code,
        email,
        nickname: args.nickname.clone(),
        locale: args.locale.clone(),
        hedge_after: Duration::from_millis(args.hedge_ms),
        max_requests: args.max_requests.max(1),
        follow_winner: args.follow_winner,
        variant: args.variant.clone().unwrap_or_else(|| json!({
            "engine": "rust",
            "hedge_ms": args.hedge_ms,
            "edge_ips": args.common.edge_ips,
            "follow_winner": args.follow_winner,
            "http1": rpc::HTTP1.load(std::sync::atomic::Ordering::Relaxed),
            "bare_answers": args.bare_answers,
        })),
        abort: (args.abort_after > 0).then(|| race::Abort {
            after: args.abort_after,
            limit: Duration::from_millis(args.abort_ms),
        }),
        runs_dir: args.runs_dir.clone(),
    };
    race::race(&session, &config, &token, llm.as_ref()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct TestArgs {
        #[command(flatten)]
        args: RaceArgs,
    }

    #[test]
    fn preparation_and_hedge_defaults_are_explicit() {
        let command = TestArgs::command();
        for (name, expected) in [
            ("connections", "2"),
            ("network_probes", "3"),
            ("hedge_ms", "150"),
            ("max_requests", "4"),
        ] {
            let argument = command
                .get_arguments()
                .find(|argument| argument.get_id() == name)
                .unwrap();
            assert_eq!(argument.get_default_values()[0].to_str(), Some(expected));
        }
    }

    #[test]
    fn preparation_flags_are_bounded_and_hedging_is_overridable() {
        for (flag, invalid) in [
            ("--connections", "0"),
            ("--connections", "1"),
            ("--connections", "17"),
            ("--network-probes", "11"),
        ] {
            assert!(TestArgs::try_parse_from(["test", flag, invalid]).is_err());
        }
        let parsed = TestArgs::try_parse_from([
            "test",
            "--connections",
            "3",
            "--network-probes",
            "0",
            "--hedge-ms",
            "100",
        ])
        .unwrap();
        assert_eq!(parsed.args.common.connections, 3);
        assert_eq!(parsed.args.common.network_probes, 0);
        assert_eq!(parsed.args.hedge_ms, 100);
    }
}
