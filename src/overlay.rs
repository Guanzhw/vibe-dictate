use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse, EndPaint,
    FillRect, FrameRect, GetMonitorInfoW, InvalidateRect, MonitorFromWindow, SelectObject,
    SetBkMode, SetTextColor, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DEFAULT_QUALITY, DRAW_TEXT_FORMAT, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    DT_WORDBREAK, FF_DONTCARE, FW_BOLD, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, GetDpiForWindow, SetThreadDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetForegroundWindow, GetSystemMetrics, LoadCursorW, PeekMessageW, PostQuitMessage,
    RegisterClassW, SetWindowPos, ShowWindow, TranslateMessage,
    CS_HREDRAW, CS_VREDRAW, HMENU, HWND_TOPMOST, IDC_ARROW,
    MSG, PM_REMOVE, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, WM_DESTROY, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_PAINT,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

const CARD_WIDTH: i32 = 480;
const CARD_HEIGHT: i32 = 120;
const HTTRANSPARENT: LRESULT = LRESULT(-1);
const MA_NOACTIVATE: LRESULT = LRESULT(3);

#[derive(Clone, Debug)]
enum OverlayState {
    Hidden,
    Recording {
        hotkey: String,
        elapsed_secs: u64,
        transcript: Option<String>,
    },
    Processing {
        transcript: Option<String>,
    },
}

enum Command {
    Set(OverlayState),
    Stop,
}

struct OverlayInner {
    tx: Sender<Command>,
}

impl Drop for OverlayInner {
    fn drop(&mut self) {
        if let Err(error) = self.tx.send(Command::Stop) {
            log::debug!("overlay worker already stopped: {error}");
        }
    }
}

/// A non-activating, topmost status card owned by a single UI thread.
/// Cloning this handle does not stop that thread; the worker stops only when
/// the last handle is dropped, so temporary call-site clones are harmless.
#[derive(Clone)]
pub struct Overlay {
    inner: Arc<OverlayInner>,
}

impl Overlay {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || run_overlay(rx));
        Self {
            inner: Arc::new(OverlayInner { tx }),
        }
    }

    /// Show immediate recording feedback. `transcript` is the newest partial
    /// from the streaming transport when one is available.
    pub fn show_recording(
        &self,
        hotkey: impl AsRef<str>,
        elapsed: Duration,
        transcript: Option<&str>,
    ) {
        self.set(OverlayState::Recording {
            hotkey: hotkey.as_ref().to_string(),
            elapsed_secs: elapsed.as_secs(),
            transcript: nonempty(transcript),
        });
    }

    /// Show processing feedback while waiting for the first or next model
    /// result. An empty partial intentionally renders a waiting message.
    pub fn show_processing(&self, transcript: Option<&str>) {
        self.set(OverlayState::Processing {
            transcript: nonempty(transcript),
        });
    }

    /// Convenience transition for a stream preview that has no live audio
    /// capture state of its own.
    pub fn show_partial(&self, transcript: &str) {
        self.show_processing(Some(transcript));
    }

    pub fn hide(&self) {
        self.set(OverlayState::Hidden);
    }

    fn set(&self, state: OverlayState) {
        if let Err(error) = self.inner.tx.send(Command::Set(state)) {
            log::warn!("overlay update failed: {error}");
        }
    }
}

