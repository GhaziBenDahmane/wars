//! The race loop. Everything off the hot path happens before `startRunV2`
//! (Turnstile, two warm routes, solver warmup, Chrome killed); in the loop a question costs
//! one solve (microseconds) plus one hedged round trip. Logs stay in memory and
//! are written once the run is over.

use crate::llm::Llm;
use crate::rpc::{self, Rpc, RpcError};
use crate::solvers;
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Margin kept between an LLM fallback and the question deadline.
const SAFETY: Duration = Duration::from_millis(250);
/// Time an LLM fallback always gets, even past the deadline: the alternative
/// is "?", which is wrong anyway, so a late answer can only help.
const LLM_FLOOR: Duration = Duration::from_secs(4);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(30);

/// Logged right before `startRunV2`; a failure without it spent no attempt.
pub const STARTING_RUN: &str = "starting the run";

pub struct Config {
    pub code: String,
    pub email: String,
    pub nickname: Option<String>,
    pub locale: String,
    pub hedge_after: Duration,
    pub max_requests: usize,
    /// Make the route that won a hedged answer the primary for the next ones.
    pub follow_winner: bool,
    /// What this race tries (hedge, edge, protocol...), written to its log.
    pub variant: Value,
    pub runs_dir: PathBuf,
}

pub struct Session {
    pub routes: Vec<Rpc>,
    pub base: Value,
}

/// `/play/SUPERCHALLENGE-JAWVUX` -> `JAWVUX` (the slug is cosmetic).
pub fn competition_code(play_url: &str) -> String {
    let path = play_url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    let segment = path.rsplit('/').next().unwrap_or("");
    segment.rsplit('-').next().unwrap_or(segment).to_string()
}

/// Per-question deadline; mirrors the client's linear ramp exactly.
pub fn deadline_ms(agent_wars: &Value, answered: usize) -> f64 {
    let floor = 1000.0 * agent_wars["questionDeadlineSec"].as_f64().unwrap_or(2.0);
    let start = agent_wars["questionDeadlineStartSec"]
        .as_f64()
        .map_or(floor, |s| 1000.0 * s);
    let ramp = agent_wars["questionDeadlineRampQuestions"]
        .as_u64()
        .unwrap_or(0) as f64;
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
        && let Some(object) = value.as_object()
    {
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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
    pub fn new(
        code: &str,
        user_agent: &str,
        cookie: Option<&str>,
        candidates: usize,
    ) -> Result<Self> {
        ensure!(
            (2..=16).contains(&candidates),
            "connection candidates must be between 2 and 16"
        );
        let routes = (0..candidates)
            .map(|route| Rpc::for_route(user_agent, cookie, route))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            routes,
            base: json!({ "productId": rpc::PRODUCT_ID, "code": code }),
        })
    }

    /// Open TLS + HTTP/2 on every route and return the competition.
    pub async fn warm(&self) -> Result<Value> {
        let mut competition = None;
        for (index, route) in self.routes.iter().enumerate() {
            let (value, info) = route.inspect("getCompetition", &self.base).await?;
            info.report(index);
            competition.get_or_insert(value);
        }
        competition.ok_or_else(|| anyhow!("no route"))
    }

    pub async fn prepare(&mut self, rounds: usize) -> Result<Value> {
        ensure!(
            (3..=10).contains(&rounds),
            "connection samples must be between 3 and 10"
        );
        ensure!(self.routes.len() >= 2, "at least two routes are required");
        tokio::time::timeout(PREPARE_TIMEOUT, async {
            crate::say!("comparing {} connections on getCompetition, not network-only ({rounds} samples each, then rechecking two)", self.routes.len());
            let competition = self.warm().await?;
            let timings = self.bench(rounds).await?;
            let ranked = rank_routes(&timings, rounds);
            ensure!(ranked.len() >= 2, "fewer than two routes completed all connection samples");
            for &index in &ranked {
                report_route("candidate", index, &timings[index]);
            }
            let finalists = &ranked[..2];
            let checked = self.probe(finalists, rounds).await?;
            let winners = rank_routes(&checked, rounds);
            ensure!(winners.len() == 2, "both finalist routes must pass the connection recheck");
            for &index in &winners {
                report_route("recheck", index, &checked[index]);
            }
            self.routes = winners.iter().map(|&index| self.routes[index].clone()).collect();
            crate::say!("selected route {} as primary, route {} as backup", winners[0], winners[1]);
            Ok(competition)
        }).await.context("connection selection exceeded 30 seconds; no attempt started")?
    }

    /// Round trips of the (harmless) `getCompetition` call on each route.
    pub async fn bench(&self, rounds: usize) -> Result<Vec<Vec<Duration>>> {
        self.probe(&(0..self.routes.len()).collect::<Vec<_>>(), rounds)
            .await
    }

    async fn probe(&self, indices: &[usize], rounds: usize) -> Result<Vec<Vec<Duration>>> {
        let mut timings = vec![Vec::new(); self.routes.len()];
        for round in 0..rounds {
            for offset in 0..indices.len() {
                let index = indices[(round + offset) % indices.len()];
                let started = Instant::now();
                match self.routes[index].call("getCompetition", &self.base).await {
                    Ok(_) => timings[index].push(started.elapsed()),
                    Err(error)
                        if error
                            .downcast_ref::<RpcError>()
                            .is_some_and(|error| error.status == 429) =>
                    {
                        return Err(
                            error.context("connection probing throttled; no attempt started")
                        );
                    }
                    Err(error) => crate::say!(
                        "route {index}: failed after {:?}: {error:#}",
                        started.elapsed()
                    ),
                }
            }
        }
        Ok(timings)
    }
}

