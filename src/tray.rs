// WMF Control (Windows Mac Fan Control) - by melinoe-ux (https://github.com/melinoe-ux) - MIT License
//! System tray icon + window hide/show (minimize to tray).
//!
//! While the window is hidden eframe stops painting frames, so everything the
//! tray menu does is handled directly in the event handlers (they run on the UI
//! thread's message loop) instead of waiting for the next egui frame.

use crate::config::Profile;
use crate::control::{Command, Worker};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Simple 32×32 fan icon drawn in code (no image files needed).
pub fn icon_rgba(size: u32) -> Vec<u8> {
    let mut px = vec![0u8; (size * size * 4) as usize];
    let c = (size as f32 - 1.0) / 2.0;
    let r_out = size as f32 / 2.0 - 0.5;
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            let r = (dx * dx + dy * dy).sqrt();
            let i = ((y * size + x) * 4) as usize;
            if r > r_out {
                continue;
            }
            // dark disc with a blue rim
            let (mut cr, mut cg, mut cb) = (30u8, 34u8, 42u8);
            if r > r_out - 2.2 {
                (cr, cg, cb) = (80, 160, 255);
            } else if r < r_out * 0.18 {
                (cr, cg, cb) = (200, 220, 255);
            } else {
                // three curved blades
                let a = dy.atan2(dx) + r / r_out * 1.2;
                let blade = (a * 3.0).rem_euclid(std::f32::consts::TAU);
                if blade < 2.2 {
                    (cr, cg, cb) = (110, 185, 255);
                }
            }
            px[i..i + 4].copy_from_slice(&[cr, cg, cb, 255]);
        }
    }
    px
}

pub struct Tray {
    pub icon: TrayIcon,
    pub mode_auto: CheckMenuItem,
    pub mode_custom: CheckMenuItem,
    pub mode_silent: CheckMenuItem,
    pub mode_blast: CheckMenuItem,
    last_tooltip: String,
}

pub struct WindowCtl {
    pub hwnd: AtomicIsize,
    pub hidden: Arc<AtomicBool>,
    /// "Minimize to system tray" preference.
    pub minimize_to_tray: AtomicBool,
}

impl WindowCtl {
    pub fn new(minimize_to_tray: bool, start_hidden: bool) -> Arc<Self> {
        let w = Arc::new(WindowCtl {
            hwnd: AtomicIsize::new(0),
            hidden: Arc::new(AtomicBool::new(start_hidden)),
            minimize_to_tray: AtomicBool::new(minimize_to_tray),
        });
        // A minimized window gets no egui frames, so watch the native state instead.
        let watch = w.clone();
        std::thread::Builder::new()
            .name("tray-watch".into())
            .spawn(move || {
                use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    let h = watch.hwnd.load(Ordering::Relaxed);
                    if h == 0 || !watch.minimize_to_tray.load(Ordering::Relaxed) {
                        continue;
                    }
                    let minimized = unsafe { IsIconic(h as _) != 0 && IsWindowVisible(h as _) != 0 };
                    if minimized {
                        watch.hide();
                    }
                }
            })
            .ok();
        w
    }

    pub fn hide(&self) {
        let h = self.hwnd.load(Ordering::Relaxed);
        if h != 0 {
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(h as _, windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE) };
        }
        self.hidden.store(true, Ordering::Relaxed);
    }

    pub fn show(&self, ctx: &eframe::egui::Context) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow};
        let h = self.hwnd.load(Ordering::Relaxed);
        if h != 0 {
            unsafe {
                ShowWindow(h as _, SW_SHOW);
                ShowWindow(h as _, SW_RESTORE);
                SetForegroundWindow(h as _);
            }
        }
        self.hidden.store(false, Ordering::Relaxed);
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
        ctx.request_repaint();
    }
}

impl Tray {
    pub fn new(worker: Arc<Mutex<Worker>>, win: Arc<WindowCtl>, ctx: eframe::egui::Context) -> anyhow::Result<Self> {
        let mode_auto = CheckMenuItem::new("Auto (Hardware)", true, false, None);
        let mode_custom = CheckMenuItem::new("Custom Curve", true, true, None);
        let mode_silent = CheckMenuItem::new("Silent", true, false, None);
        let mode_blast = CheckMenuItem::new("Full Blast", true, false, None);
        let sep1 = PredefinedMenuItem::separator();
        let show = MenuItem::new("Show WMF Control", true, None);
        let sep2 = PredefinedMenuItem::separator();
        let quit = MenuItem::new("Quit (fans back to Auto)", true, None);

        let menu = Menu::new();
        menu.append(&mode_auto)?;
        menu.append(&mode_custom)?;
        menu.append(&mode_silent)?;
        menu.append(&mode_blast)?;
        menu.append(&sep1)?;
        menu.append(&show)?;
        menu.append(&sep2)?;
        menu.append(&quit)?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("WMF Control")
            .with_icon(Icon::from_rgba(icon_rgba(32), 32, 32)?)
            .build()?;

        let auto_id = mode_auto.id().clone();
        let custom_id = mode_custom.id().clone();
        let silent_id = mode_silent.id().clone();
        let blast_id = mode_blast.id().clone();
        let show_id = show.id().clone();
        let quit_id = quit.id().clone();

        {
            let (worker, win, ctx) = (worker.clone(), win.clone(), ctx.clone());
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                if e.id == auto_id {
                    let w = worker.lock().unwrap().clone();
                    let _ = w.tx.send(Command::SetProfile(Profile::Auto));
                    ctx.request_repaint();
                } else if e.id == custom_id {
                    let w = worker.lock().unwrap().clone();
                    let _ = w.tx.send(Command::SetProfile(Profile::Custom));
                    ctx.request_repaint();
                } else if e.id == silent_id {
                    let w = worker.lock().unwrap().clone();
                    let _ = w.tx.send(Command::SetProfile(Profile::Silent));
                    ctx.request_repaint();
                } else if e.id == blast_id {
                    let w = worker.lock().unwrap().clone();
                    let _ = w.tx.send(Command::SetProfile(Profile::FullBlast));
                    ctx.request_repaint();
                } else if e.id == show_id {
                    win.show(&ctx);
                } else if e.id == quit_id {
                    // Fans are handed back to the SMC before the process exits.
                    let w = worker.lock().unwrap().clone();
                    w.shutdown();
                    std::process::exit(0);
                }
            }));
        }
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let restore = matches!(
                e,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
            );
            if restore {
                win.show(&ctx);
            }
        }));

        Ok(Tray {
            icon,
            mode_auto,
            mode_custom,
            mode_silent,
            mode_blast,
            last_tooltip: String::new(),
        })
    }

    pub fn set_tooltip(&mut self, text: String) {
        if text != self.last_tooltip {
            let _ = self.icon.set_tooltip(Some(&text));
            self.last_tooltip = text;
        }
    }
}
