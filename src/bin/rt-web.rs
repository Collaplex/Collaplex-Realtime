//! Collaplex RT 实时控制台（本地 Web）。
//!
//! 只读共享内存 + 写参数；不改动 PipeWire 图。启动/停止按钮会调 bin/collaplex-rt。

use axum::{
    Json, Router,
    extract::State,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use clap::Parser;
use collaplex_realtime::{CtlState, GapState, Live, SCOPE_LEN, ScopeState, SharedState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use rustfft::{FftPlanner, num_complex::Complex};

const INDEX: &str = include_str!("../../web/index.html");
const FAVICON: &str = include_str!("../../web/favicon.svg");

#[derive(Parser, Debug)]
#[command(name = "rt-web", about = "Collaplex RT 实时控制台")]
struct Args {
    #[arg(long, default_value_t = 8791)]
    port: u16,
    #[arg(long)]
    root: Option<PathBuf>,
}

struct AppState {
    root: PathBuf,
}

async fn index() -> Html<&'static str> {
    Html(INDEX)
}

async fn favicon() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        FAVICON,
    )
}

fn saved_target() -> String {
    std::fs::read_to_string(
        std::env::var("HOME").unwrap_or_default() + "/.local/state/collaplex-rt/target",
    )
    .unwrap_or_default()
    .trim()
    .to_string()
}

#[derive(Serialize)]
struct OutputEntry {
    name: String,
    description: String,
    analog: bool,
    channels: u64,
    mono: bool,
}

fn sink_channels(name: &str) -> Option<u64> {
    let o = Command::new("pw-dump").output().ok()?;
    let v = serde_json::from_slice::<Value>(&o.stdout).ok()?;
    for n in v.as_array()? {
        if n.get("type").and_then(|t| t.as_str()) != Some("PipeWire:Interface:Node") {
            continue;
        }
        let p = n.get("info").and_then(|i| i.get("props")).cloned().unwrap_or_default();
        if p.get("node.name").and_then(|x| x.as_str()) == Some(name) {
            return p.get("audio.channels").and_then(|x| x.as_u64());
        }
    }
    None
}

async fn outputs(_: State<Arc<AppState>>) -> impl IntoResponse {
    let mut list = Vec::new();
    if let Ok(o) = Command::new("pw-dump").output() {
        if let Ok(v) = serde_json::from_slice::<Value>(&o.stdout) {
            if let Some(arr) = v.as_array() {
                for n in arr {
                    if n.get("type").and_then(|t| t.as_str())
                        != Some("PipeWire:Interface:Node")
                    {
                        continue;
                    }
                    let p = n
                        .get("info")
                        .and_then(|i| i.get("props"))
                        .cloned()
                        .unwrap_or_default();
                    if p.get("media.class").and_then(|x| x.as_str()) != Some("Audio/Sink") {
                        continue;
                    }
                    let name = p.get("node.name").and_then(|x| x.as_str()).unwrap_or("");
                    if name.is_empty() || name.contains("collaplex") {
                        continue;
                    }
                    let desc = p
                        .get("node.description")
                        .and_then(|x| x.as_str())
                        .unwrap_or(name)
                        .to_string();
                    let analog = name.contains("analog")
                        || desc.contains("模拟")
                        || desc.to_lowercase().contains("analog");
                    let channels = p
                        .get("audio.channels")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(2);
                    list.push(OutputEntry {
                        name: name.to_string(),
                        description: desc,
                        analog,
                        channels,
                        mono: channels == 1,
                    });
                }
            }
        }
    }
    let target = saved_target();
    let target_present = list.iter().any(|o| o.name == target);
    Json(json!({"target": target, "target_present": target_present, "outputs": list}))
}

#[derive(Deserialize)]
struct OutputSel {
    name: String,
    mono: Option<bool>,
}

fn installer_path(root: &std::path::Path) -> Option<PathBuf> {
    let src = root.join("scripts/install.sh");
    if src.is_file() {
        return Some(src);
    }
    let pkg = PathBuf::from("/usr/share/collaplex-rt/install.sh");
    if pkg.is_file() {
        return Some(pkg);
    }
    None
}

