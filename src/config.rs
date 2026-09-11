use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories_next::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Global on/off switch. When false, no hotkey is registered and no
    /// VAD session runs — the app is inert but the tray stays around so
    /// the user can toggle it back on. Defaults to false so a fresh
    /// install doesn't immediately start grabbing a hotkey: the user
    /// opts in from the tray when they actually want to dictate.
    #[serde(default)]
    pub enabled: bool,
    /// `[server]` in TOML. Older configs used `[gradio]` with fields
    /// `url` / `api_token`; the serde aliases on ServerConfig let those
    /// load transparently and get rewritten on the next save.
    #[serde(default, alias = "gradio")]
    pub server: ServerConfig,
    #[serde(default)]
    pub stt: SttConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub hotkey: HotkeyConfig,
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub vad: VadConfig,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub startup: StartupConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Transport used for transcription. Streaming speaks the local
    /// VibeVoice websocket protocol; HTTP preserves the existing API path.
    #[serde(default)]
    pub backend: BackendMode,
    /// Base URL of the OpenAI-compatible STT server, e.g.
    /// `http://localhost:8080` or `https://stt.example.com`. No trailing
    /// slash. The client appends `/v1/audio/transcriptions`.
    #[serde(alias = "url")]
    pub base_url: String,
    /// Bearer token for the server. Default matches the shared
    /// `DEFAULT_API_KEY` baked into the shipped compose files so a fresh
    /// install is authenticated out of the box — change it on both ends
    /// before exposing the backend publicly.
    #[serde(alias = "api_token")]
    pub api_key: String,
    /// Model identifier sent in the multipart `model` field. Servers that
    /// host a single model (our VibeVoice shim, most self-hosted vLLM
    /// deployments) ignore this. For OpenAI Whisper, set to `whisper-1`.
    #[serde(default = "default_stt_model")]
    pub model: String,
    /// Absolute path to a PEM-encoded CA certificate (or bundle) that
    /// reqwest should trust in addition to the system roots. Leave empty
    /// for localhost / public-CA deployments; set this when pointing the
    /// client at a remote endpoint behind a self-signed cert or a
    /// private CA (e.g. Tailscale funnel, internal proxy).
    #[serde(default)]
    pub extra_ca_cert: String,
}

fn default_stt_model() -> String {
    "microsoft/VibeVoice-ASR-HF".to_string()
}

/// Shared default Bearer token. Baked into both ends (client default config
/// and the compose `.env.*.example`) so a fresh install is authenticated
/// out of the box. The value is intentionally a well-known "change me"
/// string — rotate it on both the server .env and in tray → STT server →
/// API key before exposing the backend on a public network.
pub const DEFAULT_API_KEY: &str = "vibe-dictate-default-change-me";

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            backend: BackendMode::Http,
            base_url: "http://localhost:8080".to_string(),
            api_key: DEFAULT_API_KEY.to_string(),
            model: default_stt_model(),
            extra_ca_cert: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackendMode {
    Http,
    Streaming,
}

impl Default for BackendMode {
    fn default() -> Self {
        Self::Http
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttConfig {
    pub context_info: String,
    pub max_new_tokens: u32,
    pub language_hint: String,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            context_info: default_context_info(),
            max_new_tokens: 8192,
            language_hint: "Hungarian".to_string(),
        }
    }
}

/// Map short ISO 639-1 codes (or obvious two-letter variants) to the full
/// English language names the ASR model's prompt handling responds to
/// best. Returns None if the hint already looks like a full name.
fn iso_to_language_name(hint: &str) -> Option<&'static str> {
    match hint.trim().to_ascii_lowercase().as_str() {
        "hu" => Some("Hungarian"),
        "en" => Some("English"),
        "de" => Some("German"),
        "fr" => Some("French"),
        "es" => Some("Spanish"),
        "it" => Some("Italian"),
        "pt" => Some("Portuguese"),
        "pl" => Some("Polish"),
        "nl" => Some("Dutch"),
        "ja" => Some("Japanese"),
        "ko" => Some("Korean"),
        "zh" => Some("Chinese"),
        _ => None,
    }
}

/// Default prompt we feed to VibeVoice ASR when context_info isn't filled
/// in explicitly. Without a language anchor the model guesses wildly on
/// short utterances and on sentences that mix Hungarian + English terms;
/// this prompt tells it to stay in Hungarian as the default language but
/// leave code-mixed English words intact (brand names, tech jargon, etc.).
fn default_context_info() -> String {
    "The speaker uses Hungarian as the primary language, and may mix in English technical terms, proper nouns, brand names, and abbreviations. Transcribe verbatim — keep each word in its original language, do not translate.".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    pub mic_device: String,
    pub sample_rate: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            mic_device: String::new(),
            sample_rate: 16000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    pub binding: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            binding: "F8".to_string(),
        }
    }
}

