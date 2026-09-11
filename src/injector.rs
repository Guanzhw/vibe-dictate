use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use arboard::Clipboard;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_RETURN, VK_V,
};

pub fn clipboard_paste(text: &str) -> Result<()> {
    let mut clipboard = Clipboard::new().context("clipboard open")?;
    let previous = clipboard.get_text().ok();
    clipboard
        .set_text(text.to_string())
        .context("set clipboard")?;
    send_ctrl_v()?;
    // Give the target app a moment to consume the clipboard before restoring
    thread::sleep(Duration::from_millis(120));
    if let Some(prev) = previous {
        let _ = clipboard.set_text(prev);
    }
    Ok(())
}

/// Per-character SendInput with configurable pacing. Windows accepts a
/// big batched burst happily (`SendInput(&[42 events])` returns 42) but
/// many target apps — Electron/Chromium (Discord, Slack, VS Code), some
/// terminals, Notepad on slower machines — can't drain their message
/// queue fast enough and silently drop characters. Pacing via
/// `key_delay_ms` (sleep between chars) and `key_down_delay_ms` (hold
/// duration for each key) fixes this.
///
/// Spaces are the single most-dropped character in observed failures —
/// they don't visibly render so the user sees "foobar" instead of "foo
/// bar" and assumes a single glitch rather than a pattern. We sleep an
/// extra `key_delay_ms` after every U+0020 as a cheap insurance policy;
/// the added latency is imperceptible but dropped spaces are painful.
pub fn send_input_text(text: &str, key_delay_ms: u64, key_down_delay_ms: u64) -> Result<()> {
    let cbsize = std::mem::size_of::<INPUT>() as i32;
    let mut total_events: usize = 0;
    let mut sent_events: u32 = 0;
    let char_count = text.chars().count();

    for ch in text.encode_utf16() {
        // Send key-down and key-up separately so the caller can insert a
        // hold-duration between them. Most apps don't care, but a few
        // legacy targets filter zero-duration presses.
        let down = [make_unicode_input(ch, false)];
        total_events += 1;
        let n_down = unsafe { SendInput(&down, cbsize) };
        sent_events += n_down;
        if (n_down as usize) < down.len() {
            let err = unsafe { GetLastError() };
            log::warn!(
                "SendInput down partial on U+{:04X}: {}/{} (err {:?}) — \
                 focused window may block injected input (UIPI, admin, game)",
                ch,
                n_down,
                down.len(),
                err
            );
            return Err(anyhow::anyhow!(
                "SendInput key-down failed for U+{:04X}: {}/{} (err {:?})",
                ch,
                n_down,
                down.len(),
                err
            ));
        }
        if key_down_delay_ms > 0 {
            thread::sleep(Duration::from_millis(key_down_delay_ms));
        }

        let up = [make_unicode_input(ch, true)];
        total_events += 1;
        let n_up = unsafe { SendInput(&up, cbsize) };
        sent_events += n_up;
        if (n_up as usize) < up.len() {
            let err = unsafe { GetLastError() };
            log::warn!(
                "SendInput up partial on U+{:04X}: {}/{} (err {:?})",
                ch,
                n_up,
                up.len(),
                err
            );
            return Err(anyhow::anyhow!(
                "SendInput key-up failed for U+{:04X}: {}/{} (err {:?})",
                ch,
                n_up,
                up.len(),
                err
            ));
        }

        // Pace between characters. Spaces get an extra dose because
        // they're the character apps are most likely to drop silently —
        // see the docstring on why "foobar" vs "foo bar" is why this
        // branch exists.
        let post_char_delay = if ch == 0x0020 {
            key_delay_ms.saturating_mul(2)
        } else {
            key_delay_ms
        };
        if post_char_delay > 0 {
            thread::sleep(Duration::from_millis(post_char_delay));
        }
    }

    log::info!(
        "SendInput typed {} chars ({}/{} events delivered, key_delay={}ms, key_down_delay={}ms)",
        char_count,
        sent_events,
        total_events,
        key_delay_ms,
        key_down_delay_ms,
    );
    Ok(())
}

fn send_ctrl_v() -> Result<()> {
    let inputs = [
        make_vk_input(VK_CONTROL, false),
        make_vk_input(VK_V, false),
        make_vk_input(VK_V, true),
        make_vk_input(VK_CONTROL, true),
    ];
    let n = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() {
        let err = unsafe { GetLastError() };
        return Err(anyhow::anyhow!(
            "SendInput Ctrl+V failed: {}/{} events (err {:?})",
            n,
            inputs.len(),
            err
        ));
    }
    Ok(())
}

/// Inject a single Return keypress. Used after the text injection when the
/// user has `output.send_enter = true` (chat clients, terminals where the
/// dictation is also the "send" gesture). VK_RETURN works in both
/// SendInput and clipboard-paste modes — the clipboard path can't carry a
/// reliable newline, so we always fall back to a real keystroke here.
pub fn send_enter() -> Result<()> {
    let inputs = [
        make_vk_input(VK_RETURN, false),
        make_vk_input(VK_RETURN, true),
    ];
    let n = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if (n as usize) < inputs.len() {
        let err = unsafe { GetLastError() };
        log::warn!(
            "SendInput VK_RETURN dropped ({}/{} events, err {:?})",
            n,
            inputs.len(),
            err
        );
        return Err(anyhow::anyhow!(
            "SendInput Return failed: {}/{} events (err {:?})",
            n,
            inputs.len(),
            err
        ));
    }
    Ok(())
}

fn make_vk_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn make_unicode_input(ch: u16, key_up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: ch,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