async fn set_output(
    State(s): State<Arc<AppState>>,
    Json(o): Json<OutputSel>,
) -> impl IntoResponse {
    let Some(inst) = installer_path(&s.root) else {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "找不到安装脚本").into_response();
    };
    let name = o.name.clone();
    // 以设备实际声道数为准：1 声道才混单，立体声设备直出
    let mono = sink_channels(&o.name).map(|c| c == 1).or(o.mono).unwrap_or(false);
    let out = tokio::task::spawn_blocking(move || {
        Command::new("bash")
            .arg(inst)
            .env("TARGET", &name)
            .env("MONO", if mono { "1" } else { "0" })
            .output()
    })
    .await;
    match out {
        Ok(Ok(o)) => {
            let code = if o.status.success() { 200 } else { 500 };
            (
                axum::http::StatusCode::from_u16(code)
                    .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
                String::from_utf8_lossy(&o.stdout).to_string()
                    + &String::from_utf8_lossy(&o.stderr),
            )
                .into_response()
        }
        _ => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "切换失败").into_response(),
    }
}

async fn params(_: State<Arc<AppState>>) -> Json<Live> {
    let st = CtlState::open();
    let c = st.get();
    Json(if c.version() == 0 {
        Live::default()
    } else {
        c.load()
    })
}

async fn set_params(_: State<Arc<AppState>>, Json(p): Json<Live>) -> impl IntoResponse {
    let st = CtlState::open();
    st.get().store(&p);
    Json(json!({"ok": true}))
}

async fn live(_: State<Arc<AppState>>) -> impl IntoResponse {
    let agc_st = SharedState::open();
    let gap_st = GapState::open();
    let ctl_st = CtlState::open();
    let a = agc_st.get();
    let g = gap_st.get();
    let c = ctl_st.get();
    let lufs = |z: f64| -0.691 + 10.0 * z.max(1e-20).log10();
    let e0 = f64::from_bits(a.energy[0].load(Ordering::Acquire));
    let e1 = f64::from_bits(a.energy[1].load(Ordering::Acquire));
    let d0 = f64::from_bits(g.energy[0].load(Ordering::Acquire));
    let d1 = f64::from_bits(g.energy[1].load(Ordering::Acquire));
    let s0 = f64::from_bits(g.shelf_energy[0].load(Ordering::Acquire));
    let s1 = f64::from_bits(g.shelf_energy[1].load(Ordering::Acquire));
    Json(json!({
        "running": c.version() > 0,
        "lufs_l": lufs(e0),
        "lufs_r": lufs(e1),
        "lufs": lufs(e0 + e1),
        "dry_lufs_l": lufs(d0),
        "dry_lufs_r": lufs(d1),
        "dry_lufs": lufs(d0 + d1),
        "dry_block": f64::from_bits(g.dry_block.load(Ordering::Acquire)),
        "dry_clip": f64::from_bits(g.dry_clip.load(Ordering::Acquire)),
        "shelf_lufs": lufs(s0 + s1),
        "shelf_block": f64::from_bits(g.shelf_block.load(Ordering::Acquire)),
        "shelf_clip": f64::from_bits(a.shelf_clip.load(Ordering::Acquire)),
        "wet_clip": f64::from_bits(a.wet_clip.load(Ordering::Acquire)),
        "clip_epoch": a.clip_epoch.load(Ordering::Acquire),
        "verify_wet": f64::from_bits(a.verify_wet.load(Ordering::Acquire)),
        "verify_dry": f64::from_bits(a.verify_dry.load(Ordering::Acquire)),
        "verify_shelf": f64::from_bits(a.verify_shelf.load(Ordering::Acquire)),
        "transfer": f64::from_bits(a.transfer.load(Ordering::Acquire)),
        "verify_epoch": a.verify_epoch.load(Ordering::Acquire),
        "bass_ratio": f64::from_bits(a.bass_ratio.load(Ordering::Acquire)),
        "model_valid": a.model_valid.load(Ordering::Acquire) != 0,
        "dry_bass": f64::from_bits(g.bass_ratio.load(Ordering::Acquire)),
        "gain_db": f64::from_bits(a.gain_db.load(Ordering::Acquire)),
        "l_block": f64::from_bits(a.l_block.load(Ordering::Acquire)),
        "l_smooth": f64::from_bits(a.l_smooth.load(Ordering::Acquire)),
        "clip_state": a.clip_state.load(Ordering::Acquire),
        "gap_active": g.active.load(Ordering::Acquire) != 0,
        "gap_epoch": g.epoch.load(Ordering::Acquire),
        "seq": a.seq[0].load(Ordering::Acquire),
    }))
}

