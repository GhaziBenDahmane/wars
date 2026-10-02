//! oRPC client for superchallenge.io: `POST /api/rpc/superchallenge/<proc>` with
//! `{"json": input}`, answering `{"json": output}` (non-2xx on errors).

use anyhow::{Context, Result, anyhow};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;

pub const ORIGIN: &str = "https://superchallenge.io";
pub const PRODUCT_ID: &str = "superchallenge";
const RPC_PREFIX: &str = "/api/rpc/superchallenge/";

/// A verdict from the server (e.g. 409 "question deadline passed"), as opposed
/// to a transport failure: never retried.
#[derive(Debug)]
pub struct RpcError {
    pub procedure: String,
    pub status: u16,
    pub body: Value,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = self.body.get("message").and_then(Value::as_str).unwrap_or("");
        let code = self.body.get("code").and_then(Value::as_str).unwrap_or("");
        write!(f, "{} failed ({} {code}): {message} data={}", self.procedure, self.status,
               self.body.get("data").unwrap_or(&Value::Null))
    }
}

impl std::error::Error for RpcError {}

#[derive(Clone)]
pub struct Rpc {
    client: reqwest::Client,
    endpoint: String,
}

impl Rpc {
    /// One client = one connection pool, so two `Rpc`s give two independent
    /// connections for hedging.
    pub fn new(user_agent: &str, cookie: Option<&str>) -> Result<Self> {
        Self::with_origin(ORIGIN, user_agent, cookie)
    }

    pub fn with_origin(origin: &str, user_agent: &str, cookie: Option<&str>) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("content-type", "application/json".parse()?);
        headers.insert("x-product-id", PRODUCT_ID.parse()?);
        headers.insert("origin", ORIGIN.parse()?);
        headers.insert("referer", format!("{ORIGIN}/").parse()?);
        if let Some(cookie) = cookie.filter(|c| !c.is_empty()) {
            headers.insert("cookie", cookie.parse()?);
        }
        let mut builder = reqwest::Client::builder();
        if origin.starts_with("http://127.0.0.1") {
            builder = builder.no_proxy(); // a local test server, whatever the proxy settings
        }
        let client = builder
            .user_agent(user_agent)
            .default_headers(headers)
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(600))
            .pool_max_idle_per_host(4)
            .http2_keep_alive_interval(Duration::from_secs(15))
            .http2_keep_alive_while_idle(true)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { client, endpoint: format!("{origin}{RPC_PREFIX}") })
    }

    pub async fn call(&self, procedure: &str, input: &Value) -> Result<Value> {
        let body = serde_json::to_vec(&json!({ "json": input }))?;
        self.call_raw(procedure, body).await
    }

    pub async fn call_raw(&self, procedure: &str, body: Vec<u8>) -> Result<Value> {
        let response = self
            .client
            .post(format!("{}{procedure}", self.endpoint))
            .body(body)
            .send()
            .await
            .with_context(|| format!("{procedure}: request failed"))?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.with_context(|| format!("{procedure}: body lost"))?;
        let mut envelope: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let value = envelope.get_mut("json").map(Value::take).unwrap_or(envelope);
        if !(200..300).contains(&status) {
            return Err(RpcError { procedure: procedure.into(), status, body: value }.into());
        }
        Ok(value)
    }
}

/// Hedged call: the network path sometimes stalls or drops a request, which
/// costs the 2 s deadline. The server dedupes a repeated answer (`isReplay`),
/// so a duplicate goes out on the next route whenever the in-flight ones are
/// slower than `hedge_after` (or failed), and the first response wins.
/// Wait before resending after a 429 when nothing else is in flight.
const THROTTLE_PAUSE: Duration = Duration::from_millis(100);

