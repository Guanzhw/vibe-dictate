#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Max gap between two successive Pressed events that still counts as a
/// double-tap. Tight enough that back-to-back dictations (which naturally
/// take >500ms between press-events) won't be read as a cancel.
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

/// Max gap between the previous Release and a fresh Press that still
/// counts as "cancel the in-flight transcription". Covers "oh wait, take
/// that back": user release → sees the orange tray → hits the key again.
/// Human visual reaction time is 300-500ms, but a looser window (1.2 s)
/// is still tight enough that normal back-to-back dictations (>1.5 s
/// between release and the next press) don't get swallowed as cancels.
const PROCESSING_CANCEL_WINDOW: Duration = Duration::from_millis(1200);

/// How long the red "cancelled" icon stays up after a double-tap before
/// snapping back to idle blue. Short and sharp — the user wants a quick
/// ack, not a lingering error state.
const CANCEL_FLASH_DURATION: Duration = Duration::from_millis(500);

/// Max wait-time for the user to press a key in the capture popup. If they
/// walk away, the capture auto-cancels and the prior hotkey is restored.
const HOTKEY_CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the red "error" icon stays up after a failed transcription.
/// Slightly longer than the cancel flash so the user can register it even
/// if they were looking elsewhere.
const ERROR_FLASH_DURATION: Duration = Duration::from_millis(800);

/// How long a classified error summary lingers in the tray tooltip before
/// the tooltip reverts to the default "hotkey: …" line. Long enough that
/// the user can hover to read it, short enough that stale messages don't
/// persist once they've fixed the problem.
const ERROR_NOTE_TTL: Duration = Duration::from_secs(20);

/// Heartbeat cadence for the background reachability probe. 30 s keeps the
/// gray/idle transition responsive without pointlessly hammering the
/// server when the user isn't dictating.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

use anyhow::{anyhow, Context, Result};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::MenuEvent;

mod audio;
mod autostart;
mod config;
mod hotkey_capture;
mod injector;
mod keystroke;
mod mouse_hook;
mod openai;
mod overlay;
mod singleton;
mod streaming;
mod text_input;
mod tray;
mod vad;

use config::{BackendMode, Config, InputMode, OutputMode};

#[derive(Debug, Clone)]
enum AppEvent {
    Tick,
    /// Sent by the singleton listener when a newer instance asks us to
    /// make way; the event loop should flip ControlFlow::Exit on receipt.
    Quit,
}

/// Unified binding owner: routes a binding string either to the keyboard
/// global-hotkey manager or to the low-level mouse hook, so the rest of
/// the app doesn't have to branch on "is it a mouse binding?" everywhere.
///
/// Exactly one of (`kb_current` active, `mouse_shared` Some) is ever set.
/// `disable()` clears both, for the capture-dialog pause.
struct BindingManager {
    kb_manager: GlobalHotKeyManager,
    kb_current: Option<HotKey>,
    mouse_shared: Arc<Mutex<Option<mouse_hook::MouseBinding>>>,
}

impl BindingManager {
    fn new(mouse_shared: Arc<Mutex<Option<mouse_hook::MouseBinding>>>) -> Result<Self> {
        let kb_manager = GlobalHotKeyManager::new().context("hotkey manager init")?;
        Ok(Self {
            kb_manager,
            kb_current: None,
            mouse_shared,
        })
    }

    fn apply(&mut self, binding: &str) -> Result<()> {
        // Fully tear down any previous binding first — saves us from
        // partial-state bugs when a user flips between kb and mouse.
        self.disable();
        if let Some(mb) = mouse_hook::parse_mouse_binding(binding) {
            *self.mouse_shared.lock().unwrap() = Some(mb);
            log::info!("Binding active (mouse): {}", binding);
        } else {
            let hk =
                parse_hotkey(binding).with_context(|| format!("parse hotkey '{}'", binding))?;
            self.kb_manager
                .register(hk)
                .with_context(|| format!("register hotkey '{}'", binding))?;
            self.kb_current = Some(hk);
            log::info!("Binding active (keyboard): {}", binding);
        }
        Ok(())
    }

    /// Pause both routes. Used while the capture popup is open so neither
    /// the kb manager nor the mouse hook fires recording events.
    fn disable(&mut self) {
        if let Some(hk) = self.kb_current.take() {
            let _ = self.kb_manager.unregister(hk);
        }
        *self.mouse_shared.lock().unwrap() = None;
    }
}