fn route_stats(timings: &[Duration]) -> (Duration, Duration, Duration) {
    let mut sorted = timings.to_vec();
    sorted.sort();
    let mean = sorted.iter().copied().sum::<Duration>() / sorted.len() as u32;
    let median = (sorted[(sorted.len() - 1) / 2] + sorted[sorted.len() / 2]) / 2;
    let p90 = sorted[((sorted.len() - 1) as f64 * 0.9).round() as usize];
    (mean, median, p90)
}

fn rank_routes(timings: &[Vec<Duration>], rounds: usize) -> Vec<usize> {
    let mut ranked: Vec<usize> = (0..timings.len())
        .filter(|&index| rounds > 0 && timings[index].len() == rounds)
        .collect();
    ranked.sort_by_key(|&index| {
        let (mean, median, p90) = route_stats(&timings[index]);
        ((mean + p90) / 2, median, index)
    });
    ranked
}

fn report_route(stage: &str, index: usize, timings: &[Duration]) {
    let (mean, median, p90) = route_stats(timings);
    crate::say!(
        "getCompetition {stage} route {index}: mean {:.1} p50 {:.1} p90 {:.1} ms",
        mean.as_secs_f64() * 1000.0,
        median.as_secs_f64() * 1000.0,
        p90.as_secs_f64() * 1000.0
    );
}

async fn answer(llm: Option<&Llm>, prompt: &str, budget: Duration) -> (String, &'static str) {
    if let Some(answer) = solvers::solve(prompt) {
        return (answer, "exact");
    }
    if let Some(llm) = llm
        && let Ok(Ok(answer)) =
            tokio::time::timeout(budget.max(LLM_FLOOR), llm.answer(prompt)).await
    {
        return (answer, "llm");
    }
    ("?".into(), "none")
}