pub async fn hedged(
    routes: &[Rpc],
    procedure: &str,
    input: &Value,
    hedge_after: Duration,
    max_requests: usize,
) -> Result<(Value, usize)> {
    let body = serde_json::to_vec(&json!({ "json": input }))?;
    let mut in_flight = FuturesUnordered::new();
    let mut sent = 0;
    let mut last_error = None;
    // Set by a 429: no more duplicates, and the requests still in flight decide.
    let mut throttled = false;
    loop {
        if sent < max_requests && (!throttled || in_flight.is_empty()) {
            if throttled {
                tokio::time::sleep(THROTTLE_PAUSE).await;
            }
            let route = routes[sent % routes.len()].clone();
            let body = body.clone();
            let procedure = procedure.to_string();
            in_flight.push(async move { route.call_raw(&procedure, body).await });
            sent += 1;
        } else if in_flight.is_empty() {
            return Err(last_error.unwrap_or_else(|| anyhow!("{procedure}: no route answered")));
        }
        let next = if sent < max_requests && !throttled {
            match tokio::time::timeout(hedge_after, in_flight.next()).await {
                Ok(next) => next,
                Err(_) => continue, // slow: send a duplicate
            }
        } else {
            in_flight.next().await
        };
        match next {
            Some(Ok(value)) => return Ok((value, sent)),
            Some(Err(error)) if error.downcast_ref::<RpcError>().is_some_and(|e| e.status == 429) => {
                throttled = true;
                last_error = Some(error);
            }
            Some(Err(error)) if error.downcast_ref::<RpcError>().is_some() => return Err(error),
            Some(Err(error)) => last_error = Some(error), // transport failure: next route now
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A one-request-per-connection server: request `n` (0-based) waits
    /// `delays[n]` ms, then answers `status` with `body`.
    async fn server(delays: Vec<u64>, status: u16, body: &'static str) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let n = seen.fetch_add(1, Ordering::SeqCst);
                let delay = delays.get(n).copied().unwrap_or(0);
                tokio::spawn(async move {
                    let mut buffer = vec![0; 4096];
                    let _ = socket.read(&mut buffer).await;
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        (origin, count)
    }

    fn routes(origin: &str) -> Vec<Rpc> {
        (0..2).map(|_| Rpc::with_origin(origin, "test", None).unwrap()).collect()
    }

    #[tokio::test]
    async fn a_slow_request_is_hedged() {
        let (origin, count) = server(vec![2000, 0], 200, r#"{"json":{"isCorrect":true}}"#).await;
        let started = std::time::Instant::now();
        let (value, sent) = hedged(&routes(&origin), "p", &json!({}), Duration::from_millis(100), 4)
            .await
            .unwrap();
        assert_eq!(value["isCorrect"], true);
        assert_eq!(sent, 2);
        assert!(started.elapsed() < Duration::from_millis(1000));
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_server_verdict_is_not_retried() {
        let (origin, count) = server(vec![], 409, r#"{"json":{"code":"CONFLICT"}}"#).await;
        let error = hedged(&routes(&origin), "p", &json!({}), Duration::from_millis(500), 4)
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref::<RpcError>().unwrap().status, 409);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn requests_are_capped() {
        let (origin, count) = server(vec![400; 10], 200, r#"{"json":1}"#).await;
        let (_, sent) = hedged(&routes(&origin), "p", &json!({}), Duration::from_millis(50), 3)
            .await
            .unwrap();
        assert_eq!(sent, 3);
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_429_stops_the_duplicates_and_retries_alone() {
        let (origin, count) = server(vec![0, 0, 0, 0], 429, r#"{"json":{"message":"Too Many Requests"}}"#).await;
        let route = Rpc::with_origin(&origin, "test", None).unwrap();
        let started = std::time::Instant::now();
        let result = hedged(&[route.clone(), route], "p", &json!({}), Duration::from_millis(5), 3).await;
        assert!(result.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 3);
        assert!(started.elapsed() >= THROTTLE_PAUSE * 2, "{:?}", started.elapsed());
    }
}