async fn scope(_: State<Arc<AppState>>) -> impl IntoResponse {
    let scope_st = ScopeState::open();
    let sc = scope_st.get();
    let n = 2048usize.min(SCOPE_LEN);
    let mut chans = [vec![0.0f32; n], vec![0.0f32; n]];
    for (ch, data) in chans.iter_mut().enumerate() {
        let w = sc.write[ch].load(Ordering::Acquire) as usize;
        let start = w.saturating_sub(n);
        for (i, v) in data.iter_mut().enumerate() {
            let idx = (start + i) % SCOPE_LEN;
            *v = f32::from_bits(sc.samples[ch][idx].load(Ordering::Relaxed));
        }
    }
    let mut rms = [0.0f64; 2];
    let mut peak = [0.0f64; 2];
    for ch in 0..2 {
        let mut sum = 0.0f64;
        let mut pk = 0.0f64;
        for &x in &chans[ch] {
            let x = x as f64;
            sum += x * x;
            pk = pk.max(x.abs());
        }
        rms[ch] = 10.0 * (sum / n as f64).max(1e-20).log10();
        peak[ch] = 20.0 * pk.max(1e-12).log10();
    }

    // 单声道 + Hann 窗 FFT
    let mono: Vec<f64> = (0..n)
        .map(|i| 0.5 * (chans[0][i] as f64 + chans[1][i] as f64))
        .collect();
    let nfft = n.next_power_of_two();
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(nfft);
    let hann: Vec<f64> = (0..nfft)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / nfft as f64).cos())
        .collect();
    let w_power: f64 = hann.iter().map(|w| w * w).sum::<f64>() / nfft as f64;
    let mut buf: Vec<Complex<f64>> = mono
        .iter()
        .enumerate()
        .map(|(i, x)| Complex::new(x * hann[i], 0.0))
        .collect();
    buf.resize(nfft, Complex::new(0.0, 0.0));
    fft.process(&mut buf);

    let rate = 96000.0f64;
    let bands = 48usize;
    let mut spec = Vec::with_capacity(bands);
    for i in 0..bands {
        let fc = 20.0 * (20000.0f64 / 20.0).powf(i as f64 / (bands - 1) as f64);
        let lo = fc / 2f64.powf(0.25);
        let hi = fc * 2f64.powf(0.25);
        let klo = ((lo * nfft as f64 / rate).floor() as usize).max(1);
        let khi = ((hi * nfft as f64 / rate).ceil() as usize).min(nfft / 2);
        let mut power = 0.0;
        for k in klo..=khi {
            power += buf[k].norm_sqr();
        }
        let bins = (khi.saturating_sub(klo) + 1) as f64;
        let ms = 2.0 * power / (nfft * nfft) as f64 / w_power.max(1e-12) / bins;
        let db = 10.0 * ms.max(1e-20).log10();
        spec.push(json!({"f": fc, "db": db}));
    }

    // 触发式示波器：在最后 1024 点里找上升过零
    let base = n.saturating_sub(1024);
    let mut start = base;
    for i in base + 1..n {
        if mono[i - 1] <= 0.0 && mono[i] > 0.0 {
            start = i;
            break;
        }
    }
    let count = 1024.min(n - start);
    let wave: Vec<f32> = (0..count).map(|i| mono[start + i] as f32).collect();

    Json(json!({
        "rms": rms,
        "peak": peak,
        "wave": wave,
        "spec": spec,
        "write": sc.write[0].load(Ordering::Acquire),
    }))
}