pub const HOTKEY_OPTIONS: &[&str] = &["F7", "F8", "F9", "F10", "F11", "F12", "Pause", "ScrollLock"];

/// Toggle between push-to-talk (hold hotkey/mouse button) and voice
/// activation (continuous mic listening with an RMS-based VAD). The
/// two modes are mutually exclusive — the current binding is wired to
/// exactly one of them at a time.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    PushToTalk,
    VoiceActivation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    pub mode: InputMode,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            mode: InputMode::PushToTalk,
        }
    }
}

/// Energy-based VAD tuning. Defaults target close-mic dictation at 16 kHz;
/// the adaptive noise floor tracks background noise between utterances
/// and speech_ratio is the SNR multiplier over that floor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VadConfig {
    /// How many consecutive speech frames (20 ms each) must be above
    /// threshold before we open a new utterance. Too low = false starts
    /// on door slams; too high = clipped beginnings.
    #[serde(default = "default_vad_start_frames")]
    pub start_frames: u32,
    /// How many consecutive silence frames (20 ms each) close the
    /// current utterance. 35 × 20 ms = 700 ms of silence is the classic
    /// dictation endpoint.
    #[serde(default = "default_vad_end_frames")]
    pub end_frames: u32,
    /// Max utterance length in seconds. Caps runaway captures when a
    /// loud TV or fan fools the VAD.
    #[serde(default = "default_vad_max_seconds")]
    pub max_seconds: u32,
    /// Minimum utterance length in milliseconds. Anything shorter is
    /// discarded (pops, coughs, hotkey clicks).
    #[serde(default = "default_vad_min_utt_ms")]
    pub min_utterance_ms: u32,
    /// Linear RMS ratio over the noise floor that counts as speech.
    /// 3.0 ≈ +9.5 dB SNR — safe for close-mic dictation.
    #[serde(default = "default_vad_speech_ratio")]
    pub speech_ratio: f32,
    /// Absolute floor for the noise estimate in i16 RMS units so the
    /// VAD doesn't start triggering on pure digital noise during truly
    /// silent periods.
    #[serde(default = "default_vad_noise_floor_min")]
    pub noise_floor_min: f32,
}

