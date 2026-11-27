//! Windows shell: tray icon, overlay, settings, speech. Everything UI runs on the main thread;
//! the listening pipeline posts events to it.

mod overlay;
mod settings;
mod tts;

use crate::config::Config;
use crate::engine::{self, Ui, UiEvent};
use anyhow::Result;
use overlay::{ANIM_TIMER, Overlay, Tone};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, WPARAM};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey,
    RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostMessageW, PostQuitMessage, TranslateMessage, WM_APP,
    WM_TIMER,
};
use windows::core::{PCWSTR, w};

const WM_VEXA_EVENT: u32 = WM_APP + 1;
const WM_VEXA_TTS_DONE: u32 = WM_APP + 2;

/// Posts pipeline events to the UI thread.
struct WinUi {
    tx: Mutex<Sender<UiEvent>>,
    hwnd: isize,
}

impl Ui for WinUi {
    fn send(&self, ev: UiEvent) {
        if self.tx.lock().map(|tx| tx.send(ev)).is_ok() {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.hwnd as *mut _)),
                    WM_VEXA_EVENT,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

pub fn run() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // One Vexa per user session.
        // The handle is intentionally never closed: it lives as long as the process.
        let _mutex = CreateMutexW(None, true, w!("Local\\VexaSingleInstance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            message_box(
                "Vexa is already running.\n\nLook for its icon in the system tray: click the ^ arrow \
                 next to the clock. Right-click the icon for settings or to quit.",
            );
            return Ok(());
        }
    }

    let mut cfg = Config::load();
    let mut overlay = Overlay::create()?;
    let paused = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(engine::Stats::default());
    let hwnd_raw = overlay.hwnd.0 as isize;

    let (tx, rx) = channel::<UiEvent>();
    let ui: Arc<dyn Ui> = Arc::new(WinUi {
        tx: Mutex::new(tx),
        hwnd: hwnd_raw,
    });
    let tts = tts::Tts::start(move || unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd_raw as *mut _)),
            WM_VEXA_TTS_DONE,
            WPARAM(0),
            LPARAM(0),
        );
    });

    // Tray
    let status = MenuItem::new("Vexa: starting…", false, None);
    let pause = MenuItem::new("Pause listening", true, None);
    let settings_item = MenuItem::new("Settings…", true, None);
    let autostart_item = CheckMenuItem::new("Start with Windows", true, autostart::enabled(), None);
    let quit = MenuItem::new("Quit Vexa", true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &status,
        &PredefinedMenuItem::separator(),
        &pause,
        &settings_item,
        &autostart_item,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Vexa")
        .with_icon(tray_icon_image(true))
        .build()?;

    // Pipeline (loads the model, which may download on first run)
    {
        let ui = ui.clone();
        let paused = paused.clone();
        let cfg = cfg.clone();
        let stats = stats.clone();
        std::thread::spawn(move || {
            if let Err(e) = engine::start(cfg, ui.clone(), paused, stats) {
                ui.send(UiEvent::Error(format!(
                    "Vexa could not start listening: {e}"
                )));
            }
        });
    }

    if cfg.always_show {
        overlay.keep_panel(stats.clone(), paused.clone());
    } else {
        overlay.show_card(
            "Vexa",
            "Starting…",
            Tone::Normal,
            Some(Duration::from_secs(30)),
            false,
        );
    }
    if crate::secrets::load_api_key().is_none() {
        settings::open(overlay.hwnd);
    }

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if settings::pre_translate(&msg) {
                continue;
            }
            if msg.hwnd == overlay.hwnd {
                match msg.message {
                    WM_TIMER if msg.wParam.0 == ANIM_TIMER => overlay.tick(),
                    WM_VEXA_EVENT => {
                        for ev in rx.try_iter() {
                            handle(ev, &mut overlay, &tts, &cfg, &status);
                        }
                    }
                    WM_VEXA_TTS_DONE => overlay.speech_done(Duration::from_secs(3)),
                    settings::WM_SETTINGS_SAVED => cfg = Config::load(),
                    _ => {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            } else {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                if ev.id == quit.id() {
                    tts.stop();
                    PostQuitMessage(0);
                } else if ev.id == settings_item.id() {
                    settings::open(overlay.hwnd);
                } else if ev.id == autostart_item.id() {
                    let on = autostart_item.is_checked();
                    if autostart::set(on).is_err() {
                        autostart_item.set_checked(!on);
                    }
                } else if ev.id == pause.id() {
                    let now_paused = !paused.load(Ordering::Relaxed);
                    paused.store(now_paused, Ordering::Relaxed);
                    pause.set_text(if now_paused {
                        "Resume listening"
                    } else {
                        "Pause listening"
                    });
                    status.set_text(if now_paused {
                        "Vexa: paused"
                    } else {
                        "Vexa: listening"
                    });
                    let _ = tray.set_icon(Some(tray_icon_image(!now_paused)));
                    let _ =
                        tray.set_tooltip(Some(if now_paused { "Vexa (paused)" } else { "Vexa" }));
                    if now_paused {
                        tts.stop();
                        overlay.hide();
                    }
                }
            }
        }
    }
    Ok(())
}

