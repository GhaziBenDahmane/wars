//! Non-stop racing, one race at a time, with a read-only page that streams the
//! current race. The race loop runs on its own thread and runtime, so serving
//! the page never competes with it. Races take turns between the prefixes;
//! each prefix generates `{prefix}{i}@amundi.com` emails and races each one
//! `attempts` times before the next. Positions are saved in the runs
//! directory, one file per prefix, so a restart resumes where it stopped.
//! Races also take turns between `Variants` (protocol, hedge delay, edge
//! address, following the winner), every combination in turn, and the best
//! time is kept per combination.

use crate::app::{self, RaceArgs};
use crate::{report, say};
use anyhow::Result;
use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PAGE: &str = include_str!("page.html");
pub const NICKNAME: &str = "AMUNDI STU SQUAD";
/// A racer that runs longer than this is killed (a race takes ~10 s).
const EXTERNAL_TIMEOUT: Duration = Duration::from_secs(300);
/// Pause after a failed race, so a lasting outage does not spin.
const FAILURE_PAUSE: Duration = Duration::from_secs(10);

pub fn email(prefix: &str, index: u64) -> String {
    format!("{prefix}{index}@amundi.com")
}

/// The next race: email index and attempt number (1-based) for that email.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Position {
    index: u64,
    attempt: u8,
}

impl Position {
    fn next(self, attempts: u8) -> Position {
        if self.attempt >= attempts {
            Position { index: self.index + 1, attempt: 1 }
        } else {
            Position { attempt: self.attempt + 1, ..self }
        }
    }
}

/// The fastest race that reached the goal, for one prefix and protocol.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct Best {
    elapsed_ms: u64,
    email: String,
    /// Unix seconds.
    at: u64,
    /// Races that reached the goal, the best included.
    races: u64,
}

/// A racer: this binary, or another one (the Go or C++ rewrite) run as
/// `<path> race` with the race settings in its environment.
#[derive(Clone, Debug, PartialEq)]
pub struct Engine {
    name: String,
    /// None: this binary, in process.
    path: Option<PathBuf>,
    /// It only speaks HTTP/1.1.
    http1_only: bool,
}

impl Engine {
    /// `rust`, or `name=path` for another binary, `name=path:http1` when it
    /// only speaks HTTP/1.1.
    fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let Some((name, path)) = text.split_once('=') else {
            anyhow::ensure!(text == "rust", "a racer other than rust needs a path: {text:?}");
            return Ok(Self { name: text.into(), path: None, http1_only: false });
        };
        let (path, http1_only) = match path.strip_suffix(":http1") {
            Some(path) => (path, true),
            None => (path, false),
        };
        anyhow::ensure!(!name.is_empty() && !path.is_empty(), "bad racer {text:?}");
        Ok(Self { name: name.into(), path: Some(path.into()), http1_only })
    }
}

/// What one race tries. Every combination of the lists, in turn.
#[derive(Clone, Debug, PartialEq)]
pub struct Variants {
    engines: Vec<Engine>,
    http: Vec<bool>,
    hedge_ms: Vec<u64>,
    edge_ips: Vec<Vec<IpAddr>>,
    follow_winner: Vec<bool>,
    bare_answers: Vec<bool>,
}

#[derive(Clone, Debug, PartialEq)]
struct Variant {
    engine: Engine,
    http1: bool,
    hedge_ms: u64,
    /// One per route in turn; empty keeps DNS.
    edge_ips: Vec<IpAddr>,
    follow_winner: bool,
    /// Answers without Chrome's user agent and cookie (this binary only).
    bare_answers: bool,
}

impl Variant {
    fn protocol(&self) -> &'static str {
        if self.http1 { "HTTP/1.1" } else { "HTTP/2" }
    }

    /// `rust HTTP/2 hedge 100 edge 76.76.21.21 follow`: the key of its best time.
    fn label(&self) -> String {
        let edge = if self.edge_ips.is_empty() {
            "dns".to_string()
        } else {
            self.edge_ips.iter().map(IpAddr::to_string).collect::<Vec<_>>().join("+")
        };
        let follow = if self.follow_winner { "follow" } else { "fixed" };
        let (engine, protocol, hedge) = (&self.engine.name, self.protocol(), self.hedge_ms);
        let bare = if self.bare_answers { " bare" } else { "" };
        format!("{engine} {protocol} hedge {hedge} edge {edge} {follow}{bare}")
    }

    /// Written to the race log, to compare variants afterwards.
    fn describe(&self) -> Value {
        json!({
            "engine": self.engine.name,
            "hedge_ms": self.hedge_ms,
            "edge_ips": self.edge_ips,
            "follow_winner": self.follow_winner,
            "http1": self.http1,
            "bare_answers": self.bare_answers,
        })
    }
}

