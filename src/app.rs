// WMF Control (Windows Mac Fan Control) - MIT License
use crate::config::{Config, FanMode};
use crate::control::{Command, FanState, Snapshot, Worker};
use crate::sensors::Kind;
use crate::tray::{Tray, WindowCtl};
use eframe::egui::{self, Color32, RichText};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

fn hwnd_of(w: &impl raw_window_handle::HasWindowHandle) -> Option<isize> {
    match w.window_handle().ok()?.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

struct Editor {
    fan: u8,
    choice: u8, // 0 auto, 1 constant, 2 sensor
    rpm: f64,
    sensor: String,
    low: f64,
    high: f64,
}

pub fn autostart_is_enabled() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let res = std::process::Command::new("schtasks")
            .args(["/query", "/tn", "WMFControl"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match res {
            Ok(out) => out.status.success(),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    false
}

pub fn set_autostart(enable: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if enable {
            let exe = std::env::current_exe()?;
            let exe_str = exe.to_string_lossy();
            let parent_dir = exe.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>WMF Control</Author>
    <URI>\WMFControl</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <Delay>PT5S</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{}</Command>
      <Arguments>--minimized</Arguments>
      <WorkingDirectory>{}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>"#,
                exe_str, parent_dir
            );

            let temp_xml = std::env::temp_dir().join("wmf_task.xml");
            let mut utf16_bytes: Vec<u8> = vec![0xFF, 0xFE];
            for u in xml.encode_utf16() {
                utf16_bytes.extend_from_slice(&u.to_le_bytes());
            }
            std::fs::write(&temp_xml, &utf16_bytes)?;

            let status = std::process::Command::new("schtasks")
                .args(["/create", "/tn", "WMFControl", "/xml", &temp_xml.to_string_lossy(), "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();

            let _ = std::fs::remove_file(&temp_xml);

            let status = status?;
            if !status.status.success() {
                let err = String::from_utf8_lossy(&status.stderr);
                anyhow::bail!("{err}");
            }
        } else {
            let _ = std::process::Command::new("schtasks")
                .args(["/delete", "/tn", "WMFControl", "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
    }
    Ok(())
}

pub struct App {
    worker: Worker,
    /// The same worker, shared with the tray menu handlers.
    worker_slot: Arc<Mutex<Worker>>,
    demo: bool,
    no_mmio: bool,
    editor: Option<Editor>,
    fahrenheit: bool,
    show_all: bool,
    minimize_to_tray: bool,
    start_minimized: bool,
    autostart: bool,
    autostart_err: Option<String>,
    poll_ms: u64,
    ctx: egui::Context,
    win: Arc<WindowCtl>,
    tray: Option<Tray>,
}

fn spawn(ctx: &egui::Context, demo: bool, no_mmio: bool, is_hidden: Arc<std::sync::atomic::AtomicBool>) -> Worker {
    let c = ctx.clone();
    Worker::spawn(move || crate::open_smc(demo, no_mmio), move || c.request_repaint(), is_hidden, no_mmio)
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, demo: bool, no_mmio: bool, start_minimized: bool) -> Self {
        let cfg = Config::load();
        let ctx = cc.egui_ctx.clone();
        let win = WindowCtl::new(!cfg.no_minimize_to_tray, start_minimized);
        if let Some(h) = hwnd_of(cc) {
            win.hwnd.store(h, Ordering::Relaxed);
            if start_minimized {
                win.hide();
            }
        }
        let worker = spawn(&ctx, demo, no_mmio, win.hidden.clone());
        let worker_slot = Arc::new(Mutex::new(worker.clone()));
        let tray = Tray::new(worker_slot.clone(), win.clone(), ctx.clone()).ok();
        let autostart = autostart_is_enabled();
        if autostart {
            let _ = set_autostart(true);
        }
        App {
            worker,
            worker_slot,
            demo,
            no_mmio,
            editor: None,
            fahrenheit: cfg.fahrenheit,
            show_all: cfg.show_all_sensors,
            minimize_to_tray: !cfg.no_minimize_to_tray,
            start_minimized: cfg.start_minimized,
            autostart,
            autostart_err: None,
            poll_ms: cfg.poll_ms,
            ctx,
            win,
            tray,
        }
    }

    fn update_tray(&mut self, snap: &Snapshot) {
        let cpu_txt = snap.sensors.iter().find(|s| s.key == "TCXC" || s.key == "TC0P").map(|s| self.fmt_temp(s.value));
        if let Some(t) = &mut self.tray {
            use crate::config::Profile;
            t.mode_auto.set_checked(snap.profile == Profile::Auto && !snap.full_blast);
            t.mode_custom.set_checked(snap.profile == Profile::Custom && !snap.full_blast);
            t.mode_silent.set_checked(snap.profile == Profile::Silent && !snap.full_blast);
            t.mode_blast.set_checked(snap.profile == Profile::FullBlast || snap.full_blast);

            let profile_name = if snap.full_blast || snap.profile == Profile::FullBlast {
                "Full Blast"
            } else {
                match snap.profile {
                    Profile::Auto => "Auto",
                    Profile::Custom => "Custom Curve",
                    Profile::Silent => "Silent",
                    Profile::FullBlast => "Full Blast",
                }
            };
            let mut tip = format!("WMF Control [{profile_name}]");
            if let Some(cpu) = cpu_txt {
                tip += &format!("\nCPU: {cpu}");
            }
            if snap.predictive.is_active {
                tip += &format!(" [Boost +{:.0}°C]", snap.predictive.boost_c);
            }
            for f in &snap.fans {
                tip += &format!("\nFan {}: {:.0} RPM", f.index + 1, f.actual);
            }
            t.set_tooltip(tip);
        }
    }

    fn send_prefs(&self) {
        let _ = self.worker.tx.send(Command::SetPrefs {
            fahrenheit: self.fahrenheit,
            show_all: self.show_all,
            tray: self.minimize_to_tray,
        });
    }

    fn full_blast_button(&self, ui: &mut egui::Ui, snap: &Snapshot) {
        let on = snap.full_blast;
        let (text, fill) = if on {
            (RichText::new("FULL BLAST: ON").strong().color(Color32::WHITE), Color32::from_rgb(200, 45, 40))
        } else {
            (RichText::new("Full Blast").strong(), Color32::from_rgb(70, 40, 40))
        };
        let btn = egui::Button::new(text).fill(fill).min_size(egui::vec2(120.0, 24.0));
        if ui.add_enabled(snap.ready && snap.error.is_none(), btn)
            .on_hover_text("Run every fan at maximum RPM. Click again to go back to your settings.")
            .clicked()
        {
            let _ = self.worker.tx.send(Command::FullBlast(!on));
        }
    }

    fn fmt_value(&self, kind: Kind, v: f64) -> String {
        match kind {
            Kind::Temperature if self.fahrenheit => format!("{:.0} °F", v * 9.0 / 5.0 + 32.0),
            Kind::Temperature => format!("{v:.0} °C"),
            Kind::Power => format!("{v:.2} W"),
            Kind::Voltage => format!("{v:.2} V"),
            Kind::Current => format!("{v:.2} A"),
        }
    }

    fn fmt_temp(&self, c: f64) -> String {
        self.fmt_value(Kind::Temperature, c)
    }

    fn restart(&mut self, demo: bool) {
        self.worker.shutdown();
        self.demo = demo;
        self.worker = spawn(&self.ctx, demo, self.no_mmio, self.win.hidden.clone());
        *self.worker_slot.lock().unwrap() = self.worker.clone();
    }

    fn mode_label(&self, f: &FanState, snap: &Snapshot) -> String {
        match &f.mode {
            FanMode::Auto => "Auto".into(),
            FanMode::Constant { rpm } => format!("Constant {rpm:.0} RPM"),
            FanMode::Sensor { key, low, high } => {
                let name = snap.sensors.iter().find(|s| &s.key == key).map(|s| s.name.as_str()).unwrap_or(key);
                format!("{name}: {} -> {}", self.fmt_temp(*low), self.fmt_temp(*high))
            }
        }
    }

    fn fans_ui(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        ui.heading("Fans");
        ui.add_space(4.0);

        // Predictive Cooling (AI Thermal Boost) panel
        egui::Frame::group(ui.style()).fill(ui.visuals().faint_bg_color).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("[AI]").strong().color(Color32::from_rgb(0, 210, 255)).size(14.0));
                ui.label(RichText::new("Predictive Cooling").strong().size(15.0))
                    .on_hover_text("Adaptive Feedforward Thermal Control:\nRamps up fan speeds ahead of time during CPU/GPU power or load surges, preventing thermal throttling before silicon temperatures jump.");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    use crate::config::PredictiveMode;
                    let cur_mode = snap.predictive.mode;
                    let mut selected = cur_mode;
                    let label = match cur_mode {
                        PredictiveMode::Off => "Off",
                        PredictiveMode::Silent => "Silent",
                        PredictiveMode::Balanced => "Balanced",
                        PredictiveMode::Aggressive => "Aggressive",
                    };
                    egui::ComboBox::from_id_salt("predictive_mode_combo")
                        .selected_text(label)
                        .width(105.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut selected, PredictiveMode::Off, "Off (pure real temp)");
                            ui.selectable_value(&mut selected, PredictiveMode::Silent, "Silent (light feedforward, +10° max)");
                            ui.selectable_value(&mut selected, PredictiveMode::Balanced, "Balanced (recommended, +14° max)");
                            ui.selectable_value(&mut selected, PredictiveMode::Aggressive, "Aggressive (gaming/render, +18° max)");
                        });
                    if selected != cur_mode {
                        let _ = self.worker.tx.send(Command::SetPredictiveMode(selected));
                    }
                });
            });

            if snap.predictive.mode != crate::config::PredictiveMode::Off {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    let real_fmt = self.fmt_temp(snap.predictive.raw_temp);
                    let pred_fmt = self.fmt_temp(snap.predictive.predicted_temp);
                    ui.label(RichText::new(format!("Real: {real_fmt}")).size(13.0));
                    if snap.predictive.boost_c > 0.5 {
                        ui.label(RichText::new(format!("-> Predicted: {pred_fmt}")).strong().size(13.0).color(Color32::from_rgb(0, 210, 255)));
                        let badge_txt = format!("[BOOST +{:.1}°C]", snap.predictive.boost_c);
                        ui.label(RichText::new(badge_txt).small().strong().color(Color32::from_rgb(255, 180, 50)));
                    } else {
                        ui.label(RichText::new("Standing by").weak().small());
                    }
                });
                ui.horizontal(|ui| {
                    let pwr_str = format!("{:.1} W", snap.predictive.cpu_power_w);
                    let load_str = format!("{:.0}%", snap.predictive.cpu_load_pct);
                    let gain_str = format!("{:.2} °C/W", snap.predictive.adapted_gain);
                    ui.label(RichText::new(format!("Power: {pwr_str} | CPU: {load_str} | Calibrated: {gain_str}")).weak().small());
                });
            } else {
                ui.label(RichText::new("Feedforward thermal anticipation is disabled.").weak().small());
            }
        });
        ui.add_space(8.0);

        if snap.fans.is_empty() {
            ui.label("No fans found.");
        }
        for f in &snap.fans {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Fan {}", f.index + 1)).strong().size(16.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let btn = if f.mode == FanMode::Auto { "Custom..." } else { "Change..." };
                        if ui.button(btn).clicked() {
                            self.editor = Some(editor_for(f, snap));
                        }
                        if f.mode != FanMode::Auto && ui.button("Auto").clicked() {
                            let _ = self.worker.tx.send(Command::SetMode(f.index, FanMode::Auto));
                        }
                    });
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{:.0} RPM", f.actual)).size(22.0).color(Color32::from_rgb(80, 160, 255)));
                    ui.label(format!("min {:.0} - max {:.0}", f.min, f.max));
                });
                let frac = if f.max > 0.0 { (f.actual / f.max).clamp(0.0, 1.0) } else { 0.0 } as f32;
                ui.add(egui::ProgressBar::new(frac).desired_height(6.0));
                
                let (status_text, col) = if snap.full_blast {
                    ("Full Blast (max RPM)".to_string(), Color32::from_rgb(240, 80, 70))
                } else {
                    match &f.mode {
                        FanMode::Auto => ("Auto (system controlled)".to_string(), ui.visuals().weak_text_color()),
                        FanMode::Constant { rpm } => (format!("Constant {:.0} RPM", rpm), Color32::from_rgb(255, 170, 60)),
                        FanMode::Sensor { key, low, high } => {
                            let s_name = snap.sensors.iter().find(|s| &s.key == key).map(|s| s.name.as_str()).unwrap_or(key);
                            let target_str = f.requested.map(|r| format!(" | Target: {:.0} RPM", r)).unwrap_or_default();
                            let boost_str = f.predictive_boost.map(|b| format!(" [+{:.0}°C]", b)).unwrap_or_default();
                            (format!("{s_name} ({low:.0}°C -> {high:.0}°C){target_str}{boost_str}"), Color32::from_rgb(255, 170, 60))
                        }
                    }
                };
                let hover_text = format!(
                    "Control method: {}\nRequested: {}\nActual: {:.0} RPM\nMode: {}",
                    f.method,
                    f.requested.map(|r| format!("{r:.0} RPM")).unwrap_or_else(|| "N/A".into()),
                    f.actual,
                    self.mode_label(f, snap)
                );
                ui.horizontal(|ui| {
                    ui.label("Control:");
                    ui.add(egui::Label::new(RichText::new(status_text).color(col)).truncate())
                        .on_hover_text(hover_text);
                });
            });
            ui.add_space(6.0);
        }
    }

    fn sensors_ui(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        ui.horizontal(|ui| {
            ui.heading("Sensors");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.checkbox(&mut self.show_all, "Show all sensors").changed() {
                    self.send_prefs();
                }
            });
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for kind in [Kind::Temperature, Kind::Power, Kind::Voltage, Kind::Current] {
                let mut items: Vec<_> = snap.sensors.iter().filter(|s| s.kind == kind).collect();
                if !self.show_all {
                    items.retain(|s| is_primary_sensor(s));
                }
                if items.is_empty() {
                    continue;
                }
                egui::CollapsingHeader::new(kind.title()).default_open(true).show(ui, |ui| {
                    egui::Grid::new(kind.title()).striped(true).num_columns(3).show(ui, |ui| {
                        for s in items {
                            ui.label(&s.name);
                            ui.label(RichText::new(&s.key).weak().monospace());
                            let mut t = RichText::new(self.fmt_value(kind, s.value)).strong();
                            if kind == Kind::Temperature {
                                t = t.color(temp_color(s.value));
                            }
                            ui.label(t);
                            ui.end_row();
                        }
                    });
                });
            }
        });
    }

    fn editor_ui(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let Some(ed) = &mut self.editor else { return };
        let Some(fan) = snap.fans.iter().find(|f| f.index == ed.fan).cloned() else {
            self.editor = None;
            return;
        };
        let temps: Vec<_> = snap.sensors.iter().filter(|s| s.kind == Kind::Temperature).cloned().collect();
        let mut open = true;
        let mut result: Option<Option<FanMode>> = None;
        let fahrenheit = self.fahrenheit;
        egui::Window::new(format!("Fan {} - control", fan.index + 1))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.radio_value(&mut ed.choice, 0, "Auto (system controlled)");
                ui.radio_value(&mut ed.choice, 1, "Constant value of RPM");
                ui.add_enabled_ui(ed.choice == 1, |ui| {
                    ui.add(egui::Slider::new(&mut ed.rpm, fan.default_min..=fan.max).suffix(" RPM").step_by(50.0));
                });
                ui.radio_value(&mut ed.choice, 2, "Sensor-based value");
                ui.add_enabled_ui(ed.choice == 2, |ui| {
                    let cur = temps.iter().find(|s| s.key == ed.sensor);
                    let label = cur.map(|s| format!("{} ({})", s.name, s.key)).unwrap_or_else(|| "Select sensor".into());
                    egui::ComboBox::from_label("Sensor").selected_text(label).width(260.0).show_ui(ui, |ui| {
                        for s in &temps {
                            let t = if fahrenheit { s.value * 9.0 / 5.0 + 32.0 } else { s.value };
                            ui.selectable_value(&mut ed.sensor, s.key.clone(), format!("{} ({}) - {t:.0}°", s.name, s.key));
                        }
                    });
                    let (unit, conv, back): (&str, fn(f64) -> f64, fn(f64) -> f64) = if fahrenheit {
                        ("°F", |c| c * 9.0 / 5.0 + 32.0, |f| (f - 32.0) * 5.0 / 9.0)
                    } else {
                        ("°C", |c| c, |c| c)
                    };
                    let mut lo = conv(ed.low);
                    let mut hi = conv(ed.high);
                    ui.add(egui::Slider::new(&mut lo, conv(20.0)..=conv(100.0)).text(format!("{unit} speed starts to increase")));
                    ui.add(egui::Slider::new(&mut hi, conv(25.0)..=conv(105.0)).text(format!("{unit} max speed")));
                    ed.low = back(lo);
                    ed.high = back(hi).max(ed.low + 1.0);
                    ui.label(RichText::new(format!(
                        "Below the first value the fan runs at {:.0} RPM, above the second at {:.0} RPM, linear in between.",
                        fan.default_min, fan.max
                    )).weak());
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        result = Some(Some(match ed.choice {
                            0 => FanMode::Auto,
                            1 => FanMode::Constant { rpm: ed.rpm },
                            _ => FanMode::Sensor { key: ed.sensor.clone(), low: ed.low, high: ed.high },
                        }));
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(None);
                    }
                });
            });
        if let Some(r) = result {
            if let Some(mode) = r {
                if !matches!(&mode, FanMode::Sensor { key, .. } if key.is_empty()) {
                    let _ = self.worker.tx.send(Command::SetMode(fan.index, mode));
                }
            }
            self.editor = None;
        } else if !open {
            self.editor = None;
        }
    }
}

