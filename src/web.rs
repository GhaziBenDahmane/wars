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

/// What one race tries. Every combination of the lists, in turn.
#[derive(Clone, Debug, PartialEq)]
pub struct Variants {
    http: Vec<bool>,
    hedge_ms: Vec<u64>,
    edge_ips: Vec<Vec<IpAddr>>,
    follow_winner: Vec<bool>,
    headers: Vec<Headers>,
    think_ms: Vec<u64>,
    real_page: Vec<bool>,
    duplicates: Vec<usize>,
}

/// Which headers the answers carry (the run is always started with Chrome's).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Headers {
    Chrome,
    /// No user agent or cookie.
    Bare,
    /// Only `content-type`.
    Minimal,
}

#[derive(Clone, Debug, PartialEq)]
struct Variant {
    http1: bool,
    hedge_ms: u64,
    /// One per route in turn; empty keeps DNS.
    edge_ips: Vec<IpAddr>,
    follow_winner: bool,
    /// Answers without Chrome's user agent and cookie (this binary only).
    bare_answers: bool,
    /// Answers with only `content-type` (this binary only).
    minimal_answers: bool,
    /// Pause before each answer (this binary only).
    think_ms: u64,
    /// Turnstile from the real play page instead of the stub (this binary only).
    real_page: bool,
    /// Spare requests for answers sent twice at once (this binary only).
    duplicates: usize,
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
        let (protocol, hedge) = (self.protocol(), self.hedge_ms);
        let headers = match (self.bare_answers, self.minimal_answers) {
            (_, true) => " minimal",
            (true, false) => " bare",
            _ => "",
        };
        let think = if self.think_ms > 0 { format!(" think {}", self.think_ms) } else { String::new() };
        let page = if self.real_page { " real-page" } else { "" };
        let duplicates = if self.duplicates > 0 { format!(" dup {}", self.duplicates) } else { String::new() };
        format!("rust {protocol} hedge {hedge} edge {edge} {follow}{headers}{think}{page}{duplicates}")
    }

    /// Written to the race log, to compare variants afterwards.
    fn describe(&self) -> Value {
        json!({
            "engine": "rust",
            "hedge_ms": self.hedge_ms,
            "edge_ips": self.edge_ips,
            "follow_winner": self.follow_winner,
            "http1": self.http1,
            "bare_answers": self.bare_answers,
            "minimal_answers": self.minimal_answers,
            "think_ms": self.think_ms,
            "real_page": self.real_page,
            "duplicates": self.duplicates,
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
        alternate_http: bool,
        hedge_ms: Vec<u64>,
        edge_ips: &[String],
        follow_winner: &[String],
        headers: &[String],
        think_ms: Vec<u64>,
        pages: &[String],
        duplicates: Vec<usize>,
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
        let headers = headers
            .iter()
            .map(|value| match value.trim() {
                "chrome" => Ok(Headers::Chrome),
                "bare" => Ok(Headers::Bare),
                "minimal" => Ok(Headers::Minimal),
                value => Err(anyhow::anyhow!("answer headers must be chrome, bare or minimal, not {value:?}")),
            })
            .collect::<Result<Vec<_>>>()?;
        let real_page = pages
            .iter()
            .map(|value| match value.trim() {
                "stub" => Ok(false),
                "real" => Ok(true),
                value => Err(anyhow::anyhow!("Turnstile page must be stub or real, not {value:?}")),
            })
            .collect::<Result<Vec<_>>>()?;
        let single_headers = match (args.bare_answers, args.minimal_answers) {
            (_, true) => Headers::Minimal,
            (true, false) => Headers::Bare,
            _ => Headers::Chrome,
        };
        fn or<T>(list: Vec<T>, single: T) -> Vec<T> {
            if list.is_empty() { vec![single] } else { list }
        }
        Ok(Self {
            http: if alternate_http { vec![false, true] } else { vec![false] },
            hedge_ms: or(hedge_ms, args.hedge_ms),
            edge_ips: or(edge_ips, args.common.edge_ips.clone()),
            follow_winner: or(follow_winner, args.follow_winner),
            headers: or(headers, single_headers),
            think_ms: or(think_ms, args.think_ms),
            real_page: or(real_page, args.common.real_page),
            duplicates: or(duplicates, args.duplicates),
        })
    }

    /// How many combinations there are: one cycle of turns.
    fn count(&self) -> usize {
        self.http.len()
            * self.hedge_ms.len()
            * self.edge_ips.len()
            * self.follow_winner.len()
            * self.headers.len()
            * self.think_ms.len()
            * self.real_page.len()
            * self.duplicates.len()
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
        let http1 = self.http[take(self.http.len())];
        let hedge_ms = self.hedge_ms[take(self.hedge_ms.len())];
        let edge_ips = self.edge_ips[take(self.edge_ips.len())].clone();
        let follow_winner = self.follow_winner[take(self.follow_winner.len())];
        let headers = self.headers[take(self.headers.len())];
        let bare_answers = headers != Headers::Chrome;
        let minimal_answers = headers == Headers::Minimal;
        let think_ms = self.think_ms[take(self.think_ms.len())];
        let real_page = self.real_page[take(self.real_page.len())];
        let duplicates = self.duplicates[take(self.duplicates.len())];
        Variant {
            http1, hedge_ms, edge_ips, follow_winner, bare_answers, minimal_answers, think_ms, real_page, duplicates,
        }
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
            "racers": ["rust"],
        },
    }))
}

