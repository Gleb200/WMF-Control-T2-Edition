// WMF Control (Windows Mac Fan Control) - by melinoe-ux (https://github.com/melinoe-ux) - MIT License
//! Background worker: polls the SMC, applies fan rules, publishes snapshots to the UI.
//!
//! Control strategy: we never take the fan away from
//! the SMC. Instead we raise the fan's *minimum* speed (`FnMn`). The SMC keeps full
//! authority to spin faster when it needs to, so a crash of this program can never
//! leave the machine without cooling. If writing the minimum is refused, forced mode
//! (`FnMd` / `FS!` + `FnTg`) is used as a fallback.

use crate::config::{Config, FanMode, Profile};
use crate::sensors::{self, Kind};
use crate::smc::{Key, Smc, key, key_str};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct FanState {
    pub index: u8,
    pub actual: f64,
    pub min: f64,
    pub max: f64,
    pub default_min: f64,
    /// Speed we are currently requesting (None = automatic).
    pub requested: Option<f64>,
    pub mode: FanMode,
    pub method: &'static str,
}

#[derive(Clone, Debug)]
pub struct Sensor {
    pub key: String,
    pub name: String,
    pub kind: Kind,
    pub value: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub transport: String,
    pub error: Option<String>,
    pub warnings: Vec<String>,
    pub fans: Vec<FanState>,
    pub sensors: Vec<Sensor>,
    pub key_total: u32,
    pub ready: bool,
    pub poll: Duration,
    pub full_blast: bool,
    pub profile: Profile,
}

pub enum Command {
    SetMode(u8, FanMode),
    /// Run every fan at maximum RPM (overrides the per-fan modes while on).
    FullBlast(bool),
    SetProfile(Profile),
    Reinit,
    SetPoll(u64),
    SetPrefs { fahrenheit: bool, show_all: bool, tray: bool },
    Quit,
}

/// Cheap, cloneable handle to the worker thread (the tray handler holds one too).
#[derive(Clone)]
pub struct Worker {
    pub tx: Sender<Command>,
    pub shared: Arc<Mutex<Snapshot>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Worker {
    pub fn spawn(
        open: impl FnOnce() -> anyhow::Result<Smc> + Send + 'static,
        repaint: impl Fn() + Send + 'static,
        is_hidden: Arc<AtomicBool>,
        no_mmio: bool,
    ) -> Self {
        let (tx, rx) = channel();
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let sh = shared.clone();
        let handle = std::thread::Builder::new()
            .name("smc-worker".into())
            .spawn(move || match open() {
                Ok(smc) => run(smc, rx, sh, repaint, is_hidden, no_mmio),
                Err(e) => {
                    let mut s = sh.lock().unwrap();
                    s.error = Some(format!("{e:#}"));
                    s.ready = true;
                    drop(s);
                    repaint();
                }
            })
            .expect("spawn worker");
        Worker { tx, shared, handle: Arc::new(Mutex::new(Some(handle))) }
    }

    /// Restore automatic control and wait for the worker to finish.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Command::Quit);
        let h = self.handle.lock().unwrap().take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }
}

fn fkey(i: u8, suffix: &str) -> Key {
    key(&format!("F{i}{suffix}"))
}

#[derive(Clone, Copy, PartialEq)]
enum Method {
    ForcedMd,
    ForcedFs,
}

struct Fan {
    index: u8,
    default_min: f64,
    max: f64,
    method: Method,
    applied: Option<f64>,
}

#[derive(Clone, Copy)]
struct CachedReading {
    value: f64,
    stale_count: u8,
}

struct Ctx {
    smc: Smc,
    cfg: Config,
    fans: Vec<Fan>,
    sensor_keys: Vec<(Key, Kind, String)>,
    warnings: Vec<String>,
    overheat: bool,
    full_blast: bool,
    no_mmio: bool,
    cache: HashMap<Key, CachedReading>,
    shmem: crate::shmem::SharedMem,
    smoothed_targets: HashMap<u8, f64>,
    peak_temps: HashMap<u8, f64>,
    last_tick: Instant,
}

