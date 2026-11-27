//! The bottom-centre overlay: a small animated wave while Vexa is working, then an answer card.
//!
//! It is a per-pixel-alpha layered window that never takes focus and lets every click through,
//! so it cannot interrupt typing. It only redraws (30 fps) while visible; hidden, it costs nothing.

use crate::engine::Stats;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CALCRECT, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE,
    DT_WORDBREAK, DeleteDC, DeleteObject, DrawTextW, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, GetDC,
    GetMonitorInfoW, HBITMAP, HDC, HFONT, HGDIOBJ, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromWindow, OUT_DEFAULT_PRECIS, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetForegroundWindow, KillTimer, RegisterClassW, SW_HIDE,
    SW_SHOWNOACTIVATE, SetTimer, ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::w;

pub const ANIM_TIMER: usize = 1;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Hidden,
    /// Animated wave, optionally with one line of context ("Listening…", the request).
    Wave,
    /// Answer or message card with a calmer wave on top.
    Card,
    /// Always-on status panel: live mic level, pipeline state and the last thing heard.
    Idle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Error,
}

struct TextMask {
    w: i32,
    h: i32,
    alpha: Vec<u8>,
}

pub struct Overlay {
    pub hwnd: HWND,
    mode: Mode,
    tone: Tone,
    line: String,
    body: String,
    started: Instant,
    hide_at: Option<Instant>,
    fade_from: Option<Instant>,
    lively: bool,
    scale: f32,
    work: RECT,
    line_mask: Option<TextMask>,
    body_mask: Option<TextMask>,
    /// When set, the overlay falls back to the status panel instead of hiding.
    panel: Option<Panel>,
}

struct Panel {
    stats: Arc<Stats>,
    paused: Arc<AtomicBool>,
    heard: String,
    addressed: bool,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

impl Overlay {
    pub fn create() -> windows::core::Result<Self> {
        unsafe {
            let hinstance = GetModuleHandleW(None)?;
            let class = w!("VexaOverlay");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                class,
                w!("Vexa"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinstance.into()),
                None,
            )?;
            Ok(Self {
                hwnd,
                mode: Mode::Hidden,
                tone: Tone::Normal,
                line: String::new(),
                body: String::new(),
                started: Instant::now(),
                hide_at: None,
                fade_from: None,
                lively: false,
                scale: 1.0,
                work: RECT::default(),
                line_mask: None,
                body_mask: None,
                panel: None,
            })
        }
    }

    /// Show the animated wave. `line` is optional context under it.
    pub fn show_wave(&mut self, line: &str, hide_after: Option<Duration>) {
        self.set(Mode::Wave, Tone::Normal, line, "", hide_after, true);
    }

    /// Show a card with a dim first line and the main text.
    pub fn show_card(
        &mut self,
        line: &str,
        body: &str,
        tone: Tone,
        hide_after: Option<Duration>,
        speaking: bool,
    ) {
        self.set(Mode::Card, tone, line, body, hide_after, speaking);
    }

    /// Speech finished: keep the card up a little longer so it can be read, then fade.
    pub fn speech_done(&mut self, linger: Duration) {
        self.lively = false;
        if self.mode != Mode::Hidden {
            self.hide_at = Some(Instant::now() + linger);
        }
    }

    /// Keep the status panel on screen whenever nothing else is showing.
    pub fn keep_panel(&mut self, stats: Arc<Stats>, paused: Arc<AtomicBool>) {
        self.panel = Some(Panel {
            stats,
            paused,
            heard: String::new(),
            addressed: false,
        });
        self.show_panel();
    }

    /// Update the "last heard" line of the status panel.
    pub fn set_heard(&mut self, text: &str, addressed: bool) {
        if let Some(p) = &mut self.panel {
            p.heard = text.to_string();
            p.addressed = addressed;
            if self.mode == Mode::Idle {
                self.show_panel();
            }
        }
    }

