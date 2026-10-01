//! Headless Chrome, used for one thing only: the Cloudflare Turnstile token that
//! `startRunV2` requires. Turnstile rejects stock headless Chrome (error
//! 600010); with the `HeadlessChrome` user agent and the automation flag masked
//! it issues a token in a few seconds without any click.

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

pub const TURNSTILE_SITE_KEY: &str = "0x4AAAAAAEnsuR9S0axQ8Ifl";
const PROXY_NOTICE_MARKER: &str = "notify-Notify_";

const TURNSTILE_JS: &str = r#"
new Promise((resolve, reject) => {
  const render = () => {
    let box = document.getElementById('agentwars-turnstile');
    if (!box) {
      box = document.createElement('div');
      box.id = 'agentwars-turnstile';
      box.style.cssText = 'position:fixed;top:12px;left:12px;z-index:2147483647';
      document.body.appendChild(box);
    }
    box.innerHTML = '';
    window.turnstile.render(box, {
      sitekey: SITE_KEY,
      callback: (token) => resolve(token),
      'error-callback': (code) => reject(new Error('turnstile error ' + code)),
    });
  };
  setTimeout(() => reject(new Error('turnstile timeout')), TIMEOUT_MS);
  if (window.turnstile) return render();
  const script = document.createElement('script');
  script.src = 'https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit';
  script.onload = render;
  document.head.appendChild(script);
})
"#;

pub struct Credentials {
    pub turnstile_token: String,
    pub user_agent: String,
    pub cookie: String,
}

struct Cdp {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Cdp {
    async fn send(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({ "id": id, "method": method, "params": params }).to_string();
        self.socket.send(Message::text(message)).await?;
        while let Some(frame) = self.socket.next().await {
            let Message::Text(text) = frame? else { continue };
            let reply: Value = serde_json::from_str(&text)?;
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue; // an event
            }
            if let Some(error) = reply.get("error") {
                bail!("{method}: {error}");
            }
            return Ok(reply["result"].clone());
        }
        bail!("Chrome closed the DevTools connection")
    }

    async fn evaluate(&mut self, expression: &str) -> Result<Value> {
        let result = self
            .send(
                "Runtime.evaluate",
                json!({ "expression": expression, "awaitPromise": true, "returnByValue": true }),
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            let text = details["exception"]["description"].as_str()
                .or(details["text"].as_str())
                .unwrap_or("evaluation failed");
            bail!("{text}");
        }
        Ok(result["result"]["value"].clone())
    }

    /// Wait until the play page has loaded and stopped redirecting.
    async fn settle(&mut self, settle: Duration) -> Result<()> {
        let started = Instant::now();
        let mut stable_since: Option<Instant> = None;
        while started.elapsed() < Duration::from_secs(30) {
            let state = self
                .evaluate("JSON.stringify([location.href, document.readyState])")
                .await
                .ok()
                .and_then(|v| serde_json::from_str::<(String, String)>(v.as_str()?).ok());
            if let Some((href, ready)) = state {
                if href.contains(PROXY_NOTICE_MARKER) {
                    bail!("a proxy disclaimer page is showing instead of the site");
                }
                if ready == "complete" && href.contains("/play/") {
                    let since = *stable_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= settle {
                        return Ok(());
                    }
                } else {
                    stable_since = None;
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        bail!("the play page did not finish loading")
    }
}

/// Chrome's DevTools HTTP endpoints, never through a proxy.
fn local_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(3)).build()?)
}