const OVERHEAT_ON: f64 = 90.0;
const OVERHEAT_OFF: f64 = 85.0;

fn run(
    smc: Smc,
    rx: Receiver<Command>,
    shared: Arc<Mutex<Snapshot>>,
    repaint: impl Fn(),
    is_hidden: Arc<AtomicBool>,
    no_mmio: bool,
) {
    let cfg = Config::load();
    let mut cx = Ctx {
        smc,
        cfg,
        fans: vec![],
        sensor_keys: vec![],
        warnings: vec![],
        overheat: false,
        full_blast: false,
        no_mmio,
        cache: HashMap::new(),
        shmem: crate::shmem::SharedMem::new(),
        smoothed_targets: HashMap::new(),
        peak_temps: HashMap::new(),
        last_tick: Instant::now(),
    };
    cx.discover();
    let key_total = cx.smc.key_count().unwrap_or(0);
    let transport = cx.smc.transport_name();

    loop {
        let hidden = is_hidden.load(Ordering::Relaxed);
        let start = Instant::now();
        let snap = cx.tick(hidden);
        {
            let mut s = shared.lock().unwrap();
            *s = snap;
            s.transport = transport.clone();
            s.key_total = key_total;
            s.ready = true;
        }
        if !hidden {
            repaint();
        }

        let poll_ms = if hidden {
            cx.cfg.poll_ms.max(2000)
        } else {
            cx.cfg.poll_ms.clamp(1000, 1500)
        };
        let poll = Duration::from_millis(poll_ms);
        let deadline = start + poll;
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(Command::Quit) | Err(RecvTimeoutError::Disconnected) => {
                    cx.restore_all();
                    return;
                }
                Ok(Command::SetMode(i, m)) => {
                    cx.cfg.fans.insert(i, m);
                    cx.cfg.save();
                    break; // apply immediately
                }
                Ok(Command::FullBlast(on)) => {
                    cx.full_blast = on;
                    if on {
                        cx.cfg.profile = Profile::FullBlast;
                    } else if cx.cfg.profile == Profile::FullBlast {
                        cx.cfg.profile = Profile::Custom;
                    }
                    cx.cfg.save();
                    break;
                }
                Ok(Command::SetProfile(p)) => {
                    cx.cfg.profile = p;
                    cx.full_blast = p == Profile::FullBlast;
                    cx.cfg.save();
                    break;
                }
                Ok(Command::Reinit) => {
                    let _ = cx.smc.reinit(!cx.no_mmio);
                    cx.discover();
                    break;
                }
                Ok(Command::SetPoll(ms)) => {
                    cx.cfg.poll_ms = ms.max(500);
                    cx.cfg.save();
                }
                Ok(Command::SetPrefs { fahrenheit, show_all, tray }) => {
                    cx.cfg.no_minimize_to_tray = !tray;
                    cx.cfg.fahrenheit = fahrenheit;
                    cx.cfg.show_all_sensors = show_all;
                    cx.cfg.save();
                    cx.discover_sensors();
                    break;
                }
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
    }
}

