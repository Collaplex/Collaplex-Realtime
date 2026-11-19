//! Collaplex 实时 DSP —— PipeWire `pipe` 插件载荷。
//!
//! 每声道一个进程（PipeWire 的 pipe 插件按声道 fork）。处理链：
//!
//!   低搁架(250 Hz, +8 dB) → 前馈短时 LUFS AGC（双声道共享能量）
//!   → lookahead 峰值限幅 → 输出（左右平衡在 PipeWire 图里）
//!
//! BRIR/影院 IR 卷积在 PipeWire 的 convolver 图里完成，不在本进程。
//! HRTF 前另有两路干声检测（CX_DETECT=1，gapL/gapR）：间隙检测 + BS.1770
//! 响度锚点（重采样后、任何处理前），当前只读取对比，不参与增益。
//! 本进程绝不主动退出；出错时尽量透传，避免整条链静音。

use collaplex_realtime::{CtlState, GapShared, GapState, Live, ScopeState, Shared, SharedState};
use fs2::FileExt;
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::sync::atomic::Ordering;

const BLOCK_BYTES: usize = 4096;

// ---------------- 配置 ----------------

#[derive(Clone, Debug)]
struct Config {
    rate: f64,
    shelf_db: f64,
    shelf_freq: f64,
    shelf_s: f64,
    agc_init_db: f64,
    agc: bool,
    agc_ceiling: f64,
    agc_deadzone: f64,
    agc_tau_down: f64,
    agc_tau_up: f64,
    agc_det_tau: f64,
    agc_boost: f64,
    agc_up_rate: f64,
    agc_down_s: f64,
    agc_silence_release: f64,
    agc_cut: f64,
    agc_gate: f64,
    agc_activate: f64,
    agc_hyst: f64,
    agc_clip: bool,
    clip_measure_s: f64,
    clip_boundary_ms: f64,
    clip_boundary_lufs: f64,
    clip_recheck_s: f64,
    verify_delay_s: f64,
    verify_ramp_s: f64,
    verify_deadzone: f64,
    dry_transfer_init: f64,
    trim_window_s: f64,
    trim_fast_s: f64,
    trim_interval_s: f64,
    trim_settle_s: f64,
    trim_down_max: f64,
    trim_down_ramp_s: f64,
    trim_up_margin: f64,
    trim_up_kp: f64,
    trim_up_max: f64,
    trim_up_ramp_s: f64,
    trim_hold_s: f64,
    trim_cut_max: f64,
    trim_boost_max: f64,
    gap_dbfs: f64,
    gap_ms: f64,
    limit_ceiling: f64,
}

fn env_f(name: &str, def: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(def)
}

fn env_bool(name: &str, def: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(v.as_str(), "0" | "false" | "off" | ""),
        Err(_) => def,
    }
}

impl Config {
    fn from_env() -> Self {
        Self {
            rate: env_f("CX_RATE", 96_000.0),
            shelf_db: env_f("CX_SHELF_DB", 8.0),
            shelf_freq: env_f("CX_SHELF_FREQ", 250.0),
            shelf_s: env_f("CX_SHELF_S", 1.79),
            agc_init_db: env_f("CX_AGC_INIT_DB", -18.0),
            agc: env_bool("CX_AGC", true),
            agc_ceiling: env_f("CX_AGC_CEIL", -18.5),
            agc_deadzone: env_f("CX_AGC_DZ", 2.0),
            agc_tau_down: env_f("CX_AGC_TAU_DOWN", 0.15),
            agc_tau_up: env_f("CX_AGC_TAU_UP", 1.5),
            agc_det_tau: env_f("CX_AGC_DET_TAU", 1.0),
            agc_boost: env_f("CX_AGC_BOOST", 12.0),
            agc_up_rate: env_f("CX_AGC_UP_RATE", 6.0),
            agc_down_s: env_f("CX_AGC_DOWN_S", 0.08),
            agc_silence_release: env_f("CX_AGC_SILENCE_RELEASE", 1.5),
            agc_cut: env_f("CX_AGC_CUT", 18.0),
            agc_gate: env_f("CX_AGC_GATE", -70.0),
            agc_activate: env_f("CX_AGC_ACTIVATE", -50.0),
            agc_hyst: env_f("CX_AGC_HYST", 3.0),
            agc_clip: std::env::var("CX_AGC_MODE")
                .map(|v| v != "continuous")
                .unwrap_or(true),
            clip_measure_s: env_f("CX_CLIP_MEASURE_S", 0.6),
            clip_boundary_ms: env_f("CX_CLIP_BOUNDARY_MS", 300.0),
            clip_boundary_lufs: env_f("CX_CLIP_BOUNDARY_LUFS", -60.0),
            clip_recheck_s: env_f("CX_CLIP_RECHECK_S", 0.0),
            verify_delay_s: env_f("CX_VERIFY_DELAY_S", 1.3),
            verify_ramp_s: env_f("CX_VERIFY_RAMP_S", 2.0),
            verify_deadzone: env_f("CX_VERIFY_DZ", 0.5),
            dry_transfer_init: env_f("CX_DRY_TRANSFER_INIT", -3.0),
            trim_window_s: env_f("CX_TRIM_WINDOW_S", 2.0),
            trim_fast_s: env_f("CX_TRIM_FAST_S", 0.3),
            trim_interval_s: env_f("CX_TRIM_INTERVAL_S", 0.5),
            trim_settle_s: env_f("CX_TRIM_SETTLE_S", 2.0),
            trim_down_max: env_f("CX_TRIM_DOWN_MAX", 6.0),
            trim_down_ramp_s: env_f("CX_TRIM_DOWN_RAMP_S", 0.08),
            trim_up_margin: env_f("CX_TRIM_UP_MARGIN", 1.0),
            trim_up_kp: env_f("CX_TRIM_UP_KP", 0.3),
            trim_up_max: env_f("CX_TRIM_UP_MAX", 0.8),
            trim_up_ramp_s: env_f("CX_TRIM_UP_RAMP_S", 3.0),
            trim_hold_s: env_f("CX_TRIM_HOLD_S", 3.0),
            trim_cut_max: env_f("CX_TRIM_CUT_MAX", 24.0),
            trim_boost_max: env_f("CX_TRIM_BOOST_MAX", 3.0),
            gap_dbfs: env_f("CX_GAP_DBFS", -50.0),
            gap_ms: env_f("CX_GAP_MS", 200.0),
            limit_ceiling: env_f("CX_LIMIT_CEIL", 0.891),
        }
    }
}

fn claim_slot() -> Option<usize> {
    for i in 0..2 {
        let path = format!("/dev/shm/collaplex-rt-agc.lock{i}");
        if let Ok(f) = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
        {
            if f.try_lock_exclusive().is_ok() {
                // 锁要一直持有：泄漏 File，进程退出时内核释放
                std::mem::forget(f);
                return Some(i);
            }
        }
    }
    None
}

