// WMF Control (Windows Mac Fan Control) - MIT License
//! Adaptive Predictive Cooling (Feedforward Thermal Control).
//!
//! Anticipates thermal surges by monitoring instantaneous CPU Package Power (Watts),
//! overall CPU load (%), and temperature rate of change (dT/dt), calculating a
//! virtual predicted temperature:
//!
//!   PredictedTemp = RealTemp + ThermalGain * SmoothedPowerOrLoad + DerivativeGain * Max(0, dT/dt)
//!
//! Features:
//! - Attack Delay filter: prevents fan revving on transient 1-second micro-spikes.
//! - Max Boost Cap: limits feedforward boost (+10..+18 °C depending on profile).
//! - Smooth Release / Hold: holds boost for 4-6 seconds to purge heat from heatsink fin stacks.
//! - Online Adaptation: continuously self-calibrates thermal gain to the specific Mac's paste & heatsink.

use crate::config::PredictiveMode;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct PredictiveState {
    pub mode: PredictiveMode,
    pub raw_temp: f64,
    pub predicted_temp: f64,
    pub boost_c: f64,
    pub cpu_power_w: f64,
    pub cpu_load_pct: f64,
    pub dt_dt: f64,
    pub adapted_gain: f64,
    pub is_active: bool,
}

impl Default for PredictiveState {
    fn default() -> Self {
        Self {
            mode: PredictiveMode::Balanced,
            raw_temp: 50.0,
            predicted_temp: 50.0,
            boost_c: 0.0,
            cpu_power_w: 12.0,
            cpu_load_pct: 0.0,
            dt_dt: 0.0,
            adapted_gain: 0.20,
            is_active: false,
        }
    }
}

pub struct PredictiveThermalEngine {
    mode: PredictiveMode,
    /// Base thermal gain (°C / Watt)
    base_gain: f64,
    /// Online-adapted thermal gain (°C / Watt)
    adapted_gain: f64,
    /// Derivative gain (seconds)
    derivative_gain: f64,
    /// Maximum allowed boost cap (°C)
    max_boost_cap: f64,
    /// Attack delay to ignore micro-spikes (seconds)
    attack_delay_s: f64,
    /// Hold time before release begins (seconds)
    hold_time_s: f64,
    /// Release decay rate (°C / second)
    release_rate: f64,
    /// Base idle/browser power threshold (Watts) below which predictive boost is 0.0 °C
    idle_power_threshold: f64,

    // Runtime filter state
    last_tick: Instant,
    last_temp: f64,
    smoothed_power: f64,
    smoothed_load: f64,
    spike_duration_s: f64,
    current_boost: f64,
    hold_remaining_s: f64,

    // CPU load tracking via GetSystemTimes
    last_idle_ft: u64,
    last_kernel_ft: u64,
    last_user_ft: u64,
    last_cpu_load: f64,

    // Adaptation tracking
    learned_samples: u32,
    last_save: Instant,
    needs_config_save: bool,
}

#[cfg(windows)]
fn sample_cpu_load(last_idle: &mut u64, last_kernel: &mut u64, last_user: &mut u64) -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetSystemTimes;

    fn ft_to_u64(ft: FILETIME) -> u64 {
        ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
    }

    unsafe {
        let mut idle_ft: FILETIME = std::mem::zeroed();
        let mut kernel_ft: FILETIME = std::mem::zeroed();
        let mut user_ft: FILETIME = std::mem::zeroed();

        if GetSystemTimes(&mut idle_ft, &mut kernel_ft, &mut user_ft) == 0 {
            return 0.0;
        }

        let idle = ft_to_u64(idle_ft);
        let kernel = ft_to_u64(kernel_ft);
        let user = ft_to_u64(user_ft);

        if *last_kernel == 0 && *last_user == 0 {
            *last_idle = idle;
            *last_kernel = kernel;
            *last_user = user;
            return 0.0;
        }

        let idle_diff = idle.saturating_sub(*last_idle);
        let kernel_diff = kernel.saturating_sub(*last_kernel);
        let user_diff = user.saturating_sub(*last_user);
        let total_diff = kernel_diff + user_diff;

        *last_idle = idle;
        *last_kernel = kernel;
        *last_user = user;

        if total_diff > 0 {
            let busy = total_diff.saturating_sub(idle_diff);
            ((busy as f64) / (total_diff as f64) * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        }
    }
}