fn handle(ev: UiEvent, overlay: &mut Overlay, tts: &tts::Tts, cfg: &Config, status: &MenuItem) {
    match ev {
        UiEvent::Status(s) => {
            status.set_text(format!("Vexa: {s}"));
            overlay.show_card(
                "Vexa",
                &s,
                Tone::Normal,
                Some(Duration::from_secs(30)),
                false,
            );
        }
        UiEvent::Ready => {
            status.set_text("Vexa: listening");
            overlay.show_card(
                "Vexa is listening",
                "Say \"Vexa\" with a question, any time. Nothing you say is stored.",
                Tone::Normal,
                Some(Duration::from_secs(4)),
                false,
            );
        }
        UiEvent::Listening => overlay.show_wave("Listening…", Some(Duration::from_secs(8))),
        UiEvent::Working { request } => {
            tts.stop();
            overlay.show_wave(&request, Some(Duration::from_secs(120)));
        }
        UiEvent::Answer { request, text } => {
            if cfg.speak_answers {
                tts.speak(&text);
                overlay.show_card(&request, &text, Tone::Normal, None, true);
            } else {
                let words = text.split_whitespace().count() as u64;
                let read = Duration::from_millis((words * 320).clamp(5_000, 20_000));
                overlay.show_card(&request, &text, Tone::Normal, Some(read), false);
            }
        }
        UiEvent::Error(e) => {
            overlay.show_card("Vexa", &e, Tone::Error, Some(Duration::from_secs(7)), false)
        }
        UiEvent::Stop => {
            tts.stop();
            overlay.hide();
        }
        UiEvent::Heard { text, addressed } => overlay.set_heard(&text, addressed),
    }
}

/// Show a simple message dialog (used where there is no console to print to).
pub fn message_box(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MessageBoxW,
    };
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(wide.as_ptr()),
            w!("Vexa"),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
        );
    }
}

/// With VEXA_DEBUG set, print diagnostics to the terminal Vexa was started from.
pub fn attach_console_if_debugging() {
    if engine::debug_enabled() {
        attach_console();
    }
}

/// Send output to the terminal Vexa was started from, if any.
pub fn attach_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Draw the tray icon: a violet disc with three wave bars (grey when paused).
fn tray_icon_image(active: bool) -> Icon {
    use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Transform};
    const S: u32 = 32;
    let mut pm = Pixmap::new(S, S).expect("icon pixmap");
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(if active {
        Color::from_rgba8(124, 92, 246, 255)
    } else {
        Color::from_rgba8(120, 120, 130, 255)
    });
    if let Some(c) = PathBuilder::from_circle(16.0, 16.0, 15.0) {
        pm.fill_path(&c, &paint, FillRule::Winding, Transform::identity(), None);
    }
    paint.set_color(Color::WHITE);
    for (x, h) in [(9.0, 8.0), (14.0, 16.0), (19.0, 11.0)] {
        if let Some(r) = tiny_skia::Rect::from_xywh(x, 16.0 - h / 2.0, 4.0, h) {
            pm.fill_rect(r, &paint, Transform::identity(), None);
        }
    }
    let rgba: Vec<u8> = pm
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Icon::from_rgba(rgba, S, S).expect("icon")
}

/// "Start with Windows" via the per-user Run key (no admin rights needed).
mod autostart {
    use super::*;

    const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    const VALUE: PCWSTR = w!("Vexa");

    pub fn enabled() -> bool {
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE,
                RRF_RT_REG_SZ,
                None,
                None,
                None,
            )
            .is_ok()
        }
    }

    pub fn set(on: bool) -> windows::core::Result<()> {
        unsafe {
            let mut key = HKEY::default();
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                None,
                KEY_SET_VALUE | KEY_READ,
                &mut key,
            )
            .ok()?;
            let result = if on {
                let exe =
                    std::env::current_exe().map_err(|_| windows::core::Error::from_thread())?;
                let cmd = format!("\"{}\"", exe.display());
                let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
                RegSetValueExW(key, VALUE, None, REG_SZ, Some(bytes)).ok()
            } else {
                RegDeleteValueW(key, VALUE).ok()
            };
            let _ = RegCloseKey(key);
            result
        }
    }
}