fn set_pdeathsig() {
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
    }
}

fn live_from_cfg(cfg: &Config) -> Live {
    Live {
        agc: cfg.agc,
        clip: cfg.agc_clip,
        ceiling: cfg.agc_ceiling,
        deadzone: cfg.agc_deadzone,
        tau_down: cfg.agc_tau_down,
        tau_up: cfg.agc_tau_up,
        det_tau: cfg.agc_det_tau,
        activate: cfg.agc_activate,
        hyst: cfg.agc_hyst,
        boost: cfg.agc_boost,
        cut: cfg.agc_cut,
        gate: cfg.agc_gate,
        init_db: cfg.agc_init_db,
        clip_measure: cfg.clip_measure_s,
        clip_recheck: cfg.clip_recheck_s,
        shelf_db: cfg.shelf_db,
        shelf_freq: cfg.shelf_freq,
        shelf_s: cfg.shelf_s,
        gap_dbfs: cfg.gap_dbfs,
        gap_ms: cfg.gap_ms,
        limit_ceil: cfg.limit_ceiling,
    }
}

fn apply_live(
    cfg: &mut Config,
    shelf: &mut Biquad,
    limiter: &mut Limiter,
    agc: &mut Agc,
    l: &Live,
) {
    let shelf_changed =
        cfg.shelf_db != l.shelf_db || cfg.shelf_freq != l.shelf_freq || cfg.shelf_s != l.shelf_s;
    let mode_changed = cfg.agc_clip != l.clip;
    cfg.agc = l.agc;
    cfg.agc_clip = l.clip;
    cfg.agc_ceiling = l.ceiling;
    cfg.agc_deadzone = l.deadzone;
    cfg.agc_tau_down = l.tau_down;
    cfg.agc_tau_up = l.tau_up;
    cfg.agc_det_tau = l.det_tau;
    cfg.agc_activate = l.activate;
    cfg.agc_hyst = l.hyst;
    cfg.agc_boost = l.boost;
    cfg.agc_cut = l.cut;
    cfg.agc_gate = l.gate;
    cfg.agc_init_db = l.init_db;
    cfg.clip_measure_s = l.clip_measure;
    cfg.clip_recheck_s = l.clip_recheck;
    cfg.shelf_db = l.shelf_db;
    cfg.shelf_freq = l.shelf_freq;
    cfg.shelf_s = l.shelf_s;
    cfg.gap_dbfs = l.gap_dbfs;
    cfg.gap_ms = l.gap_ms;
    cfg.limit_ceiling = l.limit_ceil;
    if shelf_changed {
        *shelf = Biquad::low_shelf(cfg.shelf_freq, cfg.shelf_db, cfg.shelf_s, cfg.rate);
    }
    limiter.ceiling = cfg.limit_ceiling;
    if mode_changed {
        agc.reset_mode();
    }
    if !cfg.agc {
        agc.gain_to_unity();
    }
}

// ---------------- biquad ----------------

