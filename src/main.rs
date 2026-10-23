use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
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
#[serde(default)]
struct Reading {
    host: String,
    source: String,
    ts_ms: u64,
    cpu_c: Option<f64>,
    gpu_c: Option<f64>,
    cpu_used_pct: Option<f64>,
    load1: Option<f64>,
    load5: Option<f64>,
    load15: Option<f64>,
    mem_used_pct: Option<f64>,
    mem_total_gb: Option<f64>,
    disk_used_pct: Option<f64>,
    disk_total_gb: Option<f64>,
    uptime_s: Option<u64>,
    cores: Option<u32>,
    freq_mhz: Option<u64>,
    fans: Vec<Fan>,
    zones: Vec<Zone>,
}

#[derive(Serialize)]
struct Snapshot {
    nodes: Vec<Reading>,
    history: HashMap<String, Vec<Reading>>,
    token_required: bool,
}

#[derive(Clone)]
struct AppState {
    inner: Arc<Mutex<Inner>>,
    token: Option<String>,
}

#[derive(Default)]
struct Inner {
    nodes: HashMap<String, Reading>,
    history: HashMap<String, VecDeque<Reading>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn cmd_out(program: &str, args: &[&str]) -> Option<String> {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}

fn hostname() -> String {
    if let Ok(label) = std::env::var("HOST_LABEL") {
        if !label.trim().is_empty() {
            return label.trim().to_string();
        }
    }
    if cfg!(target_os = "macos") {
        if let Some(name) = cmd_out("scutil", &["--get", "ComputerName"]) {
            let name = name.trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    read_trim("/etc/hostname").unwrap_or_else(|| "server".to_string())
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(target_os = "linux")]
fn disk_usage(path: &str) -> Option<(f64, f64)> {
    let c = CString::new(path).ok()?;
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) == 0 && st.f_blocks > 0 {
            let total = st.f_blocks as f64 * st.f_frsize as f64;
            let avail = st.f_bavail as f64 * st.f_frsize as f64;
            return Some(((total - avail) / total * 100.0, total / 1_073_741_824.0));
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn disk_usage(path: &str) -> Option<(f64, f64)> {
    let c = CString::new(path).ok()?;
    unsafe {
        let mut st: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut st) == 0 && st.f_blocks > 0 {
            let total = st.f_blocks as f64 * st.f_bsize as f64;
            let avail = st.f_bavail as f64 * st.f_bsize as f64;
            return Some(((total - avail) / total * 100.0, total / 1_073_741_824.0));
        }
    }
    None
}

fn linux_stats(r: &mut Reading) {
    if let Some(load) = read_trim("/proc/loadavg") {
        let nums: Vec<f64> = load
            .split_whitespace()
            .take(3)
            .filter_map(|v| v.parse().ok())
            .collect();
        if nums.len() == 3 {
            r.load1 = Some(round1(nums[0]));
            r.load5 = Some(round1(nums[1]));
            r.load15 = Some(round1(nums[2]));
        }
    }

    if let Some(mem) = read_trim("/proc/meminfo") {
        let field = |name: &str| -> Option<f64> {
            mem.lines()
                .find(|l| l.starts_with(name))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<f64>().ok())
        };
        if let (Some(total_kb), Some(avail_kb)) = (field("MemTotal:"), field("MemAvailable:")) {
            if total_kb > 0.0 {
                r.mem_used_pct = Some(round1((total_kb - avail_kb) / total_kb * 100.0));
                r.mem_total_gb = Some(round1(total_kb / 1_048_576.0));
            }
        }
    }

    if let Some(up) = read_trim("/proc/uptime") {
        r.uptime_s = up
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok())
            .map(|v| v as u64);
    }

    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        r.cores = Some(cpuinfo.lines().filter(|l| l.starts_with("processor")).count() as u32);
        r.freq_mhz = r.freq_mhz.or_else(|| {
            cpuinfo
                .lines()
                .find(|l| l.starts_with("cpu MHz"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().parse::<f64>().ok())
                .map(|v| v as u64)
        });
    }
    if r.freq_mhz.is_none() {
        if let Some(khz) = read_trim("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq")
            .and_then(|v| v.parse::<u64>().ok())
        {
            r.freq_mhz = Some(khz / 1000);
        }
    }

    let disk_path = if Path::new("/host").exists() { "/host" } else { "/" };
    if let Some((pct, total_gb)) = disk_usage(disk_path) {
        r.disk_used_pct = Some(round1(pct));
        r.disk_total_gb = Some(round1(total_gb));
    }

    r.fans = linux_fans();
}

fn linux_fans() -> Vec<Fan> {
    let mut fans = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") else {
        return fans;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let chip = read_trim(dir.join("name").to_str().unwrap_or("")).unwrap_or_default();
        let Ok(files) = std::fs::read_dir(&dir) else { continue };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            if !name.starts_with("fan") || !name.ends_with("_input") {
                continue;
            }
            let Some(rpm) = read_trim(f.path().to_str().unwrap_or(""))
                .and_then(|v| v.parse::<i64>().ok())
            else {
                continue;
            };
            let label_file = dir.join(name.replace("_input", "_label"));
            let label = read_trim(label_file.to_str().unwrap_or(""))
                .unwrap_or_else(|| format!("{chip} {}", name.trim_end_matches("_input")));
            let max_file = dir.join(name.replace("_input", "_max"));
            let max_rpm = read_trim(max_file.to_str().unwrap_or(""))
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            fans.push(Fan { name: label, rpm, max_rpm });
        }
    }
    fans.truncate(4);
    fans
}

