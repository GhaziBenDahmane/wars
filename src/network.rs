use crate::rpc::{ORIGIN, PRODUCT_ID};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

#[derive(Deserialize)]
struct Timings {
    time_namelookup: f64,
    time_connect: f64,
    time_appconnect: f64,
    time_starttransfer: f64,
    time_total: f64,
    http_code: u16,
    http_version: String,
    remote_ip: String,
    remote_port: u16,
}

#[derive(Deserialize)]
struct Sample {
    timing: Timings,
    headers: HashMap<String, Vec<String>>,
}

impl Sample {
    fn parse(output: &[u8]) -> Result<Self> {
        let sample: Self = serde_json::from_slice(output)
            .context("invalid curl timing output; network diagnostics require curl >= 7.83")?;
        let timing = &sample.timing;
        ensure!(
            [
                timing.time_namelookup,
                timing.time_connect,
                timing.time_appconnect,
                timing.time_starttransfer,
                timing.time_total,
            ]
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0),
            "invalid curl timing values"
        );
        ensure!(
            timing.time_connect >= timing.time_namelookup
                && (timing.time_appconnect == 0.0 || timing.time_appconnect >= timing.time_connect)
                && timing.time_starttransfer >= sample.setup_seconds()
                && timing.time_total >= timing.time_starttransfer,
            "inconsistent curl timing order"
        );
        Ok(sample)
    }

    fn setup_seconds(&self) -> f64 {
        self.timing.time_appconnect.max(self.timing.time_connect)
    }

    fn tcp_ms(&self) -> f64 {
        (self.timing.time_connect - self.timing.time_namelookup) * 1000.0
    }

    fn tls_ms(&self) -> Option<f64> {
        (self.timing.time_appconnect > 0.0)
            .then_some((self.timing.time_appconnect - self.timing.time_connect) * 1000.0)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .and_then(|(_, values)| values.first())
            .map(String::as_str)
    }

    fn report(&self, index: usize) {
        let timing = &self.timing;
        let tls = self
            .tls_ms()
            .map_or_else(|| "n/a".into(), |value| format!("{value:.2} ms"));
        crate::say!(
            "network probe {}: DNS {:.2} ms | TCP connect {:.2} ms | TLS {tls} | setup {:.2} ms",
            index + 1,
            timing.time_namelookup * 1000.0,
            self.tcp_ms(),
            self.setup_seconds() * 1000.0
        );
        crate::say!(
            "  getCompetition: TTFB {:.2} ms | post-setup wait {:.2} ms | total {:.2} ms",
            timing.time_starttransfer * 1000.0,
            (timing.time_starttransfer - self.setup_seconds()) * 1000.0,
            timing.time_total * 1000.0
        );
        crate::say!(
            "  HTTP/{} peer={}:{} status={} x-vercel-id={:?} server-timing={:?}",
            timing.http_version,
            timing.remote_ip,
            timing.remote_port,
            timing.http_code,
            self.header("x-vercel-id"),
            self.header("server-timing")
        );
    }
}

