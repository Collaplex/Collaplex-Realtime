//! Collaplex RT 共享内存：DSP 进程与 Web 服务共用。
//!
//! - `/dev/shm/collaplex-rt-agc`  双声道 AGC 状态（能量/序号/增益/片段状态）
//! - `/dev/shm/collaplex-rt-gap`  干声间隙检测（active / epoch）
//! - `/dev/shm/collaplex-rt-ctl`  实时参数（Web 写，DSP 读）

use memmap2::MmapMut;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const SHM_PATH: &str = "/dev/shm/collaplex-rt-agc";
pub const GAP_SHM: &str = "/dev/shm/collaplex-rt-gap";
pub const CTL_SHM: &str = "/dev/shm/collaplex-rt-ctl";
pub const SCOPE_SHM: &str = "/dev/shm/collaplex-rt-scope";
pub const SCOPE_LEN: usize = 4096;
pub const SHM_SIZE: usize = 4096;

fn map(path: &str) -> Option<MmapMut> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()?;
    if f.metadata().ok()?.len() < SHM_SIZE as u64 {
        f.set_len(SHM_SIZE as u64).ok()?;
    }
    unsafe { MmapMut::map_mut(&f).ok() }
}

// ---------------- AGC 状态 ----------------

#[repr(C)]
pub struct Shared {
    pub energy: [AtomicU64; 2],
    pub seq: [AtomicU64; 2],
    pub gain_db: AtomicU64,
    pub clip_state: AtomicU64,
    pub l_block: AtomicU64,
    pub l_smooth: AtomicU64,
    /// 最近一次片段测量：湿声（AGC 检测点）LUFS
    pub wet_clip: AtomicU64,
    /// 最近一次片段测量时的干声锚点 LUFS
    pub dry_clip: AtomicU64,
    /// 最近一次片段测量时的搁架模型干声 LUFS
    pub shelf_clip: AtomicU64,
    /// 该次测量对应的间隙 epoch
    pub clip_epoch: AtomicU64,
    /// 尾巴衰减后的湿声校验值 LUFS
    pub verify_wet: AtomicU64,
    /// 同一窗口的干声校验值 LUFS
    pub verify_dry: AtomicU64,
    /// 同一窗口的搁架模型干声 LUFS
    pub verify_shelf: AtomicU64,
    /// 当前房间（搁架后干→湿）传递估计（LU）
    pub transfer: AtomicU64,
    /// 校验对应的间隙 epoch
    pub verify_epoch: AtomicU64,
    /// 片段测量时的低频占比（dB）
    pub bass_ratio: AtomicU64,
    /// 内容相关传递模型是否已生效
    pub model_valid: AtomicU64,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            energy: [AtomicU64::new(0), AtomicU64::new(0)],
            seq: [AtomicU64::new(0), AtomicU64::new(0)],
            gain_db: AtomicU64::new(0),
            clip_state: AtomicU64::new(0),
            l_block: AtomicU64::new(0),
            l_smooth: AtomicU64::new(0),
            wet_clip: AtomicU64::new((-120.0f64).to_bits()),
            dry_clip: AtomicU64::new((-120.0f64).to_bits()),
            shelf_clip: AtomicU64::new((-120.0f64).to_bits()),
            clip_epoch: AtomicU64::new(0),
            verify_wet: AtomicU64::new((-120.0f64).to_bits()),
            verify_dry: AtomicU64::new((-120.0f64).to_bits()),
            verify_shelf: AtomicU64::new((-120.0f64).to_bits()),
            transfer: AtomicU64::new((-3.0f64).to_bits()),
            verify_epoch: AtomicU64::new(0),
            bass_ratio: AtomicU64::new((-60.0f64).to_bits()),
            model_valid: AtomicU64::new(0),
        }
    }
}

pub struct SharedState {
    _map: Option<MmapMut>,
    _local: Box<Shared>,
    ptr: *mut Shared,
}

