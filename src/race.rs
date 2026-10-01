//! The race loop. Everything off the hot path happens before `startRunV2`
//! (Turnstile, TLS on both routes, Chrome killed); in the loop a question costs
//! one solve (microseconds) plus one hedged round trip. Logs stay in memory and
//! are written once the run is over.

use crate::llm::Llm;
use crate::rpc::{self, Rpc, RpcError};
use crate::solvers;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Margin kept between an LLM fallback and the question deadline.
const SAFETY: Duration = Duration::from_millis(250);

pub struct Config {
    pub code: String,
    pub email: String,
    pub nickname: Option<String>,
    pub locale: String,
    pub hedge_after: Duration,
    pub max_requests: usize,
    pub runs_dir: PathBuf,
}

pub struct Session {
    pub routes: Vec<Rpc>,
    pub base: Value,
}

/// `/play/SUPERCHALLENGE-JAWVUX` -> `JAWVUX` (the slug is cosmetic).
pub fn competition_code(play_url: &str) -> String {
    let path = play_url.split(['?', '#']).next().unwrap_or("").trim_end_matches('/');
    let segment = path.rsplit('/').next().unwrap_or("");
    segment.rsplit('-').next().unwrap_or(segment).to_string()
}

/// Per-question deadline; mirrors the client's linear ramp exactly.
pub fn deadline_ms(agent_wars: &Value, answered: usize) -> f64 {
    let floor = 1000.0 * agent_wars["questionDeadlineSec"].as_f64().unwrap_or(2.0);
    let start = agent_wars["questionDeadlineStartSec"].as_f64().map_or(floor, |s| 1000.0 * s);
    let ramp = agent_wars["questionDeadlineRampQuestions"].as_u64().unwrap_or(0) as f64;
    if ramp <= 0.0 || start <= floor {
        return floor.round();
    }
    (start - (start - floor) * (answered as f64).min(ramp) / ramp).round()
}

/// Drills of an RPC response: `drills`, `next`, or nested one level down.
pub fn find_drills(value: &Value) -> Vec<Value> {
    let mut found: Vec<Value> = Vec::new();
    if let Some(drills) = value["drills"].as_array() {
        found.extend(drills.iter().filter(|d| d.is_object()).cloned());
    }
    if value["next"].is_object() {
        found.push(value["next"].clone());
    }
    if found.is_empty()
        && let Some(object) = value.as_object() {
            for nested in object.values() {
                if let Some(drills) = nested["drills"].as_array() {
                    found.extend(drills.iter().filter(|d| d.is_object()).cloned());
                }
            }
        }
    found
}

pub fn drill_prompt(drill: &Value) -> String {
    match drill["patternData"]["prompt"].as_str() {
        Some(prompt) => prompt.to_string(),
        None => drill["patternData"].to_string(),
    }
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

struct Log {
    lines: Vec<String>,
}

impl Log {
    fn write(&mut self, event: &str, mut data: Value) {
        if let Some(object) = data.as_object_mut() {
            object.insert("t".into(), json!(now()));
            object.insert("event".into(), json!(event));
        }
        self.lines.push(data.to_string());
    }

    fn save(&self, dir: &PathBuf) -> Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("race-{}.jsonl", now() as u64));
        let mut file = std::fs::File::create(&path)?;
        for line in &self.lines {
            writeln!(file, "{line}")?;
        }
        Ok(path)
    }
}

impl Session {
    pub fn new(code: &str, user_agent: &str, cookie: Option<&str>) -> Result<Self> {
        // Two clients = two independent connections for the hedged duplicates.
        let routes = vec![Rpc::new(user_agent, cookie)?, Rpc::new(user_agent, cookie)?];
        Ok(Self { routes, base: json!({ "productId": rpc::PRODUCT_ID, "code": code }) })
    }

    /// Open TLS + HTTP/2 on every route and return the competition.
    pub async fn warm(&self) -> Result<Value> {
        let calls = self.routes.iter().map(|route| route.call("getCompetition", &self.base));
        let mut results = futures_util::future::join_all(calls).await.into_iter();
        let competition = results.next().ok_or_else(|| anyhow!("no route"))??;
        for result in results {
            result?;
        }
        Ok(competition)
    }

    /// Round trips of the (harmless) `getCompetition` call on each route.
    pub async fn bench(&self, rounds: usize) -> Result<Vec<Vec<Duration>>> {
        let mut timings = vec![Vec::new(); self.routes.len()];
        for _ in 0..rounds {
            for (index, (route, timing)) in self.routes.iter().zip(timings.iter_mut()).enumerate() {
                let started = Instant::now();
                match route.call("getCompetition", &self.base).await {
                    Ok(_) => timing.push(started.elapsed()),
                    Err(error) => crate::say!("route {index}: failed after {:?}: {error:#}", started.elapsed()),
                }
            }
        }
        Ok(timings)
    }
}

async fn answer(llm: Option<&Llm>, prompt: &str, budget: Duration) -> (String, &'static str) {
    if let Some(answer) = solvers::solve(prompt) {
        return (answer, "exact");
    }
    if let Some(llm) = llm
        && let Ok(Ok(answer)) = tokio::time::timeout(budget, llm.answer(prompt)).await {
            return (answer, "llm");
        }
    ("?".into(), "none")
}

