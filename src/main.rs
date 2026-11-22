//! WMF Control - Windows Mac Fan Control
//! Fan control and sensor monitor for Intel Macs running Windows (Boot Camp).
//! License: MIT

#![windows_subsystem = "windows"]

mod app;

pub const APP_NAME: &str = "WMF Control";
pub const APP_FULL_NAME: &str = "Windows Mac Fan Control (T2 Edition)";
pub const REPO_URL: &str = "https://github.com/Gleb200/WMF-Control-T2-Edition";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
mod config;
mod control;
mod io;
mod predictive;
mod sensors;
mod shmem;
mod sim;
mod smc;
mod tray;

use eframe::egui;

#[cfg(windows)]
fn kill_other_instances() {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, TerminateProcess, PROCESS_TERMINATE,
    };
    use windows_sys::Win32::Foundation::CloseHandle;

    unsafe {
        let current_pid = GetCurrentProcessId();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() {
            return;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID != current_pid {
                    let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
                    if name == "wmf-control.exe" {
                        let h = OpenProcess(PROCESS_TERMINATE, 0, entry.th32ProcessID);
                        if !h.is_null() {
                            TerminateProcess(h, 0);
                            CloseHandle(h);
                        }
                    }
                }
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
}

fn main() -> eframe::Result<()> {
    #[cfg(windows)]
    kill_other_instances();

    let args: Vec<String> = std::env::args().collect();
    let demo = args.iter().any(|a| a == "--demo");
    let no_mmio = args.iter().any(|a| a == "--no-mmio");
    let start_minimized_arg = args.iter().any(|a| a == "--minimized" || a == "-minimized" || a == "/tray" || a == "--tray");
    let cfg = config::Config::load();
    let start_minimized = start_minimized_arg || cfg.start_minimized;

    if args.iter().any(|a| a == "--register-autostart") {
        let _ = app::set_autostart(true);
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest") {
        selftest(demo, no_mmio);
        return Ok(());
    }
    if args.iter().any(|a| a == "--dump") {
        dump(demo, no_mmio);
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("{APP_NAME} ({APP_FULL_NAME})"))
            .with_inner_size([900.0, 600.0])
            .with_min_inner_size([640.0, 400.0])
            .with_visible(!start_minimized)
            .with_active(!start_minimized)
            .with_icon(std::sync::Arc::new(egui::IconData { rgba: tray::icon_rgba(32), width: 32, height: 32 })),
        ..Default::default()
    };
    eframe::run_native(
        "WMF Control",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, demo, no_mmio, start_minimized)))),
    )
}

pub fn open_smc(demo: bool, no_mmio: bool) -> anyhow::Result<smc::Smc> {
    if demo {
        return Ok(smc::Smc::simulated());
    }
    let io = io::open_any()?;
    smc::Smc::open(io, !no_mmio)
}

/// `wmf-control.exe --selftest` – raise fan 0 minimum by 300 RPM, read back, restore.
fn selftest(demo: bool, no_mmio: bool) {
    let out = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("selftest.txt")))
        .unwrap_or_else(|| "selftest.txt".into());
    let mut log = String::new();
    match open_smc(demo, no_mmio) {
        Err(e) => log += &format!("open error: {e:#}\n"),
        Ok(mut s) => {
            let k = smc::key("F0Mn");
            let orig = s.read_f64(&k).unwrap_or(0.0);
            log += &format!("F0Mn original = {orig}\n");
            let want = orig + 300.0;
            log += &format!("write {want}: {:?}\n", s.write_f64(&k, want).err().map(|e| e.to_string()));
            std::thread::sleep(std::time::Duration::from_millis(300));
            log += &format!("read back = {:?}\n", s.read_f64(&k));
            log += &format!("restore {orig}: {:?}\n", s.write_f64(&k, orig).err().map(|e| e.to_string()));
            log += &format!("final = {:?}\n", s.read_f64(&k));

            // forced mode: target = max (no loss of cooling), then back to auto
            let (md, tg, mx) = (smc::key("F0Md"), smc::key("F0Tg"), smc::key("F0Mx"));
            let max = s.read_f64(&mx).unwrap_or(0.0);
            log += &format!("Md before = {:?}, Tg before = {:?}, max = {max}\n", s.read_f64(&md), s.read_f64(&tg));
            log += &format!("write Md=1: {:?}\n", s.write_f64(&md, 1.0).err().map(|e| e.to_string()));
            log += &format!("write Tg={max}: {:?}\n", s.write_f64(&tg, max).err().map(|e| e.to_string()));
            std::thread::sleep(std::time::Duration::from_millis(500));
            log += &format!("Md = {:?}, Tg = {:?}, Ac = {:?}\n", s.read_f64(&md), s.read_f64(&tg), s.read_f64(&smc::key("F0Ac")));
            log += &format!("write Md=0: {:?}\n", s.write_f64(&md, 0.0).err().map(|e| e.to_string()));
            std::thread::sleep(std::time::Duration::from_millis(500));
            log += &format!("Md after = {:?}, Tg after = {:?}\n", s.read_f64(&md), s.read_f64(&tg));
        }
    }
    let _ = std::fs::write(out, log);
}

/// `wmf-control.exe --dump` – print every SMC key to a text file next to the exe (diagnostics).
fn dump(demo: bool, no_mmio: bool) {
    let out = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("smc-dump.txt")))
        .unwrap_or_else(|| "smc-dump.txt".into());
    let text = match open_smc(demo, no_mmio) {
        Err(e) => format!("error: {e:#}\n"),
        Ok(mut s) => {
            let mut t = format!("transport: {}\n", s.transport_name());
            for k in s.all_keys() {
                let ks = smc::key_str(&k);
                match s.read_raw(&k) {
                    Ok((info, data)) => {
                        let hex: Vec<String> = data.iter().map(|b| format!("{b:02x}")).collect();
                        let v = smc::decode(&info, &data).map(|v| format!("{v:.3}")).unwrap_or_default();
                        t += &format!("{ks}  [{}] len={} {:<24} {v}\n", info.type_str(), info.len, hex.join(" "));
                    }
                    Err(e) => t += &format!("{ks}  <{e}>\n"),
                }
            }
            t
        }
    };
    let _ = std::fs::write(&out, text);
}