impl SharedState {
    pub fn open() -> Self {
        let mut local = Box::new(Shared::default());
        let local_ptr: *mut Shared = &mut *local;
        let map = map(SHM_PATH);
        let ptr = match &map {
            Some(m) => m.as_ptr() as *mut Shared,
            None => local_ptr,
        };
        Self {
            _map: map,
            _local: local,
            ptr,
        }
    }
    pub fn get(&self) -> &Shared {
        unsafe { &*self.ptr }
    }
}

// ---------------- 间隙 ----------------

#[repr(C)]
pub struct GapShared {
    pub active: AtomicU64,
    pub epoch: AtomicU64,
    /// 干声 K 加权能量（每 100 ms 一块，左右各一份）
    pub energy: [AtomicU64; 2],
    pub seq: [AtomicU64; 2],
    /// 干声联合块响度（LUFS，最近 100 ms）
    pub dry_block: AtomicU64,
    /// 干声锚点：最近一个测量窗口（CLIP_MEASURE_S）的联合 LUFS
    pub dry_clip: AtomicU64,
    /// 搁架模型后的干声 K 加权能量（左右各一份）
    pub shelf_energy: [AtomicU64; 2],
    pub shelf_seq: [AtomicU64; 2],
    /// 搁架模型后的块响度 / 测量窗口响度
    pub shelf_block: AtomicU64,
    pub shelf_clip: AtomicU64,
    /// 原始干声低频（<250 Hz）与总能量，用于内容相关的传递预测
    pub low_energy: [AtomicU64; 2],
    pub total_energy: [AtomicU64; 2],
    pub low_seq: [AtomicU64; 2],
    /// 测量窗口内的低频占比（dB）
    pub bass_ratio: AtomicU64,
}

impl Default for GapShared {
    fn default() -> Self {
        Self {
            active: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            energy: [AtomicU64::new(0), AtomicU64::new(0)],
            seq: [AtomicU64::new(0), AtomicU64::new(0)],
            dry_block: AtomicU64::new((-120.0f64).to_bits()),
            dry_clip: AtomicU64::new((-120.0f64).to_bits()),
            shelf_energy: [AtomicU64::new(0), AtomicU64::new(0)],
            shelf_seq: [AtomicU64::new(0), AtomicU64::new(0)],
            shelf_block: AtomicU64::new((-120.0f64).to_bits()),
            shelf_clip: AtomicU64::new((-120.0f64).to_bits()),
            low_energy: [AtomicU64::new(0), AtomicU64::new(0)],
            total_energy: [AtomicU64::new(0), AtomicU64::new(0)],
            low_seq: [AtomicU64::new(0), AtomicU64::new(0)],
            bass_ratio: AtomicU64::new((-60.0f64).to_bits()),
        }
    }
}

pub struct GapState {
    _map: Option<MmapMut>,
    _local: Box<GapShared>,
    ptr: *mut GapShared,
}

impl GapState {
    pub fn open() -> Self {
        let mut local = Box::new(GapShared::default());
        let local_ptr: *mut GapShared = &mut *local;
        let map = map(GAP_SHM);
        let ptr = match &map {
            Some(m) => m.as_ptr() as *mut GapShared,
            None => local_ptr,
        };
        Self {
            _map: map,
            _local: local,
            ptr,
        }
    }
    pub fn get(&self) -> &GapShared {
        unsafe { &*self.ptr }
    }
}

// ---------------- 实时参数 ----------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Live {
    pub agc: bool,
    pub clip: bool,
    pub ceiling: f64,
    pub deadzone: f64,
    pub tau_down: f64,
    pub tau_up: f64,
    pub det_tau: f64,
    pub activate: f64,
    pub hyst: f64,
    pub boost: f64,
    pub cut: f64,
    pub gate: f64,
    pub init_db: f64,
    pub clip_measure: f64,
    pub clip_recheck: f64,
    pub shelf_db: f64,
    pub shelf_freq: f64,
    pub shelf_s: f64,
    pub gap_dbfs: f64,
    pub gap_ms: f64,
    pub limit_ceil: f64,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            agc: true,
            clip: true,
            ceiling: -18.5,
            deadzone: 2.0,
            tau_down: 0.15,
            tau_up: 1.5,
            det_tau: 1.0,
            activate: -50.0,
            hyst: 3.0,
            boost: 12.0,
            cut: 18.0,
            gate: -70.0,
            init_db: -18.0,
            clip_measure: 0.6,
            clip_recheck: 0.0,
            shelf_db: 8.0,
            shelf_freq: 250.0,
            shelf_s: 1.79,
            gap_dbfs: -50.0,
            gap_ms: 200.0,
            limit_ceil: 0.891,
        }
    }
}