/// Labels from before there were several racers start with the protocol.
fn engine_of(label: &str) -> &str {
    match label.split(' ').next() {
        Some(word) if !word.starts_with("HTTP") => word,
        _ => "rust",
    }
}

impl Variants {
    /// Empty lists keep the single race setting.
    pub fn parse(
        args: &RaceArgs,
        engines: &[String],
        alternate_http: bool,
        hedge_ms: Vec<u64>,
        edge_ips: &[String],
        follow_winner: &[String],
        headers: &[String],
    ) -> Result<Self> {
        let edge_ips = edge_ips
            .iter()
            .map(|edge| match edge.trim() {
                "" | "dns" => Ok(Vec::new()),
                edge => edge
                    .split('+')
                    .map(|ip| ip.trim().parse().map_err(|_| anyhow::anyhow!("bad edge address {ip:?}")))
                    .collect(),
            })
            .collect::<Result<Vec<_>>>()?;
        let follow_winner = follow_winner
            .iter()
            .map(|value| match value.trim() {
                "1" | "true" => Ok(true),
                "0" | "false" => Ok(false),
                value => Err(anyhow::anyhow!("follow winner must be 0 or 1, not {value:?}")),
            })
            .collect::<Result<Vec<_>>>()?;
        let bare_answers = headers
            .iter()
            .map(|value| match value.trim() {
                "chrome" => Ok(false),
                "bare" => Ok(true),
                value => Err(anyhow::anyhow!("answer headers must be chrome or bare, not {value:?}")),
            })
            .collect::<Result<Vec<_>>>()?;
        fn or<T>(list: Vec<T>, single: T) -> Vec<T> {
            if list.is_empty() { vec![single] } else { list }
        }
        let engines = engines.iter().map(|engine| Engine::parse(engine)).collect::<Result<Vec<_>>>()?;
        Ok(Self {
            engines: or(engines, Engine { name: "rust".into(), path: None, http1_only: false }),
            http: if alternate_http { vec![false, true] } else { vec![false] },
            hedge_ms: or(hedge_ms, args.hedge_ms),
            edge_ips: or(edge_ips, args.common.edge_ips.clone()),
            follow_winner: or(follow_winner, args.follow_winner),
            bare_answers: or(bare_answers, args.bare_answers),
        })
    }

    /// The combination for one turn: the first list changes every race, the
    /// next one every full cycle of the first, and so on.
    fn pick(&self, turn: usize) -> Variant {
        let mut rest = turn;
        let mut take = |len: usize| {
            let index = rest % len;
            rest /= len;
            index
        };
        let engine = self.engines[take(self.engines.len())].clone();
        let http1 = self.http[take(self.http.len())] || engine.http1_only;
        let hedge_ms = self.hedge_ms[take(self.hedge_ms.len())];
        let edge_ips = self.edge_ips[take(self.edge_ips.len())].clone();
        // Only this binary follows the winner.
        let follow_winner = self.follow_winner[take(self.follow_winner.len())] && engine.path.is_none();
        let bare_answers = self.bare_answers[take(self.bare_answers.len())] && engine.path.is_none();
        Variant { engine, http1, hedge_ms, edge_ips, follow_winner, bare_answers }
    }
}

/// prefix -> variant label -> best.
type Bests = BTreeMap<String, BTreeMap<String, Best>>;

struct Panel {
    args: RaceArgs,
    prefixes: Vec<String>,
    attempts: u8,
    variants: Variants,
    bests: Mutex<Bests>,
    current: Mutex<Value>,
}

/// `ghazi_` -> `next_race_ghazi`.
fn position_path(runs_dir: &Path, prefix: &str) -> PathBuf {
    runs_dir.join(format!("next_race_{}", prefix.trim_end_matches(['.', '_', '-'])))
}