    /// Status line, body text and tone for the status panel.
    fn panel_texts(&self) -> (String, String, Tone) {
        let Some(p) = &self.panel else {
            return (String::new(), String::new(), Tone::Normal);
        };
        if p.paused.load(Ordering::Relaxed) {
            return ("Vexa · Paused".into(), String::new(), Tone::Normal);
        }
        if let Some(problem) = p.stats.mic_problem() {
            let line = format!("Vexa · Not hearing anything ({})", p.stats.mic_name());
            return (line, problem, Tone::Error);
        }
        let state = if !p.stats.ready.load(Ordering::Relaxed) {
            "Starting…".to_string()
        } else if p.stats.transcribing.load(Ordering::Relaxed) {
            "Transcribing…".to_string()
        } else if p.stats.in_speech.load(Ordering::Relaxed) {
            "Hearing you…".to_string()
        } else if p.stats.follow_up_open() {
            "Listening for a follow-up (no need to say \"Vexa\")".to_string()
        } else {
            format!("Listening on {}", p.stats.mic_name())
        };
        let body = match (p.heard.is_empty(), p.addressed) {
            (true, _) => "Say \"Vexa\" and a question.".to_string(),
            (false, true) => format!("Heard (for Vexa): \u{201c}{}\u{201d}", p.heard),
            (false, false) => format!("Heard: \u{201c}{}\u{201d}", p.heard),
        };
        (format!("Vexa · {state}"), body, Tone::Normal)
    }

    fn show_panel(&mut self) {
        if self.panel.is_none() {
            return;
        }
        let (line, body, tone) = self.panel_texts();
        self.set(Mode::Idle, tone, &line, &body, None, false);
    }