#[derive(Serialize)]
struct Status {
    nodes: Vec<String>,
    default_sink: String,
    target: String,
    hw_rate: u32,
    processes: usize,
    ctl_version: u64,
}

fn node_names() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(o) = Command::new("pw-dump").output() {
        if let Ok(v) = serde_json::from_slice::<Value>(&o.stdout) {
            if let Some(arr) = v.as_array() {
                for n in arr {
                    if n.get("type").and_then(|t| t.as_str())
                        != Some("PipeWire:Interface:Node")
                    {
                        continue;
                    }
                    let p = n
                        .get("info")
                        .and_then(|i| i.get("props"))
                        .cloned()
                        .unwrap_or_default();
                    let name = p.get("node.name").and_then(|x| x.as_str()).unwrap_or("");
                    if name.starts_with("collaplex") {
                        out.push(name.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn default_sink() -> String {
    Command::new("wpctl")
        .args(["inspect", "@DEFAULT_SINK@"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.find("node.name = ")
                    .map(|i| l[i + "node.name = ".len()..].trim_matches('"').trim().to_string())
            })
        })
        .unwrap_or_default()
}

fn hw_rate() -> u32 {
    let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    let card = cards
        .lines()
        .find(|l| l.contains("Fit900NB"))
        .and_then(|l| l.split_whitespace().next());
    if let Some(c) = card {
        let p = format!("/proc/asound/card{c}/pcm0p/sub0/hw_params");
        if let Ok(s) = std::fs::read_to_string(p) {
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("rate:") {
                    return v
                        .split_whitespace()
                        .next()
                        .and_then(|x| x.parse().ok())
                        .unwrap_or(0);
                }
            }
        }
    }
    0
}

fn process_count() -> usize {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            if let Ok(b) = std::fs::read(e.path().join("cmdline")) {
                if String::from_utf8_lossy(&b).contains("collaplex-realtime-bin") {
                    n += 1;
                }
            }
        }
    }
    n
}

async fn status(State(_s): State<Arc<AppState>>) -> Json<Status> {
    let target = saved_target();
    Json(Status {
        nodes: node_names(),
        default_sink: default_sink(),
        target,
        hw_rate: hw_rate(),
        processes: process_count(),
        ctl_version: CtlState::open().get().version(),
    })
}

#[derive(Deserialize)]
struct Action {
    action: String,
}

async fn action(State(s): State<Arc<AppState>>, Json(a): Json<Action>) -> impl IntoResponse {
    let root = s.root.clone();
    let act = a.action;
    tokio::task::spawn_blocking(move || {
        let out = Command::new("bash")
            .arg(root.join("bin/collaplex-rt"))
            .arg(&act)
            .output();
        match out {
            Ok(o) => (
                if o.status.success() { 200 } else { 500 },
                String::from_utf8_lossy(&o.stdout).to_string()
                    + &String::from_utf8_lossy(&o.stderr),
            ),
            Err(e) => (500, format!("{e}")),
        }
    })
    .await
    .map(|(code, msg)| (axum::http::StatusCode::from_u16(code).unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR), msg))
    .unwrap_or((axum::http::StatusCode::INTERNAL_SERVER_ERROR, "action failed".into()))
}

fn project_root(arg: Option<PathBuf>) -> PathBuf {
    if let Some(p) = arg {
        return p;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(release) = exe.parent() {
            if let Some(root) = release.parent().and_then(|p| p.parent()) {
                if root.join("bin/collaplex-rt").is_file() {
                    return root.to_path_buf();
                }
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let state = Arc::new(AppState {
        root: project_root(args.root),
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/favicon.svg", get(favicon))
        .route("/api/outputs", get(outputs))
        .route("/api/output", post(set_output))
        .route("/api/live", get(live))
        .route("/api/scope", get(scope))
        .route("/api/status", get(status))
        .route("/api/params", get(params))
        .route("/api/params", post(set_params))
        .route("/api/action", post(action))
        .with_state(state);
    let addr = format!("127.0.0.1:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("绑定 {addr} 失败: {e}"));
    println!("Collaplex RT 控制台: http://{addr}");
    axum::serve(listener, app).await.ok();
}