/// The saved position, or the first attempt of `start` if nothing is saved
/// or the saved index is behind `start`.
fn load_position(runs_dir: &Path, prefix: &str, start: u64) -> Position {
    let saved = std::fs::read_to_string(position_path(runs_dir, prefix))
        .ok()
        .and_then(|text| {
            let (index, attempt) = text.trim().split_once(' ')?;
            Some(Position {
                index: index.parse().ok()?,
                attempt: attempt.parse().ok()?,
            })
        });
    match saved {
        Some(position) if position.index >= start && position.attempt >= 1 => position,
        _ => Position { index: start, attempt: 1 },
    }
}

fn save_position(runs_dir: &Path, prefix: &str, position: Position) {
    let saved = std::fs::create_dir_all(runs_dir).and_then(|()| {
        std::fs::write(
            position_path(runs_dir, prefix),
            format!("{} {}\n", position.index, position.attempt),
        )
    });
    if let Err(error) = saved {
        say!("could not save the next race: {error}");
    }
}

fn bests_path(runs_dir: &Path) -> PathBuf {
    runs_dir.join("best_times.json")
}

fn load_bests(runs_dir: &Path) -> Bests {
    std::fs::read_to_string(bests_path(runs_dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_bests(runs_dir: &Path, bests: &Bests) {
    let saved = serde_json::to_string_pretty(bests)
        .map_err(std::io::Error::other)
        .and_then(|text| std::fs::write(bests_path(runs_dir), text));
    if let Err(error) = saved {
        say!("could not save the best times: {error}");
    }
}

/// The server-measured time of a race that reached the goal, from its
/// `score saved:` line.
fn goal_elapsed_ms(lines: &[String]) -> Option<u64> {
    let saved: Value = lines
        .iter()
        .find_map(|line| line.strip_prefix("score saved: "))
        .and_then(|text| serde_json::from_str(text).ok())?;
    let agent_wars = &saved["agentWars"];
    (agent_wars["ended"] == "goal")
        .then(|| agent_wars["elapsedMs"].as_u64())
        .flatten()
}

fn record_race(bests: &mut Bests, prefix: &str, protocol: &str, email: &str, elapsed_ms: u64, at: u64) {
    let slot = bests.entry(prefix.to_string()).or_default();
    let races = slot.get(protocol).map_or(0, |best| best.races) + 1;
    match slot.get_mut(protocol) {
        Some(best) if best.elapsed_ms <= elapsed_ms => best.races = races,
        _ => {
            slot.insert(
                protocol.to_string(),
                Best { elapsed_ms, email: email.to_string(), at, races },
            );
        }
    }
}

pub(crate) fn clean_email(value: &str) -> Result<String, &'static str> {
    let email = value.trim();
    let mut parts = email.split('@');
    let valid = !email.is_empty()
        && email.len() <= 254
        && !email
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        && parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some_and(|part| {
            part.contains('.') && !part.starts_with('.') && !part.ends_with('.')
        })
        && parts.next().is_none();
    valid
        .then(|| email.to_string())
        .ok_or("Enter a valid email address")
}

pub(crate) fn clean_nickname(value: &str) -> Result<String, &'static str> {
    let nickname = value.trim();
    if nickname.is_empty()
        || nickname.chars().count() > 40
        || nickname.chars().any(char::is_control)
    {
        return Err("Leaderboard name must be 1–40 characters");
    }
    Ok(nickname.to_string())
}

/// Across prefixes: the best race and race count per variant, fastest first.
fn by_variant(bests: &Bests) -> Vec<(String, Best)> {
    let mut variants: BTreeMap<String, Best> = BTreeMap::new();
    for (label, best) in bests.values().flatten() {
        let slot = variants.entry(label.clone()).or_insert_with(|| Best { races: 0, ..best.clone() });
        let races = slot.races + best.races;
        if best.elapsed_ms < slot.elapsed_ms {
            *slot = best.clone();
        }
        slot.races = races;
    }
    let mut variants: Vec<_> = variants.into_iter().collect();
    variants.sort_by_key(|(label, best)| (best.elapsed_ms, label.clone()));
    variants
}

/// Per racer (rust, go, cpp): its best race, the variant that ran it, and
/// how many of its races reached the goal.
fn by_engine(variants: &[(String, Best)]) -> Value {
    let mut engines = serde_json::Map::new();
    for (label, best) in variants {
        let engine = engine_of(label);
        let races = engines.get(engine).and_then(|e| e["races"].as_u64()).unwrap_or(0) + best.races;
        if !engines.contains_key(engine) {
            // Fastest first: the first variant seen is the racer's best.
            engines.insert(engine.into(), json!({ "best": best, "variant": label }));
        }
        engines[engine]["races"] = json!(races);
    }
    Value::Object(engines)
}

async fn status(State(panel): State<Arc<Panel>>) -> Json<Value> {
    let args = &panel.args;
    let bests = panel.bests.lock().unwrap().clone();
    let variants = by_variant(&bests);
    Json(json!({
        "current": *panel.current.lock().unwrap(),
        "bests": bests,
        "racers": by_engine(&variants),
        "variants": variants.iter().take(15)
            .map(|(label, best)| json!({ "variant": label, "best": best }))
            .collect::<Vec<_>>(),
        "lines": report::lines(),
        "config": {
            "code": crate::race::competition_code(&args.common.url),
            "nickname": NICKNAME,
            "prefixes": panel.prefixes,
            "emails": panel.prefixes.iter()
                .map(|prefix| email(prefix, 0).replacen("0@", "{i}@", 1))
                .collect::<Vec<_>>(),
            "attempts": panel.attempts,
            "alternate_http": panel.variants.http.len() > 1,
            "hedge_ms": panel.variants.hedge_ms,
            "racers": panel.variants.engines.iter().map(|e| &e.name).collect::<Vec<_>>(),
        },
    }))
}

/// Never returns: races back to back, one prefix after the other.
fn race_forever(panel: &Panel, start: u64) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            say!("error: could not start the race runtime: {error}");
            return;
        }
    };
    let attempts = panel.attempts;
    let mut positions: Vec<Position> = panel
        .prefixes
        .iter()
        .map(|prefix| {
            let mut position = load_position(&panel.args.runs_dir, prefix, start);
            position.attempt = position.attempt.min(attempts);
            position
        })
        .collect();
    for turn in 0usize.. {
        let slot = turn % positions.len();
        let prefix = &panel.prefixes[slot];
        let variant = panel.variants.pick(turn);
        crate::rpc::HTTP1.store(variant.http1, std::sync::atomic::Ordering::Relaxed);
        let protocol = variant.label();
        let position = &mut positions[slot];
        let Position { index, attempt } = *position;
        let email = email(prefix, index);
        let mut args = panel.args.clone();
        args.email = Some(email.clone());
        args.nickname = Some(NICKNAME.to_string());
        args.hedge_ms = variant.hedge_ms;
        args.common.edge_ips = variant.edge_ips.clone();
        args.follow_winner = variant.follow_winner;
        args.bare_answers = variant.bare_answers;
        args.variant = Some(variant.describe());
        *position = position.next(attempts);
        // Saved before racing: a crash mid-race must not repeat this attempt.
        save_position(&args.runs_dir, prefix, *position);
        *panel.current.lock().unwrap() = json!({
            "index": index,
            "email": email,
            "attempt": attempt,
            "protocol": protocol,
            "state": "racing",
        });
        report::clear();
        say!("{NICKNAME} <{email}>: attempt {attempt}/{attempts} over {protocol}");
        let result = match &variant.engine.path {
            None => runtime.block_on(app::run_race(&args)),
            Some(path) => runtime.block_on(run_external(path, &args, &variant)),
        };
        if let Some(elapsed_ms) = goal_elapsed_ms(&report::lines()) {
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs());
            let mut bests = panel.bests.lock().unwrap();
            record_race(&mut bests, prefix, &protocol, &email, elapsed_ms, at);
            save_bests(&args.runs_dir, &bests);
        }
        if let Err(error) = result {
            say!("error: {error:#}");
            panel.current.lock().unwrap()["state"] = json!("failed");
            let started = report::lines()
                .iter()
                .any(|line| line == crate::race::STARTING_RUN);
            if !started {
                // Failed before startRunV2 (Turnstile, warmup): retry this attempt.
                *position = Position { index, attempt };
                save_position(&args.runs_dir, prefix, *position);
            }
            std::thread::sleep(FAILURE_PAUSE);
        }
    }
}

