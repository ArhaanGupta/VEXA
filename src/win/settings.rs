//! A small native settings window: API key and "speak answers" toggle.

use std::cell::Cell;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_WINDOW, CreateFontW, DEFAULT_CHARSET,
    FF_DONTCARE, FW_NORMAL, HBRUSH, HFONT, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CW_USEDEFAULT,
    CreateWindowExW, DefWindowProcW, DestroyWindow, ES_AUTOHSCROLL, ES_PASSWORD, GetSystemMetrics,
    GetWindowTextLengthW, GetWindowTextW, HMENU, IsDialogMessageW, MSG, PostMessageW,
    RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, SendMessageW, SetForegroundWindow,
    ShowWindow, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_SETFONT,
    WNDCLASSW, WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_OVERLAPPED,
    WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

const ID_KEY: i32 = 101;
const ID_SPEAK: i32 = 102;
const ID_SAVE: i32 = 103;
const ID_CANCEL: i32 = 104;

thread_local! {
    static WINDOW: Cell<isize> = const { Cell::new(0) };
    static NOTIFY: Cell<isize> = const { Cell::new(0) };
}

/// Message posted to the overlay window when settings were saved.
pub const WM_SETTINGS_SAVED: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 30;

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

/// Let Tab/Enter work inside the settings window. Returns true if the message was handled.
pub fn pre_translate(msg: &MSG) -> bool {
    let raw = WINDOW.with(Cell::get);
    raw != 0 && unsafe { IsDialogMessageW(hwnd_of(raw), msg).as_bool() }
}

pub fn open(notify: HWND) {
    let existing = WINDOW.with(Cell::get);
    if existing != 0 {
        unsafe {
            let _ = SetForegroundWindow(hwnd_of(existing));
        }
        return;
    }
    NOTIFY.with(|n| n.set(notify.0 as isize));
    let cfg = crate::config::Config::load();
    let has_key = crate::secrets::load_api_key().is_some();
    unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else {
            return;
        };
        let class = w!("VexaSettings");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as isize as *mut _),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let (w, h) = (460, 250);
        let x = (GetSystemMetrics(SM_CXSCREEN) - w) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - h) / 2;
        let Ok(win) = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class,
            w!("Vexa settings"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            if x > 0 { x } else { CW_USEDEFAULT },
            if y > 0 { y } else { CW_USEDEFAULT },
            w,
            h,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) else {
            return;
        };
        WINDOW.with(|c| c.set(win.0 as isize));

        let font: HFONT = CreateFontW(
            -15,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            FF_DONTCARE.0 as u32,
            w!("Segoe UI"),
        );
        let child = |ex: WINDOW_EX_STYLE,
                     class: PCWSTR,
                     text: PCWSTR,
                     style: WINDOW_STYLE,
                     x,
                     y,
                     w,
                     h,
                     id: i32| {
            let c = CreateWindowExW(
                ex,
                class,
                text,
                WS_CHILD | WS_VISIBLE | style,
                x,
                y,
                w,
                h,
                Some(win),
                Some(HMENU(id as isize as *mut _)),
                Some(hinstance.into()),
                None,
            )
            .unwrap_or_default();
            SendMessageW(
                c,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            );
            c
        };

        let label = if has_key {
            w!("Anthropic API key (saved; paste a new one to replace it):")
        } else {
            w!("Anthropic API key (get one at console.anthropic.com):")
        };
        child(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            label,
            WINDOW_STYLE(0),
            20,
            18,
            410,
            22,
            0,
        );
        let edit = child(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            w!(""),
            WS_TABSTOP | WINDOW_STYLE((ES_PASSWORD | ES_AUTOHSCROLL) as u32),
            20,
            44,
            410,
            28,
            ID_KEY,
        );
        let speak = child(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Speak answers aloud"),
            WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            20,
            86,
            300,
            24,
            ID_SPEAK,
        );
        SendMessageW(
            speak,
            BM_SETCHECK,
            Some(WPARAM(usize::from(cfg.speak_answers))),
            None,
        );
        child(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Your key is stored in Windows Credential Manager. Audio is never saved."),
            WINDOW_STYLE(0),
            20,
            122,
            410,
            22,
            0,
        );
        child(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Save"),
            WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            230,
            160,
            96,
            32,
            ID_SAVE,
        );
        child(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Cancel"),
            WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            334,
            160,
            96,
            32,
            ID_CANCEL,
        );

        let _ = ShowWindow(win, SW_SHOW);
        let _ = SetForegroundWindow(win);
        let _ = SetFocus(Some(edit));
    }
}

fn read_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = (wp.0 & 0xFFFF) as i32;
                // IDOK (Enter) maps to Save, IDCANCEL (Esc) to Cancel.
                if id == ID_SAVE || id == 1 {
                    let key = read_text(GetDlgItem(Some(hwnd), ID_KEY).unwrap_or_default());
                    let speak = SendMessageW(
                        GetDlgItem(Some(hwnd), ID_SPEAK).unwrap_or_default(),
                        BM_GETCHECK,
                        None,
                        None,
                    )
                    .0 == 1;
                    let key = key.trim();
                    if !key.is_empty() {
                        let _ = crate::secrets::save_api_key(key);
                    }
                    let mut cfg = crate::config::Config::load();
                    cfg.speak_answers = speak;
                    let _ = cfg.save();
                    let notify = NOTIFY.with(Cell::get);
                    let _ = PostMessageW(
                        Some(hwnd_of(notify)),
                        WM_SETTINGS_SAVED,
                        WPARAM(0),
                        LPARAM(0),
                    );
                    let _ = DestroyWindow(hwnd);
                } else if id == ID_CANCEL || id == 2 {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                WINDOW.with(|c| c.set(0));
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