pub async fn race(
    session: &Session,
    config: &Config,
    turnstile_token: &str,
    llm: Option<&Llm>,
) -> Result<()> {
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
        // From here on the attempt counts as spent, even if the call fails.
        crate::say!("{STARTING_RUN}");
        let race_started = Instant::now();
        // Never duplicated: a second startRunV2 could spend a second attempt.
        let start = session.routes[0].call("startRunV2", &start_input).await?;
        let mut received = Instant::now();
        log.write("start", json!({ "response": start, "variant": config.variant }));
        let run_token = start["runToken"]
            .as_str()
            .context("no runToken")?
            .to_string();
        let agent_wars = start["setup"]["agentWars"].clone();
        let mut queue = find_drills(&start);
        let mut answered = 0;
        let mut rtt = Duration::from_millis(150);
        let mut summary = Vec::with_capacity(256);
        let mut routes = session.routes.clone();
        let mut swaps = 0;
        let ended = loop {
            let Some(drill) = queue.get(answered).cloned() else {
                break "no drill left".to_string();
            };
            let prompt = drill_prompt(&drill);
            let deadline =
                received + Duration::from_millis(deadline_ms(&agent_wars, answered) as u64);
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
            let hedged = rpc::hedged(
                &routes,
                "submitAnswerV2",
                &input,
                config.hedge_after,
                config.max_requests,
            );
            let reply = match hedged.await {
                Ok(reply) => reply,
                // Too late: the run is over but its score can still be saved.
                Err(error)
                    if error
                        .downcast_ref::<RpcError>()
                        .is_some_and(|e| e.status == 409) =>
                {
                    crate::say!(
                        "[{}] {error:#} ({source} answer {submission:?} for: {prompt})",
                        answered + 1
                    );
                    log.write(
                        "answer",
                        json!({ "index": answered, "drill": drill, "submission": submission,
                                                "source": source, "error": error.to_string() }),
                    );
                    break format!("{error:#}");
                }
                Err(error) => return Err(error),
            };
            let rpc::HedgedResponse {
                value: response,
                sent,
                winner_route,
                timing,
            } = reply;
            received = Instant::now();
            if config.follow_winner && winner_route != 0 {
                routes.swap(0, winner_route);
                swaps += 1;
            }
            let round_trip = received - solved;
            rtt = (rtt * 7 + round_trip * 3) / 10;
            let correct = response["isCorrect"].as_bool().unwrap_or(false);
            log.write(
                "answer",
                json!({
                    "index": answered,
                    "drill": drill,
                    "submission": submission,
                    "source": source,
                    "solve_ms": (solved - thought).as_secs_f64() * 1000.0,
                    "rtt_ms": round_trip.as_secs_f64() * 1000.0,
                    "headers_ms": timing.headers.as_secs_f64() * 1000.0,
                    "body_ms": timing.body.as_secs_f64() * 1000.0,
                    "request_ms": timing.total.as_secs_f64() * 1000.0,
                    "requests": sent,
                    "winner_route": winner_route,
                    "http_version": timing.version,
                    "peer": timing.peer,
                    "x_vercel_id": timing.vercel_id,
                    "server_timing": timing.server_timing,
                    "response": response,
                }),
            );
            summary.push((
                source,
                solved - thought,
                round_trip,
                correct,
                timing.headers,
                timing.body,
            ));
            if source != "exact" {
                crate::say!(
                    "[{}] UNKNOWN ({source}, {}) answered {submission:?} for: {prompt}",
                    answered + 1,
                    if correct { "right" } else { "wrong" }
                );
            } else if !correct {
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
        crate::say!(
            "run ended: {ended} after {answered} correct in {:.3}s ({swaps} primary swaps)",
            elapsed.as_secs_f64()
        );
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
        let data = error
            .downcast_ref::<RpcError>()
            .map(|e| e.body.clone())
            .unwrap_or(Value::Null);
        log.write("error", json!({ "error": error.to_string(), "data": data }));
    }
    match log.save(&config.runs_dir) {
        Ok(path) => crate::say!("log: {}", path.display()),
        Err(error) => crate::say!("could not write the log: {error}"),
    }
    result
}

fn print_summary(summary: &[(&str, Duration, Duration, bool, Duration, Duration)]) {
    if summary.is_empty() {
        return;
    }
    let mut rtts: Vec<f64> = summary.iter().map(|s| s.2.as_secs_f64() * 1000.0).collect();
    rtts.sort_by(f64::total_cmp);
    let pick = |q: f64| rtts[((rtts.len() - 1) as f64 * q).round() as usize];
    let solve_max = summary.iter().map(|s| s.1).max().unwrap_or_default();
    let fallbacks = summary.iter().filter(|s| s.0 != "exact").count();
    let mut headers: Vec<f64> = summary.iter().map(|s| s.4.as_secs_f64() * 1000.0).collect();
    headers.sort_by(f64::total_cmp);
    let header_pick = |q: f64| headers[((headers.len() - 1) as f64 * q).round() as usize];
    let body_max = summary.iter().map(|s| s.5).max().unwrap_or_default();
    crate::say!(
        "rtt ms: p50 {:.1} p90 {:.1} max {:.1} | slowest solve {:?} | non-exact answers {fallbacks}",
        pick(0.5),
        pick(0.9),
        pick(1.0),
        solve_max
    );
    crate::say!(
        "response headers ms: p50 {:.1} p90 {:.1} | slowest body read {:?}",
        header_pick(0.5),
        header_pick(0.9),
        body_max
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{ConnectInfo, Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::{Json, Router, routing::post};
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    struct ProbeState {
        seen: Mutex<Vec<(usize, String, SocketAddr)>>,
        fail_at: Option<usize>,
        failure_status: StatusCode,
    }

    async fn probe_reply(
        State(state): State<Arc<ProbeState>>,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
        Path(procedure): Path<String>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        let index: usize = headers["user-agent"].to_str().unwrap().parse().unwrap();
        let (count, total) = {
            let mut seen = state.seen.lock().unwrap();
            seen.push((index, procedure, peer));
            (
                seen.iter().filter(|entry| entry.0 == index).count(),
                seen.len(),
            )
        };
        if state.fail_at == Some(total) {
            return (
                state.failure_status,
                Json(json!({"json": {"message": "probe failed"}})),
            );
        }
        let delay = match (index, count) {
            (0, _) => 80,
            (1, 1) => 100,
            (1, 2..=4) => 5,
            (1, _) => 70,
            (2, 1..=4) => 30,
            _ => 5,
        };
        tokio::time::sleep(Duration::from_millis(delay)).await;
        (StatusCode::OK, Json(json!({"json": {"route": index}})))
    }

    async fn probe_server(
        fail_at: Option<usize>,
        failure_status: StatusCode,
    ) -> (Session, Arc<ProbeState>, tokio::task::JoinHandle<()>) {
        let state = Arc::new(ProbeState {
            seen: Mutex::new(Vec::new()),
            fail_at,
            failure_status,
        });
        let router = Router::new()
            .route("/api/rpc/superchallenge/{procedure}", post(probe_reply))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let session = Session {
            routes: (0..3)
                .map(|index| Rpc::with_origin(&origin, &index.to_string(), None).unwrap())
                .collect(),
            base: json!({"code": "test"}),
        };
        (session, state, server)
    }

    #[test]
    fn route_ranking_penalizes_tails_and_excludes_incomplete_samples() {
        let timings: Vec<Vec<Duration>> =
            [vec![1, 2, 3, 80, 100], vec![40; 5], vec![30; 5], vec![1; 4]]
                .into_iter()
                .map(|samples| samples.into_iter().map(Duration::from_millis).collect())
                .collect();
        assert_eq!(rank_routes(&timings, 5), vec![2, 1, 0]);
        assert!(rank_routes(&timings, 0).is_empty());
        assert!(rank_routes(&[vec![], vec![]], 5).is_empty());
        let tied = vec![vec![Duration::from_millis(20); 3]; 2];
        assert_eq!(rank_routes(&tied, 3), vec![0, 1]);
    }

    #[test]
    fn candidate_count_is_validated_before_creating_clients() {
        for count in [0, 1, 17, usize::MAX] {
            assert!(Session::new("test", "test", None, count).is_err());
        }
    }

    #[tokio::test]
    async fn preparation_rechecks_finalists_and_reuses_winning_connections() {
        let (mut session, state, server) = probe_server(None, StatusCode::OK).await;
        assert_eq!(session.prepare(3).await.unwrap()["route"], 0);
        assert_eq!(session.routes.len(), 2);
        {
            let seen = state.seen.lock().unwrap();
            assert_eq!(seen.len(), 18);
            assert!(seen.iter().all(|entry| entry.1 == "getCompetition"));
            assert_eq!(
                seen[3..12].iter().map(|entry| entry.0).collect::<Vec<_>>(),
                vec![0, 1, 2, 1, 2, 0, 2, 0, 1]
            );
            assert_eq!(
                seen[12..].iter().map(|entry| entry.0).collect::<Vec<_>>(),
                vec![1, 2, 2, 1, 1, 2]
            );
        }
        assert_eq!(
            session.routes[0]
                .call("submitAnswerV2", &json!({}))
                .await
                .unwrap()["route"],
            2
        );
        assert_eq!(
            session.routes[1]
                .call("submitAnswerV2", &json!({}))
                .await
                .unwrap()["route"],
            1
        );
        {
            let seen = state.seen.lock().unwrap();
            for index in 0..3 {
                let peers: std::collections::HashSet<_> = seen
                    .iter()
                    .filter(|entry| entry.0 == index)
                    .map(|entry| entry.2)
                    .collect();
                assert_eq!(
                    peers.len(),
                    1,
                    "route {index} must reuse its original connection"
                );
            }
            assert_ne!(seen[0].2, seen[1].2);
            assert_ne!(seen[1].2, seen[2].2);
        }
        server.abort();
    }

    #[tokio::test]
    async fn preparation_stops_immediately_on_throttling() {
        for fail_at in [1, 4, 13] {
            let (mut session, state, server) =
                probe_server(Some(fail_at), StatusCode::TOO_MANY_REQUESTS).await;
            let error = session.prepare(3).await.unwrap_err();
            assert_eq!(error.downcast_ref::<RpcError>().unwrap().status, 429);
            assert_eq!(session.routes.len(), 3);
            let seen = state.seen.lock().unwrap();
            assert_eq!(seen.len(), fail_at);
            assert!(seen.iter().all(|entry| entry.1 == "getCompetition"));
            server.abort();
        }
    }

    #[tokio::test]
    async fn preparation_rejects_an_incomplete_finalist_recheck() {
        let (mut session, state, server) =
            probe_server(Some(13), StatusCode::INTERNAL_SERVER_ERROR).await;
        let error = session.prepare(3).await.unwrap_err();
        assert!(error.to_string().contains("recheck"));
        assert_eq!(session.routes.len(), 3);
        assert!(
            state
                .seen
                .lock()
                .unwrap()
                .iter()
                .all(|entry| entry.1 == "getCompetition")
        );
        server.abort();
    }

    #[tokio::test]
    async fn preparation_excludes_a_candidate_with_a_failed_sample() {
        let (mut session, state, server) =
            probe_server(Some(4), StatusCode::INTERNAL_SERVER_ERROR).await;
        session.prepare(3).await.unwrap();
        assert_eq!(session.routes.len(), 2);
        assert!(
            state.seen.lock().unwrap()[12..]
                .iter()
                .all(|entry| entry.0 != 0)
        );
        server.abort();
    }

    #[tokio::test]
    async fn preparation_requires_two_complete_candidates() {
        let (mut session, state, server) =
            probe_server(Some(3), StatusCode::INTERNAL_SERVER_ERROR).await;
        session.routes.truncate(2);
        let error = session.prepare(3).await.unwrap_err();
        assert!(error.to_string().contains("fewer than two routes"));
        let seen = state.seen.lock().unwrap();
        assert_eq!(seen.len(), 8);
        assert!(seen.iter().all(|entry| entry.1 == "getCompetition"));
        server.abort();
    }

    #[tokio::test]
    async fn preparation_validates_sample_count_before_probing() {
        let (mut session, state, server) = probe_server(None, StatusCode::OK).await;
        for rounds in [0, 1, 2, 11] {
            assert!(session.prepare(rounds).await.is_err());
        }
        assert!(state.seen.lock().unwrap().is_empty());
        server.abort();
    }

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
        assert_eq!(
            competition_code("https://superchallenge.io/play/SUPERCHALLENGE-JAWVUX/"),
            "JAWVUX"
        );
    }

    #[test]
    fn drills_from_responses() {
        assert_eq!(find_drills(&json!({ "next": { "id": "gen-2" } })).len(), 1);
        assert_eq!(
            find_drills(&json!({ "run": { "drills": [{ "id": "a" }] } })).len(),
            1
        );
        assert!(find_drills(&json!({ "ended": "goal", "next": null })).is_empty());
    }
}