#[cfg(not(windows))]
fn sample_cpu_load(_: &mut u64, _: &mut u64, _: &mut u64) -> f64 {
    0.0
}

impl PredictiveThermalEngine {
    pub fn new(mode: PredictiveMode, saved_gain: Option<f64>) -> Self {
        let mut engine = Self {
            mode,
            base_gain: 0.20,
            adapted_gain: saved_gain.unwrap_or(0.20),
            derivative_gain: 2.0,
            max_boost_cap: 14.0,
            attack_delay_s: 1.5,
            hold_time_s: 4.5,
            release_rate: 1.8,
            idle_power_threshold: 20.0,

            last_tick: Instant::now(),
            last_temp: 50.0,
            smoothed_power: 12.0,
            smoothed_load: 0.0,
            spike_duration_s: 0.0,
            current_boost: 0.0,
            hold_remaining_s: 0.0,

            last_idle_ft: 0,
            last_kernel_ft: 0,
            last_user_ft: 0,
            last_cpu_load: 0.0,

            learned_samples: 0,
            last_save: Instant::now(),
            needs_config_save: false,
        };
        engine.apply_mode_params(mode);
        if let Some(gain) = saved_gain {
            engine.adapted_gain = gain.clamp(0.08, 0.50);
        }
        engine
    }

    pub fn set_mode(&mut self, mode: PredictiveMode) {
        self.mode = mode;
        self.apply_mode_params(mode);
        if mode == PredictiveMode::Off {
            self.current_boost = 0.0;
            self.spike_duration_s = 0.0;
            self.hold_remaining_s = 0.0;
        }
    }