pub async fn race(session: &Session, config: &Config, turnstile_token: &str, llm: Option<&Llm>) -> Result<()> {
    let base = &session.base;
    let mut log = Log { lines: Vec::new() };
    let result: Result<()> = async {
        let start_input = json!({
            "productId": rpc::PRODUCT_ID,
            "code": config.code,
            "locale": config.locale,
            "uiLocale": config.locale,
            "turnstileToken": turnstile_token,
            "email": config.email,
        });
        let race_started = Instant::now();
        // Never duplicated: a second startRunV2 could spend a second attempt.
        let start = session.routes[0].call("startRunV2", &start_input).await?;
        let mut received = Instant::now();
        log.write("start", json!({ "response": start }));
        if let Some(notice) = start.get("_notice").filter(|n| !n.is_null()) {
            crate::say!("notice: {notice}");
        }
        let run_token = start["runToken"].as_str().context("no runToken")?.to_string();
        let agent_wars = start["setup"]["agentWars"].clone();
        let mut queue = find_drills(&start);
        let mut answered = 0;
        let mut rtt = Duration::from_millis(150);
        let mut summary = Vec::with_capacity(256);
        let ended = loop {
            let Some(drill) = queue.get(answered).cloned() else {
                break "no drill left".to_string();
            };
            let prompt = drill_prompt(&drill);
            let deadline = received + Duration::from_millis(deadline_ms(&agent_wars, answered) as u64);
            let budget = deadline.saturating_duration_since(Instant::now() + rtt + SAFETY);
            let thought = Instant::now();
            let (submission, source) = answer(llm, &prompt, budget).await;
            let solved = Instant::now();
            let input = json!({
                "productId": rpc::PRODUCT_ID,
                "code": config.code,
                "runToken": run_token,
                "drillId": drill["id"],
                "submission": submission,
            });
            let (response, sent) =
                rpc::hedged(&session.routes, "submitAnswerV2", &input, config.hedge_after, config.max_requests).await?;
            received = Instant::now();
            let round_trip = received - solved;
            rtt = (rtt * 7 + round_trip * 3) / 10;
            let correct = response["isCorrect"].as_bool().unwrap_or(false);
            log.write("answer", json!({
                "index": answered,
                "drill": drill,
                "submission": submission,
                "source": source,
                "solve_ms": (solved - thought).as_secs_f64() * 1000.0,
                "rtt_ms": round_trip.as_secs_f64() * 1000.0,
                "requests": sent,
                "response": response,
            }));
            summary.push((source, solved - thought, round_trip, correct));
            if !correct {
                crate::say!("[{}] WRONG {submission:?} for: {prompt}", answered + 1);
            }
            if !correct || response.get("ended").is_some_and(|e| !e.is_null()) {
                answered += correct as usize;
                break format!("{} (score {})", response["ended"], response["runningScore"]);
            }
            answered += 1;
            for next in find_drills(&response) {
                if !queue.iter().any(|d| d["id"] == next["id"]) {
                    queue.push(next);
                }
            }
        };
        let elapsed = race_started.elapsed();
        crate::say!("run ended: {ended} after {answered} correct in {:.3}s", elapsed.as_secs_f64());
        print_summary(&summary);
        if let Some(nickname) = config.nickname.as_deref().filter(|n| !n.is_empty()) {
            let mut input = base.clone();
            input["runToken"] = json!(run_token);
            input["email"] = json!(config.email);
            input["nickname"] = json!(nickname);
            let saved = session.routes[0].call("submitScoreV2", &input).await?;
            crate::say!("score saved: {saved}");
            log.write("score", json!({ "response": saved }));
        } else {
            crate::say!("no nickname: score not submitted to the leaderboard");
        }
        Ok(())
    }
    .await;
    if let Err(error) = &result {
        let data = error.downcast_ref::<RpcError>().map(|e| e.body.clone()).unwrap_or(Value::Null);
        log.write("error", json!({ "error": error.to_string(), "data": data }));
    }
    match log.save(&config.runs_dir) {
        Ok(path) => crate::say!("log: {}", path.display()),
        Err(error) => crate::say!("could not write the log: {error}"),
    }
    result
}

fn print_summary(summary: &[(&str, Duration, Duration, bool)]) {
    if summary.is_empty() {
        return;
    }
    let mut rtts: Vec<f64> = summary.iter().map(|s| s.2.as_secs_f64() * 1000.0).collect();
    rtts.sort_by(f64::total_cmp);
    let pick = |q: f64| rtts[((rtts.len() - 1) as f64 * q).round() as usize];
    let solve_max = summary.iter().map(|s| s.1).max().unwrap_or_default();
    let fallbacks = summary.iter().filter(|s| s.0 != "exact").count();
    crate::say!(
        "rtt ms: p50 {:.1} p90 {:.1} max {:.1} | slowest solve {:?} | non-exact answers {fallbacks}",
        pick(0.5), pick(0.9), pick(1.0), solve_max
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_ramps_to_the_floor() {
        let setup = json!({ "questionDeadlineSec": 2, "questionDeadlineStartSec": 5,
                            "questionDeadlineRampQuestions": 40 });
        assert_eq!(deadline_ms(&setup, 0), 5000.0);
        assert_eq!(deadline_ms(&setup, 20), 3500.0);
        assert_eq!(deadline_ms(&setup, 199), 2000.0);
    }

    #[test]
    fn code_from_url() {
        assert_eq!(competition_code("https://superchallenge.io/play/SUPERCHALLENGE-JAWVUX/"), "JAWVUX");
    }

    #[test]
    fn drills_from_responses() {
        assert_eq!(find_drills(&json!({ "next": { "id": "gen-2" } })).len(), 1);
        assert_eq!(find_drills(&json!({ "run": { "drills": [{ "id": "a" }] } })).len(), 1);
        assert!(find_drills(&json!({ "ended": "goal", "next": null })).is_empty());
    }
}