fn editor_for(f: &FanState, snap: &Snapshot) -> Editor {
    let default_sensor = snap
        .sensors
        .iter()
        .find(|s| s.key == "TC0P")
        .or_else(|| snap.sensors.iter().find(|s| s.kind == Kind::Temperature))
        .map(|s| s.key.clone())
        .unwrap_or_default();
    let mut ed = Editor { fan: f.index, choice: 0, rpm: f.default_min.max(f.actual).round(), sensor: default_sensor, low: 62.0, high: 93.0 };
    match &f.mode {
        FanMode::Auto => {}
        FanMode::Constant { rpm } => {
            ed.choice = 1;
            ed.rpm = *rpm;
        }
        FanMode::Sensor { key, low, high } => {
            ed.choice = 2;
            ed.sensor = key.clone();
            ed.low = *low;
            ed.high = *high;
        }
    }
    ed
}

fn is_primary_sensor(s: &crate::control::Sensor) -> bool {
    let k = s.key.as_str();
    let n = s.name.as_str();
    if k.starts_with("TC")
        || k.starts_with("TG")
        || k.starts_with("Th")
        || k == "TC0P"
        || k == "TC0H"
        || k == "TaLC"
        || k == "TaRC"
        || k.starts_with("TA")
        || k.starts_with("Ts")
        || k.starts_with("TB")
        || k.starts_with("TM")
        || k == "PSTR"
        || k == "PCPC"
        || k == "PCPT"
        || k == "PC0R"
        || k == "PCTR"
        || k == "PG0R"
        || k == "PGTR"
        || k == "PDTR"
        || k == "PPBR"
    {
        return true;
    }
    if n.contains("CPU")
        || n.contains("GPU")
        || n.contains("Heatsink")
        || n.contains("Battery")
        || n.contains("Palm Rest")
        || n.contains("Airflow")
    {
        return true;
    }
    false
}