/// One race by another binary: `<path> race` with this race's settings in its
/// environment. Its output goes to the page like ours, so its `score saved:`
/// line is read the same way.
async fn run_external(path: &Path, args: &RaceArgs, variant: &Variant) -> Result<()> {
    use tokio::io::AsyncBufReadExt;
    let edges = variant.edge_ips.iter().map(IpAddr::to_string).collect::<Vec<_>>().join("+");
    let mut command = tokio::process::Command::new(path);
    command
        .arg("race")
        .env("QUIZ_SC_EMAIL", args.email.as_deref().unwrap_or_default())
        .env("QUIZ_SC_NICKNAME", args.nickname.as_deref().unwrap_or_default())
        .env("QUIZ_SC_HEDGE_MS", variant.hedge_ms.to_string())
        .env("QUIZ_SC_MAX_REQUESTS", args.max_requests.to_string())
        .env("QUIZ_SC_RUNS_DIR", &args.runs_dir)
        .env("QUIZ_SC_HTTP1", if variant.http1 { "1" } else { "0" })
        .env("QUIZ_SC_VARIANT", variant.describe().to_string())
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if edges.is_empty() {
        command.env_remove("QUIZ_SC_EDGE_IP");
    } else {
        command.env("QUIZ_SC_EDGE_IP", edges);
    }
    say!("{} race: {}", variant.engine.name, path.display());
    let mut child = command.spawn()?;
    let mut lines = tokio::io::BufReader::new(child.stderr.take().expect("piped")).lines();
    let status = tokio::time::timeout(EXTERNAL_TIMEOUT, async {
        while let Some(line) = lines.next_line().await? {
            say!("{line}");
        }
        child.wait().await
    })
    .await
    .map_err(|_| anyhow::anyhow!("{} race still running after {EXTERNAL_TIMEOUT:?}", variant.engine.name))??;
    anyhow::ensure!(status.success(), "{} race exited with {status}", variant.engine.name);
    Ok(())
}