fn default_vad_start_frames() -> u32 {
    3
}
fn default_vad_end_frames() -> u32 {
    35
}
fn default_vad_max_seconds() -> u32 {
    30
}
fn default_vad_min_utt_ms() -> u32 {
    300
}
fn default_vad_speech_ratio() -> f32 {
    3.0
}
fn default_vad_noise_floor_min() -> f32 {
    80.0
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            start_frames: default_vad_start_frames(),
            end_frames: default_vad_end_frames(),
            max_seconds: default_vad_max_seconds(),
            min_utterance_ms: default_vad_min_utt_ms(),
            speech_ratio: default_vad_speech_ratio(),
            noise_floor_min: default_vad_noise_floor_min(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    pub mode: OutputMode,
    pub trailing_space: bool,
    /// If true, inject a Return keypress after the transcription. Useful for
    /// chat clients / terminals where the user wants the message sent
    /// immediately. Default false because most editors don't want a stray
    /// newline appended to dictated text.
    #[serde(default)]
    pub send_enter: bool,
    /// Milliseconds to sleep between successive characters in SendInput
    /// mode. Too fast and Electron/Chromium apps (Discord, Slack, VS Code),
    /// Notepad, and some terminals silently drop characters; too slow and
    /// dictation feels sluggish. 20 ms ≈ 50 chars/sec is the safe default
    /// that works even on slower CPUs; drop to 5-10 ms on fast machines
    /// into well-behaved editors if you want faster injection.
    #[serde(default = "default_send_key_delay_ms")]
    pub send_key_delay_ms: u64,
    /// Milliseconds to hold each key "down" before releasing (down→up gap
    /// per character). 0 works against some targets but several apps filter
    /// out zero-duration keypresses, so 15 ms is the default — still
    /// imperceptible but reliable. Raise further only if characters still
    /// drop after bumping the inter-char delay.
    #[serde(default = "default_send_key_down_delay_ms")]
    pub send_key_down_delay_ms: u64,
    /// "Interactive" mode: if the transcription parses cleanly as a single
    /// key combo ("escape", "control shift s"), press that combo via
    /// SendInput instead of pasting the text. Anything not parseable as
    /// a combo falls through to the normal text injection, so this is
    /// additive and safe to leave on.
    #[serde(default = "default_interactive_keystrokes")]
    pub interactive_keystrokes: bool,
}

fn default_send_key_delay_ms() -> u64 {
    30
}
fn default_send_key_down_delay_ms() -> u64 {
    15
}
fn default_interactive_keystrokes() -> bool {
    true
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            mode: OutputMode::Clipboard,
            trailing_space: true,
            send_enter: false,
            send_key_delay_ms: default_send_key_delay_ms(),
            send_key_down_delay_ms: default_send_key_down_delay_ms(),
            interactive_keystrokes: default_interactive_keystrokes(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    Clipboard,
    Sendinput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupConfig {
    pub autostart: bool,
    pub start_minimized: bool,
}

impl Default for StartupConfig {
    fn default() -> Self {
        Self {
            autostart: false,
            start_minimized: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            server: ServerConfig::default(),
            stt: SttConfig::default(),
            audio: AudioConfig::default(),
            hotkey: HotkeyConfig::default(),
            input: InputConfig::default(),
            vad: VadConfig::default(),
            output: OutputConfig::default(),
            startup: StartupConfig::default(),
        }
    }
}

impl Config {
    pub fn config_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("com", "chestercs", "vibe-dictate")
            .context("Could not resolve APPDATA directory")?;
        let dir = dirs.config_dir();
        fs::create_dir_all(dir).context("Failed to create config dir")?;
        Ok(dir.join("config.toml"))
    }

    pub fn log_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("com", "chestercs", "vibe-dictate")
            .context("Could not resolve APPDATA directory")?;
        let dir = dirs.cache_dir();
        fs::create_dir_all(dir).context("Failed to create cache dir")?;
        Ok(dir.join("vibe-dictate.log"))
    }

    pub fn load_or_default() -> Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            let cfg = Config::default();
            cfg.save()?;
            log::info!("Created default config at {}", path.display());
            return Ok(cfg);
        }
        let text = fs::read_to_string(&path).with_context(|| format!("Read {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("Parse {}", path.display()))?;
        log::info!("Loaded config from {}", path.display());
        if cfg.migrate_in_place() {
            cfg.save()?;
            log::info!("Migrated stale config values to defaults");
        }
        Ok(cfg)
    }

    fn migrate_in_place(&mut self) -> bool {
        let mut changed = false;
        // Alt-based hotkeys conflict with Windows app menus and AltGr (RightAlt = Ctrl+Alt
        // on Hungarian layouts) tends to leave Alt stuck. Force-migrate to F8 default.
        let lower = self.hotkey.binding.to_ascii_lowercase();
        let has_alt = lower
            .split('+')
            .any(|t| matches!(t.trim(), "alt" | "rightalt" | "altgr" | "leftalt"));
        if has_alt {
            log::warn!(
                "Migrating Alt-based hotkey '{}' to 'F8' (Alt conflicts with app menus)",
                self.hotkey.binding
            );
            self.hotkey.binding = "F8".to_string();
            changed = true;
        }

        // Upgrade 2-letter ISO hints to full English language names — ASR
        // prompt quality is measurably better with "Hungarian" than "hu".
        if let Some(expanded) = iso_to_language_name(&self.stt.language_hint) {
            log::info!(
                "Migrating language_hint '{}' → '{}'",
                self.stt.language_hint,
                expanded
            );
            self.stt.language_hint = expanded.to_string();
            changed = true;
        }

        // If context_info is empty, seed a sensible default prompt that
        // anchors on the user's primary language and allows mixed terms.
        if self.stt.context_info.trim().is_empty() {
            log::info!("Seeding default context_info prompt");
            self.stt.context_info = default_context_info();
            changed = true;
        }

        // v0.1 → v0.2 transport migration: Gradio (7860) → OpenAI-compat
        // (8080). Only auto-migrate the exact legacy default URL; if the
        // user pointed at a remote/custom URL we leave it — they'll know
        // whether it's OpenAI-compat or not.
        if self.server.base_url.trim_end_matches('/') == "http://localhost:7860" {
            log::info!("Migrating server.base_url 7860 → 8080 (OpenAI-compat default)");
            self.server.base_url = "http://localhost:8080".to_string();
            changed = true;
        }
        changed
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        let text = toml::to_string_pretty(self)?;
        fs::write(&path, text).with_context(|| format!("Write {}", path.display()))?;
        Ok(())
    }
}