impl Ctx {
    fn write_with_retry(&mut self, k: &Key, val: f64) -> anyhow::Result<()> {
        let mut last_err = None;
        for _ in 0..3 {
            match self.smc.write_f64(k, val) {
                Ok(()) => {
                    std::thread::sleep(Duration::from_millis(5));
                    return Ok(());
                }
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(Duration::from_millis(8));
                }
            }
        }
        if let Some(e) = last_err {
            Err(e)
        } else {
            anyhow::bail!("Write failed for key {}", key_str(k))
        }
    }

    fn discover(&mut self) {
        let _ = self.smc.reinit(!self.no_mmio);
        let count = self.smc.read_f64(&key("FNum")).unwrap_or(0.0) as u8;
        let has_md = self.smc.key_info(&fkey(0, "Md")).is_some();
        let has_fs = self.smc.key_info(&key("FS! ")).is_some();
        self.fans.clear();
        for i in 0..count.min(8) {
            let cur_min = self.smc.read_f64(&fkey(i, "Mn")).unwrap_or(1800.0);
            let default_min = if cur_min > 0.0 { cur_min } else { 1800.0 };
            let max = self.smc.read_f64(&fkey(i, "Mx")).unwrap_or(6000.0);
            // On Apple T2 / Intel Macs, control is ALWAYS done via forced target RPM (FnMd / FS! + FnTg)
            // FnMn is strictly read-only (error 0x86).
            let method = if self.smc.key_info(&fkey(i, "Md")).is_some() || has_md {
                Method::ForcedMd
            } else if has_fs {
                Method::ForcedFs
            } else {
                Method::ForcedMd
            };
            self.fans.push(Fan { index: i, default_min, max, method, applied: None });
        }
        if count == 0 {
            self.warnings.push("SMC reports no fans (FNum = 0)".into());
        }
        // Release any forced modes from a previous crashed run. NEVER write to FnMn!
        for i in 0..count.min(8) {
            let _ = self.write_with_retry(&fkey(i, "Md"), 0.0);
        }
        if has_fs {
            let _ = self.write_with_retry(&key("FS! "), 0.0);
        }
        self.cfg.saved_min.clear();
        self.cfg.save();
        self.discover_sensors();
    }

    /// Hottest CPU / GPU temperature, used by the overheat guard.
    fn hottest(sensors: &[Sensor]) -> f64 {
        sensors
            .iter()
            .filter(|s| s.kind == Kind::Temperature && (s.key.starts_with("TC") || s.key.starts_with("TG")))
            .map(|s| s.value)
            .fold(0.0, f64::max)
    }

    fn discover_sensors(&mut self) {
        self.sensor_keys.clear();
        let show_all = self.cfg.show_all_sensors;
        for k in self.smc.all_keys() {
            let ks = key_str(&k);
            let Some(kind) = Kind::from_key(&ks) else { continue };
            let Some(info) = self.smc.key_info(&k) else { continue };
            if info.len > 8 {
                continue;
            }
            // Retry up to 3 times with 8ms delay so sensor list is complete and stable
            let mut read_val = None;
            for _ in 0..3 {
                if let Some(v) = self.smc.read_f64(&k) {
                    read_val = Some(v);
                    break;
                }
                std::thread::sleep(Duration::from_millis(8));
            }
            let Some(v) = read_val else { continue };
            if !kind.plausible(v) {
                continue;
            }
            let named = sensors::name(&ks);
            if named.is_none() && !show_all {
                continue;
            }
            let name = named.unwrap_or_else(|| ks.clone());
            self.sensor_keys.push((k, kind, name));
        }
        self.sensor_keys.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)));
    }

    fn read_f64_cached(&mut self, k: &Key) -> Option<f64> {
        match self.smc.read_f64(k) {
            Some(v) => {
                self.cache.insert(*k, CachedReading { value: v, stale_count: 0 });
                Some(v)
            }
            None => {
                if let Some(entry) = self.cache.get_mut(k) {
                    if entry.stale_count < 3 {
                        entry.stale_count += 1;
                        Some(entry.value)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        }
    }

    fn tick(&mut self, hidden: bool) -> Snapshot {
        let now = Instant::now();
        let elapsed = self.last_tick.elapsed();
        self.last_tick = now;
        // Wakeup / sleep detection: normal interval is 1-2s. If > 6s elapsed, system was suspended
        if elapsed > Duration::from_secs(6) {
            let _ = self.smc.reinit(!self.no_mmio);
            self.cache.clear();
            self.smoothed_targets.clear();
            self.peak_temps.clear();
        }

        // 1. In the very beginning of the sensor update cycle (before fan control commands):
        // Read CPU PECI (TCXC, fallback to TC1C or TC0P) and immediately write to Local\WMF_CpuTemp (4-byte float)
        let cpu_temp = self.read_f64_cached(&key("TCXC"))
            .or_else(|| self.read_f64_cached(&key("TC1C")))
            .or_else(|| self.read_f64_cached(&key("TC0P")))
            .unwrap_or(0.0);
        self.shmem.write_cpu_temp(cpu_temp);

        let mut needed_keys: Vec<Key> = Vec::new();
        if hidden {
            // Essential keys for tray icon & CPU temp
            needed_keys.push(key("TC0P"));
            needed_keys.push(key("TCXC"));
            needed_keys.push(key("TC1C"));
            // Overheat protection sensors
            needed_keys.push(key("TG0P"));
            needed_keys.push(key("TG0D"));
            // Active fan curve sensor(s)
            for fi in 0..self.fans.len() {
                let mode = self.cfg.fans.get(&self.fans[fi].index).cloned().unwrap_or_default();
                if let FanMode::Sensor { ref key, .. } = mode {
                    needed_keys.push(crate::smc::key(key));
                }
            }
        }

        let mut sensors = Vec::with_capacity(self.sensor_keys.len());
        let sensor_keys = self.sensor_keys.clone();
        for (k, kind, name) in &sensor_keys {
            if hidden && !needed_keys.contains(k) {
                // Background mode: reuse cached value if available, avoid bus transaction
                if let Some(entry) = self.cache.get(k) {
                    sensors.push(Sensor { key: key_str(k), name: name.clone(), kind: *kind, value: entry.value });
                }
                continue;
            }
            if let Some(v) = self.read_f64_cached(k) {
                sensors.push(Sensor { key: key_str(k), name: name.clone(), kind: *kind, value: v });
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        // Overheat guard (hysteresis 90 °C on / 85 °C off)
        let hot = Self::hottest(&sensors).max(cpu_temp);
        if hot >= OVERHEAT_ON {
            self.overheat = true;
        } else if hot < OVERHEAT_OFF {
            self.overheat = false;
        }

        let mut fans = Vec::new();
        let mut errors = Vec::new();
        if self.overheat {
            errors.push(format!("Overheat guard active ({hot:.0} °C): fans at maximum"));
        }

        let active_profile = self.cfg.profile;
        let is_blast = self.full_blast || active_profile == Profile::FullBlast;

        for fi in 0..self.fans.len() {
            let mode = self.cfg.fans.get(&self.fans[fi].index).cloned().unwrap_or_default();
            let target = match active_profile {
                _ if is_blast => Some(self.fans[fi].max),
                _ if self.overheat => Some(self.fans[fi].max),
                Profile::Auto => None,
                Profile::Silent => Some(self.fans[fi].default_min),
                Profile::FullBlast => Some(self.fans[fi].max),
                Profile::Custom => self.target_for(fi, &mode, &sensors),
            };
            if let Err(e) = self.apply(fi, target) {
                errors.push(format!("Fan {}: {e:#}", self.fans[fi].index + 1));
            }
            let (idx, default_min, max, applied, method) = {
                let f = &self.fans[fi];
                (f.index, f.default_min, f.max, f.applied, f.method)
            };
            let actual = self.read_f64_cached(&fkey(idx, "Ac")).unwrap_or(0.0);
            std::thread::sleep(Duration::from_millis(2));
            let min = self.read_f64_cached(&fkey(idx, "Mn")).unwrap_or(default_min);
            std::thread::sleep(Duration::from_millis(2));

            fans.push(FanState {
                index: idx,
                actual,
                min,
                max,
                default_min,
                requested: applied,
                mode,
                method: match method {
                    Method::ForcedMd => "forced (FnMd)",
                    Method::ForcedFs => "forced (FS!)",
                },
            });
        }

        // Update full status in Windows Named Shared Memory Local\WMF_CpuTemp
        let fan1 = fans.get(0).map(|f| f.actual).unwrap_or(0.0);
        let fan2 = fans.get(1).map(|f| f.actual).unwrap_or(0.0);
        self.shmem.write_status(cpu_temp, fan1, fan2);

        let mut warnings = self.warnings.clone();
        warnings.extend(errors);
        let poll_ms = if hidden {
            self.cfg.poll_ms.max(2000)
        } else {
            self.cfg.poll_ms.clamp(1000, 1500)
        };
        Snapshot {
            fans,
            sensors,
            warnings,
            poll: Duration::from_millis(poll_ms),
            full_blast: is_blast,
            profile: active_profile,
            ..Default::default()
        }
    }

    fn target_for(&mut self, fi: usize, mode: &FanMode, sensors: &[Sensor]) -> Option<f64> {
        let (idx, default_min, max) = {
            let f = &self.fans[fi];
            (f.index, f.default_min, f.max)
        };
        match mode {
            FanMode::Auto => None,
            FanMode::Constant { rpm } => {
                let clamped = rpm.clamp(default_min, max);
                self.smoothed_targets.insert(idx, clamped);
                Some(clamped)
            }
            FanMode::Sensor { key, low, high } => {
                // Fail safe: if the sensor disappears, run at full speed.
                let Some(t) = sensors.iter().find(|s| &s.key == key).map(|s| s.value) else {
                    return Some(max);
                };
                // Hysteresis: only lower the tracked temperature after a 2.5 °C cooldown
                let peak = self.peak_temps.entry(idx).or_insert(t);
                if t > *peak {
                    *peak = t;
                } else if t < *peak - 2.5 {
                    *peak = t + 2.5;
                }
                let effective_t = (*peak).max(t);
                let span = (high - low).max(1.0);
                let r = ((effective_t - low) / span).clamp(0.0, 1.0);
                let raw_target = default_min + (max - default_min) * r;
                let target_clamped = raw_target.clamp(default_min, max);

                // Smooth RPM ramping: max +250 RPM up, max -80 RPM down per cycle
                let prev = self.smoothed_targets.entry(idx).or_insert(target_clamped);
                if target_clamped > *prev {
                    *prev = (*prev + 250.0).min(target_clamped);
                } else if target_clamped < *prev {
                    *prev = (*prev - 80.0).max(target_clamped);
                }
                Some(*prev)
            }
        }
    }

    fn apply(&mut self, fi: usize, target: Option<f64>) -> anyhow::Result<()> {
        let (index, method, applied) = {
            let f = &self.fans[fi];
            (f.index, f.method, f.applied)
        };
        // Skip tiny changes to avoid hammering the SMC.
        let changed = match (applied, target) {
            (None, None) => false,
            (Some(a), Some(t)) => (a - t).abs() >= 25.0,
            _ => true,
        };
        if !changed {
            return Ok(());
        }
        let res = self.write_target(index, method, target);
        if res.is_ok() {
            self.fans[fi].applied = target;
        }
        res
    }

    fn write_target(&mut self, i: u8, method: Method, target: Option<f64>) -> anyhow::Result<()> {
        match (method, target) {
            (Method::ForcedMd, Some(rpm)) => {
                // 1. Enable forced mode for fan i: FnMd = 1.0
                let _ = self.write_with_retry(&fkey(i, "Md"), 1.0);
                // 2. Set target RPM for fan i: FnTg = rpm
                self.write_with_retry(&fkey(i, "Tg"), rpm)
            }
            (Method::ForcedMd, None) => {
                // Return fan i to automatic SMC hardware control: FnMd = 0.0
                self.write_with_retry(&fkey(i, "Md"), 0.0)
            }
            (Method::ForcedFs, Some(rpm)) => {
                let cur = self.smc.read_f64(&key("FS! ")).unwrap_or(0.0) as u16;
                let bit = 1u16 << i;
                let next = cur | bit;
                let _ = self.write_with_retry(&key("FS! "), next as f64);
                self.write_with_retry(&fkey(i, "Tg"), rpm)
            }
            (Method::ForcedFs, None) => {
                let cur = self.smc.read_f64(&key("FS! ")).unwrap_or(0.0) as u16;
                let bit = 1u16 << i;
                let next = cur & !bit;
                self.write_with_retry(&key("FS! "), next as f64)
            }
        }
    }

    /// Hand every fan back to the SMC.
    fn restore_all(&mut self) {
        for fi in 0..self.fans.len() {
            let (i, m) = (self.fans[fi].index, self.fans[fi].method);
            let _ = self.write_target(i, m, None);
            self.fans[fi].applied = None;
        }
        if self.smc.key_info(&key("FS! ")).is_some() {
            let _ = self.write_with_retry(&key("FS! "), 0.0);
        }
        self.cfg.saved_min.clear();
        self.cfg.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(w: &Worker, pred: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let start = Instant::now();
        loop {
            let s = w.shared.lock().unwrap().clone();
            if s.ready && pred(&s) {
                return s;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "timeout: {s:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn modes_and_restore() {
        let dir = std::env::temp_dir().join("wmf-control-test");
        let _ = std::fs::remove_dir_all(&dir);
        unsafe { std::env::set_var("APPDATA", &dir) };

        let w = Worker::spawn(|| Ok(Smc::simulated()), || {}, Arc::new(AtomicBool::new(false)), false);
        let s = wait_for(&w, |s| s.fans.len() == 2);
        assert_eq!(s.fans[0].min, 1800.0);
        assert!(s.sensors.iter().any(|x| x.key == "TC0P"));

        // constant: target speed set
        w.tx.send(Command::SetMode(0, FanMode::Constant { rpm: 4000.0 })).unwrap();
        let s = wait_for(&w, |s| s.fans[0].requested == Some(4000.0));
        assert_eq!(s.fans[0].requested, Some(4000.0));

        // clamped to [default_min, max]
        w.tx.send(Command::SetMode(1, FanMode::Constant { rpm: 99999.0 })).unwrap();
        wait_for(&w, |s| s.fans[1].requested == Some(5800.0));

        // sensor based: TC0P is between 20 and 200 -> somewhere between min and max
        w.tx.send(Command::SetMode(0, FanMode::Sensor { key: "TC0P".into(), low: 20.0, high: 200.0 })).unwrap();
        let s = wait_for(&w, |s| matches!(s.fans[0].mode, FanMode::Sensor { .. }));
        let r = s.fans[0].requested.unwrap();
        assert!(r > 1800.0 && r < 5300.0, "{r}");

        // missing sensor -> fail safe to max
        w.tx.send(Command::SetMode(0, FanMode::Sensor { key: "NONE".into(), low: 40.0, high: 80.0 })).unwrap();
        wait_for(&w, |s| s.fans[0].requested == Some(5300.0));

        // full blast: every fan at max, then back to its own mode
        w.tx.send(Command::SetMode(1, FanMode::Auto)).unwrap();
        wait_for(&w, |s| s.fans[1].requested.is_none());
        w.tx.send(Command::FullBlast(true)).unwrap();
        wait_for(&w, |s| s.full_blast && s.fans[0].requested == Some(5300.0) && s.fans[1].requested == Some(5800.0));
        w.tx.send(Command::FullBlast(false)).unwrap();
        wait_for(&w, |s| !s.full_blast && s.fans[1].requested.is_none());

        // auto restores factory min
        w.tx.send(Command::SetMode(0, FanMode::Auto)).unwrap();
        let s = wait_for(&w, |s| s.fans[0].requested.is_none());
        assert_eq!(s.fans[0].min, 1800.0);

        // shutdown restores everything and clears the crash-recovery record
        w.shutdown();
        assert!(Config::load().saved_min.is_empty());
    }
}