#[repr(C)]
pub struct Ctl {
    pub version: AtomicU64,
    pub agc: AtomicU64,
    pub clip: AtomicU64,
    pub ceiling: AtomicU64,
    pub deadzone: AtomicU64,
    pub tau_down: AtomicU64,
    pub tau_up: AtomicU64,
    pub det_tau: AtomicU64,
    pub activate: AtomicU64,
    pub hyst: AtomicU64,
    pub boost: AtomicU64,
    pub cut: AtomicU64,
    pub gate: AtomicU64,
    pub init_db: AtomicU64,
    pub clip_measure: AtomicU64,
    pub clip_recheck: AtomicU64,
    pub shelf_db: AtomicU64,
    pub shelf_freq: AtomicU64,
    pub shelf_s: AtomicU64,
    pub gap_dbfs: AtomicU64,
    pub gap_ms: AtomicU64,
    pub limit_ceil: AtomicU64,
}

impl Default for Ctl {
    fn default() -> Self {
        Self {
            version: AtomicU64::new(0),
            agc: AtomicU64::new(0),
            clip: AtomicU64::new(0),
            ceiling: AtomicU64::new(0),
            deadzone: AtomicU64::new(0),
            tau_down: AtomicU64::new(0),
            tau_up: AtomicU64::new(0),
            det_tau: AtomicU64::new(0),
            activate: AtomicU64::new(0),
            hyst: AtomicU64::new(0),
            boost: AtomicU64::new(0),
            cut: AtomicU64::new(0),
            gate: AtomicU64::new(0),
            init_db: AtomicU64::new(0),
            clip_measure: AtomicU64::new(0),
            clip_recheck: AtomicU64::new(0),
            shelf_db: AtomicU64::new(0),
            shelf_freq: AtomicU64::new(0),
            shelf_s: AtomicU64::new(0),
            gap_dbfs: AtomicU64::new(0),
            gap_ms: AtomicU64::new(0),
            limit_ceil: AtomicU64::new(0),
        }
    }
}

impl Ctl {
    pub fn store(&self, l: &Live) {
        let f = |x: f64| x.to_bits();
        self.agc.store(l.agc as u64, Ordering::Relaxed);
        self.clip.store(l.clip as u64, Ordering::Relaxed);
        self.ceiling.store(f(l.ceiling), Ordering::Relaxed);
        self.deadzone.store(f(l.deadzone), Ordering::Relaxed);
        self.tau_down.store(f(l.tau_down), Ordering::Relaxed);
        self.tau_up.store(f(l.tau_up), Ordering::Relaxed);
        self.det_tau.store(f(l.det_tau), Ordering::Relaxed);
        self.activate.store(f(l.activate), Ordering::Relaxed);
        self.hyst.store(f(l.hyst), Ordering::Relaxed);
        self.boost.store(f(l.boost), Ordering::Relaxed);
        self.cut.store(f(l.cut), Ordering::Relaxed);
        self.gate.store(f(l.gate), Ordering::Relaxed);
        self.init_db.store(f(l.init_db), Ordering::Relaxed);
        self.clip_measure.store(f(l.clip_measure), Ordering::Relaxed);
        self.clip_recheck.store(f(l.clip_recheck), Ordering::Relaxed);
        self.shelf_db.store(f(l.shelf_db), Ordering::Relaxed);
        self.shelf_freq.store(f(l.shelf_freq), Ordering::Relaxed);
        self.shelf_s.store(f(l.shelf_s), Ordering::Relaxed);
        self.gap_dbfs.store(f(l.gap_dbfs), Ordering::Relaxed);
        self.gap_ms.store(f(l.gap_ms), Ordering::Relaxed);
        self.limit_ceil.store(f(l.limit_ceil), Ordering::Relaxed);
        self.version.fetch_add(1, Ordering::AcqRel);
    }