fn macmon_reading(host: &str) -> Result<Reading> {
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

    let mut r = Reading {
        host: host.to_string(),
        source: "macmon".into(),
        ts_ms: now_ms(),
        cpu_c: v["temp"]["cpu_temp_avg"].as_f64(),
        gpu_c: v["temp"]["gpu_temp_avg"].as_f64(),
        cpu_used_pct: v["cpu_usage_pct"].as_f64().map(|x| round1(x * 100.0)),
        freq_mhz: v["pcpu_freq_mhz"].as_u64(),
        fans,
        ..Default::default()
    };

    if let Some(load) = cmd_out("sysctl", &["-n", "vm.loadavg"]) {
        let nums: Vec<f64> = load
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .filter(|s| !s.is_empty())
            .filter_map(|v| v.parse().ok())
            .collect();
        if nums.len() >= 3 {
            r.load1 = Some(round1(nums[0]));
            r.load5 = Some(round1(nums[1]));
            r.load15 = Some(round1(nums[2]));
        }
    }

    let total_bytes = cmd_out("sysctl", &["-n", "hw.memsize"])
        .and_then(|s| s.trim().parse::<f64>().ok());
    if let Some(total) = total_bytes {
        if let Some(vm) = cmd_out("vm_stat", &[]) {
            let page_size = vm
                .lines()
                .next()
                .and_then(|l| l.split("page size of ").nth(1))
                .and_then(|s| s.split_whitespace().next())
                .and_then(|n| n.parse::<f64>().ok())
                .unwrap_or(4096.0);
            let pages = |key: &str| -> f64 {
                vm.lines()
                    .find(|l| l.starts_with(key))
                    .and_then(|l| l.split(':').nth(1))
                    .and_then(|v| v.trim().replace('.', "").parse::<f64>().ok())
                    .unwrap_or(0.0)
            };
            let used = (pages("Pages active") + pages("Pages wired down") + pages("Pages occupied by compressor"))
                * page_size;
            r.mem_used_pct = Some(round1(used / total * 100.0));
            r.mem_total_gb = Some(round1(total / 1_073_741_824.0));
        }
    }

    if let Some(boot) = cmd_out("sysctl", &["-n", "kern.boottime"]) {
        let secs = boot
            .split("sec = ")
            .nth(1)
            .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|s| s.parse::<u64>().ok());
        if let Some(secs) = secs {
            let now = now_ms() / 1000;
            r.uptime_s = Some(now.saturating_sub(secs));
        }
    }

    r.cores = cmd_out("sysctl", &["-n", "hw.ncpu"])
        .and_then(|s| s.trim().parse::<u32>().ok());

    if let Some((pct, total_gb)) = disk_usage("/") {
        r.disk_used_pct = Some(round1(pct));
        r.disk_total_gb = Some(round1(total_gb));
    }

    Ok(r)
}