pub async fn serve(
    args: RaceArgs,
    host: &str,
    port: u16,
    prefixes: Vec<String>,
    attempts: u8,
    variants: Variants,
    start: u64,
) -> Result<()> {
    if prefixes.is_empty() {
        anyhow::bail!("QUIZ_SC_EMAIL_PREFIXES is empty");
    }
    for prefix in &prefixes {
        if let Err(message) = clean_email(&email(prefix, 0)) {
            anyhow::bail!("email prefix {prefix:?} does not make an email: {message}");
        }
    }
    let bests = load_bests(&args.runs_dir);
    let panel = Arc::new(Panel {
        args,
        prefixes,
        attempts,
        variants,
        bests: Mutex::new(bests),
        current: Mutex::new(Value::Null),
    });
    let worker = panel.clone();
    std::thread::spawn(move || race_forever(&worker, start));
    let router = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/status", get(status))
        .with_state(panel);
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    eprintln!("live page on http://{host}:{port}");
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_follow_the_index() {
        assert_eq!(email("ghazi.", 7), "ghazi.7@amundi.com");
        assert_eq!(email("studio_", 7), "studio_7@amundi.com");
        assert!(clean_email(&email("studio_", 0)).is_ok());
        assert!(clean_nickname(NICKNAME).is_ok());
    }

    #[test]
    fn bests_keep_the_fastest_goal_per_prefix_and_protocol() {
        let line = |ended: &str, ms: u64| {
            vec![format!(
                r#"score saved: {{"agentWars":{{"elapsedMs":{ms},"ended":"{ended}"}},"score":1}}"#
            )]
        };
        assert_eq!(goal_elapsed_ms(&line("goal", 10228)), Some(10228));
        assert_eq!(goal_elapsed_ms(&line("wrong", 9000)), None);
        assert_eq!(goal_elapsed_ms(&["run ended".to_string()]), None);

        let mut bests = Bests::new();
        record_race(&mut bests, "ghazi_", "HTTP/2", "ghazi_1@amundi.com", 11000, 1);
        record_race(&mut bests, "ghazi_", "HTTP/2", "ghazi_2@amundi.com", 10500, 2);
        record_race(&mut bests, "ghazi_", "HTTP/2", "ghazi_3@amundi.com", 12000, 3);
        record_race(&mut bests, "ghazi_", "HTTP/1.1", "ghazi_3@amundi.com", 10900, 4);
        let http2 = &bests["ghazi_"]["HTTP/2"];
        assert_eq!((http2.elapsed_ms, http2.email.as_str(), http2.races), (10500, "ghazi_2@amundi.com", 3));
        assert_eq!(bests["ghazi_"]["HTTP/1.1"].races, 1);
    }

    #[test]
    fn races_take_turns_between_every_combination() {
        let variants = Variants {
            engines: vec![Engine::parse("rust").unwrap()],
            http: vec![false],
            hedge_ms: vec![150, 80],
            edge_ips: vec![vec![], vec!["76.76.21.21".parse().unwrap()]],
            follow_winner: vec![false, true],
            bare_answers: vec![false],
        };
        let labels: Vec<String> = (0..8).map(|turn| variants.pick(turn).label()).collect();
        let mut unique = labels.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 8);
        assert_eq!(labels[0], "rust HTTP/2 hedge 150 edge dns fixed");
        assert_eq!(labels[1], "rust HTTP/2 hedge 80 edge dns fixed");
        assert_eq!(labels[7], "rust HTTP/2 hedge 80 edge 76.76.21.21 follow");
        assert_eq!(variants.pick(8), variants.pick(0));
    }

    #[test]
    fn an_edge_setting_can_put_each_route_on_its_own_address() {
        let args = <crate::app::RaceArgs as clap::FromArgMatches>::from_arg_matches(
            &<crate::app::RaceArgs as clap::Args>::augment_args(clap::Command::new("t"))
                .get_matches_from(["t"]),
        )
        .unwrap();
        let edges = ["dns".to_string(), "216.150.1.1+76.76.21.21".to_string()];
        let variants = Variants::parse(&args, &[], false, vec![], &edges, &[], &[]).unwrap();
        assert_eq!(variants.pick(0).label(), "rust HTTP/2 hedge 150 edge dns fixed");
        assert_eq!(variants.pick(1).label(), "rust HTTP/2 hedge 150 edge 216.150.1.1+76.76.21.21 fixed");
        assert!(Variants::parse(&args, &[], false, vec![], &["1.2.3".to_string()], &[], &[]).is_err());
    }

    fn default_args() -> RaceArgs {
        <RaceArgs as clap::FromArgMatches>::from_arg_matches(
            &<RaceArgs as clap::Args>::augment_args(clap::Command::new("t")).get_matches_from(["t"]),
        )
        .unwrap()
    }

    #[test]
    fn racers_change_every_race_and_only_rust_follows_the_winner() {
        let engines = ["rust", "go=/bin/go-racer", "cpp=/bin/cpp-racer:http1"].map(String::from);
        let variants =
            Variants::parse(&default_args(), &engines, false, vec![], &[], &["1".into()], &[]).unwrap();
        let labels: Vec<String> = (0..3).map(|turn| variants.pick(turn).label()).collect();
        assert_eq!(labels, [
            "rust HTTP/2 hedge 150 edge dns follow",
            "go HTTP/2 hedge 150 edge dns fixed",
            "cpp HTTP/1.1 hedge 150 edge dns fixed",
        ]);
        assert_eq!(variants.pick(2).engine.path, Some(PathBuf::from("/bin/cpp-racer")));
        assert!(Engine::parse("go").is_err());
        assert_eq!(engine_of("HTTP/2"), "rust");
        let headers = ["chrome", "bare"].map(String::from);
        let variants = Variants::parse(&default_args(), &engines, false, vec![], &[], &[], &headers).unwrap();
        let labels: Vec<String> = (0..6).map(|turn| variants.pick(turn).label()).collect();
        assert_eq!(labels[3], "rust HTTP/2 hedge 150 edge dns fixed bare");
        assert_eq!(labels[4], "go HTTP/2 hedge 150 edge dns fixed", "only rust answers bare");
        assert!(Variants::parse(&default_args(), &[], false, vec![], &[], &[], &["firefox".into()]).is_err());
        assert_eq!(engine_of("go HTTP/2 hedge 80 edge dns fixed"), "go");
    }

    #[test]
    fn the_page_gets_each_racers_best_across_prefixes() {
        let mut bests = Bests::new();
        record_race(&mut bests, "a_", "rust HTTP/2 hedge 80 edge dns follow", "a_1@x.com", 9000, 1);
        record_race(&mut bests, "b_", "rust HTTP/2 hedge 80 edge dns follow", "b_1@x.com", 8500, 2);
        record_race(&mut bests, "a_", "HTTP/2", "a_2@x.com", 8900, 3);
        record_race(&mut bests, "a_", "go HTTP/2 hedge 150 edge dns fixed", "a_3@x.com", 9100, 4);
        let variants = by_variant(&bests);
        assert_eq!(variants[0].0, "rust HTTP/2 hedge 80 edge dns follow");
        assert_eq!((variants[0].1.elapsed_ms, variants[0].1.races), (8500, 2));
        let racers = by_engine(&variants);
        assert_eq!(racers["rust"]["best"]["email"], "b_1@x.com");
        assert_eq!(racers["rust"]["races"], 3);
        assert_eq!(racers["go"]["best"]["elapsed_ms"], 9100);
    }

    #[test]
    fn another_racer_runs_with_the_race_settings_and_its_output_is_read() {
        let dir = std::env::temp_dir().join(format!("agentwars-external-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("racer.sh");
        std::fs::write(&script, concat!(
            "#!/bin/sh\n",
            "echo \"$1 $QUIZ_SC_EMAIL $QUIZ_SC_HEDGE_MS $QUIZ_SC_EDGE_IP $QUIZ_SC_HTTP1\" >&2\n",
            "echo \"variant $QUIZ_SC_VARIANT\" >&2\n",
            "[ \"$QUIZ_SC_EMAIL\" = fail@x.com ] && exit 3\n",
            "echo 'score saved: {\"agentWars\":{\"elapsedMs\":8123,\"ended\":\"goal\"}}' >&2\n",
        )).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let engine = Engine::parse(&format!("cpp={}:http1", script.display())).unwrap();
        let variant = Variant {
            engine,
            http1: true,
            hedge_ms: 80,
            edge_ips: vec!["216.150.1.1".parse().unwrap(), "76.76.21.21".parse().unwrap()],
            follow_winner: false,
            bare_answers: false,
        };
        let mut args = default_args();
        args.email = Some("ok@x.com".into());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        report::clear();
        runtime.block_on(run_external(variant.engine.path.as_ref().unwrap(), &args, &variant)).unwrap();
        let lines = report::lines();
        assert!(lines.contains(&"race ok@x.com 80 216.150.1.1+76.76.21.21 1".to_string()), "{lines:?}");
        assert!(lines.iter().any(|line| line.starts_with("variant {") && line.contains("\"engine\":\"cpp\"")));
        assert_eq!(goal_elapsed_ms(&lines), Some(8123));
        args.email = Some("fail@x.com".into());
        assert!(runtime.block_on(run_external(variant.engine.path.as_ref().unwrap(), &args, &variant)).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn each_email_gets_its_attempts_then_the_next_starts() {
        let mut position = Position { index: 3, attempt: 1 };
        for expected in 2..=10 {
            position = position.next(10);
            assert_eq!(position, Position { index: 3, attempt: expected });
        }
        assert_eq!(position.next(10), Position { index: 4, attempt: 1 });
    }

    #[test]
    fn the_saved_position_resumes_per_prefix_but_never_goes_below_start() {
        let dir = std::env::temp_dir().join(format!("agentwars-position-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_position(&dir, "ghazi.", 5), Position { index: 5, attempt: 1 });
        save_position(&dir, "ghazi.", Position { index: 42, attempt: 7 });
        assert!(dir.join("next_race_ghazi").exists());
        assert_eq!(load_position(&dir, "ghazi.", 5), Position { index: 42, attempt: 7 });
        assert_eq!(load_position(&dir, "studio_", 5), Position { index: 5, attempt: 1 });
        assert_eq!(load_position(&dir, "ghazi.", 100), Position { index: 100, attempt: 1 });
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
