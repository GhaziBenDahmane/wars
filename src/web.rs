//! A one-page control panel: buttons start a job, the page polls its output.
//! Each job runs on its own thread and runtime, so serving the page never
//! competes with the race loop.

use crate::app::{self, RaceArgs};
use crate::{report, say};
use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const PAGE: &str = include_str!("page.html");

struct Panel {
    args: RaceArgs,
    run_token: Option<String>,
    running: Mutex<Option<String>>,
    last: Mutex<Value>,
}

/// Comparison time does not depend on where the tokens differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn status(State(panel): State<Arc<Panel>>) -> Json<Value> {
    let args = &panel.args;
    Json(json!({
        "running": *panel.running.lock().unwrap(),
        "last": *panel.last.lock().unwrap(),
        "lines": report::lines(),
        "config": {
            "url": args.common.url,
            "email": args.email.as_deref().is_some_and(|e| !e.is_empty()),
            "nickname": args.nickname,
            "hedge_ms": args.hedge_ms,
            "buttons": panel.run_token.is_some(),
        },
    }))
}

async fn run(
    State(panel): State<Arc<Panel>>,
    Path(job): Path<String>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    let reply = |status, message: &str| (status, Json(json!({ "message": message })));
    let Some(expected) = panel.run_token.as_deref() else {
        return reply(StatusCode::FORBIDDEN, "QUIZ_SC_RUN_TOKEN is not set on the server");
    };
    let given = headers.get("x-run-token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !same(given, expected) {
        return reply(StatusCode::UNAUTHORIZED, "wrong run token");
    }
    if !["race", "dry-run", "bench"].contains(&job.as_str()) {
        return reply(StatusCode::NOT_FOUND, "unknown job");
    }
    {
        let mut running = panel.running.lock().unwrap();
        if let Some(current) = running.as_deref() {
            return reply(StatusCode::CONFLICT, &format!("{current} is already running"));
        }
        *running = Some(job.clone());
    }
    report::clear();
    *panel.last.lock().unwrap() = Value::Null;
    let worker = panel.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
        let result = match runtime {
            Ok(runtime) => runtime.block_on(async {
                match job.as_str() {
                    "race" => app::run_race(&worker.args).await,
                    "dry-run" => app::dry_run(&worker.args.common).await,
                    _ => app::bench(&worker.args.common, 20).await,
                }
            }),
            Err(error) => Err(error.into()),
        };
        let outcome = match result {
            Ok(()) => json!({ "job": job, "ok": true }),
            Err(error) => {
                say!("error: {error:#}");
                json!({ "job": job, "ok": false, "error": format!("{error:#}") })
            }
        };
        *worker.last.lock().unwrap() = outcome;
        *worker.running.lock().unwrap() = None;
    });
    reply(StatusCode::ACCEPTED, "started")
}

pub async fn serve(args: RaceArgs, port: u16, run_token: Option<String>) -> Result<()> {
    let run_token = run_token.filter(|t| !t.is_empty());
    if run_token.is_none() {
        eprintln!("QUIZ_SC_RUN_TOKEN is not set: the buttons are disabled");
    }
    let panel = Arc::new(Panel { args, run_token, running: Mutex::new(None), last: Mutex::new(Value::Null) });
    let router = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/status", get(status))
        .route("/run/{job}", post(run))
        .with_state(panel);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    eprintln!("control panel on http://0.0.0.0:{port}");
    axum::serve(listener, router).await?;
    Ok(())
}