async fn devtools_ready(cdp_url: &str, child: &mut Option<Child>) -> Result<Value> {
    let client = local_client()?;
    for _ in 0..80 {
        if let Some(child) = child.as_mut()
            && let Some(status) = child.try_wait()? {
                bail!("Chrome exited early with {status}");
            }
        if let Ok(response) = client.get(format!("{cdp_url}/json/version")).send().await
            && let Ok(version) = response.json::<Value>().await {
                return Ok(version);
            }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("Chrome did not open its DevTools port at {cdp_url}")
}

/// The user agent of this Chrome without "Headless". It has to be a command
/// line flag: a CDP override does not reach Turnstile's cross-origin iframe.
async fn user_agent(chrome: &str) -> Result<String> {
    let output = Command::new(chrome).arg("--version").output().await
        .with_context(|| format!("cannot start Chrome at {chrome:?}"))?;
    let version = String::from_utf8_lossy(&output.stdout);
    let major = version
        .split_whitespace()
        .find_map(|word| word.split('.').next()?.parse::<u32>().ok())
        .ok_or_else(|| anyhow!("unexpected `{chrome} --version` output: {version}"))?;
    Ok(format!(
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
    ))
}

fn launch(chrome: &str, port: u16, profile: &str, user_agent: &str) -> Result<Child> {
    Command::new(chrome)
        .args([
            &format!("--user-agent={user_agent}"),
            "--headless=new",
            &format!("--remote-debugging-port={port}"),
            &format!("--user-data-dir={profile}"),
            "--no-sandbox", // containers rarely allow Chrome's own sandbox
            "--disable-dev-shm-usage",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--window-size=1280,900",
            "--disable-blink-features=AutomationControlled",
            "about:blank",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot start Chrome at {chrome:?}"))
}

/// Launch (or attach to `cdp_url`) Chrome, open the play page and get a token.
pub async fn credentials(
    chrome: &str,
    cdp_url: Option<&str>,
    profile: Option<&std::path::Path>,
    play_url: &str,
    timeout: Duration,
) -> Result<Credentials> {
    let port = 9334;
    // A throwaway profile unless one is given (kept, e.g. with accepted notices).
    let temporary = profile.is_none();
    let profile = profile.map(Into::into).unwrap_or_else(|| {
        std::env::temp_dir().join(format!("agentwars-chrome-{}", std::process::id()))
    });
    let launched_agent = match cdp_url {
        Some(_) => None,
        None => Some(user_agent(chrome).await?),
    };
    let mut child = match &launched_agent {
        Some(agent) => Some(launch(chrome, port, &profile.to_string_lossy(), agent)?),
        None => None,
    };
    let cdp_url = cdp_url.map(str::to_string).unwrap_or(format!("http://127.0.0.1:{port}"));
    let result = async {
        let version = devtools_ready(&cdp_url, &mut child).await?;
        let user_agent = launched_agent.clone().unwrap_or_else(|| {
            version["User-Agent"].as_str().unwrap_or_default().replace("HeadlessChrome", "Chrome")
        });
        let targets: Value = local_client()?.get(format!("{cdp_url}/json/list")).send().await?.json().await?;
        let page = targets
            .as_array()
            .and_then(|list| list.iter().find(|t| t["type"] == "page"))
            .ok_or_else(|| anyhow!("Chrome has no page target"))?;
        let ws_url = page["webSocketDebuggerUrl"].as_str().ok_or_else(|| anyhow!("no DevTools URL"))?;
        let (socket, _) = connect_async(ws_url).await.context("DevTools websocket")?;
        let mut cdp = Cdp { socket, next_id: 0 };
        cdp.send("Network.enable", json!({})).await?;
        cdp.send("Page.navigate", json!({ "url": play_url })).await?;
        cdp.settle(Duration::from_millis(1500)).await?;
        let script = TURNSTILE_JS
            .replace("SITE_KEY", &json!(TURNSTILE_SITE_KEY).to_string())
            .replace("TIMEOUT_MS", &timeout.as_millis().to_string());
        let token = cdp.evaluate(&script).await.context("Turnstile")?;
        let cookies = cdp
            .send("Network.getCookies", json!({ "urls": [format!("{}/", crate::rpc::ORIGIN)] }))
            .await?;
        let cookie = cookies["cookies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| Some(format!("{}={}", c["name"].as_str()?, c["value"].as_str()?)))
            .collect::<Vec<_>>()
            .join("; ");
        Ok(Credentials {
            turnstile_token: token.as_str().ok_or_else(|| anyhow!("empty Turnstile token"))?.to_string(),
            user_agent,
            cookie,
        })
    }
    .await;
    if let Some(mut child) = child {
        let _ = child.start_kill(); // free the CPU before the race starts
        let _ = child.wait().await;
        if temporary {
            let _ = std::fs::remove_dir_all(&profile);
        }
    }
    result
}