    pub fn load(&self) -> Live {
        let f = |a: &AtomicU64| f64::from_bits(a.load(Ordering::Acquire));
        Live {
            agc: self.agc.load(Ordering::Acquire) != 0,
            clip: self.clip.load(Ordering::Acquire) != 0,
            ceiling: f(&self.ceiling),
            deadzone: f(&self.deadzone),
            tau_down: f(&self.tau_down),
            tau_up: f(&self.tau_up),
            det_tau: f(&self.det_tau),
            activate: f(&self.activate),
            hyst: f(&self.hyst),
            boost: f(&self.boost),
            cut: f(&self.cut),
            gate: f(&self.gate),
            init_db: f(&self.init_db),
            clip_measure: f(&self.clip_measure),
            clip_recheck: f(&self.clip_recheck),
            shelf_db: f(&self.shelf_db),
            shelf_freq: f(&self.shelf_freq),
            shelf_s: f(&self.shelf_s),
            gap_dbfs: f(&self.gap_dbfs),
            gap_ms: f(&self.gap_ms),
            limit_ceil: f(&self.limit_ceil),
        }
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

pub struct CtlState {
    _map: Option<MmapMut>,
    _local: Box<Ctl>,
    ptr: *mut Ctl,
}

impl CtlState {
    pub fn open() -> Self {
        let mut local = Box::new(Ctl::default());
        let local_ptr: *mut Ctl = &mut *local;
        let map = map(CTL_SHM);
        let ptr = match &map {
            Some(m) => m.as_ptr() as *mut Ctl,
            None => local_ptr,
        };
        Self {
            _map: map,
            _local: local,
            ptr,
        }
    }
    pub fn get(&self) -> &Ctl {
        unsafe { &*self.ptr }
    }
}

unsafe impl Send for SharedState {}
unsafe impl Sync for SharedState {}
unsafe impl Send for GapState {}
unsafe impl Sync for GapState {}
unsafe impl Send for CtlState {}
unsafe impl Sync for CtlState {}

// ---------------- 示波器 / 频谱环形缓冲 ----------------

#[repr(C)]
pub struct ScopeShared {
    pub write: [AtomicU64; 2],
    pub samples: [[AtomicU32; SCOPE_LEN]; 2],
}

impl Default for ScopeShared {
    fn default() -> Self {
        Self {
            write: [AtomicU64::new(0), AtomicU64::new(0)],
            samples: [
                std::array::from_fn(|_| AtomicU32::new(0)),
                std::array::from_fn(|_| AtomicU32::new(0)),
            ],
        }
    }
}

pub struct ScopeState {
    _map: Option<MmapMut>,
    _local: Box<ScopeShared>,
    ptr: *mut ScopeShared,
}

impl ScopeState {
    pub fn open() -> Self {
        let mut local = Box::new(ScopeShared::default());
        let local_ptr: *mut ScopeShared = &mut *local;
        let size = std::mem::size_of::<ScopeShared>().max(4096);
        let map = (|| {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(SCOPE_SHM)
                .ok()?;
            if f.metadata().ok()?.len() < size as u64 {
                f.set_len(size as u64).ok()?;
            }
            unsafe { MmapMut::map_mut(&f).ok() }
        })();
        let ptr = match &map {
            Some(m) => m.as_ptr() as *mut ScopeShared,
            None => local_ptr,
        };
        Self { _map: map, _local: local, ptr }
    }
    pub fn get(&self) -> &ScopeShared {
        unsafe { &*self.ptr }
    }
}

unsafe impl Send for ScopeState {}
unsafe impl Sync for ScopeState {}