/// A Turnstile token older than this is thrown away (they expire after 300 s).
const TOKEN_MAX_AGE: Duration = Duration::from_secs(240);

/// Keeps one set of Chrome credentials ready on its own thread, so the next
/// race starts without waiting for Chrome.
fn prefetch_credentials(common: crate::app::Common) -> std::sync::mpsc::Receiver<(std::time::Instant, crate::browser::Credentials)> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            return;
        };
        loop {
            match runtime.block_on(app::credentials(&common)) {
                Ok(credentials) => {
                    if sender.send((std::time::Instant::now(), credentials)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    say!("error: prefetching the Turnstile token: {error:#}");
                    std::thread::sleep(FAILURE_PAUSE);
                }
            }
        }
    });
    receiver
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
    let tokens = prefetch_credentials(panel.args.common.clone());
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
        args.minimal_answers = variant.minimal_answers;
        args.think_ms = variant.think_ms;
        args.common.real_page = variant.real_page;
        args.duplicates = variant.duplicates;
        args.variant = Some(variant.describe());
        // Whole cycles run to the end, so every combination gets full races.
        if args.full_every > 0 && (turn / panel.variants.count()) % args.full_every == 0 {
            args.abort_after = 0;
            if let Some(described) = args.variant.as_mut() {
                described["full_race"] = json!(true);
            }
        }
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
        // Real-page races get their own token; the prefetched ones use the stub.
        if variant.real_page == panel.args.common.real_page {
            args.prefetched = std::iter::from_fn(|| tokens.recv().ok())
                .find(|(got, _)| got.elapsed() < TOKEN_MAX_AGE)
                .map(|(_, credentials)| credentials);
        }
        let result = runtime.block_on(app::run_race(&args));
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
        } else {
            std::thread::sleep(Duration::from_millis(args.pause_ms));
        }
    }
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
            http: vec![false],
            hedge_ms: vec![150, 80],
            edge_ips: vec![vec![], vec!["76.76.21.21".parse().unwrap()]],
            follow_winner: vec![false, true],
            headers: vec![Headers::Chrome],
            think_ms: vec![0],
            real_page: vec![false],
            duplicates: vec![0],
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
        assert_eq!(variants.count(), 8);
    }

    #[test]
    fn an_edge_setting_can_put_each_route_on_its_own_address() {
        let args = <crate::app::RaceArgs as clap::FromArgMatches>::from_arg_matches(
            &<crate::app::RaceArgs as clap::Args>::augment_args(clap::Command::new("t"))
                .get_matches_from(["t"]),
        )
        .unwrap();
        let edges = ["dns".to_string(), "216.150.1.1+76.76.21.21".to_string()];
        let variants = Variants::parse(&args, false, vec![], &edges, &[], &[], vec![], &[], vec![]).unwrap();
        assert_eq!(variants.pick(0).label(), "rust HTTP/2 hedge 150 edge dns fixed");
        assert_eq!(variants.pick(1).label(), "rust HTTP/2 hedge 150 edge 216.150.1.1+76.76.21.21 fixed");
        assert!(Variants::parse(&args, false, vec![], &["1.2.3".to_string()], &[], &[], vec![], &[], vec![]).is_err());
    }

    fn default_args() -> RaceArgs {
        <RaceArgs as clap::FromArgMatches>::from_arg_matches(
            &<RaceArgs as clap::Args>::augment_args(clap::Command::new("t")).get_matches_from(["t"]),
        )
        .unwrap()
    }

    #[test]
    fn answer_headers_pauses_and_pages_take_turns() {
        let headers = ["chrome", "minimal"].map(String::from);
        let pages = ["stub", "real"].map(String::from);
        let variants =
            Variants::parse(&default_args(), false, vec![], &[], &["1".into()], &headers, vec![0, 8], &pages, vec![])
                .unwrap();
        let labels: Vec<String> = (0..8).map(|turn| variants.pick(turn).label()).collect();
        assert_eq!(labels[0], "rust HTTP/2 hedge 150 edge dns follow");
        assert_eq!(labels[1], "rust HTTP/2 hedge 150 edge dns follow minimal");
        assert_eq!(labels[2], "rust HTTP/2 hedge 150 edge dns follow think 8");
        assert_eq!(labels[4], "rust HTTP/2 hedge 150 edge dns follow real-page");
        assert_eq!(labels[7], "rust HTTP/2 hedge 150 edge dns follow minimal think 8 real-page");
        let minimal = variants.pick(1);
        assert!(minimal.bare_answers && minimal.minimal_answers);
        assert_eq!(minimal.describe()["minimal_answers"], true);
        assert!(Variants::parse(&default_args(), false, vec![], &[], &[], &["firefox".into()], vec![], &[], vec![]).is_err());
        assert!(Variants::parse(&default_args(), false, vec![], &[], &[], &[], vec![], &["live".into()], vec![]).is_err());
        assert_eq!(engine_of("HTTP/2"), "rust");
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