fn nonempty(text: Option<&str>) -> Option<String> {
    text.filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

static OVERLAY_STATE: OnceLock<Mutex<OverlayState>> = OnceLock::new();

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let width = (client.right - client.left).max(CARD_WIDTH);
            let height = (client.bottom - client.top).max(CARD_HEIGHT);
            let scale = width as f32 / CARD_WIDTH as f32;
            let card = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };

            // COLORREF stores colors as 0x00BBGGRR.
            let background = CreateSolidBrush(COLORREF(0x00140d0a));
            FillRect(hdc, &card, background);
            let _ = DeleteObject(HGDIOBJ(background.0));
            let border = CreateSolidBrush(COLORREF(0x004f4038));
            FrameRect(hdc, &card, border);
            let _ = DeleteObject(HGDIOBJ(border.0));
            SetBkMode(hdc, TRANSPARENT);

            let state = OVERLAY_STATE
                .get_or_init(|| Mutex::new(OverlayState::Hidden))
                .lock()
                .unwrap()
                .clone();
            let (dot, title, detail, transcript) = match state {
                OverlayState::Hidden => (0x00808080, "", "".to_string(), None),
                OverlayState::Recording {
                    hotkey,
                    elapsed_secs,
                    transcript,
                } => (
                    0x003333ee,
                    "正在录音",
                    format!("{hotkey} · 松开结束 · {elapsed_secs}s"),
                    transcript,
                ),
                OverlayState::Processing { transcript } => {
                    let detail = if transcript.is_some() {
                        "实时转写中…"
                    } else {
                        "等待转写结果…"
                    };
                    (0x0000a5ff, "正在识别", detail.to_string(), transcript)
                }
            };

            if title.is_empty() {
                let _ = EndPaint(hwnd, &ps);
                return LRESULT(0);
            }

            let pad = scaled(20, scale);
            let dot_size = scaled(16, scale);
            let title_font = CreateFontW(
                -scaled(20, scale),
                0,
                0,
                0,
                FW_BOLD.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                DEFAULT_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Microsoft YaHei"),
            );
            let body_font = CreateFontW(
                -scaled(16, scale),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                DEFAULT_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Microsoft YaHei"),
            );
            let original_font = SelectObject(hdc, title_font);
            let dot_brush = CreateSolidBrush(COLORREF(dot));
            let old_brush = windows::Win32::Graphics::Gdi::SelectObject(hdc, dot_brush);
            let _ = Ellipse(
                hdc,
                pad,
                scaled(18, scale),
                pad + dot_size,
                scaled(18, scale) + dot_size,
            );
            let _ = windows::Win32::Graphics::Gdi::SelectObject(hdc, old_brush);
            let _ = DeleteObject(HGDIOBJ(dot_brush.0));

            SetTextColor(hdc, COLORREF(0x00ffffff));
            let mut title_utf16: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
            let mut title_rect = RECT {
                left: pad + dot_size + scaled(12, scale),
                top: scaled(10, scale),
                right: width - pad,
                bottom: scaled(38, scale),
            };
            DrawTextW(
                hdc,
                &mut title_utf16,
                &mut title_rect,
                DRAW_TEXT_FORMAT(DT_LEFT.0 | DT_VCENTER.0 | DT_SINGLELINE.0),
            );

            let _ = SelectObject(hdc, body_font);
            SetTextColor(hdc, COLORREF(0x00c7bdb6));
            let mut detail_utf16: Vec<u16> = detail.encode_utf16().chain(std::iter::once(0)).collect();
            let mut detail_rect = RECT {
                left: pad + dot_size + scaled(12, scale),
                top: scaled(34, scale),
                right: width - pad,
                bottom: scaled(54, scale),
            };
            DrawTextW(
                hdc,
                &mut detail_utf16,
                &mut detail_rect,
                DRAW_TEXT_FORMAT(DT_LEFT.0 | DT_VCENTER.0 | DT_SINGLELINE.0 | DT_END_ELLIPSIS.0),
            );

            let transcript_text = transcript.map(|text| tail_text(&text)).unwrap_or_else(|| {
                if title == "正在录音" {
                    "正在聆听，等待首个转写片段…".to_string()
                } else {
                    "已松开按键，等待模型返回…".to_string()
                }
            });
            SetTextColor(hdc, COLORREF(0x00f5f1ed));
            let mut transcript_utf16: Vec<u16> = transcript_text
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut transcript_rect = RECT {
                left: pad,
                top: scaled(62, scale),
                right: width - pad,
                bottom: height - scaled(14, scale),
            };
            DrawTextW(
                hdc,
                &mut transcript_utf16,
                &mut transcript_rect,
                DRAW_TEXT_FORMAT(DT_LEFT.0 | DT_WORDBREAK.0 | DT_END_ELLIPSIS.0),
            );
            let _ = SelectObject(hdc, original_font);
            let _ = DeleteObject(HGDIOBJ(title_font.0));
            let _ = DeleteObject(HGDIOBJ(body_font.0));
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE,
        WM_NCHITTEST => HTTRANSPARENT,
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn scaled(value: i32, scale: f32) -> i32 {
    ((value as f32 * scale).round() as i32).max(1)
}