#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn from_parts(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn low_shelf(freq: f64, gain_db: f64, s: f64, fs: f64) -> Self {
        let amp = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * freq / fs;
        let (cos_w0, sin_w0) = (w0.cos(), w0.sin());
        let alpha = sin_w0 / 2.0 * ((amp + 1.0 / amp) * (1.0 / s - 1.0) + 2.0).sqrt();
        let sq = amp.sqrt();
        let b0 = amp * ((amp + 1.0) - (amp - 1.0) * cos_w0 + 2.0 * sq * alpha);
        let b1 = 2.0 * amp * ((amp - 1.0) - (amp + 1.0) * cos_w0);
        let b2 = amp * ((amp + 1.0) - (amp - 1.0) * cos_w0 - 2.0 * sq * alpha);
        let a0 = (amp + 1.0) + (amp - 1.0) * cos_w0 + 2.0 * sq * alpha;
        let a1 = -2.0 * ((amp - 1.0) + (amp + 1.0) * cos_w0);
        let a2 = (amp + 1.0) + (amp - 1.0) * cos_w0 - 2.0 * sq * alpha;
        Self::from_parts(b0, b1, b2, a0, a1, a2)
    }

    fn k_high_shelf(fs: f64) -> Self {
        let f0 = 1681.974_450_955_533;
        let gain_db = 3.999_843_853_973_347;
        let q = 0.707_175_236_955_419_6;
        let k = (std::f64::consts::PI * f0 / fs).tan();
        let vh = 10f64.powf(gain_db / 20.0);
        let vb = vh.powf(0.499_666_774_154_541_6);
        let a0 = 1.0 + k / q + k * k;
        let b0 = (vh + vb * k / q + k * k) / a0;
        let b1 = 2.0 * (k * k - vh) / a0;
        let b2 = (vh - vb * k / q + k * k) / a0;
        let a1 = 2.0 * (k * k - 1.0) / a0;
        let a2 = (1.0 - k / q + k * k) / a0;
        Self::from_parts(b0, b1, b2, 1.0, a1, a2)
    }

    /// RBJ 低通（Q=0.707 为 Butterworth），用于低频能量占比。
    pub fn low_pass(freq: f64, q: f64, fs: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * freq / fs;
        let (cos_w0, sin_w0) = (w0.cos(), w0.sin());
        let alpha = sin_w0 / (2.0 * q);
        let b0 = (1.0 - cos_w0) / 2.0;
        let b1 = 1.0 - cos_w0;
        let b2 = (1.0 - cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;
        Self::from_parts(b0, b1, b2, a0, a1, a2)
    }

    fn k_high_pass(fs: f64) -> Self {
        let f0 = 38.135_470_876_024_44;
        let q = 0.500_327_037_323_877_3;
        let k = (std::f64::consts::PI * f0 / fs).tan();
        let a0 = 1.0 + k / q + k * k;
        let b0 = 1.0 / a0;
        let b1 = -2.0 / a0;
        let b2 = 1.0 / a0;
        let a1 = 2.0 * (k * k - 1.0) / a0;
        let a2 = (1.0 - k / q + k * k) / a0;
        Self::from_parts(b0, b1, b2, 1.0, a1, a2)
    }

    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

// ---------------- lookahead 限幅 ----------------

struct Limiter {
    delay: usize,
    ring: Vec<f64>,
    w: usize,
    idx: u64,
    win: VecDeque<(u64, f64)>,
    gain: f64,
    attack: f64,
    release: f64,
    ceiling: f64,
}

impl Limiter {
    fn new(rate: f64, ceiling: f64) -> Self {
        let delay = ((0.005 * rate).round() as usize).max(1);
        Self {
            delay,
            ring: vec![0.0; delay],
            w: 0,
            idx: 0,
            win: VecDeque::with_capacity(delay + 1),
            gain: 1.0,
            attack: 1.0 - (-1.0 / (0.001 * rate)).exp(),
            release: 1.0 - (-1.0 / (0.080 * rate)).exp(),
            ceiling,
        }
    }

    fn process(&mut self, x: f64) -> f64 {
        let delayed = self.ring[self.w];
        self.ring[self.w] = x;
        self.w = (self.w + 1) % self.delay;
        self.idx += 1;

        let ax = x.abs();
        while let Some(&(_, v)) = self.win.back() {
            if v <= ax {
                self.win.pop_back();
            } else {
                break;
            }
        }
        self.win.push_back((self.idx, ax));
        while let Some(&(i, _)) = self.win.front() {
            if i + self.delay as u64 <= self.idx {
                self.win.pop_front();
            } else {
                break;
            }
        }
        let peak = self.win.front().map(|&(_, v)| v).unwrap_or(0.0);
        let target = if peak > self.ceiling {
            self.ceiling / peak
        } else {
            1.0
        };
        let coef = if target < self.gain {
            self.attack
        } else {
            self.release
        };
        self.gain += (target - self.gain) * coef;
        delayed * self.gain
    }
}

// ---------------- AGC ----------------

struct Agc {
    slot: usize,
    ring: VecDeque<f64>,
    l_smooth: f64,
    have_l: bool,
    gain_db: f64,
    gain_now_db: f64,
    gain_start: f64,
    gain_end: f64,
    target_db: f64,
    ramp_pos: usize,
    gain_tau_coef: f64,
    gain_max_step: f64,
    period: usize,
    det_n: usize,
    det_energy: f64,
    det_raw_sq: f64,
    silent: usize,
    peer_z: f64,
    peer_seq: u64,
    active: bool,
    below: usize,
    clip_state: u8, // 0 wait / 1 measure / 2 hold
    clip_silent: usize,
    clip_measure_blocks: usize,
    clip_measure_energy: f64,
    clip_hold_blocks: u64,
    gap_epoch: u64,
    wet_clip: f64,
    dry_clip: f64,
    shelf_clip: f64,
    clip_meas_epoch: u64,
    ramp_len: usize,
    verify_state: u8, // 0 空闲 / 1 等尾巴 / 2 采集中
    verify_wait: usize,
    verify_energy: f64,
    verify_blocks: usize,
    verify_wet: f64,
    verify_dry: f64,
    verify_shelf: f64,
    verify_epoch: u64,
    transfer_est: f64,
    have_transfer: bool,
    bass_ratio: f64,
    model_valid: bool,
    model_n: f64,
    model_sx: f64,
    model_sy: f64,
    model_sxx: f64,
    model_sxy: f64,
    model_a: f64,
    model_b: f64,
    trim_ring: VecDeque<f64>,
    trim_fast_ring: VecDeque<f64>,
    trim_base_gain: f64,
    trim_hold_blocks: usize,
    trim_interval_count: usize,
    kf: (Biquad, Biquad),
}

impl Agc {
    fn new(cfg: &Config) -> Self {
        let period = ((0.1 * cfg.rate).round() as usize).max(1);
        let init = cfg.agc_init_db;
        Self {
            slot: 0,
            ring: VecDeque::with_capacity(32),
            l_smooth: -120.0,
            have_l: false,
            gain_db: init,
            gain_now_db: init,
            gain_start: init,
            gain_end: init,
            target_db: init,
            ramp_pos: period,
            ramp_len: period,
            gain_tau_coef: 1.0,
            gain_max_step: 0.0,
            period,
            det_n: 0,
            det_energy: 0.0,
            det_raw_sq: 0.0,
            silent: 0,
            peer_z: 0.0,
            peer_seq: 0,
            active: false,
            below: 0,
            clip_state: 0,
            clip_silent: 0,
            clip_measure_blocks: 0,
            clip_measure_energy: 0.0,
            clip_hold_blocks: 0,
            gap_epoch: 0,
            wet_clip: -120.0,
            dry_clip: -120.0,
            shelf_clip: -120.0,
            clip_meas_epoch: 0,
            verify_state: 0,
            verify_wait: 0,
            verify_energy: 0.0,
            verify_blocks: 0,
            verify_wet: -120.0,
            verify_dry: -120.0,
            verify_shelf: -120.0,
            verify_epoch: 0,
            transfer_est: cfg.dry_transfer_init,
            have_transfer: false,
            bass_ratio: -60.0,
            // 先验模型 transfer ≈ -3.2 + 1.30·bass_ratio（当前 HRTF/房间链的标定值），
            // 用 3 个伪观测起头；之后由真实校验样本逐步覆盖。
            model_valid: true,
            model_n: 3.0,
            model_sx: -3.6,
            model_sy: -14.28,
            model_sxx: 5.94,
            model_sxy: 19.242,
            model_a: -3.2,
            model_b: 1.30,
            trim_ring: VecDeque::with_capacity(64),
            trim_fast_ring: VecDeque::with_capacity(16),
            trim_base_gain: init,
            trim_hold_blocks: 0,
            trim_interval_count: 0,
            kf: (
                Biquad::k_high_shelf(cfg.rate),
                Biquad::k_high_pass(cfg.rate),
            ),
        }
    }

    /// 误差比例收敛：差值越大走得越快（有速率上限），越接近目标越慢，
    /// 单调逼近、不回冲。ramp_len 是“按原平均速率走完”的参考时长。
    #[inline]
    fn gain_linear(&mut self) -> f64 {
        let err = self.gain_end - self.gain_now_db;
        if err.abs() < 1e-4 {
            self.gain_now_db = self.gain_end;
        } else {
            let step = (err * (1.0 - self.gain_tau_coef))
                .clamp(-self.gain_max_step, self.gain_max_step);
            self.gain_now_db += step;
            if (self.gain_now_db - self.gain_end).abs() < 1e-3 {
                self.gain_now_db = self.gain_end;
            }
        }
        self.ramp_pos = (self.ramp_pos + 1).min(self.ramp_len);
        10f64.powf(self.gain_now_db / 20.0)
    }

    fn start_ramp(&mut self, start: f64, end: f64, len: usize) {
        self.gain_start = start;
        self.gain_end = end;
        self.ramp_len = len.max(1);
        self.ramp_pos = 0;
        // 一阶收敛的时间常数：len/4；速率上限 = 原线性平均速率
        self.gain_tau_coef = (-4.0 / self.ramp_len as f64).exp();
        self.gain_max_step = (end - start).abs() / self.ramp_len as f64;
    }

    /// 用内容特征（低频占比）预测房间传递；模型未成型时退回滑动估计。
    fn transfer_pred(&self) -> f64 {
        // 模型只在标定过的低频占比范围内可信；超出范围（很薄/很闷的内容）
        // 直接退回近期的实测传递估计，绝不外推——外推会给出过大的提升。
        if self.model_valid && (-6.0..=0.5).contains(&self.bass_ratio) {
            (self.model_a + self.model_b * self.bass_ratio).clamp(-8.0, -1.0)
        } else {
            self.transfer_est.clamp(-8.0, -1.0)
        }
    }

    fn reset_mode(&mut self) {
        self.clip_state = 0;
        self.clip_silent = 0;
        self.clip_measure_blocks = 0;
        self.clip_measure_energy = 0.0;
        self.clip_hold_blocks = 0;
        self.ring.clear();
        self.have_l = false;
        self.active = false;
        self.below = 0;
        self.l_smooth = -120.0;
        self.verify_state = 0;
        self.verify_wait = 0;
        self.verify_energy = 0.0;
        self.verify_blocks = 0;
        self.reset_trim();
    }

    fn gain_to_unity(&mut self) {
        if self.gain_now_db.abs() > 1e-9 {
            let prev = self.gain_now_db;
            self.gain_db = 0.0;
            self.start_ramp(prev, 0.0, self.period);
        }
    }

    /// 快降慢升：衰减用很短的渐变，提升按 dB/s 限制（最多 max_up_s）。
    fn set_gain_target(&mut self, cfg: &Config, target: f64, max_up_s: f64) {
        // 从“实际正在输出的增益”出发，保证换目标时曲线连续（不能从上一次的
        // 目标值出发，否则渐变中途改目标会瞬跳）。
        let prev = self.gain_now_db;
        if (target - prev).abs() < 1e-9 {
            self.gain_db = target;
            return;
        }
        self.gain_db = target;
        self.target_db = target;
        let secs = if target < prev {
            cfg.agc_down_s
        } else {
            let d = target - prev;
            (d / cfg.agc_up_rate.max(0.1))
                .clamp(cfg.agc_down_s, max_up_s.max(cfg.agc_down_s))
        };
        let len = ((secs * cfg.rate).round() as usize).max(1);
        self.start_ramp(prev, target, len);
    }

    /// 干声静音时把正增益慢慢放回 0 dB，不携带提升进入下一段。
    fn silence_release(&mut self, cfg: &Config, block_seconds: f64) {
        if self.gain_now_db > 0.0 {
            let next = (self.gain_now_db - cfg.agc_silence_release * block_seconds).max(0.0);
            if next < self.gain_now_db {
                let prev = self.gain_now_db;
                self.gain_db = next;
                self.start_ramp(prev, next, self.period);
            }
        }
    }

    fn reset_trim(&mut self) {
        self.trim_ring.clear();
        self.trim_fast_ring.clear();
        self.trim_hold_blocks = 0;
        self.trim_interval_count = 0;
        self.trim_base_gain = self.gain_db;
    }

    fn apply_trim(&mut self, cfg: &Config, adj: f64, ramp_s: f64) {
        let target = (self.gain_now_db + adj)
            .clamp(
                self.trim_base_gain - cfg.trim_cut_max,
                self.trim_base_gain + cfg.trim_boost_max,
            )
            .clamp(-cfg.agc_cut, cfg.agc_boost);
        let prev = self.gain_now_db;
        self.gain_db = target;
        self.target_db = target;
        let len = ((ramp_s * cfg.rate).round() as usize).max(1);
        self.start_ramp(prev, target, len);
    }

    /// 非对称闭环修正：超过基准用 0.6 s 快窗立刻降；低于基准要先持续
    /// 3 s、且瞄准“基准 − 1 dB”才慢慢抬，防止在目标附近来回震荡。
    fn trim_update(&mut self, cfg: &Config, joint: f64) {
        // 测量阶段也要守住上限：换片后前 0.6 s 仍在用上一段的增益
        if self.clip_state == 0 {
            return;
        }
        // 输出能量 = 增益前联合能量 × 增益² = joint × 10^(gain_db/10)
        let out_z = joint * 10f64.powf(self.gain_now_db / 10.0);
        self.trim_ring.push_back(out_z);
        self.trim_fast_ring.push_back(out_z);
        let window_blocks = ((cfg.trim_window_s / 0.1).round() as usize).max(1);
        let fast_blocks = ((cfg.trim_fast_s / 0.1).round() as usize).max(1);
        while self.trim_ring.len() > window_blocks {
            self.trim_ring.pop_front();
        }
        while self.trim_fast_ring.len() > fast_blocks {
            self.trim_fast_ring.pop_front();
        }

        // 1) 响度上限：快窗每块都检查，超过 CEIL 立刻降，不等慢窗/不等渐变走完
        if self.trim_fast_ring.len() >= fast_blocks {
            let fz = self.trim_fast_ring.iter().sum::<f64>() / fast_blocks as f64;
            let fast = -0.691 + 10.0 * fz.max(1e-20).log10();
            if fast > cfg.agc_ceiling + 0.1 {
                let adj =
                    (cfg.agc_ceiling - fast).clamp(-cfg.trim_down_max, 0.0);
                if adj < -0.01 {
                    self.apply_trim(cfg, adj, cfg.trim_down_ramp_s);
                    self.trim_fast_ring.clear();
                    self.trim_hold_blocks = 0;
                    return;
                }
            }
        }

        // 2) 慢抬：等增益渐变和稳定期结束，慢窗持续低于 CEIL−margin 才抬
        if (self.gain_now_db - self.gain_db).abs() > 0.1 {
            return;
        }
        let settle_blocks = ((cfg.trim_settle_s / 0.1).round() as usize).max(1);
        if (self.clip_hold_blocks as usize) < settle_blocks {
            return;
        }
        self.trim_interval_count += 1;
        let interval_blocks = ((cfg.trim_interval_s / 0.1).round() as usize).max(1);
        if self.trim_interval_count < interval_blocks || self.trim_ring.len() < window_blocks {
            return;
        }
        self.trim_interval_count = 0;
        let sz = self.trim_ring.iter().sum::<f64>() / self.trim_ring.len() as f64;
        let slow = -0.691 + 10.0 * sz.max(1e-20).log10();
        if slow < cfg.agc_ceiling - cfg.trim_up_margin {
            self.trim_hold_blocks += interval_blocks;
            let hold_blocks = ((cfg.trim_hold_s / 0.1).round() as usize).max(1);
            if self.trim_hold_blocks >= hold_blocks {
                let err = (cfg.agc_ceiling - cfg.trim_up_margin) - slow;
                let adj = (err * cfg.trim_up_kp).clamp(0.0, cfg.trim_up_max);
                if adj > 0.01 {
                    self.apply_trim(cfg, adj, cfg.trim_up_ramp_s);
                }
                self.trim_hold_blocks = 0;
            }
        } else {
            self.trim_hold_blocks = 0;
        }
    }

    fn publish(&self, shared: &Shared, slot: usize, l_block: f64) {
        if slot == 0 {
            shared.gain_db.store(self.gain_now_db.to_bits(), Ordering::Release);
            shared.clip_state.store(self.clip_state as u64, Ordering::Release);
            shared.l_block.store(l_block.to_bits(), Ordering::Release);
            shared.l_smooth.store(self.l_smooth.to_bits(), Ordering::Release);
            shared.wet_clip.store(self.wet_clip.to_bits(), Ordering::Release);
            shared.dry_clip.store(self.dry_clip.to_bits(), Ordering::Release);
            shared.clip_epoch.store(self.clip_meas_epoch, Ordering::Release);
            shared.shelf_clip.store(self.shelf_clip.to_bits(), Ordering::Release);
            shared.verify_wet.store(self.verify_wet.to_bits(), Ordering::Release);
            shared.verify_dry.store(self.verify_dry.to_bits(), Ordering::Release);
            shared.verify_shelf.store(self.verify_shelf.to_bits(), Ordering::Release);
            shared.transfer.store(self.transfer_pred().to_bits(), Ordering::Release);
            shared.verify_epoch.store(self.verify_epoch, Ordering::Release);
            shared.bass_ratio.store(self.bass_ratio.to_bits(), Ordering::Release);
            shared.model_valid.store(self.model_valid as u64, Ordering::Release);
        }
    }

    fn feed_detector(&mut self, s: f64) {
        let k = self.kf.0.process(s);
        let k = self.kf.1.process(k);
        self.det_energy += k * k;
        self.det_raw_sq += s * s;
        self.det_n += 1;
    }

    /// 按片段归一：静音边界 → 测开头一段 → 定增益 → 冻结到下一次换片。
    fn clip_update(&mut self, cfg: &Config, joint: f64, l_block: f64, gap: &GapShared) {
        let boundary_blocks = ((cfg.clip_boundary_ms / 100.0).round() as usize).max(1);
        let measure_blocks = ((cfg.clip_measure_s / 0.1).round() as usize).max(1);

        if l_block < cfg.clip_boundary_lufs {
            self.clip_silent += 1;
            if self.clip_silent >= boundary_blocks {
                self.clip_state = 0; // 等待下一段音频
                self.verify_state = 0;
            }
            return;
        }
        self.clip_silent = 0;

        if self.clip_state == 0 {
            self.clip_state = 1;
            self.clip_measure_blocks = 0;
            self.clip_measure_energy = 0.0;
            // 本片段允许的最大提升以换片时的增益为基准，初值+校验合计不超过 STEP
        }

        if self.clip_state == 1 {
            self.clip_measure_energy += joint;
            self.clip_measure_blocks += 1;
            if self.clip_measure_blocks >= measure_blocks {
                let mean = self.clip_measure_energy / self.clip_measure_blocks as f64;
                let l_clip = -0.691 + 10.0 * mean.max(1e-20).log10();
                // 只读锚点：干声在同一个测量窗口里的联合响度
                self.wet_clip = l_clip;
                self.dry_clip = f64::from_bits(gap.dry_clip.load(Ordering::Acquire));
                self.shelf_clip = f64::from_bits(gap.shelf_clip.load(Ordering::Acquire));
                self.bass_ratio = f64::from_bits(gap.bass_ratio.load(Ordering::Acquire));
                self.clip_meas_epoch = gap.epoch.load(Ordering::Acquire);
                if self.slot == 0
                    && std::env::var("CX_DEBUG").map(|v| v != "0").unwrap_or(false)
                {
                    if let Ok(mut f) = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open("/dev/shm/collaplex-rt-clip.log")
                    {
                        let _ = writeln!(
                            f,
                            "epoch {} wet {:.2} dry {:.2} shelf {:.2} target {:.2} gain {:.2}",
                            self.clip_meas_epoch,
                            self.wet_clip,
                            self.dry_clip,
                            self.shelf_clip,
                            cfg.agc_ceiling - l_clip,
                            self.gain_db
                        );
                    }
                }
                // 太安静的测量不参与增益决策，避免把停顿/极弱段抬起来
                let too_quiet = l_clip < -50.0 && self.shelf_clip < -50.0;
                if !too_quiet {
                    // 主锚：搁架模型干声 + 房间传递估计；还没有传递样本时先用湿声兜底
                    let basis = if (self.model_valid || self.have_transfer)
                        && self.shelf_clip > -100.0
                    {
                        self.shelf_clip + self.transfer_pred()
                    } else {
                        l_clip
                    };
                    let mut target =
                        (cfg.agc_ceiling - basis).clamp(-cfg.agc_cut, cfg.agc_boost);
                    // 提升不能超过湿声实测所支持的量（留 1 dB 余量），
                    // 防止内容模型估偏时一下子抬老高；衰减不受此限。
                    if target > self.gain_db && l_clip > -100.0 {
                        target = target.min(cfg.agc_ceiling - l_clip + 1.0);
                    }
                    if (target - self.gain_db).abs() > cfg.agc_deadzone {
                        self.set_gain_target(cfg, target, 8.0);
                    }
                    self.reset_trim();
                    // 开始等待混响尾巴衰减，之后做一次湿声校验
                    self.verify_state = 1;
                    self.verify_wait = 0;
                    self.verify_energy = 0.0;
                    self.verify_blocks = 0;
                } else {
                    self.verify_state = 0;
                    self.reset_trim();
                }
                self.clip_state = 2;
                self.clip_hold_blocks = 0;
            }
        } else if self.clip_state == 2 {
            self.clip_hold_blocks += 1;
            self.verify_step(cfg, joint, gap);
            if cfg.clip_recheck_s > 0.0 {
                let recheck = ((cfg.clip_recheck_s / 0.1).round() as u64).max(1);
                if self.clip_hold_blocks >= recheck {
                    self.clip_state = 1;
                    self.clip_measure_blocks = 0;
                    self.clip_measure_energy = 0.0;
                    self.verify_state = 0;
                }
            }
        }
    }

    /// 混响尾巴衰减后的一次湿声校验：修正干声初值的内容相关误差。
    fn verify_step(&mut self, cfg: &Config, joint: f64, gap: &GapShared) {
        if self.verify_state != 1 {
            return;
        }
        let block_s = 0.1;
        let measure_blocks = ((cfg.clip_measure_s / block_s).round() as usize).max(1);
        let wait_blocks =
            (((cfg.verify_delay_s - cfg.clip_measure_s).max(0.0)) / block_s).round() as usize;
        if self.verify_wait < wait_blocks {
            self.verify_wait += 1;
            return;
        }
        self.verify_energy += joint;
        self.verify_blocks += 1;
        if self.verify_blocks < measure_blocks {
            return;
        }
        let mean = self.verify_energy / self.verify_blocks as f64;
        let wet = -0.691 + 10.0 * mean.max(1e-20).log10();
        let dry = f64::from_bits(gap.dry_clip.load(Ordering::Acquire));
        let shelf = f64::from_bits(gap.shelf_clip.load(Ordering::Acquire));
        let r = f64::from_bits(gap.bass_ratio.load(Ordering::Acquire));
        self.verify_wet = wet;
        self.verify_dry = dry;
        self.verify_shelf = shelf;
        self.verify_epoch = gap.epoch.load(Ordering::Acquire);
        if shelf > -100.0 {
            // 同窗口房间传递（搁架模型干声→湿声）；平滑后给下一段做初值
            let transfer = (wet - shelf).clamp(-12.0, 12.0);
            if self.have_transfer {
                self.transfer_est = 0.5 * self.transfer_est + 0.5 * transfer;
            } else {
                self.transfer_est = transfer;
                self.have_transfer = true;
            }
            // 内容相关模型：房间传递 ≈ a + b·低频占比
            // 只收标定范围内的样本，异常窗口不参与拟合，避免把模型带偏
            if (-6.0..=0.5).contains(&r) && (-8.0..=0.0).contains(&transfer) {
                self.model_n += 1.0;
                self.model_sx += r;
                self.model_sy += transfer;
                self.model_sxx += r * r;
                self.model_sxy += r * transfer;
                if self.model_n >= 3.0 {
                    let det = self.model_n * self.model_sxx - self.model_sx * self.model_sx;
                    if det.abs() > 1e-6 {
                        let b = (self.model_n * self.model_sxy - self.model_sx * self.model_sy)
                            / det;
                        let a = (self.model_sy - b * self.model_sx) / self.model_n;
                        if b.abs() <= 5.0 {
                            self.model_a = a;
                            self.model_b = b;
                            self.model_valid = true;
                        }
                    }
                }
            }
        }
        // 只修正传递估计误差，不追片段内部的内容起伏：
        // 用片段起点测到的搁架模型干声 + 真实房间传递，重算本段该用的增益。
        let basis = if self.shelf_clip > -100.0 && shelf > -100.0 {
            self.shelf_clip + (wet - shelf)
        } else {
            wet
        };
        // 校验是第二次实测，允许补足初值被 STEP 限掉的提升
        let target = (cfg.agc_ceiling - basis).clamp(-cfg.agc_cut, cfg.agc_boost);
        if (target - self.gain_db).abs() > cfg.verify_deadzone {
            self.set_gain_target(cfg, target, cfg.verify_ramp_s);
        }
        self.reset_trim();
        self.verify_state = 2;
        if self.slot == 0 && std::env::var("CX_DEBUG").map(|v| v != "0").unwrap_or(false) {
            if let Ok(mut f) = OpenOptions::new()
                .create(true)
                .append(true)
                .open("/dev/shm/collaplex-rt-clip.log")
            {
                let _ = writeln!(
                    f,
                    "verify epoch {} wet {:.2} dry {:.2} shelf {:.2} transfer {:.2} est {:.2} target {:.2} gain {:.2}",
                    self.verify_epoch,
                    self.verify_wet,
                    self.verify_dry,
                    self.verify_shelf,
                    self.verify_wet - self.verify_shelf,
                    self.transfer_est,
                    target,
                    self.gain_db
                );
            }
        }
    }

    fn maybe_update(&mut self, cfg: &Config, shared: &Shared, slot: usize, gap: &GapShared) {
        if self.det_n < self.period {
            return;
        }
        let z = self.det_energy / self.det_n as f64;
        let raw = self.det_raw_sq / self.det_n as f64;
        self.det_energy = 0.0;
        self.det_raw_sq = 0.0;
        self.det_n = 0;

        let peer = 1 - slot;
        shared.energy[slot].store(z.to_bits(), Ordering::Release);
        shared.seq[slot].fetch_add(1, Ordering::AcqRel);
        let ps = shared.seq[peer].load(Ordering::Acquire);
        if ps != self.peer_seq {
            self.peer_seq = ps;
            self.peer_z = f64::from_bits(shared.energy[peer].load(Ordering::Acquire));
        }

        let joint = z + self.peer_z;
        let l_block = -0.691 + 10.0 * joint.max(1e-20).log10();
        let raw_db = 10.0 * raw.max(1e-20).log10();

        let gap_epoch = gap.epoch.load(Ordering::Acquire);
        if gap_epoch != self.gap_epoch {
            self.gap_epoch = gap_epoch;
            self.clip_state = 0;
            self.clip_silent = 0;
            self.clip_measure_blocks = 0;
            self.clip_measure_energy = 0.0;
            self.verify_state = 0;
            self.reset_trim();
        }
        if gap.active.load(Ordering::Acquire) != 0 {
            self.clip_state = 0;
            self.clip_silent = 0;
            self.clip_measure_blocks = 0;
            self.clip_measure_energy = 0.0;
            self.verify_state = 0;
            self.reset_trim();
            self.silence_release(cfg, 0.1);
            self.publish(shared, slot, l_block);
            return;
        }

        if cfg.agc_clip {
            self.clip_update(cfg, joint, l_block, gap);
            self.trim_update(cfg, joint);
            self.publish(shared, slot, l_block);
            if std::env::var("CX_DEBUG").map(|v| v != "0").unwrap_or(false) {
                use std::io::Write as _;
                if let Ok(mut f) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(format!("/dev/shm/collaplex-rt-debug-{slot}.log"))
                {
                    let _ = writeln!(
                        f,
                        "clip state {} l_block {:.2} gain {:.2} measure_blocks {}",
                        self.clip_state, l_block, self.gain_db, self.clip_measure_blocks
                    );
                }
            }
            return;
        }

        if raw_db < -60.0 {
            self.silent += 1;
            if self.silent == 2 {
                self.ring.clear();
                self.have_l = false;
                // 长静音后回到保守的初始增益，音频恢复时不会带着上一段的提升冲出来
                self.gain_db = cfg.agc_init_db;
                self.gain_now_db = cfg.agc_init_db;
                self.gain_start = cfg.agc_init_db;
                self.gain_end = cfg.agc_init_db;
                self.target_db = cfg.agc_init_db;
                self.gain_tau_coef = 1.0;
                self.gain_max_step = 0.0;
                self.ramp_pos = self.period;
                self.active = false;
                self.below = 0;
            }
        } else {
            self.silent = 0;
        }

        if joint <= 0.0 {
            return;
        }
        if self.ring.len() >= 30 {
            self.ring.pop_front();
        }
        self.ring.push_back(joint);

        if self.ring.len() < 3 {
            return;
        }
        let z_st = self.ring.iter().sum::<f64>() / self.ring.len() as f64;
        let l_st = -0.691 + 10.0 * z_st.max(1e-20).log10();

        if !self.have_l {
            self.l_smooth = l_st;
            self.have_l = true;
        } else {
            let a = 1.0 - (-0.1 / cfg.agc_det_tau.max(1e-3)).exp();
            self.l_smooth += (l_st - self.l_smooth) * a;
        }

        if l_block < cfg.agc_gate || self.l_smooth < cfg.agc_gate {
            return;
        }

        // 激活阈值 + 迟滞：低于阈值时退出控制，慢速释放回 0 dB，不上抬安静内容
        if !self.active {
            if self.l_smooth > cfg.agc_activate + cfg.agc_hyst {
                self.active = true;
                self.below = 0;
            }
        } else if self.l_smooth < cfg.agc_activate - cfg.agc_hyst {
            self.below += 1;
            if self.below >= 10 {
                self.active = false;
                self.below = 0;
            }
        } else {
            self.below = 0;
        }
        if !self.active {
            let prev = self.gain_now_db;
            let alpha = 1.0 - (-0.1 / 2.0_f64).exp();
            let next = prev + (0.0 - prev) * alpha;
            self.gain_db = next;
            self.target_db = 0.0;
            self.start_ramp(prev, next, self.period);
            return;
        }

        let measured = (cfg.agc_ceiling - self.l_smooth).clamp(-cfg.agc_cut, cfg.agc_boost);
        if (measured - self.target_db).abs() > cfg.agc_deadzone {
            self.target_db = measured;
        }
        let tau = if self.target_db < self.gain_db {
            cfg.agc_tau_down
        } else {
            cfg.agc_tau_up
        };
        let alpha = 1.0 - (-0.1 / tau.max(1e-4)).exp();
        let prev = self.gain_now_db;
        let next = prev + (self.target_db - prev) * alpha;
        self.gain_db = next;
        self.start_ramp(prev, next, self.period);

        self.publish(shared, slot, l_block);
        if std::env::var("CX_DEBUG").map(|v| v != "0").unwrap_or(false) {
            use std::io::Write as _;
            if let Ok(mut f) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("/dev/shm/collaplex-rt-debug-{slot}.log"))
            {
                let _ = writeln!(
                    f,
                    "slot{} active {} l_st {:.2} l_smooth {:.2} target {:.2} gain {:.2} z {:.4} peer {:.4}",
                    slot, self.active, l_st, self.l_smooth, self.target_db, self.gain_db, z, self.peer_z
                );
            }
        }
    }
}

