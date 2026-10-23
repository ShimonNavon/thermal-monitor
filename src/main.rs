use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

const HISTORY: usize = 180;

#[derive(Serialize, Deserialize, Clone, Default)]
struct Fan {
    name: String,
    rpm: i64,
    max_rpm: i64,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Zone {
    name: String,
    temp_c: f64,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Reading {
    host: String,
    source: String,
    ts_ms: u64,
    cpu_c: Option<f64>,
    gpu_c: Option<f64>,
    fans: Vec<Fan>,
    zones: Vec<Zone>,
}

#[derive(Deserialize)]
struct Report {
    host: Option<String>,
    source: Option<String>,
    cpu_c: Option<f64>,
    gpu_c: Option<f64>,
    #[serde(default)]
    fans: Vec<Fan>,
}

#[derive(Serialize)]
struct Snapshot {
    mac: Option<Reading>,
    server: Reading,
    mac_history: Vec<Reading>,
    server_history: Vec<Reading>,
    token_required: bool,
}

#[derive(Clone)]
struct AppState {
    inner: Arc<Mutex<Inner>>,
    token: Option<String>,
    host_label: String,
}

#[derive(Default)]
struct Inner {
    mac: Option<Reading>,
    mac_history: VecDeque<Reading>,
    server_history: VecDeque<Reading>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn hostname() -> String {
    if cfg!(target_os = "macos") {
        if let Ok(out) = Command::new("scutil").args(["--get", "ComputerName"]).output() {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOST_LABEL").ok())
        .unwrap_or_else(|| "server".to_string())
}

fn sample_macmon() -> Result<Reading> {
    let mut child = Command::new("macmon")
        .arg("pipe")
        .arg("-i")
        .arg("500")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning macmon (brew install macmon)")?;

    let stdout = child.stdout.take().context("macmon stdout")?;
    let mut line = String::new();
    let read = BufReader::new(stdout).read_line(&mut line);
    let _ = child.kill();
    let _ = child.wait();
    read.context("reading macmon output")?;

    let v: serde_json::Value = serde_json::from_str(line.trim()).context("parsing macmon JSON")?;
    let cpu = v["temp"]["cpu_temp_avg"].as_f64();
    let gpu = v["temp"]["gpu_temp_avg"].as_f64();
    let fans = v["fans"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|f| Fan {
                    name: f["name"].as_str().unwrap_or("fan").to_string(),
                    rpm: f["rpm"].as_i64().unwrap_or(0),
                    max_rpm: f["max_rpm"].as_i64().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Reading {
        host: hostname(),
        source: "macmon".into(),
        ts_ms: now_ms(),
        cpu_c: cpu,
        gpu_c: gpu,
        fans,
        zones: Vec::new(),
    })
}

fn sample_sysfs(host: &str) -> Reading {
    let mut zones = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/thermal") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("thermal_zone") {
                continue;
            }
            let base = entry.path();
            let label = std::fs::read_to_string(base.join("type"))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| name.clone());
            if let Ok(raw) = std::fs::read_to_string(base.join("temp")) {
                if let Ok(milli) = raw.trim().parse::<f64>() {
                    let c = milli / 1000.0;
                    if (1.0..=150.0).contains(&c) {
                        zones.push(Zone {
                            name: label,
                            temp_c: (c * 10.0).round() / 10.0,
                        });
                    }
                }
            }
        }
    }

    let cpu_like = |z: &Zone| {
        let n = z.name.to_lowercase();
        ["pkg", "core", "cpu", "tdie", "tctl", "soc"]
            .iter()
            .any(|k| n.contains(k))
    };
    let cpu_c = zones
        .iter()
        .filter(|z| cpu_like(z))
        .map(|z| z.temp_c)
        .reduce(f64::max)
        .or_else(|| zones.iter().map(|z| z.temp_c).reduce(f64::max));

    Reading {
        host: host.to_string(),
        source: "sysfs".into(),
        ts_ms: now_ms(),
        cpu_c,
        gpu_c: None,
        fans: Vec::new(),
        zones,
    }
}

fn sample_local(host: &str) -> Reading {
    if cfg!(target_os = "macos") {
        match sample_macmon() {
            Ok(r) => r,
            Err(e) => Reading {
                host: host.to_string(),
                source: format!("macmon error: {e}"),
                ts_ms: now_ms(),
                ..Default::default()
            },
        }
    } else {
        sample_sysfs(host)
    }
}

fn token_from_file(path: &str) -> Result<String> {
    Ok(std::fs::read_to_string(Path::new(path))
        .with_context(|| format!("reading token file {path}"))?
        .trim()
        .to_string())
}

fn push_loop(url: &str, token: Option<String>, interval: Duration, host: &str, once: bool) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()?;
    let mut failures = 0u32;
    loop {
        let reading = sample_local(host);
        let payload = serde_json::json!({
            "host": reading.host,
            "source": reading.source,
            "cpu_c": reading.cpu_c,
            "gpu_c": reading.gpu_c,
            "fans": reading.fans,
        });
        let mut req = client.post(url).json(&payload);
        if let Some(t) = &token {
            req = req.header("x-thermal-token", t);
        }
        match req.send() {
            Ok(resp) if resp.status().is_success() => {
                failures = 0;
                eprintln!(
                    "{} pushed cpu={:?} gpu={:?}",
                    reading.host, reading.cpu_c, reading.gpu_c
                );
            }
            Ok(resp) => eprintln!("push failed: HTTP {}", resp.status()),
            Err(e) => {
                failures = failures.saturating_add(1);
                if failures <= 3 || failures % 20 == 0 {
                    eprintln!("push failed: {e}");
                }
            }
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

async fn api_temps(State(state): State<AppState>) -> Json<Snapshot> {
    let server = sample_sysfs(&state.host_label);
    let (mac, mac_history, server_history) = {
        let mut inner = state.inner.lock().unwrap();
        let changed = match (inner.server_history.back(), server.cpu_c) {
            (Some(last), Some(now)) => last.cpu_c.map_or(true, |prev| (prev - now).abs() >= 0.5),
            (None, _) => true,
            _ => false,
        };
        let stale = inner
            .server_history
            .back()
            .map_or(true, |last| server.ts_ms.saturating_sub(last.ts_ms) >= 55_000);
        if changed || stale {
            inner.server_history.push_back(server.clone());
            while inner.server_history.len() > HISTORY {
                inner.server_history.pop_front();
            }
        }
        (
            inner.mac.clone(),
            inner.mac_history.iter().cloned().collect::<Vec<_>>(),
            inner.server_history.iter().cloned().collect::<Vec<_>>(),
        )
    };
    Json(Snapshot {
        mac,
        server,
        mac_history,
        server_history,
        token_required: state.token.is_some(),
    })
}

async fn api_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(report): Json<Report>,
) -> StatusCode {
    if let Some(expected) = &state.token {
        let got = headers
            .get("x-thermal-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if got != expected {
            return StatusCode::UNAUTHORIZED;
        }
    }
    let Some(cpu_c) = report.cpu_c else {
        return StatusCode::BAD_REQUEST;
    };
    let reading = Reading {
        host: report.host.unwrap_or_else(|| "mac".into()),
        source: report.source.unwrap_or_else(|| "push".into()),
        ts_ms: now_ms(),
        cpu_c: Some(cpu_c),
        gpu_c: report.gpu_c,
        fans: report.fans,
        zones: Vec::new(),
    };
    let mut inner = state.inner.lock().unwrap();
    inner.mac = Some(reading.clone());
    inner.mac_history.push_back(reading);
    while inner.mac_history.len() > HISTORY {
        inner.mac_history.pop_front();
    }
    StatusCode::NO_CONTENT
}

async fn health() -> &'static str {
    "ok"
}

async fn index() -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("serve");

    let flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    match mode {
        "push" => {
            let url = flag("--url").context("push needs --url")?;
            let token = match flag("--token-file") {
                Some(p) => Some(token_from_file(&p)?),
                None => flag("--token"),
            };
            let interval = flag("--interval")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(15);
            let host = flag("--host").unwrap_or_else(hostname);
            push_loop(
                &url,
                token,
                Duration::from_secs(interval.max(2)),
                &host,
                args.iter().any(|a| a == "--once"),
            )
        }
        "read" => {
            let reading = sample_local(&hostname());
            println!("{}", serde_json::to_string_pretty(&reading)?);
            Ok(())
        }
        "serve" | _ => {
            let token = std::env::var("THERMAL_TOKEN").ok().filter(|t| !t.is_empty());
            let host_label =
                std::env::var("HOST_LABEL").unwrap_or_else(|_| hostname());
            let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
            let state = AppState {
                inner: Arc::new(Mutex::new(Inner::default())),
                token,
                host_label,
            };

            let addr: std::net::SocketAddr = bind.parse().context("BIND address")?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(async move {
                    let app = Router::new()
                        .route("/", get(index))
                        .route("/api/temps", get(api_temps))
                        .route("/api/report", post(api_report))
                        .route("/healthz", get(health))
                        .with_state(state);
                    let listener = tokio::net::TcpListener::bind(addr).await?;
                    eprintln!("listening on http://{addr}");
                    axum::serve(listener, app).await?;
                    Ok::<(), anyhow::Error>(())
                })
        }
    }
}