fn main() -> Result<()> {
    init_logger();
    log::info!("vibe-dictate v{} starting", env!("CARGO_PKG_VERSION"));
    if let Ok(p) = Config::log_path() {
        log::info!("Log file: {}", p.display());
    }

    // Headless transport smoke path. It uses the same websocket/resampler
    // code as PTT and is intentionally opt-in for injection so automated
    // checks can prove model latency without touching the focused app.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--preview-overlay") {
        run_overlay_preview();
        return Ok(());
    }
    if let Some(file) = args
        .iter()
        .position(|arg| arg == "--transcribe-file")
        .and_then(|i| args.get(i + 1))
    {
        let cfg = Config::load_or_default()?;
        let realtime = args.iter().any(|arg| arg == "--realtime");
        let inject = args.iter().any(|arg| arg == "--inject");
        let preview = args.iter().any(|arg| arg == "--preview");
        let result = if preview {
            let (event_tx, event_rx) = crossbeam_channel::bounded(64);
            let overlay = overlay::Overlay::start();
            overlay.show_processing(None);
            let server = cfg.server.clone();
            let stt = cfg.stt.clone();
            let path = file.clone();
            let worker = thread::spawn(move || {
                streaming::transcribe_file(&path, server, stt, realtime, Some(event_tx))
            });
            while !worker.is_finished() {
                while let Ok(event) = event_rx.try_recv() {
                    match event {
                        streaming::StreamEvent::Partial(text) => {
                            overlay.show_partial(&text);
                        }
                        streaming::StreamEvent::Done(_) => {}
                        streaming::StreamEvent::Error(message) => {
                            log::error!("streaming preview failed: {message}");
                            overlay.hide();
                        }
                        streaming::StreamEvent::Cancelled => overlay.hide(),
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
            let result = worker
                .join()
                .map_err(|_| anyhow!("streaming preview worker panicked"))?;
            overlay.hide();
            result
        } else {
            streaming::transcribe_file(
                file,
                cfg.server.clone(),
                cfg.stt.clone(),
                realtime,
                None,
            )
        };
        match result {
            Ok(result) => {
                println!("{}", serde_json::to_string(&result)?);
                if inject {
                    let output_cfg = cfg.output;
                    let text = result.clean_text.unwrap_or(result.text);
                    let mut text = text.trim().to_string();
                    if output_cfg.trailing_space {
                        text.push(' ');
                    }
                    match output_cfg.mode {
                        OutputMode::Clipboard => injector::clipboard_paste(&text)?,
                        OutputMode::Sendinput => injector::send_input_text(
                            &text,
                            output_cfg.send_key_delay_ms,
                            output_cfg.send_key_down_delay_ms,
                        )?,
                    }
                    if output_cfg.send_enter {
                        injector::send_enter()?;
                    }
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }
    if let Some(pos) = args.iter().position(|arg| arg == "--record-seconds") {
        let seconds = args
            .get(pos + 1)
            .ok_or_else(|| anyhow!("--record-seconds requires an integer from 1 to 15"))?
            .parse::<u64>()
            .context("parse --record-seconds")?;
        if !(1..=15).contains(&seconds) {
            return Err(anyhow!("--record-seconds must be between 1 and 15"));
        }
        let cfg = Config::load_or_default()?;
        let result = streaming::record_seconds(&cfg.audio, cfg.server, cfg.stt, seconds)?;
        println!("{}", serde_json::to_string(&result)?);
        return Ok(());
    }

    let cfg = Arc::new(Mutex::new(Config::load_or_default()?));

    // Enforce autostart state from config
    {
        let c = cfg.lock().unwrap();
        if let Err(e) = autostart::set_enabled(c.startup.autostart) {
            log::warn!("autostart sync failed: {e:#}");
        }
    }

    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();

    // Single-instance lock — if another vibe-dictate.exe is running, ask it
    // to quit so we can take its place. The listener thread fires Quit
    // through the event loop proxy when a *future* instance (after us)
    // asks the same of us.
    let quit_proxy = event_loop.create_proxy();
    singleton::acquire_or_replace(move || {
        let _ = quit_proxy.send_event(AppEvent::Quit);
    })
    .context("singleton acquire")?;

    // Tray icon + menu
    let tray_state = tray::build(&cfg.lock().unwrap())?;

    // Mouse hook — runs as a dedicated thread with its own message pump.
    // Always active, but only fires MouseEvent when the current binding
    // matches a mouse button; otherwise stays silent.
    let mouse_handle = mouse_hook::start();

    // Binding router — covers both keyboard (via global-hotkey) and mouse
    // (via the hook) paths behind a single `apply(binding)` entrypoint.
    let mut binding_manager = BindingManager::new(mouse_handle.binding.clone())?;

    // Voice-activation session handle. `Some` ⇒ mic is hot, VAD worker is
    // producing `VadSessionEvent::Utterance` messages; `None` ⇒ PTT mode.
    // Exactly one of (binding_manager active, vad_session Some) is ever
    // engaged — `apply_input_mode` enforces that invariant.
    let mut vad_session: Option<audio::VadSession> = None;
    // Declared here (before the startup `apply_input_mode` call) because
    // mode switches clear it to unstick a pending SpeechStart when the
    // session is torn down.
    let vad_speech_active: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // Apply the configured input mode on startup — but only if the app
    // is enabled. Starting up disabled means the hotkey stays free for
    // other apps and the mic isn't opened until the user opts in from
    // the tray.
    {
        let snapshot = cfg.lock().unwrap().clone();
        if snapshot.enabled {
            apply_input_mode(
                &snapshot,
                &mut binding_manager,
                &mut vad_session,
                &vad_speech_active,
            );
        } else {
            log::info!("Startup: app is disabled — hotkey + VAD not engaged");
        }
    }

    // Recording state
    let recorder: Arc<Mutex<Option<audio::Recorder>>> = Arc::new(Mutex::new(None));
    // Streaming PTT owns a live cpal stream while the key is held. The
    // websocket worker and its bounded audio queue live in `StreamingCapture`.
    let mut streaming_capture: Option<streaming::StreamingCapture> = None;
    let mut streaming_events: Option<crossbeam_channel::Receiver<streaming::StreamEvent>> = None;
    let streaming_partial: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let status_overlay = overlay::Overlay::start();
    let press_time: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    // Double-tap detection state. `last_press_at` tracks the previous Pressed
    // timestamp (regardless of whether it was a single tap or the start of a
    // tap-tap). `cancel_flag` is set the moment a double-tap is detected and
    // checked by the in-flight transcription worker right before paste —
    // that's the "still in flight" cancel window. `in_flight` tells the
    // Pressed handler whether there is actually something to cancel.
    let last_press_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    // Timestamp of the previous successful Release (the point where the
    // recording ended and was handed to the network worker). The Press
    // handler checks it to detect "processing-cancel" — a single quick
    // re-press within PROCESSING_CANCEL_WINDOW after release → drop the
    // in-flight transcription instead of starting a new one.
    let last_release_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let cancel_flag: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let in_flight: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    // When Some(t), the tray should show the red cancel-flash icon until `t`;
    // the event-loop reconciler snaps it back to idle blue once the deadline
    // passes.
    let flash_until: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    // Same shape as flash_until but for transcription-error flashes
    // (red icon + tooltip briefly after a failed request).
    let flash_error_until: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    // Sticky error summary for the tray tooltip. `(expires_at, message)`;
    // cleared by the reconciler once expires_at has passed, or when a
    // successful transcription clears the state.
    let last_error_note: Arc<Mutex<Option<(Instant, String)>>> = Arc::new(Mutex::new(None));
    // Connection health flag. Flipped false by connect/TLS failures during
    // a real transcription or by the heartbeat probe; flipped true on
    // any successful request. Drives the gray Disconnected state.
    let connection_ok: Arc<AtomicBool> = Arc::new(AtomicBool::new(true));
    // Last-applied tray status + tooltip note. The reconciler only repaints
    // when the tuple changes, so a red flash can never get stranded if
    // in_flight delays idle.
    let last_status: Arc<Mutex<(tray::TrayStatus, Option<String>)>> =
        Arc::new(Mutex::new((tray::TrayStatus::Idle, None)));
    // Tracks whether the VAD has an open utterance right now. Used purely
    // for the tray icon (Recording green while VAD speaks, else the teal
    // VadListening when the mic is hot but idle). Declared earlier —
    // apply_input_mode on startup needs it.

    let cfg_loop = cfg.clone();
    let recorder_loop = recorder.clone();
    let press_time_loop = press_time.clone();
    let last_press_at_loop = last_press_at.clone();
    let last_release_at_loop = last_release_at.clone();
    let cancel_flag_loop = cancel_flag.clone();
    let in_flight_loop = in_flight.clone();
    let flash_until_loop = flash_until.clone();
    let flash_error_until_loop = flash_error_until.clone();
    let last_error_note_loop = last_error_note.clone();
    let connection_ok_loop = connection_ok.clone();
    let last_status_loop = last_status.clone();
    let vad_speech_active_loop = vad_speech_active.clone();
    let streaming_partial_loop = streaming_partial.clone();
    let status_overlay_loop = status_overlay.clone();

    // Keep tray + binding + mouse-hook state alive for the event loop.
    let tray_keep_alive = tray_state;
    let mouse_rx = mouse_handle.rx;

    // In-flight hotkey capture. When a menu event fires "Rebind…" we
    // disable the current binding, spawn the capture worker, and stash both
    // the channel and the previous binding string here. Each tick polls —
    // on success we save + re-apply, on cancel we restore the previous.
    struct PendingCapture {
        handle: hotkey_capture::CaptureHandle,
        previous: String,
    }
    let mut pending_capture: Option<PendingCapture> = None;

    // Same shape as PendingCapture but for scalar config fields (language,
    // context info, STT server URL/key/model/CA path). We remember which
    // field the popup is for so the callback at result-time knows where
    // to store.
    struct PendingTextInput {
        handle: text_input::TextInputHandle,
        field: tray::TextField,
    }
    let mut pending_text_input: Option<PendingTextInput> = None;

    // Pump hotkey/menu events periodically
    let proxy = event_loop.create_proxy();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(40));
        let _ = proxy.send_event(AppEvent::Tick);
    });

    // Background reachability probe. Build a fresh SttClient per cycle so
    // mid-run config changes (URL, API key, extra_ca_cert) are picked up
    // without needing to reach into the main loop. The probe itself is a
    // short-timeout GET /v1/models; classified errors flip connection_ok
    // and, for long-running outages, populate last_error_note so the
    // tooltip tells the user *why* the tray went gray.
    {
        let cfg_hb = cfg.clone();
        let connection_ok_hb = connection_ok.clone();
        let last_error_note_hb = last_error_note.clone();
        thread::spawn(move || loop {
            let server_cfg = cfg_hb.lock().unwrap().server.clone();
            let probe = if server_cfg.backend == BackendMode::Streaming {
                streaming::health_check(&server_cfg).map_err(|e| e.to_string())
            } else {
                openai::SttClient::new(&server_cfg)
                    .map_err(|e| e.to_string())
                    .and_then(|c| c.health_check().map_err(|e| e.short_summary()))
            };
            match probe {
                Ok(()) => {
                    let was_offline = !connection_ok_hb.swap(true, Ordering::SeqCst);
                    if was_offline {
                        log::info!("Heartbeat: STT server reachable again");
                        // Only clear a *connection*-flavoured note; if the
                        // user had a recent auth / endpoint error they
                        // probably still want to see it.
                        let mut note = last_error_note_hb.lock().unwrap();
                        if let Some((_, msg)) = note.as_ref() {
                            if msg.contains("Cannot reach")
                                || msg.contains("TLS")
                                || msg.contains("certificate")
                            {
                                *note = None;
                            }
                        }
                    }
                }
                Err(summary) => {
                    let was_online = connection_ok_hb.swap(false, Ordering::SeqCst);
                    if was_online {
                        log::warn!("Heartbeat: STT server unreachable ({})", summary);
                        *last_error_note_hb.lock().unwrap() =
                            Some((Instant::now() + ERROR_NOTE_TTL, summary));
                    }
                }
            }
            thread::sleep(HEARTBEAT_INTERVAL);
        });
    }

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            tao::event::Event::UserEvent(AppEvent::Quit) => {
                if let Some(capture) = streaming_capture.as_mut() {
                    capture.cancel();
                }
                *control_flow = ControlFlow::Exit;
            }
            tao::event::Event::UserEvent(AppEvent::Tick) => {
                // Reconcile tray icon to the single source of truth.
                // Priority (highest first):
                //   error flash   → ErrorFlash    (red, transcription failed)
                //   cancel flash  → CancelFlash   (red, double-tap cancel)
                //   recording     → Recording     (green)
                //   in-flight     → Processing    (orange)
                //   !connection   → Disconnected  (gray)
                //   otherwise     → Idle          (blue)
                let now = Instant::now();
                let flash_cancel = expire_instant(&flash_until_loop, now);
                let flash_error = expire_instant(&flash_error_until_loop, now);
                // Fade out a stale error note so the tooltip reverts to
                // the default after ERROR_NOTE_TTL elapses.
                let note_text = {
                    let mut g = last_error_note_loop.lock().unwrap();
                    match g.as_ref() {
                        Some((t, _)) if now >= *t => {
                            *g = None;
                            None
                        }
                        Some((_, msg)) => Some(msg.clone()),
                        None => None,
                    }
                };
                let recording = recorder_loop.lock().unwrap().is_some()
                    || streaming_capture.as_ref().map(|c| c.is_recording()).unwrap_or(false);
                let vad_speaking = vad_speech_active_loop.load(Ordering::SeqCst);
                let cancel_pending = cancel_flag_loop.load(Ordering::SeqCst);
                let processing = !recording
                    && !vad_speaking
                    && in_flight_loop.load(Ordering::SeqCst);
                let disconnected = !connection_ok_loop.load(Ordering::SeqCst);
                let (enabled, vad_mode_active) = {
                    let c = cfg_loop.lock().unwrap();
                    (c.enabled, c.input.mode == InputMode::VoiceActivation)
                };

                // Streaming updates are drained on the UI thread. Partial
                // text is deliberately only status feedback; exactly one
                // injection happens after the final done message.
                if let Some(rx) = streaming_events.as_ref() {
                    while let Ok(evt) = rx.try_recv() {
                        match evt {
                            streaming::StreamEvent::Partial(partial) => {
                                *streaming_partial_loop.lock().unwrap() = Some(partial.clone());
                                log::debug!("streaming partial: {}", partial);
                            }
                            streaming::StreamEvent::Done(result) => {
                                *streaming_partial_loop.lock().unwrap() = None;
                                if cancel_flag_loop.swap(false, Ordering::SeqCst) {
                                    log::info!("Streaming cancel honored, transcription discarded");
                                    in_flight_loop.store(false, Ordering::SeqCst);
                                    continue;
                                }
                                let cfg_clone = cfg_loop.clone();
                                let conn_clone = connection_ok_loop.clone();
                                let flash_err_clone = flash_error_until_loop.clone();
                                let note_clone = last_error_note_loop.clone();
                                let in_flight_output = in_flight_loop.clone();
                                let run_cancel = streaming_capture
                                    .as_ref()
                                    .map(|capture| capture.cancel_token())
                                    .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                                thread::spawn(move || {
                                    if let Err(e) = streaming_send_and_inject(
                                        result,
                                        cfg_clone,
                                        conn_clone,
                                        flash_err_clone,
                                        note_clone,
                                        in_flight_output,
                                        run_cancel,
                                    ) {
                                        log::error!("Streaming output failed: {e:#}");
                                    }
                                });
                            }
                            streaming::StreamEvent::Error(msg) => {
                                *streaming_partial_loop.lock().unwrap() = None;
                                if let Some(capture) = streaming_capture.as_mut() {
                                    capture.cancel();
                                }
                                in_flight_loop.store(false, Ordering::SeqCst);
                                connection_ok_loop.store(false, Ordering::SeqCst);
                                report_pipeline_error(&msg, true, &flash_error_until_loop, &last_error_note_loop);
                            }
                            streaming::StreamEvent::Cancelled => {
                                *streaming_partial_loop.lock().unwrap() = None;
                                in_flight_loop.store(false, Ordering::SeqCst);
                                log::info!("Streaming session cancelled");
                            }
                        }
                    }
                }

                // If a cancel is still pending on an in-flight transcription,
                // keep the red flash instead of briefly flipping to yellow
                // "Processing" between flash expiry and the HTTP response.
                // Disabled sits between flashes and recording: flashes
                // from a just-toggled-off moment still render, but otherwise
                // Disabled trumps every normal state — no point showing
                // "Disconnected" when the app isn't going to dictate anyway.
                let desired = if flash_error {
                    tray::TrayStatus::ErrorFlash
                } else if flash_cancel || (cancel_pending && processing) {
                    tray::TrayStatus::CancelFlash
                } else if !enabled {
                    tray::TrayStatus::Disabled
                } else if recording || vad_speaking {
                    tray::TrayStatus::Recording
                } else if processing {
                    tray::TrayStatus::Processing
                } else if disconnected {
                    tray::TrayStatus::Disconnected
                } else if vad_mode_active {
                    tray::TrayStatus::VadListening
                } else {
                    tray::TrayStatus::Idle
                };
                // Only surface the note when we have something relevant to
                // say: any red/gray state, or idle with a still-fresh note.
                let partial_note = streaming_partial_loop.lock().unwrap().clone();
                let hotkey_label = cfg_loop.lock().unwrap().hotkey.binding.clone();
                if recording {
                    let elapsed = press_time_loop
                        .lock()
                        .unwrap()
                        .map(|started| started.elapsed())
                        .unwrap_or_default();
                    status_overlay_loop.show_recording(
                        &hotkey_label,
                        elapsed,
                        partial_note.as_deref(),
                    );
                } else if processing {
                    status_overlay_loop.show_processing(partial_note.as_deref());
                } else {
                    status_overlay_loop.hide();
                }
                let tip_note = match desired {
                    tray::TrayStatus::Recording | tray::TrayStatus::Processing => partial_note,
                    tray::TrayStatus::VadListening => None,
                    _ => note_text,
                };
                {
                    let mut last = last_status_loop.lock().unwrap();
                    if last.0 != desired || last.1 != tip_note {
                        let binding = cfg_loop.lock().unwrap().hotkey.binding.clone();
                        if let Err(e) = tray::apply_status(
                            &tray_keep_alive,
                            desired,
                            &binding,
                            tip_note.as_deref(),
                        ) {
                            log::warn!("tray apply failed: {e:#}");
                        }
                        *last = (desired, tip_note);
                    }
                }

                // Collapse keyboard + mouse events into a single stream of
                // Press/Release actions so the downstream recording logic
                // doesn't care which input device triggered the user.
                #[derive(Copy, Clone)]
                enum PushAction {
                    Press,
                    Release,
                }
                let mut actions: Vec<PushAction> = Vec::new();
                while let Ok(hk_evt) = GlobalHotKeyEvent::receiver().try_recv() {
                    actions.push(match hk_evt.state {
                        HotKeyState::Pressed => PushAction::Press,
                        HotKeyState::Released => PushAction::Release,
                    });
                }
                while let Ok(m_evt) = mouse_rx.try_recv() {
                    actions.push(match m_evt {
                        mouse_hook::MouseEvent::Pressed => PushAction::Press,
                        mouse_hook::MouseEvent::Released => PushAction::Release,
                    });
                }

                // Drain any pending VAD events. Utterances get routed into
                // the same send_and_inject pipeline the PTT path uses — the
                // only difference is who triggered the capture.
                if let Some(vs) = vad_session.as_ref() {
                    while let Ok(evt) = vs.rx.try_recv() {
                        match evt {
                            audio::VadSessionEvent::Opened { sample_rate } => {
                                log::info!(
                                    "VAD session opened at {} Hz",
                                    sample_rate
                                );
                            }
                            audio::VadSessionEvent::SpeechStart => {
                                vad_speech_active_loop.store(true, Ordering::SeqCst);
                            }
                            audio::VadSessionEvent::Utterance { wav, duration_ms } => {
                                vad_speech_active_loop.store(false, Ordering::SeqCst);
                                log::info!(
                                    "VAD utterance captured: {} bytes ({} ms)",
                                    wav.len(), duration_ms,
                                );
                                let cfg_clone = cfg_loop.clone();
                                // Share the global cancel flag so the VAD
                                // hotkey cancel (see PushAction::Press in VAD
                                // mode) can drop the in-flight utterance
                                // before it gets pasted.
                                let cancel_clone = cancel_flag_loop.clone();
                                let in_flight_clone = in_flight_loop.clone();
                                let conn_clone = connection_ok_loop.clone();
                                let flash_err_clone = flash_error_until_loop.clone();
                                let note_clone = last_error_note_loop.clone();
                                in_flight_clone.store(true, Ordering::SeqCst);
                                thread::spawn(move || {
                                    let res = send_and_inject(
                                        wav,
                                        cfg_clone,
                                        cancel_clone,
                                        conn_clone,
                                        flash_err_clone,
                                        note_clone,
                                    );
                                    in_flight_clone.store(false, Ordering::SeqCst);
                                    if let Err(e) = res {
                                        log::error!(
                                            "VAD transcription pipeline failed: {e:#}"
                                        );
                                    }
                                });
                            }
                            audio::VadSessionEvent::Error(msg) => {
                                log::error!("VAD session error: {}", msg);
                                *last_error_note_loop.lock().unwrap() = Some((
                                    Instant::now() + ERROR_NOTE_TTL,
                                    format!("VAD: {}", msg),
                                ));
                                vad_speech_active_loop.store(false, Ordering::SeqCst);
                            }
                            audio::VadSessionEvent::Closed => {
                                log::info!("VAD session closed");
                                vad_speech_active_loop.store(false, Ordering::SeqCst);
                            }
                        }
                    }
                }

                for action in actions {
                    match action {
                        PushAction::Press => {
                            let now = Instant::now();
                            let prev = last_press_at_loop.lock().unwrap().replace(now);
                            let is_double_tap = prev
                                .map(|t| now.duration_since(t) <= DOUBLE_TAP_WINDOW)
                                .unwrap_or(false);
                            let currently_in_flight = in_flight_loop.load(Ordering::SeqCst);
                            if !currently_in_flight
                                && streaming_capture
                                    .as_ref()
                                    .map(|c| !c.is_recording())
                                    .unwrap_or(false)
                            {
                                streaming_capture = None;
                                streaming_events = None;
                            }
                            let currently_recording = recorder_loop.lock().unwrap().is_some()
                                || streaming_capture.as_ref().map(|c| c.is_recording()).unwrap_or(false);

                            // VAD mode has no PTT semantics — the hotkey only
                            // means "cancel whatever is currently running /
                            // being processed". No recording to start, no
                            // double-tap gesture.
                            let in_vad_mode = {
                                let c = cfg_loop.lock().unwrap();
                                c.input.mode == InputMode::VoiceActivation
                            };
                            if in_vad_mode {
                                if currently_in_flight || vad_speech_active_loop.load(Ordering::SeqCst) {
                                    cancel_flag_loop.store(true, Ordering::SeqCst);
                                    vad_speech_active_loop
                                        .store(false, Ordering::SeqCst);
                                    log::info!(
                                        "VAD cancel: hotkey pressed — dropping active/in-flight utterance"
                                    );
                                    *flash_until_loop.lock().unwrap() =
                                        Some(Instant::now() + CANCEL_FLASH_DURATION);
                                } else {
                                    log::info!(
                                        "VAD cancel hotkey pressed but nothing in flight"
                                    );
                                }
                                continue;
                            }
                            // Quick-press-after-release cancel: user just
                            // released, saw the processing state, and
                            // slammed the key again to take it back. Only
                            // valid when there's no live recording and the
                            // last release is still fresh.
                            let last_release =
                                *last_release_at_loop.lock().unwrap();
                            let is_processing_cancel = !currently_recording
                                && currently_in_flight
                                && last_release
                                    .map(|t| {
                                        now.duration_since(t)
                                            <= PROCESSING_CANCEL_WINDOW
                                    })
                                    .unwrap_or(false);

                            if is_processing_cancel {
                                cancel_flag_loop.store(true, Ordering::SeqCst);
                                if let Some(capture) = streaming_capture.as_mut() {
                                    if !capture.is_recording() {
                                        capture.cancel();
                                    }
                                }
                                *last_release_at_loop.lock().unwrap() = None;
                                log::info!(
                                    "Fast-press cancel: in-flight transcription will be dropped"
                                );
                                *flash_until_loop.lock().unwrap() =
                                    Some(Instant::now() + CANCEL_FLASH_DURATION);
                            } else if currently_in_flight && !currently_recording {
                                // Close-but-no-cigar: press landed while a
                                // transcription was still running, but the
                                // release→press gap was over the cancel
                                // window. Log it so we can tell whether the
                                // user is missing the gesture or whether the
                                // window is set too tight.
                                if let Some(t) = last_release {
                                    log::info!(
                                        "Press during in-flight transcription ({} ms after release, > {} ms cancel window) — starting new recording",
                                        now.duration_since(t).as_millis(),
                                        PROCESSING_CANCEL_WINDOW.as_millis(),
                                    );
                                }
                                log::info!("Press during in-flight transcription ignored");
                            } else if is_double_tap && currently_recording {
                                // Classic in-recording cancel (rec still live).
                                cancel_flag_loop.store(true, Ordering::SeqCst);
                                let mut slot = recorder_loop.lock().unwrap();
                                let dropped = slot.take().is_some();
                                drop(slot);
                                if let Some(capture) = streaming_capture.as_mut() {
                                    capture.cancel();
                                }
                                if dropped {
                                    log::info!("Double-tap cancel: recording aborted");
                                }
                                *press_time_loop.lock().unwrap() = None;
                                *flash_until_loop.lock().unwrap() =
                                    Some(Instant::now() + CANCEL_FLASH_DURATION);
                            } else {
                                if recorder_loop.lock().unwrap().is_none() && streaming_capture.is_none() && !currently_in_flight {
                                    // New session — clear any stale cancel flag from a
                                    // prior aborted run, and snap the tray off any
                                    // leftover red flash so the icon goes straight
                                    // from red → green when the user re-presses
                                    // shortly after a cancel.
                                    cancel_flag_loop.store(false, Ordering::SeqCst);
                                    *flash_until_loop.lock().unwrap() = None;
                                    let (audio_cfg, server_cfg, stt_cfg, backend) = {
                                        let c = cfg_loop.lock().unwrap();
                                        (c.audio.clone(), c.server.clone(), c.stt.clone(), c.server.backend)
                                    };
                                    if backend == BackendMode::Streaming {
                                        match streaming::StreamingCapture::start(&audio_cfg, server_cfg, stt_cfg) {
                                            Ok((capture, rx)) => {
                                                log::info!("Streaming recording started");
                                                streaming_capture = Some(capture);
                                                streaming_events = Some(rx);
                                                *press_time_loop.lock().unwrap() = Some(Instant::now());
                                            }
                                            Err(e) => log::error!("Failed to start streaming recording: {e:#}"),
                                        }
                                    } else {
                                        let mut slot = recorder_loop.lock().unwrap();
                                        match audio::Recorder::start(&audio_cfg) {
                                            Ok(r) => {
                                                log::info!("Recording started");
                                                *slot = Some(r);
                                                *press_time_loop.lock().unwrap() = Some(Instant::now());
                                            }
                                            Err(e) => log::error!("Failed to start recording: {e:#}"),
                                        }
                                    }
                                }
                            }
                        }
                        PushAction::Release => {
                            let streaming_is_recording = streaming_capture
                                .as_ref()
                                .map(|capture| capture.is_recording())
                                .unwrap_or(false);
                            if streaming_capture.is_some() && !streaming_is_recording {
                                // Error/Cancelled can terminate the worker
                                // while the physical key is still held. A
                                // later Release must not resurrect processing
                                // for that terminal capture.
                                *press_time_loop.lock().unwrap() = None;
                                continue;
                            }
                            if let Some(capture) = streaming_capture.as_mut() {
                                let duration = press_time_loop.lock().unwrap().take()
                                    .map(|t| t.elapsed()).unwrap_or_default();
                                if duration < Duration::from_millis(150) {
                                    capture.cancel();
                                    log::info!("Streaming recording too short ({}ms), discarded", duration.as_millis());
                                } else {
                                    capture.release();
                                    in_flight_loop.store(true, Ordering::SeqCst);
                                    log::info!("Streaming recording released; waiting for done");
                                }
                                *last_release_at_loop.lock().unwrap() = Some(Instant::now());
                                continue;
                            }
                            let rec = recorder_loop.lock().unwrap().take();
                            let started = press_time_loop.lock().unwrap().take();
                            // Remember this release so a follow-up quick
                            // press within PROCESSING_CANCEL_WINDOW can
                            // cancel the in-flight transcription.
                            *last_release_at_loop.lock().unwrap() = Some(Instant::now());
                            if let Some(r) = rec {
                                let duration = started
                                    .map(|t| t.elapsed())
                                    .unwrap_or_else(|| Duration::from_millis(0));
                                if duration < Duration::from_millis(150) {
                                    log::info!(
                                        "Recording too short ({}ms), discarded",
                                        duration.as_millis()
                                    );
                                    drop(r);
                                } else {
                                    // cpal::Stream is !Send, encode WAV on this thread
                                    // before handing bytes off to the network worker.
                                    match r.stop_and_encode_wav() {
                                        Ok(wav) => {
                                            log::info!(
                                                "Captured {} bytes WAV ({}ms), sending",
                                                wav.len(),
                                                duration.as_millis()
                                            );
                                            let cfg_clone = cfg_loop.clone();
                                            let cancel_clone = cancel_flag_loop.clone();
                                            let in_flight_clone = in_flight_loop.clone();
                                            let conn_clone = connection_ok_loop.clone();
                                            let flash_err_clone = flash_error_until_loop.clone();
                                            let note_clone = last_error_note_loop.clone();
                                            in_flight_clone.store(true, Ordering::SeqCst);
                                            thread::spawn(move || {
                                                let res = send_and_inject(
                                                    wav,
                                                    cfg_clone,
                                                    cancel_clone,
                                                    conn_clone,
                                                    flash_err_clone,
                                                    note_clone,
                                                );
                                                in_flight_clone.store(false, Ordering::SeqCst);
                                                if let Err(e) = res {
                                                    log::error!(
                                                        "Transcription pipeline failed: {e:#}"
                                                    );
                                                }
                                            });
                                        }
                                        Err(e) => {
                                            log::error!("WAV encode failed: {e:#}");
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Drain menu events
                while let Ok(me) = MenuEvent::receiver().try_recv() {
                    match tray::handle_menu_event(&me, &cfg_loop) {
                        Ok(outcome) => {
                            // When the master enable toggle flips, engage
                            // or tear down the full input path. Same
                            // branch handles both the on→off and off→on
                            // transitions — apply_input_mode rebuilds the
                            // active side and clears the other.
                            if outcome.enabled_changed {
                                let snapshot = cfg_loop.lock().unwrap().clone();
                                if snapshot.enabled {
                                    apply_input_mode(
                                        &snapshot,
                                        &mut binding_manager,
                                        &mut vad_session,
                                        &vad_speech_active_loop,
                                    );
                                } else {
                                    if let Some(capture) = streaming_capture.as_mut() {
                                        capture.cancel();
                                    }
                                    binding_manager.disable();
                                    if let Some(s) = vad_session.take() {
                                        s.stop();
                                    }
                                    vad_speech_active_loop
                                        .store(false, Ordering::SeqCst);
                                    log::info!("Disabled: input path torn down");
                                }
                            } else if outcome.input_mode_changed {
                                if let Some(capture) = streaming_capture.as_mut() {
                                    capture.cancel();
                                }
                                let snapshot = cfg_loop.lock().unwrap().clone();
                                if snapshot.enabled {
                                    apply_input_mode(
                                        &snapshot,
                                        &mut binding_manager,
                                        &mut vad_session,
                                        &vad_speech_active_loop,
                                    );
                                }
                            }
                            if outcome.hotkey_changed {
                                // Both modes use the binding now (PTT as the
                                // talk key, VAD as a cancel-in-flight hotkey),
                                // so always re-register on a binding change —
                                // but only while enabled; an off-state rebind
                                // is purely a config edit and the binding
                                // should still stay unregistered.
                                let snapshot = cfg_loop.lock().unwrap().clone();
                                if snapshot.enabled {
                                    if let Err(e) =
                                        binding_manager.apply(&snapshot.hotkey.binding)
                                    {
                                        log::error!("apply new binding failed: {e:#}");
                                    }
                                }
                            }
                            if outcome.request_capture {
                                if pending_capture.is_some() {
                                    log::info!(
                                        "Hotkey capture already in progress, ignoring duplicate request"
                                    );
                                } else {
                                    let previous =
                                        cfg_loop.lock().unwrap().hotkey.binding.clone();
                                    binding_manager.disable();
                                    log::info!("Opening hotkey capture popup");
                                    pending_capture = Some(PendingCapture {
                                        handle: hotkey_capture::capture_hotkey_async(
                                            HOTKEY_CAPTURE_TIMEOUT,
                                        ),
                                        previous,
                                    });
                                }
                            }
                            if let Some(field) = outcome.text_input_request {
                                if pending_text_input.is_some() {
                                    log::info!(
                                        "Text input already in progress, ignoring duplicate request"
                                    );
                                } else {
                                    let (title, prompt, initial) =
                                        text_input_params(field, &cfg_loop);
                                    log::info!("Opening text input popup: {}", title);
                                    pending_text_input = Some(PendingTextInput {
                                        handle: text_input::ask_text_async(
                                            &title, &prompt, &initial,
                                        ),
                                        field,
                                    });
                                }
                            }
                            if outcome.menu_dirty {
                                let snapshot = cfg_loop.lock().unwrap().clone();
                                if let Err(e) = tray::rebuild_menu(&tray_keep_alive, &snapshot) {
                                    log::error!("tray menu rebuild failed: {e:#}");
                                }
                            }
                        }
                        Err(e) => log::error!("menu event error: {e:#}"),
                    }
                    if tray::is_quit(&me) {
                        *control_flow = ControlFlow::Exit;
                    }
                }

                // Poll any in-flight hotkey capture. try_recv is non-blocking
                // and Disconnected means the worker panicked / exited without
                // sending — treat that as cancel + restore.
                if let Some(pending) = pending_capture.as_ref() {
                    // Capture opened → the binding is already disabled
                    // while the modal is up. Re-apply only when the app
                    // is actually enabled, so a rebind during Disabled
                    // state silently updates the config without grabbing
                    // the hotkey from other apps.
                    let enabled_now = cfg_loop.lock().unwrap().enabled;
                    match pending.handle.rx.try_recv() {
                        Ok(Ok(Some(new_binding))) => {
                            log::info!("Hotkey capture returned: {}", new_binding);
                            let apply_res = if enabled_now {
                                binding_manager.apply(&new_binding)
                            } else {
                                Ok(())
                            };
                            if let Err(e) = apply_res {
                                log::error!("apply captured binding failed: {e:#}");
                                // Fall back to previous so the app isn't left silent.
                                if enabled_now {
                                    if let Err(e2) = binding_manager.apply(&pending.previous) {
                                        log::error!("restore previous binding failed: {e2:#}");
                                    }
                                }
                            } else {
                                let save_res = {
                                    let mut c = cfg_loop.lock().unwrap();
                                    c.hotkey.binding = new_binding.clone();
                                    c.save()
                                };
                                if let Err(e) = save_res {
                                    log::error!("save config after capture failed: {e:#}");
                                }
                            }
                            let snapshot = cfg_loop.lock().unwrap().clone();
                            if let Err(e) = tray::rebuild_menu(&tray_keep_alive, &snapshot) {
                                log::error!("tray menu rebuild failed after capture: {e:#}");
                            }
                            pending_capture = None;
                        }
                        Ok(Ok(None)) => {
                            log::info!("Hotkey capture cancelled, restoring previous binding");
                            if enabled_now {
                                if let Err(e) = binding_manager.apply(&pending.previous) {
                                    log::error!("restore previous binding failed: {e:#}");
                                }
                            }
                            pending_capture = None;
                        }
                        Ok(Err(e)) => {
                            log::error!("Hotkey capture errored: {e:#}");
                            if enabled_now {
                                if let Err(e2) = binding_manager.apply(&pending.previous) {
                                    log::error!("restore previous binding failed: {e2:#}");
                                }
                            }
                            pending_capture = None;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            log::error!(
                                "Hotkey capture worker disconnected without sending a result"
                            );
                            if enabled_now {
                                if let Err(e) = binding_manager.apply(&pending.previous) {
                                    log::error!("restore previous binding failed: {e:#}");
                                }
                            }
                            pending_capture = None;
                        }
                    }
                }

                // Poll pending text-input popup. Same polling shape as the
                // hotkey capture above.
                if let Some(pending) = pending_text_input.as_ref() {
                    match pending.handle.rx.try_recv() {
                        Ok(Ok(Some(new_val))) => {
                            apply_text_input(pending.field, &new_val, &cfg_loop);
                            let snapshot = cfg_loop.lock().unwrap().clone();
                            if let Err(e) = tray::rebuild_menu(&tray_keep_alive, &snapshot) {
                                log::error!(
                                    "tray menu rebuild failed after text input: {e:#}"
                                );
                            }
                            pending_text_input = None;
                        }
                        Ok(Ok(None)) => {
                            log::info!("Text input cancelled");
                            pending_text_input = None;
                        }
                        Ok(Err(e)) => {
                            log::error!("Text input errored: {e:#}");
                            pending_text_input = None;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            log::error!(
                                "Text input worker disconnected without sending a result"
                            );
                            pending_text_input = None;
                        }
                    }
                }
            }
            _ => {}
        }
    });
}

/// Deterministic UI-only diagnostic. It exercises the same recording,
/// partial, processing, and hide transitions as a real session, without
/// opening the microphone, contacting the STT server, or injecting text.
fn run_overlay_preview() {
    let overlay = overlay::Overlay::start();
    for elapsed in 0..10 {
        let partial = if elapsed >= 2 {
            Some("这是实时转写预览，新增内容会显示在卡片下方。")
        } else {
            None
        };
        overlay.show_recording("F8", Duration::from_secs(elapsed), partial);
        thread::sleep(Duration::from_secs(1));
    }
    overlay.show_partial("这是实时转写预览，新增内容会显示在卡片下方。");
    thread::sleep(Duration::from_secs(5));
    overlay.show_processing(Some("最终识别中，松开按键后只会写入一次。"));
    thread::sleep(Duration::from_secs(3));
    overlay.hide();
    thread::sleep(Duration::from_millis(200));
}

/// Engage exactly one input path — either the global-hotkey / mouse-hook
/// binding (push-to-talk) or the always-listening VAD session (voice
/// activation). Tears down the other so their state machines can't race.
fn apply_input_mode(
    cfg: &Config,
    binding_manager: &mut BindingManager,
    vad_session: &mut Option<audio::VadSession>,
    vad_speech_active: &Arc<AtomicBool>,
) {
    match cfg.input.mode {
        InputMode::PushToTalk => {
            if let Some(s) = vad_session.take() {
                log::info!("Input mode → PTT: stopping VAD session");
                s.stop();
            }
            // The event rx dies with the session, so any SpeechStart /
            // Utterance that was in-flight when we tore down won't be
            // drained by the main loop. Clear the flag explicitly —
            // otherwise the reconciler sees vad_speaking=true forever
            // and leaves the tray stuck on the green recording icon.
            vad_speech_active.store(false, Ordering::SeqCst);
            let binding = cfg.hotkey.binding.clone();
            if let Err(e) = binding_manager.apply(&binding) {
                log::error!(
                    "Input mode → PTT: apply binding '{}' failed: {:#}",
                    binding,
                    e
                );
            }
        }
        InputMode::VoiceActivation => {
            // Keep the hotkey binding alive in VAD mode too, but interpret
            // Press events as "cancel the in-flight transcription" instead
            // of "start a recording". Gives the user a fast escape hatch
            // when the VAD picked up something they didn't want.
            let binding = cfg.hotkey.binding.clone();
            if let Err(e) = binding_manager.apply(&binding) {
                log::warn!(
                    "VAD mode: could not bind cancel hotkey '{}': {:#}",
                    binding,
                    e
                );
            }
            if vad_session.is_none() {
                log::info!("Input mode → VAD: starting capture + VAD worker");
                *vad_session = Some(audio::VadSession::start(cfg.audio.clone(), cfg.vad.clone()));
            }
        }
    }
}

/// Compose (title, prompt, initial value) for the Win32 text-input popup
/// based on which config field the user asked to edit.
fn text_input_params(field: tray::TextField, cfg: &Arc<Mutex<Config>>) -> (String, String, String) {
    let c = cfg.lock().unwrap();
    match field {
        tray::TextField::LanguageHint => (
            "vibe-dictate — language hint".to_string(),
            "Preferred language name (e.g. Hungarian, English, Finnish):".to_string(),
            c.stt.language_hint.clone(),
        ),
        tray::TextField::ContextInfo => (
            "vibe-dictate — context info".to_string(),
            "Prompt describing speaker + expected vocabulary (fed to ASR):".to_string(),
            c.stt.context_info.clone(),
        ),
        tray::TextField::ServerUrl => (
            "vibe-dictate — STT server URL".to_string(),
            "Base URL of the OpenAI-compatible STT server (http/https, no trailing slash):"
                .to_string(),
            c.server.base_url.clone(),
        ),
        tray::TextField::ServerKey => (
            "vibe-dictate — API key".to_string(),
            "Bearer token for the STT server (leave empty for local http://localhost):".to_string(),
            c.server.api_key.clone(),
        ),
        tray::TextField::ServerModel => (
            "vibe-dictate — STT model".to_string(),
            "Model identifier sent in the request (e.g. microsoft/VibeVoice-ASR-HF, whisper-1):"
                .to_string(),
            c.server.model.clone(),
        ),
        tray::TextField::ServerCaCert => (
            "vibe-dictate — extra CA cert path".to_string(),
            "Absolute path to an extra PEM CA cert (leave empty for public/system CAs):"
                .to_string(),
            c.server.extra_ca_cert.clone(),
        ),
    }
}

/// Persist the user's text-input answer into the shared Config and save to
/// disk. The trim matters because users often paste values with trailing
/// whitespace — which would e.g. break Bearer auth silently.
fn apply_text_input(field: tray::TextField, value: &str, cfg: &Arc<Mutex<Config>>) {
    let trimmed = value.trim().to_string();
    let mut c = cfg.lock().unwrap();
    match field {
        tray::TextField::LanguageHint => {
            if trimmed.is_empty() {
                log::info!("Empty language hint, keeping previous value");
                return;
            }
            c.stt.language_hint = trimmed.clone();
            log::info!("Language hint set to '{}'", trimmed);
        }
        tray::TextField::ContextInfo => {
            // Keep the raw (untrimmed) value — the user might intentionally
            // want leading/trailing whitespace in prompt wording.
            c.stt.context_info = value.to_string();
            log::info!("Context info updated ({} chars)", value.len());
        }
        tray::TextField::ServerUrl => {
            c.server.base_url = trimmed.clone();
            log::info!("STT server URL set to '{}'", trimmed);
        }
        tray::TextField::ServerKey => {
            c.server.api_key = trimmed;
            log::info!("STT API key updated (length {})", c.server.api_key.len());
        }
        tray::TextField::ServerModel => {
            if trimmed.is_empty() {
                log::info!("Empty STT model, keeping previous value");
                return;
            }
            c.server.model = trimmed.clone();
            log::info!("STT model set to '{}'", trimmed);
        }
        tray::TextField::ServerCaCert => {
            c.server.extra_ca_cert = trimmed.clone();
            log::info!("STT extra CA cert path set to '{}'", trimmed);
        }
    }
    if let Err(e) = c.save() {
        log::error!("save config after text input failed: {e:#}");
    }
}

fn send_and_inject(
    wav: Vec<u8>,
    cfg: Arc<Mutex<Config>>,
    cancel_flag: Arc<AtomicBool>,
    connection_ok: Arc<AtomicBool>,
    flash_error_until: Arc<Mutex<Option<Instant>>>,
    last_error_note: Arc<Mutex<Option<(Instant, String)>>>,
) -> Result<()> {
    let (server_cfg, stt_cfg, output_cfg) = {
        let c = cfg.lock().unwrap();
        (c.server.clone(), c.stt.clone(), c.output.clone())
    };

    let client = match openai::SttClient::new(&server_cfg) {
        Ok(c) => c,
        Err(e) => {
            // Usually a bad extra_ca_cert path — treat as a config-level
            // error, flash red + surface the message for the user.
            let summary = format!("STT client init failed: {}", e);
            log::error!("{}", summary);
            report_pipeline_error(&summary, false, &flash_error_until, &last_error_note);
            return Ok(());
        }
    };
    let text = match client.transcribe(wav, &stt_cfg.language_hint, &stt_cfg.context_info) {
        Ok(t) => {
            connection_ok.store(true, Ordering::SeqCst);
            t
        }
        Err(e) => {
            log::error!("Transcription failed: {} — {}", e.short_summary(), e);
            if e.is_connection_issue() {
                connection_ok.store(false, Ordering::SeqCst);
            }
            report_pipeline_error(
                &e.short_summary(),
                e.is_connection_issue(),
                &flash_error_until,
                &last_error_note,
            );
            return Ok(());
        }
    };
    let _ = stt_cfg.max_new_tokens; // server-enforced; see ServerConfig docs

    // Final cancel check: user double-tapped while the server was crunching.
    // Drop the result instead of pasting it. We don't try to abort the HTTP
    // call itself — reqwest blocking has no cheap cancellation — but
    // swallowing the output is what the user actually cares about.
    // Consume the flag as part of the check so a single cancel press only
    // drops the one in-flight utterance, not every subsequent VAD pickup.
    if cancel_flag.swap(false, Ordering::SeqCst) {
        log::info!("Cancel honored, transcription discarded");
        return Ok(());
    }

    let raw = text.trim().to_string();
    if raw.is_empty() {
        log::warn!("Empty transcription returned");
        return Ok(());
    }
    // Strip VibeVoice's non-speech meta tags ("[Music]", "[Noise]",
    // "[Environmental noise]", "[Unintelligible speech]", …) wherever they
    // appear — not just when the entire response is a single tag.
    let stripped = strip_bracket_tags(&raw);
    if stripped.is_empty() {
        log::warn!("Non-speech transcription '{}', skipping paste", raw);
        return Ok(());
    }
    if stripped != raw {
        log::info!("Stripped bracket tags: '{}' → '{}'", raw, stripped);
    }

    // Interactive keystroke path: if the user said only a parseable
    // combo ("escape", "control shift s"), inject the keystroke instead
    // of pasting the text. Falls through to normal text output on any
    // parse failure, so dictation of actual prose is never hijacked.
    if output_cfg.interactive_keystrokes {
        if let Some(combo) = keystroke::parse_speech_keystroke(&stripped) {
            log::info!(
                "Interactive keystroke: '{}' → ctrl={} shift={} alt={} win={} vk=0x{:02X}",
                stripped,
                combo.mods.ctrl,
                combo.mods.shift,
                combo.mods.alt,
                combo.mods.win,
                combo.vk.0,
            );
            keystroke::send_combo(combo)?;
            *last_error_note.lock().unwrap() = None;
            return Ok(());
        }
    }

    let mut out = stripped;
    if output_cfg.trailing_space {
        out.push(' ');
    }
    log::info!("Transcription ({} chars): {}", out.len(), out);

    match output_cfg.mode {
        OutputMode::Clipboard => injector::clipboard_paste(&out)?,
        OutputMode::Sendinput => injector::send_input_text(
            &out,
            output_cfg.send_key_delay_ms,
            output_cfg.send_key_down_delay_ms,
        )?,
    }
    if output_cfg.send_enter {
        injector::send_enter()?;
    }
    // Successful end-to-end — clear any stale error note so the tooltip
    // doesn't keep showing last run's problem after it was fixed.
    *last_error_note.lock().unwrap() = None;
    Ok(())
}

fn streaming_send_and_inject(
    result: streaming::StreamResult,
    cfg: Arc<Mutex<Config>>,
    connection_ok: Arc<AtomicBool>,
    flash_error_until: Arc<Mutex<Option<Instant>>>,
    last_error_note: Arc<Mutex<Option<(Instant, String)>>>,
    in_flight: Arc<AtomicBool>,
    run_cancel: Arc<AtomicBool>,
) -> Result<()> {
    if run_cancel.load(Ordering::Acquire) {
        in_flight.store(false, Ordering::SeqCst);
        return Ok(());
    }
    let output_cfg = cfg.lock().unwrap().output.clone();
    let raw = result.clean_text.unwrap_or(result.text).trim().to_string();
    if raw.is_empty() {
        log::warn!("Empty streaming transcription returned");
        in_flight.store(false, Ordering::SeqCst);
        return Ok(());
    }
    let mut out = raw;
    if output_cfg.trailing_space {
        out.push(' ');
    }
    if run_cancel.load(Ordering::Acquire) {
        in_flight.store(false, Ordering::SeqCst);
        return Ok(());
    }
    let injection = match output_cfg.mode {
        OutputMode::Clipboard => injector::clipboard_paste(&out),
        OutputMode::Sendinput => injector::send_input_text(
            &out,
            output_cfg.send_key_delay_ms,
            output_cfg.send_key_down_delay_ms,
        ),
    };
    if let Err(e) = injection {
        let summary = format!("Streaming output failed: {e:#}");
        report_pipeline_error(&summary, false, &flash_error_until, &last_error_note);
        in_flight.store(false, Ordering::SeqCst);
        return Err(e);
    }
    if output_cfg.send_enter {
        if let Err(e) = injector::send_enter() {
            let summary = format!("Streaming Enter failed: {e:#}");
            report_pipeline_error(&summary, false, &flash_error_until, &last_error_note);
            in_flight.store(false, Ordering::SeqCst);
            return Err(e);
        }
    }
    connection_ok.store(true, Ordering::SeqCst);
    *last_error_note.lock().unwrap() = None;
    in_flight.store(false, Ordering::SeqCst);
    log::info!("Streaming transcription injected ({} chars)", out.len());
    Ok(())
}

/// Fire the red ErrorFlash and stash a short summary for the tooltip.
/// Connection issues get a slightly longer TTL because the user may need
/// time to bring the backend back online.
fn report_pipeline_error(
    summary: &str,
    is_connection_issue: bool,
    flash_error_until: &Arc<Mutex<Option<Instant>>>,
    last_error_note: &Arc<Mutex<Option<(Instant, String)>>>,
) {
    let now = Instant::now();
    *flash_error_until.lock().unwrap() = Some(now + ERROR_FLASH_DURATION);
    let ttl = if is_connection_issue {
        ERROR_NOTE_TTL * 2
    } else {
        ERROR_NOTE_TTL
    };
    *last_error_note.lock().unwrap() = Some((now + ttl, summary.to_string()));
}

/// Shared helper for "drain a deadline-based flash": returns true if the
/// deadline is still in the future, false otherwise (and clears the slot
/// once the deadline passes so the reconciler can see a clean edge).
fn expire_instant(cell: &Arc<Mutex<Option<Instant>>>, now: Instant) -> bool {
    let mut g = cell.lock().unwrap();
    match *g {
        Some(t) if now < t => true,
        Some(_) => {
            *g = None;
            false
        }
        None => false,
    }
}

/// Strip every `[...]` bracketed section from `text`, collapsing any
/// runs of whitespace left behind. VibeVoice ASR emits tags like
/// "[Music]", "[Noise]", "[Silence]", "[Environmental noise]",
/// "[Unintelligible speech]" — including mid-utterance — whenever a
/// stretch of audio doesn't map to clean speech. The old "whole string
/// is exactly one tag" check missed inline tags and tags containing
/// punctuation (commas, em-dashes), so switch to a generic strip.
///
/// Nested brackets are tolerated: depth counter survives a mismatched
/// closing bracket. We preserve the characters outside any tag verbatim
/// — only the bracketed content (and the brackets themselves) is dropped.
fn strip_bracket_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth: u32 = 0;
    for c in text.chars() {
        match c {
            '[' => depth = depth.saturating_add(1),
            ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    // Collapse whitespace — stripping "[Music] hello" → " hello" left an
    // awkward leading space; this normalizes it cheaply without pulling
    // in a regex crate.
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_ws = true;
    for c in out.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                collapsed.push(' ');
            }
            prev_ws = true;
        } else {
            collapsed.push(c);
            prev_ws = false;
        }
    }
    collapsed.trim().to_string()
}

fn init_logger() {
    let env = env_logger::Env::default().default_filter_or("info");
    let mut builder = env_logger::Builder::from_env(env);

    // In windowed builds there is no console, so stderr is dropped. Always
    // try to also tee logs into a rotating-on-startup file in the cache dir.
    if let Ok(path) = Config::log_path() {
        // Truncate on each start so the file never grows unbounded; the user
        // can keep an external tail open if they want history.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path);
        match file {
            Ok(f) => {
                builder.target(env_logger::Target::Pipe(Box::new(f)));
            }
            Err(e) => {
                eprintln!(
                    "vibe-dictate: cannot open log file {}: {e:#}",
                    path.display()
                );
            }
        }
    }
    builder.init();
}

fn parse_hotkey(s: &str) -> Result<HotKey> {
    let mut mods = Modifiers::empty();
    let mut code: Option<Code> = None;
    for raw in s.split('+') {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "shift" => mods |= Modifiers::SHIFT,
            "alt" => mods |= Modifiers::ALT,
            "rightalt" | "altgr" => mods |= Modifiers::ALT_GRAPH,
            "win" | "super" | "meta" => mods |= Modifiers::META,
            other => {
                code =
                    Some(parse_code(other).ok_or_else(|| {
                        anyhow!("unknown key token '{}' in hotkey '{}'", other, s)
                    })?);
            }
        }
    }
    let code = code.ok_or_else(|| anyhow!("no key code in hotkey '{}'", s))?;
    Ok(HotKey::new(Some(mods), code))
}

fn parse_code(name: &str) -> Option<Code> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "space" => Code::Space,
        "enter" | "return" => Code::Enter,
        "tab" => Code::Tab,
        "escape" | "esc" => Code::Escape,
        "backspace" => Code::Backspace,
        "capslock" => Code::CapsLock,
        "pause" => Code::Pause,
        "scrolllock" | "scroll" => Code::ScrollLock,
        "f1" => Code::F1,
        "f2" => Code::F2,
        "f3" => Code::F3,
        "f4" => Code::F4,
        "f5" => Code::F5,
        "f6" => Code::F6,
        "f7" => Code::F7,
        "f8" => Code::F8,
        "f9" => Code::F9,
        "f10" => Code::F10,
        "f11" => Code::F11,
        "f12" => Code::F12,
        s if s.len() == 1 => {
            let ch = s.chars().next().unwrap().to_ascii_uppercase();
            match ch {
                'A' => Code::KeyA,
                'B' => Code::KeyB,
                'C' => Code::KeyC,
                'D' => Code::KeyD,
                'E' => Code::KeyE,
                'F' => Code::KeyF,
                'G' => Code::KeyG,
                'H' => Code::KeyH,
                'I' => Code::KeyI,
                'J' => Code::KeyJ,
                'K' => Code::KeyK,
                'L' => Code::KeyL,
                'M' => Code::KeyM,
                'N' => Code::KeyN,
                'O' => Code::KeyO,
                'P' => Code::KeyP,
                'Q' => Code::KeyQ,
                'R' => Code::KeyR,
                'S' => Code::KeyS,
                'T' => Code::KeyT,
                'U' => Code::KeyU,
                'V' => Code::KeyV,
                'W' => Code::KeyW,
                'X' => Code::KeyX,
                'Y' => Code::KeyY,
                'Z' => Code::KeyZ,
                '0' => Code::Digit0,
                '1' => Code::Digit1,
                '2' => Code::Digit2,
                '3' => Code::Digit3,
                '4' => Code::Digit4,
                '5' => Code::Digit5,
                '6' => Code::Digit6,
                '7' => Code::Digit7,
                '8' => Code::Digit8,
                '9' => Code::Digit9,
                _ => return None,
            }
        }
        _ => return None,
    })
}