fn temp_color(c: f64) -> Color32 {
    if c > 82.0 {
        Color32::from_rgb(240, 70, 60)
    } else if c >= 65.0 {
        Color32::from_rgb(255, 170, 50)
    } else {
        Color32::from_rgb(90, 200, 120)
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Closing the window hands every fan back to the SMC.
        self.worker.shutdown();
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.win.hwnd.load(Ordering::Relaxed) == 0 {
            if let Some(h) = hwnd_of(frame) {
                self.win.hwnd.store(h, Ordering::Relaxed);
                if self.win.hidden.load(Ordering::Relaxed) {
                    self.win.hide();
                }
            }
        }
        // Minimize → hide to the tray is done by the watcher thread in WindowCtl.
        self.win.minimize_to_tray.store(self.minimize_to_tray && self.tray.is_some(), Ordering::Relaxed);

        let snap = self.worker.shared.lock().unwrap().clone();
        self.update_tray(&snap);

        // When hidden in tray, completely skip rendering UI and ensure viewport is hidden
        if self.win.hidden.load(Ordering::Relaxed) {
            root.ctx().send_viewport_cmd(egui::ViewportCommand::Visible(false));
            return;
        }

        let ctx = &root.ctx().clone();

        egui::Panel::top("top").show(root, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(crate::APP_NAME).strong().size(18.0)).on_hover_text(crate::APP_FULL_NAME);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button("About", |ui| {
                        ui.label(RichText::new(format!("{} v{}", crate::APP_NAME, crate::VERSION)).strong().size(16.0));
                        ui.label(crate::APP_FULL_NAME);
                        ui.label("Fan control and sensor monitor for Intel Macs running Windows (Boot Camp).");
                        ui.separator();
                        ui.hyperlink_to("Source code on GitHub", crate::REPO_URL);
                        ui.label(RichText::new("MIT License").weak());
                    });
                    ui.menu_button("Donate", |ui| {
                        ui.set_min_width(290.0);
                        ui.label(RichText::new("Support Developer").strong().size(15.0));
                        ui.label("If you find WMF Control helpful, consider supporting its open-source development:");
                        ui.separator();

                        ui.horizontal(|ui| {
                            let boosty_btn = egui::Button::new(RichText::new("Boosty (Card / Global)").strong())
                                .min_size(egui::vec2(210.0, 24.0));
                            if ui.add(boosty_btn).on_hover_text("Open Boosty page in default browser").clicked() {
                                ui.ctx().open_url(egui::OpenUrl::new_tab("https://boosty.to/gringleb/donate"));
                            }
                            if ui.button("Copy").on_hover_text("Copy Boosty link to clipboard").clicked() {
                                ui.ctx().copy_text("https://boosty.to/gringleb/donate".to_string());
                            }
                        });

                        ui.add_space(4.0);

                        ui.horizontal(|ui| {
                            let crypto_btn = egui::Button::new(RichText::new("NOWPayments (Crypto: USDT / TON / BTC)").strong())
                                .min_size(egui::vec2(210.0, 24.0));
                            if ui.add(crypto_btn).on_hover_text("Open NOWPayments crypto donation page in default browser").clicked() {
                                ui.ctx().open_url(egui::OpenUrl::new_tab("https://nowpayments.io/donation/glebgrin"));
                            }
                            if ui.button("Copy").on_hover_text("Copy crypto donation link to clipboard").clicked() {
                                ui.ctx().copy_text("https://nowpayments.io/donation/glebgrin".to_string());
                            }
                        });

                        ui.separator();
                        ui.label(RichText::new("Thank you for supporting independent open-source software!").weak().small());
                    }).response.on_hover_text("Support independent open-source development (Boosty or Crypto)");
                    ui.menu_button("Preferences", |ui| {
                        let mut changed = ui.checkbox(&mut self.fahrenheit, "Show °F").changed();
                        changed |= ui.checkbox(&mut self.show_all, "Show all sensors").changed();
                        changed |= ui.checkbox(&mut self.minimize_to_tray, "Minimize to system tray").changed();
                        if ui.checkbox(&mut self.start_minimized, "Start minimized to tray").changed() {
                            let mut c = Config::load();
                            c.start_minimized = self.start_minimized;
                            c.save();
                        }
                        let mut auto = self.autostart;
                        if ui.checkbox(&mut auto, "Start with Windows (Task Scheduler, no UAC)").changed() {
                            match set_autostart(auto) {
                                Ok(()) => {
                                    self.autostart = auto;
                                    self.autostart_err = None;
                                }
                                Err(e) => {
                                    self.autostart_err = Some(e.to_string());
                                }
                            }
                        }
                        if let Some(err) = &self.autostart_err {
                            ui.label(RichText::new(format!("Autostart error: {err}")).color(Color32::from_rgb(240, 90, 80)).small());
                        }
                        if changed {
                            self.send_prefs();
                        }
                        ui.separator();
                        ui.label(RichText::new("Predictive Cooling:").strong());
                        ui.horizontal(|ui| {
                            use crate::config::PredictiveMode;
                            for (m, lbl) in [
                                (PredictiveMode::Off, "Off"),
                                (PredictiveMode::Silent, "Silent"),
                                (PredictiveMode::Balanced, "Balanced"),
                                (PredictiveMode::Aggressive, "Aggressive"),
                            ] {
                                if ui.selectable_label(snap.predictive.mode == m, lbl).clicked() {
                                    let _ = self.worker.tx.send(Command::SetPredictiveMode(m));
                                }
                            }
                        });
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("Refresh (active window):");
                            for ms in [1000u64, 1500, 2000, 5000] {
                                if ui.selectable_label(self.poll_ms == ms, format!("{:.1}s", ms as f64 / 1000.0)).clicked() {
                                    self.poll_ms = ms;
                                    let _ = self.worker.tx.send(Command::SetPoll(ms));
                                }
                            }
                        });
                        ui.label(RichText::new("Background mode: 2.0s, polling only active fan/CPU sensors").weak().small());
                    });
                    if self.tray.is_some() && ui.button("Hide to tray").on_hover_text("Keeps running in the background; click the tray icon to come back").clicked() {
                        self.win.hide();
                    }
                    let smart_btn = egui::Button::new(RichText::new("Smart Setup").strong())
                        .fill(Color32::from_rgb(35, 75, 115))
                        .min_size(egui::vec2(95.0, 24.0));
                    if ui.add_enabled(snap.ready && snap.error.is_none(), smart_btn)
                        .on_hover_text("1-Click Smart Auto-Setup:\nAutomatically selects optimal CPU & GPU sensors, sets quiet acoustic fan curves (62°C -> 93°C), and enables Balanced Predictive Cooling.")
                        .clicked()
                    {
                        let _ = self.worker.tx.send(Command::SmartSetup);
                    }
                    self.full_blast_button(ui, &snap);
                });
            });
            if let Some(msg) = &snap.smart_setup_msg {
                ui.add_space(3.0);
                egui::Frame::group(ui.style())
                    .fill(Color32::from_rgb(20, 60, 40))
                    .stroke(egui::Stroke::new(1.0, Color32::from_rgb(60, 180, 100)))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("[OK]").strong().color(Color32::from_rgb(100, 255, 150)));
                            ui.label(RichText::new(msg).color(Color32::WHITE).size(13.0));
                        });
                    });
            }
        });

        egui::Panel::bottom("status").show(root, |ui| {
            ui.horizontal(|ui| {
                if snap.ready && snap.error.is_none() {
                    ui.label(RichText::new(format!("{} SMC keys | {} sensors", snap.key_total, snap.sensors.len())).weak());
                }
                if !snap.transport.is_empty() {
                    ui.label(RichText::new(&snap.transport).weak());
                }
                for w in &snap.warnings {
                    ui.label(RichText::new(w).color(Color32::from_rgb(255, 170, 60)));
                }
            });
        });

        egui::CentralPanel::default().show(root, |ui| {
            if !snap.ready {
                ui.centered_and_justified(|ui| ui.spinner());
                return;
            }
            if let Some(err) = &snap.error {
                ui.heading("Cannot access the SMC");
                ui.add_space(6.0);
                ui.label(RichText::new(err).color(Color32::from_rgb(240, 90, 80)).monospace());
                ui.add_space(10.0);
                ui.label("This app needs a port I/O driver. Put inpoutx64.dll (recommended, supports T2 MMIO) or WinRing0x64.dll + WinRing0x64.sys next to wmf-control.exe and run it as administrator.");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Retry").clicked() {
                        self.restart(false);
                    }
                    if ui.button("Start demo mode").clicked() {
                        self.restart(true);
                    }
                });
                return;
            }
            ui.columns(2, |cols| {
                cols[0].set_max_width(cols[0].available_width());
                cols[1].set_max_width(cols[1].available_width());
                self.fans_ui(&mut cols[0], &snap);
                self.sensors_ui(&mut cols[1], &snap);
            });
        });

        self.editor_ui(ctx, &snap);
        if self.demo {
            egui::Area::new(egui::Id::new("demo")).anchor(egui::Align2::RIGHT_BOTTOM, [-8.0, -28.0]).show(ctx, |ui| {
                ui.label(RichText::new("DEMO - simulated SMC").color(Color32::from_rgb(255, 120, 200)));
            });
        }
    }
}