fn command(origin: &str, code: &str, user_agent: &str) -> Command {
    let mut command = Command::new("curl");
    command
        .arg("--disable")
        .args([
            "--silent",
            "--show-error",
            "--noproxy",
            "*",
            "--tcp-nodelay",
        ])
        .args(["--connect-timeout", "5", "--max-time", "10"])
        .args(["--output", if cfg!(windows) { "NUL" } else { "/dev/null" }])
        .args([
            "--write-out",
            "{\"timing\":%{json},\"headers\":%{header_json}}",
        ])
        .args(["--user-agent", user_agent])
        .args(["--header", "content-type: application/json"])
        .arg("--header")
        .arg(format!("x-product-id: {PRODUCT_ID}"))
        .arg("--header")
        .arg(format!("origin: {ORIGIN}"))
        .arg("--referer")
        .arg(format!("{ORIGIN}/"))
        .arg("--data-binary")
        .arg(json!({"json": {"productId": PRODUCT_ID, "code": code}}).to_string())
        .arg("--url")
        .arg(format!("{origin}/api/rpc/superchallenge/getCompetition"))
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn probe(origin: &str, code: &str, user_agent: &str) -> Result<Sample> {
    let output = tokio::time::timeout(
        Duration::from_secs(12),
        command(origin, code, user_agent).output(),
    )
    .await
    .context("curl network diagnostic exceeded 12 seconds")?
    .context(
        "cannot start curl; install curl >= 7.83, or disable diagnostics with --network-probes 0",
    )?;
    ensure!(
        output.status.success(),
        "curl network diagnostic failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Sample::parse(&output.stdout)
}

pub async fn bench(code: &str, user_agent: &str, rounds: usize) -> Result<()> {
    bench_at(ORIGIN, code, user_agent, rounds).await
}

async fn bench_at(origin: &str, code: &str, user_agent: &str, rounds: usize) -> Result<()> {
    ensure!(rounds <= 10, "network probes must be between 0 and 10");
    if rounds == 0 {
        crate::say!("network setup diagnostics disabled (--network-probes 0)");
        return Ok(());
    }
    crate::say!(
        "network setup diagnostics: {rounds} fresh curl connections; separate from the racer's reqwest pools"
    );
    crate::say!(
        "TCP/TLS setup is not endpoint RTT. TTFB and post-setup wait include network and server work."
    );
    let mut samples = Vec::with_capacity(rounds);
    for index in 0..rounds {
        let sample = probe(origin, code, user_agent).await?;
        sample.report(index);
        ensure!(
            (200..300).contains(&sample.timing.http_code),
            "network diagnostic returned HTTP {}; stopping before further probes",
            sample.timing.http_code
        );
        samples.push(sample);
    }
    let mut connects: Vec<f64> = samples.iter().map(Sample::tcp_ms).collect();
    connects.sort_by(f64::total_cmp);
    let median = (connects[(connects.len() - 1) / 2] + connects[connects.len() / 2]) / 2.0;
    crate::say!(
        "TCP connect summary: min {:.2} p50 {:.2} max {:.2} ms (DNS/TLS excluded)",
        connects[0],
        median,
        connects[connects.len() - 1]
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> serde_json::Value {
        json!({
            "timing": {
                "time_namelookup": 0.002, "time_connect": 0.003, "time_appconnect": 0.007,
                "time_starttransfer": 0.150, "time_total": 0.151,
                "http_code": 200, "http_version": "2", "remote_ip": "127.0.0.1", "remote_port": 443
            },
            "headers": {"x-vercel-id": ["fra1::fra1::test"], "server-timing": ["app;dur=140"]}
        })
    }

    #[test]
    fn separates_cumulative_curl_timings_into_phases() {
        let sample = Sample::parse(&serde_json::to_vec(&fixture()).unwrap()).unwrap();
        assert!((sample.tcp_ms() - 1.0).abs() < 1e-9);
        assert!((sample.tls_ms().unwrap() - 4.0).abs() < 1e-9);
        assert!((sample.setup_seconds() * 1000.0 - 7.0).abs() < 1e-9);
        assert_eq!(sample.header("x-vercel-id"), Some("fra1::fra1::test"));
        assert_eq!(sample.header("Server-Timing"), Some("app;dur=140"));
    }

    #[test]
    fn supports_plain_http_and_missing_routing_headers() {
        let mut fixture = fixture();
        fixture["timing"]["time_appconnect"] = json!(0.0);
        fixture["headers"] = json!({});
        let sample = Sample::parse(&serde_json::to_vec(&fixture).unwrap()).unwrap();
        assert_eq!(sample.tls_ms(), None);
        assert_eq!(sample.setup_seconds(), 0.003);
        assert_eq!(sample.header("x-vercel-id"), None);
    }

    #[test]
    fn rejects_invalid_or_inconsistent_timings() {
        for (field, value) in [
            ("time_namelookup", -1.0),
            ("time_connect", 0.001),
            ("time_appconnect", 0.001),
            ("time_starttransfer", 0.001),
            ("time_total", 0.001),
        ] {
            let mut fixture = fixture();
            fixture["timing"][field] = json!(value);
            assert!(Sample::parse(&serde_json::to_vec(&fixture).unwrap()).is_err());
        }
        assert!(Sample::parse(b"curl: unknown --write-out variable").is_err());
    }

    #[test]
    fn command_is_direct_bounded_and_does_not_start_a_race() {
        let command = command(ORIGIN, "test", "test-agent");
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(args[0], "--disable");
        assert!(args.windows(2).any(|pair| pair == ["--noproxy", "*"]));
        assert!(args.windows(2).any(|pair| pair == ["--max-time", "10"]));
        assert!(args.last().unwrap().ends_with("/getCompetition"));
        assert!(
            !args.iter().any(|arg| arg.contains("startRun")
                || *arg == "--insecure"
                || *arg == "--location")
        );
    }

    #[tokio::test]
    #[ignore = "requires curl >= 7.83; uses only a loopback server"]
    async fn curl_probe_reads_timings_and_headers_from_a_local_server() {
        use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let count = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/api/rpc/superchallenge/getCompetition",
                post(
                    |State(count): State<Arc<AtomicUsize>>,
                     Json(input): Json<serde_json::Value>| async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        let status = if input["json"]["code"] == "throttled" {
                            StatusCode::TOO_MANY_REQUESTS
                        } else {
                            StatusCode::OK
                        };
                        (
                            status,
                            [("x-vercel-id", "fra1::fra1::test")],
                            Json(json!({"json": {}})),
                        )
                    },
                ),
            )
            .with_state(count.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let result = probe(&origin, "test", "test").await;
        let throttled = bench_at(&origin, "throttled", "test", 3).await;
        let disabled = bench_at(&origin, "test", "test", 0).await;
        let invalid = bench_at(&origin, "test", "test", 11).await;
        server.abort();
        let sample = result.unwrap();
        assert_eq!(sample.timing.http_code, 200);
        assert_eq!(sample.timing.remote_ip, "127.0.0.1");
        assert_eq!(sample.header("x-vercel-id"), Some("fra1::fra1::test"));
        assert_eq!(sample.tls_ms(), None);
        assert!(throttled.unwrap_err().to_string().contains("HTTP 429"));
        assert!(disabled.is_ok());
        assert!(invalid.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