    fn apply_mode_params(&mut self, mode: PredictiveMode) {
        match mode {
            PredictiveMode::Off => {
                self.max_boost_cap = 0.0;
                self.idle_power_threshold = 50.0;
            }
            PredictiveMode::Silent => {
                self.base_gain = 0.12;
                self.derivative_gain = 1.2;
                self.max_boost_cap = 10.0;
                self.attack_delay_s = 2.0;
                self.hold_time_s = 3.5;
                self.release_rate = 2.2;
                self.idle_power_threshold = 25.0;
            }
            PredictiveMode::Balanced => {
                self.base_gain = 0.20;
                self.derivative_gain = 2.0;
                self.max_boost_cap = 14.0;
                self.attack_delay_s = 1.5;
                self.hold_time_s = 4.5;
                self.release_rate = 1.8;
                self.idle_power_threshold = 20.0;
            }
            PredictiveMode::Aggressive => {
                self.base_gain = 0.28;
                self.derivative_gain = 2.8;
                self.max_boost_cap = 18.0;
                self.attack_delay_s = 0.8;
                self.hold_time_s = 5.5;
                self.release_rate = 1.4;
                self.idle_power_threshold = 14.0;
            }
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.mode != PredictiveMode::Off
    }

    pub fn current_gain(&self) -> f64 {
        self.adapted_gain
    }

    pub fn check_and_clear_save_request(&mut self) -> bool {
        if self.needs_config_save {
            self.needs_config_save = false;
            true
        } else {
            false
        }
    }

    pub fn update(&mut self, raw_temp: f64, smc_power_w: Option<f64>) -> PredictiveState {
        let now = Instant::now();
        let dt = self.last_tick.elapsed().as_secs_f64().clamp(0.05, 10.0);
        self.last_tick = now;

        // Reset tracking on system wake from sleep (> 6s delay)
        if dt > 6.0 {
            self.current_boost = 0.0;
            self.spike_duration_s = 0.0;
            self.hold_remaining_s = 0.0;
            self.last_temp = raw_temp;
        }

        // 1. Telemetry: CPU load (%)
        let cpu_load = sample_cpu_load(&mut self.last_idle_ft, &mut self.last_kernel_ft, &mut self.last_user_ft);
        self.last_cpu_load = cpu_load;

        // 2. Telemetry: Instantaneous power in Watts
        // Package TDP ~45-65W on MacBook Pro 15"/16". Baseline idle ~8-12W.
        let load_power_est = 8.0 + (cpu_load / 100.0) * 55.0;
        let effective_power = match smc_power_w {
            Some(p) if p > 1.0 => p.max(load_power_est * 0.85),
            _ => load_power_est,
        };

        // Smooth power and load (time constant ~1.5s)
        let alpha = (dt / 1.5).clamp(0.0, 1.0);
        self.smoothed_power += alpha * (effective_power - self.smoothed_power);
        self.smoothed_load += alpha * (cpu_load - self.smoothed_load);

        // 3. Telemetry: Temperature rate of change dT/dt (°C / sec)
        let dt_dt = if dt > 0.0 {
            ((raw_temp - self.last_temp) / dt).max(0.0)
        } else {
            0.0
        };
        self.last_temp = raw_temp;

        if self.mode == PredictiveMode::Off {
            return PredictiveState {
                mode: self.mode,
                raw_temp,
                predicted_temp: raw_temp,
                boost_c: 0.0,
                cpu_power_w: effective_power,
                cpu_load_pct: cpu_load,
                dt_dt,
                adapted_gain: self.adapted_gain,
                is_active: false,
            };
        }

        // 4. False positive protection: Attack Delay
        // A sustained power jump above idle_power_threshold increments spike_duration_s
        let threshold = self.idle_power_threshold;
        let is_elevated = self.smoothed_power > threshold;

        if is_elevated {
            self.spike_duration_s += dt;
        } else {
            self.spike_duration_s = (self.spike_duration_s - dt * 2.0).max(0.0);
        }

        // Ramp attack factor from 0.0 to 1.0 after sustaining past attack_delay_s
        let attack_factor = if self.spike_duration_s >= self.attack_delay_s {
            ((self.spike_duration_s - self.attack_delay_s) / 1.0).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // 5. Feedforward calculation:
        // Predicted boost = ThermalGain * ExcessPower + DerivativeGain * Max(0, dT/dt)
        // If smoothed power is below idle_power_threshold (e.g. browsing / idle at 15-19W),
        // target boost is strictly 0.0 °C so fans remain quiet.
        let target_boost = if self.smoothed_power <= threshold {
            0.0
        } else {
            let excess_power = self.smoothed_power - threshold;
            let power_boost = self.adapted_gain * excess_power;
            let deriv_boost = self.derivative_gain * dt_dt.min(8.0);
            let raw_boost = attack_factor * (power_boost + deriv_boost);
            raw_boost.clamp(0.0, self.max_boost_cap)
        };

        // 6. Smooth Release / Hold filter:
        if target_boost >= self.current_boost {
            // Rising or sustaining higher boost
            self.current_boost = target_boost;
            self.hold_remaining_s = self.hold_time_s;
        } else {
            // Load dropped: hold first, then smoothly decay
            if self.hold_remaining_s > 0.0 {
                self.hold_remaining_s = (self.hold_remaining_s - dt).max(0.0);
            } else {
                self.current_boost = (self.current_boost - self.release_rate * dt).max(target_boost);
            }
        }

        let predicted_temp = raw_temp + self.current_boost;

        // 7. Online Adaptation (self-calibrating thermal response):
        // Under sustained load (> 4s), observe physical heating delta per excess Watt
        let excess_pwr = (self.smoothed_power - threshold).max(0.0);
        if is_elevated && self.spike_duration_s > 4.0 && excess_pwr > 10.0 && raw_temp > 62.0 {
            let observed = ((raw_temp - 55.0) / excess_pwr).clamp(0.08, 0.45);
            let lr = 0.005; // Gentle learning rate per tick
            self.adapted_gain = (1.0 - lr) * self.adapted_gain + lr * observed;
            self.learned_samples += 1;

            // Periodically request saving adapted gain to config (every 3 minutes of active load)
            if self.last_save.elapsed() > Duration::from_secs(180) {
                self.last_save = Instant::now();
                self.needs_config_save = true;
            }
        }

        PredictiveState {
            mode: self.mode,
            raw_temp,
            predicted_temp,
            boost_c: self.current_boost,
            cpu_power_w: effective_power,
            cpu_load_pct: cpu_load,
            dt_dt,
            adapted_gain: self.adapted_gain,
            is_active: self.current_boost > 0.5,
        }
    }
}