    pub fn hide(&mut self) {
        if self.panel.is_some() {
            self.show_panel();
            return;
        }
        self.mode = Mode::Hidden;
        self.hide_at = None;
        self.fade_from = None;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), ANIM_TIMER);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn set(
        &mut self,
        mode: Mode,
        tone: Tone,
        line: &str,
        body: &str,
        hide_after: Option<Duration>,
        lively: bool,
    ) {
        let was_hidden = self.mode == Mode::Hidden;
        if was_hidden {
            self.place_on_active_monitor();
            self.started = Instant::now();
        }
        self.mode = mode;
        self.tone = tone;
        self.lively = lively;
        self.fade_from = None;
        self.hide_at = hide_after.map(|d| Instant::now() + d);
        if self.line != line || self.body != body || was_hidden {
            self.line = line.to_string();
            self.body = body.to_string();
            self.layout_text();
        }
        self.render();
        if was_hidden {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                SetTimer(Some(self.hwnd), ANIM_TIMER, 33, None);
            }
        }
    }

    /// Called on every animation timer tick.
    pub fn tick(&mut self) {
        if self.mode == Mode::Hidden {
            return;
        }
        let now = Instant::now();
        if self.fade_from.is_none() && self.hide_at.is_some_and(|t| now >= t) {
            self.fade_from = Some(now);
        }
        if let Some(f) = self.fade_from
            && now.duration_since(f) >= Duration::from_millis(250)
        {
            self.hide();
            return;
        }
        if self.mode == Mode::Idle {
            let (line, body, tone) = self.panel_texts();
            if line != self.line || body != self.body {
                self.line = line;
                self.body = body;
                self.layout_text();
            }
            self.tone = tone;
        }
        self.render();
    }

    fn place_on_active_monitor(&mut self) {
        unsafe {
            let mon = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(mon, &mut info);
            self.work = info.rcWork;
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            self.scale = dx as f32 / 96.0;
        }
    }

    fn px(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }

    fn layout_text(&mut self) {
        let card_w = self.px(460.0);
        let pad = self.px(18.0);
        let text_w = card_w - pad * 2;
        self.line_mask = (!self.line.is_empty()).then(|| {
            let max_w = if self.mode == Mode::Wave {
                self.px(320.0)
            } else {
                text_w
            };
            render_text(&self.line, self.px(12.5), false, max_w, true)
        });
        self.body_mask = (!self.body.is_empty())
            .then(|| render_text(&self.body, self.px(15.5), true, text_w, false));
    }

    fn size(&self) -> (i32, i32) {
        let pad = self.px(18.0);
        let wave_h = self.px(26.0);
        match self.mode {
            Mode::Hidden => (1, 1),
            Mode::Wave => {
                let line_w = self.line_mask.as_ref().map_or(0, |m| m.w + self.px(14.0));
                (self.px(96.0) + line_w + pad, self.px(48.0))
            }
            Mode::Card | Mode::Idle => {
                let mut h = pad + wave_h;
                if let Some(m) = &self.line_mask {
                    h += self.px(8.0) + m.h;
                }
                if let Some(m) = &self.body_mask {
                    h += self.px(6.0) + m.h;
                }
                (self.px(460.0), h + pad)
            }
        }
    }

    fn render(&mut self) {
        let (w, h) = self.size();
        let Some(mut pm) = Pixmap::new(w as u32, h as u32) else {
            return;
        };
        let t = self.started.elapsed().as_secs_f32();
        let s = self.scale;

        // Background
        let radius = if self.mode == Mode::Wave {
            h as f32 / 2.0
        } else {
            18.0 * s
        };
        if let Some(path) = rounded_rect(0.5, 0.5, w as f32 - 1.0, h as f32 - 1.0, radius) {
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(16, 16, 22, 236));
            paint.anti_alias = true;
            pm.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
            let mut border = Paint::default();
            border.set_color(match self.tone {
                Tone::Normal => Color::from_rgba8(150, 120, 255, 70),
                Tone::Error => Color::from_rgba8(255, 110, 110, 110),
            });
            border.anti_alias = true;
            pm.stroke_path(
                &path,
                &border,
                &Stroke {
                    width: s.max(1.0),
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
        }

        // Wave
        let (wave_x, wave_cy, wave_w) = match self.mode {
            Mode::Wave => (self.px(20.0) as f32, h as f32 / 2.0, self.px(64.0) as f32),
            _ => {
                let ww = self.px(90.0) as f32;
                (
                    (w as f32 - ww) / 2.0,
                    (self.px(18.0) + self.px(13.0)) as f32,
                    ww,
                )
            }
        };
        let bars = 13;
        let gap = wave_w / bars as f32;
        let bar_w = (gap * 0.55).max(2.0);
        let max_h = self.px(22.0) as f32;
        let speed = if self.lively { 7.0 } else { 2.2 };
        let mic = match (&self.panel, self.mode) {
            (Some(p), Mode::Idle) => Some(p.stats.level()),
            _ => None,
        };
        for i in 0..bars {
            let fi = i as f32;
            let centre =
                1.0 - ((fi - (bars as f32 - 1.0) / 2.0).abs() / (bars as f32 / 2.0)).powf(1.6);
            let wobble = ((t * speed + fi * 0.62).sin() * 0.5 + 0.5) * 0.7
                + ((t * speed * 0.47 + fi * 1.3).sin() * 0.5 + 0.5) * 0.3;
            let amp = if let Some(level) = mic {
                // Live microphone meter.
                0.12 + 0.88 * level * (0.55 + 0.45 * wobble)
            } else if self.lively {
                0.25 + 0.75 * wobble
            } else {
                0.3 + 0.25 * wobble
            };
            let bh = (max_h * centre.max(0.18) * amp).max(bar_w);
            let x = wave_x + fi * gap + (gap - bar_w) / 2.0;
            let mix = fi / (bars as f32 - 1.0);
            let (r, g, b) = match self.tone {
                Tone::Normal => (
                    lerp(139.0, 56.0, mix),
                    lerp(92.0, 189.0, mix),
                    lerp(246.0, 248.0, mix),
                ),
                Tone::Error => (255.0, lerp(90.0, 140.0, mix), lerp(90.0, 120.0, mix)),
            };
            if let Some(path) = rounded_rect(x, wave_cy - bh / 2.0, bar_w, bh, bar_w / 2.0) {
                let mut p = Paint::default();
                p.set_color(Color::from_rgba8(r as u8, g as u8, b as u8, 255));
                p.anti_alias = true;
                pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
            }
        }

        // Text
        match self.mode {
            Mode::Wave => {
                if let Some(m) = &self.line_mask {
                    blit_mask(&mut pm, m, self.px(96.0), (h - m.h) / 2, [205, 205, 220]);
                }
            }
            Mode::Card | Mode::Idle => {
                let pad = self.px(18.0);
                let mut y = pad + self.px(26.0);
                if let Some(m) = &self.line_mask {
                    y += self.px(8.0);
                    blit_mask(&mut pm, m, pad, y, [150, 150, 170]);
                    y += m.h;
                }
                if let Some(m) = &self.body_mask {
                    y += self.px(6.0);
                    blit_mask(&mut pm, m, pad, y, [245, 245, 250]);
                }
            }
            Mode::Hidden => {}
        }

        let alpha = match self.fade_from {
            Some(f) => (255.0 * (1.0 - f.elapsed().as_secs_f32() / 0.25)).clamp(0.0, 255.0) as u8,
            None => (255.0 * (self.started.elapsed().as_secs_f32() / 0.15).min(1.0)) as u8,
        };
        let x = self.work.left + ((self.work.right - self.work.left) - w) / 2;
        let y = self.work.bottom - h - self.px(28.0);
        present(self.hwnd, &pm, x, y, alpha);
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
    let _ = Rect::from_xywh(x, y, w, h)?;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

/// Alpha-blend a white-on-black text coverage mask onto the pixmap in the given colour.
fn blit_mask(pm: &mut Pixmap, m: &TextMask, ox: i32, oy: i32, rgb: [u8; 3]) {
    let pw = pm.width() as i32;
    let ph = pm.height() as i32;
    let data = pm.data_mut();
    for y in 0..m.h {
        let py = oy + y;
        if py < 0 || py >= ph {
            continue;
        }
        for x in 0..m.w {
            let px = ox + x;
            if px < 0 || px >= pw {
                continue;
            }
            let a = m.alpha[(y * m.w + x) as usize] as u32;
            if a == 0 {
                continue;
            }
            let i = ((py * pw + px) * 4) as usize;
            // Premultiplied "source over".
            for c in 0..3 {
                let src = rgb[c] as u32 * a / 255;
                data[i + c] = (src + data[i + c] as u32 * (255 - a) / 255) as u8;
            }
            data[i + 3] = (a + data[i + 3] as u32 * (255 - a) / 255) as u8;
        }
    }
}

/// Render text with GDI into a coverage mask. GDI text does not write alpha, so it is drawn white
/// on black and the brightness is used as coverage.
fn render_text(text: &str, px_height: i32, bold: bool, max_w: i32, single_line: bool) -> TextMask {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);
        let font: HFONT = CreateFontW(
            -px_height,
            0,
            0,
            0,
            if bold {
                FW_SEMIBOLD.0 as i32
            } else {
                FW_NORMAL.0 as i32
            },
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
        let old_font = SelectObject(dc, HGDIOBJ(font.0));
        let flags = if single_line {
            DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX
        } else {
            DT_WORDBREAK | DT_NOPREFIX
        };

        let mut rc = RECT {
            left: 0,
            top: 0,
            right: max_w,
            bottom: 0,
        };
        DrawTextW(dc, &mut wide, &mut rc, flags | DT_CALCRECT);
        let line_h = (px_height as f32 * 1.4) as i32;
        let w = if single_line {
            rc.right.min(max_w)
        } else {
            max_w
        }
        .max(1);
        // Cap long answers at 9 lines; the full answer is still spoken.
        let h = rc.bottom.clamp(1, line_h * 9);

        let (bmp, bits) = dib(dc, w, h);
        let old_bmp = SelectObject(dc, HGDIOBJ(bmp.0));
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00FF_FFFF));
        let mut draw_rc = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        DrawTextW(dc, &mut wide, &mut draw_rc, flags);

        let px = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
        let alpha = px.chunks_exact(4).map(|p| p[1]).collect();

        SelectObject(dc, old_bmp);
        SelectObject(dc, old_font);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = DeleteDC(dc);
        TextMask { w, h, alpha }
    }
}

/// A top-down 32-bit DIB section, zero-filled.
unsafe fn dib(dc: HDC, w: i32, h: i32) -> (HBITMAP, *mut core::ffi::c_void) {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bmp = unsafe { CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
        .unwrap_or_default();
    (bmp, bits)
}

/// Push the pixmap to the layered window.
fn present(hwnd: HWND, pm: &Pixmap, x: i32, y: i32, alpha: u8) {
    let (w, h) = (pm.width() as i32, pm.height() as i32);
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let (bmp, bits) = dib(dc, w, h);
        if bits.is_null() {
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            return;
        }
        // tiny-skia is premultiplied RGBA; Windows wants premultiplied BGRA.
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
        for (d, s) in dst.chunks_exact_mut(4).zip(pm.data().chunks_exact(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = s[3];
        }
        let old = SelectObject(dc, HGDIOBJ(bmp.0));
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let pos = POINT { x, y };
        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let _ = UpdateLayeredWindow(
            hwnd,
            Some(screen),
            Some(&pos),
            Some(&size),
            Some(dc),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
    }
}