// ---------------- 主循环 ----------------

fn run_detector() {
    set_pdeathsig();
    // 左右各一个 pipe 进程：wrapper 用 CX_GAP_SIDE 区分（图上 gapL/gapR）
    let side = match std::env::var("CX_GAP_SIDE").ok().as_deref() {
        Some("R") | Some("r") | Some("1") => 1usize,
        _ => match std::env::args().nth(1).as_deref() {
            Some("R") | Some("r") | Some("1") => 1usize,
            _ => 0usize,
        },
    };
    let rate = env_f("CX_RATE", 96_000.0);
    let mut threshold_db = env_f("CX_GAP_DBFS", -50.0);
    let mut gap_samples = (env_f("CX_GAP_MS", 200.0) / 1000.0 * rate).round() as usize;
    let mut measure_blocks =
        ((env_f("CX_CLIP_MEASURE_S", 0.6) / 0.1).round() as usize).max(1);
    let period = ((0.1 * rate).round() as usize).max(1);
    let gap = GapState::open();
    let ctl_state = CtlState::open();
    let ctl = ctl_state.get();
    let mut ctl_version = ctl.version();
    let g = gap.get();
    let mut kf = (Biquad::k_high_shelf(rate), Biquad::k_high_pass(rate));
    let mut kf_shelf = (Biquad::k_high_shelf(rate), Biquad::k_high_pass(rate));
    let mut shelf_db = env_f("CX_SHELF_DB", 8.0);
    let mut shelf_freq = env_f("CX_SHELF_FREQ", 250.0);
    let mut shelf_s = env_f("CX_SHELF_S", 1.79);
    let mut shelf = Biquad::low_shelf(shelf_freq, shelf_db, shelf_s, rate);
    let mut lp = Biquad::low_pass(250.0, 0.707, rate);
    let mut det_energy = 0.0f64;
    let mut shelf_energy = 0.0f64;
    let mut low_acc = 0.0f64;
    let mut tot_acc = 0.0f64;
    let mut det_n = 0usize;
    let mut peer_z = 0.0f64;
    let mut peer_seq = 0u64;
    let mut peer_shelf_z = 0.0f64;
    let mut peer_shelf_seq = 0u64;
    let mut peer_low = 0.0f64;
    let mut peer_tot = 0.0f64;
    let mut peer_low_seq = 0u64;
    let mut ring: VecDeque<f64> = VecDeque::with_capacity(64);
    let mut ring_shelf: VecDeque<f64> = VecDeque::with_capacity(64);
    let mut ring_low: VecDeque<f64> = VecDeque::with_capacity(64);
    let mut ring_tot: VecDeque<f64> = VecDeque::with_capacity(64);
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut buf = [0u8; BLOCK_BYTES];
    let mut silent = 0usize;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let n = n - (n % 4);
        let v = ctl.version();
        if v != ctl_version && v > 0 {
            ctl_version = v;
            let l = ctl.load();
            threshold_db = l.gap_dbfs;
            gap_samples = (l.gap_ms / 1000.0 * rate).round().max(1.0) as usize;
            measure_blocks = ((l.clip_measure / 0.1).round() as usize).max(1);
            if l.shelf_db != shelf_db || l.shelf_freq != shelf_freq || l.shelf_s != shelf_s {
                shelf_db = l.shelf_db;
                shelf_freq = l.shelf_freq;
                shelf_s = l.shelf_s;
                shelf = Biquad::low_shelf(shelf_freq, shelf_db, shelf_s, rate);
            }
        }
        let mut sum = 0.0f64;
        let mut cnt = 0usize;
        for chunk in buf[..n].chunks_exact(4) {
            let x = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64;
            sum += x * x;
            cnt += 1;
            // 干声锚点：BS.1770 K 加权，每 100 ms 发布一次联合能量
            let k = kf.1.process(kf.0.process(x));
            det_energy += k * k;
            // 搁架模型：先套同一个低搁架，再 K 加权
            let xs = shelf.process(x);
            let ks = kf_shelf.1.process(kf_shelf.0.process(xs));
            shelf_energy += ks * ks;
            let lo = lp.process(x);
            low_acc += lo * lo;
            tot_acc += x * x;
            det_n += 1;
            if det_n >= period {
                let z = det_energy / det_n as f64;
                let zs = shelf_energy / det_n as f64;
                let low = low_acc / det_n as f64;
                let tot = tot_acc / det_n as f64;
                det_energy = 0.0;
                shelf_energy = 0.0;
                low_acc = 0.0;
                tot_acc = 0.0;
                det_n = 0;
                g.energy[side].store(z.to_bits(), Ordering::Release);
                g.seq[side].fetch_add(1, Ordering::AcqRel);
                g.shelf_energy[side].store(zs.to_bits(), Ordering::Release);
                g.shelf_seq[side].fetch_add(1, Ordering::AcqRel);
                g.low_energy[side].store(low.to_bits(), Ordering::Release);
                g.total_energy[side].store(tot.to_bits(), Ordering::Release);
                g.low_seq[side].fetch_add(1, Ordering::AcqRel);
                let other = 1 - side;
                let ps = g.seq[other].load(Ordering::Acquire);
                if ps != peer_seq {
                    peer_seq = ps;
                    peer_z = f64::from_bits(g.energy[other].load(Ordering::Acquire));
                }
                let pss = g.shelf_seq[other].load(Ordering::Acquire);
                if pss != peer_shelf_seq {
                    peer_shelf_seq = pss;
                    peer_shelf_z = f64::from_bits(g.shelf_energy[other].load(Ordering::Acquire));
                }
                let pls = g.low_seq[other].load(Ordering::Acquire);
                if pls != peer_low_seq {
                    peer_low_seq = pls;
                    peer_low = f64::from_bits(g.low_energy[other].load(Ordering::Acquire));
                    peer_tot = f64::from_bits(g.total_energy[other].load(Ordering::Acquire));
                }
                let joint = z + peer_z;
                let joint_shelf = zs + peer_shelf_z;
                ring.push_back(joint);
                ring_shelf.push_back(joint_shelf);
                while ring.len() > measure_blocks {
                    ring.pop_front();
                }
                while ring_shelf.len() > measure_blocks {
                    ring_shelf.pop_front();
                }
                if side == 0 {
                    let l_block = -0.691 + 10.0 * joint.max(1e-20).log10();
                    g.dry_block.store(l_block.to_bits(), Ordering::Release);
                    let mean = ring.iter().sum::<f64>() / ring.len() as f64;
                    let l_clip = -0.691 + 10.0 * mean.max(1e-20).log10();
                    g.dry_clip.store(l_clip.to_bits(), Ordering::Release);
                    let s_block = -0.691 + 10.0 * joint_shelf.max(1e-20).log10();
                    g.shelf_block.store(s_block.to_bits(), Ordering::Release);
                    let smean = ring_shelf.iter().sum::<f64>() / ring_shelf.len() as f64;
                    let s_clip = -0.691 + 10.0 * smean.max(1e-20).log10();
                    g.shelf_clip.store(s_clip.to_bits(), Ordering::Release);
                    ring_low.push_back(low + peer_low);
                    ring_tot.push_back(tot + peer_tot);
                    while ring_low.len() > measure_blocks {
                        ring_low.pop_front();
                    }
                    while ring_tot.len() > measure_blocks {
                        ring_tot.pop_front();
                    }
                    let sl: f64 = ring_low.iter().sum();
                    let st: f64 = ring_tot.iter().sum();
                    let ratio = 10.0 * (sl / st.max(1e-30)).max(1e-30).log10();
                    g.bass_ratio.store(ratio.to_bits(), Ordering::Release);
                }
            }
        }
        // 间隙检测只在左声道做，保持原有换片边界语义
        if side == 0 {
            let rms_db = if cnt > 0 {
                10.0 * (sum / cnt as f64).max(1e-20).log10()
            } else {
                -120.0
            };
            if rms_db < threshold_db {
                silent = silent.saturating_add(cnt);
                if silent >= gap_samples && g.active.load(Ordering::Acquire) == 0 {
                    g.active.store(1, Ordering::Release);
                    g.epoch.fetch_add(1, Ordering::AcqRel);
                }
            } else {
                silent = 0;
                if g.active.load(Ordering::Acquire) != 0 {
                    g.active.store(0, Ordering::Release);
                }
            }
        }
        if writer.write_all(&buf[..n]).is_err() {
            break;
        }
        if writer.flush().is_err() {
            break;
        }
    }
}