fn tail_text(text: &str) -> String {
    const MAX_VISIBLE_CHARS: usize = 56;
    let count = text.chars().count();
    if count <= MAX_VISIBLE_CHARS {
        return text.to_string();
    }
    let tail: String = text
        .chars()
        .skip(count.saturating_sub(MAX_VISIBLE_CHARS - 1))
        .collect();
    format!("…{tail}")
}

fn card_size_for(hwnd: HWND) -> (i32, i32, f32) {
    let dpi = unsafe {
        let window_dpi = GetDpiForWindow(hwnd);
        if window_dpi == 0 {
            GetDpiForSystem()
        } else {
            window_dpi
        }
    };
    let scale = (dpi as f32 / 96.0).max(1.0);
    (scaled(CARD_WIDTH, scale), scaled(CARD_HEIGHT, scale), scale)
}

fn place_overlay(hwnd: HWND) -> (i32, i32) {
    unsafe {
        let foreground = GetForegroundWindow();
        let (width, height, scale) = card_size_for(foreground);
        let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let (left, top, right, bottom) = if !monitor.0.is_null()
            && GetMonitorInfoW(monitor, &mut info).as_bool()
        {
            (
                info.rcWork.left,
                info.rcWork.top,
                info.rcWork.right,
                info.rcWork.bottom,
            )
        } else {
            (0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
        };
        let x = left + ((right - left - width) / 2).max(0);
        let y = (bottom - height - scaled(28, scale)).max(top);
        if let Err(error) = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        ) {
            log::warn!("overlay SetWindowPos failed: {error}");
        }
        (width, height)
    }
}

fn run_overlay(rx: Receiver<Command>) {
    unsafe {
        // Keep this UI thread per-monitor aware even when the host process was
        // started by a launcher with a different process-wide DPI policy.
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hinst: HINSTANCE = match GetModuleHandleW(None) {
            Ok(module) => module.into(),
            Err(error) => {
                log::error!("overlay GetModuleHandleW failed: {error}");
                return;
            }
        };
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("VibeDictateOverlay"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let (width, height) = (scaled(CARD_WIDTH, 1.0), scaled(CARD_HEIGHT, 1.0));
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("VibeDictateOverlay"),
            w!("VibeVoice语音输入"),
            WS_POPUP,
            0,
            0,
            width,
            height,
            HWND::default(),
            HMENU::default(),
            hinst,
            None,
        );
        let hwnd = match hwnd {
            Ok(hwnd) => hwnd,
            Err(error) => {
                log::error!("overlay CreateWindowExW failed: {error}");
                return;
            }
        };
        let _ = ShowWindow(hwnd, SW_HIDE);
        loop {
            while let Ok(command) = rx.try_recv() {
                match command {
                    Command::Set(state) => {
                        let visible = !matches!(state, OverlayState::Hidden);
                        *OVERLAY_STATE
                            .get_or_init(|| Mutex::new(OverlayState::Hidden))
                            .lock()
                            .unwrap() = state;
                        let _ = InvalidateRect(hwnd, None, false);
                        if visible {
                            let _ = place_overlay(hwnd);
                        } else {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                        }
                    }
                    Command::Stop => {
                        if let Err(error) = DestroyWindow(hwnd) {
                            log::error!("overlay DestroyWindow failed: {error}");
                        }
                        return;
                    }
                }
            }
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_DESTROY {
                    return;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