fn sample_local(host: &str) -> Reading {
    if cfg!(target_os = "macos") {
        macmon_reading(host).unwrap_or_else(|e| Reading {
            host: host.to_string(),
            source: format!("macmon error: {e}"),
            ts_ms: now_ms(),
            ..Default::default()
        })
    } else {
        let mut r = Reading {
            host: host.to_string(),
            source: "sysfs".into(),
            ts_ms: now_ms(),
            ..Default::default()
        };
        r.zones = linux_zones();
        r.cpu_c = estimate_cpu(&r.zones);
        linux_stats(&mut r);
        r
    }
}

fn linux_zones() -> Vec<Zone> {
    let mut zones = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/thermal") else {
        return zones;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let base = entry.path();
        let label = read_trim(base.join("type").to_str().unwrap_or(""))
            .unwrap_or_else(|| name.clone());
        if let Some(milli) = read_trim(base.join("temp").to_str().unwrap_or(""))
            .and_then(|v| v.parse::<f64>().ok())
        {
            let c = milli / 1000.0;
            if (1.0..=150.0).contains(&c) {
                zones.push(Zone {
                    name: label,
                    temp_c: round1(c),
                });
            }
        }
    }
    zones
}

fn estimate_cpu(zones: &[Zone]) -> Option<f64> {
    let cpu_like = |z: &Zone| {
        let n = z.name.to_lowercase();
        ["pkg", "core", "cpu", "tdie", "tctl", "soc"]
            .iter()
            .any(|k| n.contains(k))
    };
    zones
        .iter()
        .filter(|z| cpu_like(z))
        .map(|z| z.temp_c)
        .reduce(f64::max)
        .or_else(|| zones.iter().map(|z| z.temp_c).reduce(f64::max))
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
        let payload = serde_json::to_value(&reading)?;
        let mut req = client.post(url).json(&payload);
        if let Some(t) = &token {
            req = req.header("x-thermal-token", t);
        }
        match req.send() {
            Ok(resp) if resp.status().is_success() => {
                failures = 0;
                eprintln!(
                    "{} pushed cpu={:?} load={:?} mem={:?}",
                    reading.host, reading.cpu_c, reading.load1, reading.mem_used_pct
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
    let inner = state.inner.lock().unwrap();
    let mut nodes: Vec<Reading> = inner.nodes.values().cloned().collect();
    nodes.sort_by(|a, b| {
        let mac = |r: &Reading| (r.source != "macmon") as u8;
        mac(a)
            .cmp(&mac(b))
            .then_with(|| a.host.to_lowercase().cmp(&b.host.to_lowercase()))
    });
    let history = inner
        .history
        .iter()
        .map(|(k, v)| (k.clone(), v.iter().cloned().collect::<Vec<_>>()))
        .collect();
    Json(Snapshot {
        nodes,
        history,
        token_required: state.token.is_some(),
    })
}

async fn api_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut reading): Json<Reading>,
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
    if reading.host.trim().is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    reading.ts_ms = now_ms();
    let host = reading.host.clone();
    let mut inner = state.inner.lock().unwrap();
    inner.nodes.insert(host.clone(), reading.clone());
    let h = inner.history.entry(host).or_default();
    h.push_back(reading);
    while h.len() > HISTORY {
        h.pop_front();
    }
    StatusCode::NO_CONTENT
}

async fn agent_binary() -> Response {
    let bytes = match std::env::current_exe().ok().and_then(|p| std::fs::read(p).ok()) {
        Some(b) => b,
        None => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from(bytes),
    )
        .into_response()
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
        _ => {
            let token = std::env::var("THERMAL_TOKEN").ok().filter(|t| !t.is_empty());
            let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
            let state = AppState {
                inner: Arc::new(Mutex::new(Inner::default())),
                token,
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
                        .route("/agent/binary", get(agent_binary))
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