fn main() {
    set_pdeathsig();
    if env_bool("CX_DETECT", false) {
        run_detector();
        return;
    }
    let mut cfg = Config::from_env();
    let slot = match claim_slot() {
        Some(s) => s,
        None => {
            // 已经有一对实例在跑
            std::process::exit(0);
        }
    };
    let shared_state = SharedState::open();
    let shared = shared_state.get();
    let scope_state = ScopeState::open();
    let scope = scope_state.get();
    let mut scope_pos = 0u64;
    let gap_state = GapState::open();
    let gap = gap_state.get();
    let mut shelf = Biquad::low_shelf(cfg.shelf_freq, cfg.shelf_db, cfg.shelf_s, cfg.rate);
    let mut agc = Agc::new(&cfg);
    agc.slot = slot;
    let mut limiter = Limiter::new(cfg.rate, cfg.limit_ceiling);

    let ctl_state = CtlState::open();
    let ctl = ctl_state.get();
    let mut ctl_version = ctl.version();
    if ctl_version == 0 {
        ctl.store(&live_from_cfg(&cfg));
        ctl_version = ctl.version();
    } else {
        apply_live(&mut cfg, &mut shelf, &mut limiter, &mut agc, &ctl.load());
    }

    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut buf = [0u8; BLOCK_BYTES];

    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let n = n - (n % 4);
        let v = ctl.version();
        if v != ctl_version {
            ctl_version = v;
            apply_live(&mut cfg, &mut shelf, &mut limiter, &mut agc, &ctl.load());
        }
        let mut out = Vec::with_capacity(n);
        for chunk in buf[..n].chunks_exact(4) {
            let x = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64;
            let mut s = shelf.process(x);
            if cfg.agc {
                agc.feed_detector(s);
                agc.maybe_update(&cfg, shared, slot, gap);
            }
            s *= agc.gain_linear();
            let y = limiter.process(s) as f32;
            scope.samples[slot][(scope_pos as usize) % collaplex_realtime::SCOPE_LEN]
                .store(y.to_bits(), Ordering::Relaxed);
            scope_pos += 1;
            scope.write[slot].store(scope_pos, Ordering::Release);
            out.extend_from_slice(&y.to_le_bytes());
        }
        if writer.write_all(&out).is_err() {
            break;
        }
        if writer.flush().is_err() {
            break;
        }
    }
}
