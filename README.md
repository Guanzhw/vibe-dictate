# VibeVoice 语音输入 · 个人定制版

基于 [chestercs/vibe-dictate](https://github.com/chestercs/vibe-dictate) 维护的 Windows 语音输入客户端，使用 **VibeVoice-ASR-Streaming-1.5B** 和本地 NVIDIA GPU 推理。

- 按住 **F8** 说话，浮窗显示红色录音提示、计时和实时转写；松开后将完整文字写入当前输入框。
- Windows 原生 Rust 客户端负责托盘、全局快捷键、WASAPI 录音和文字输入；Ubuntu WSL2 运行模型，通过本地 WebSocket 传输音频。
- 安装脚本创建“VibeVoice 语音输入”快捷方式；启动器按需启动模型，退出时停止自己启动的服务。

这是为当前使用环境维护的定制版本。安装预设使用 Ubuntu、WSL 用户 `qq110`、中文优先转写和 F8；更换机器时请按文档调整脚本参数。模型约每 2.93 秒处理一块音频，首段文字需要先积累约 3.47 秒音频。

参见 [安装与使用](docs/windows-streaming.md) 和 [实际验证记录](docs/windows-streaming-validation.md)。原项目历史及功能说明保留如下。

## Original vibe-dictate documentation

![vibe-dictate tray menu](assets/tray-menu.png)

![vibe-dictate demo](assets/demo.gif)

> 🎬 Higher-quality MP4: [`assets/demo.mp4`](assets/demo.mp4)

A Windows tray-resident dictation tool that talks to any
**OpenAI-compatible** speech-to-text server — most notably Microsoft's
**VibeVoice ASR**, but also OpenAI Whisper itself, self-hosted Whisper
/vLLM deployments, and anything else that speaks
`POST /v1/audio/transcriptions`. Hold a hotkey, speak, release — the
transcription lands in whatever window has focus. Or flip it into
**voice-activation mode** and let the built-in VAD slice your speech
into utterances automatically; the hotkey then becomes an "undo the
last thing it picked up" button.

Built for Hungarian dictation but works in any of the 50+ languages
VibeVoice-ASR supports. Multilingual by accident of the backend; Hungarian
by intent of the author.

---

## Table of contents

- [What is VibeVoice?](#what-is-vibevoice)
- [System requirements](#system-requirements)
- [Hosting a backend](#hosting-a-backend)
- [Building the client](#building-the-client)
- [Running](#running)
- [Configuration reference](#configuration-reference)
- [Hotkey reference](#hotkey-reference)
- [Tray menu reference](#tray-menu-reference)
- [Output modes](#output-modes)
- [Troubleshooting](#troubleshooting)
- [Architecture](#architecture)
- [Development](#development)

---

## What is VibeVoice?

**VibeVoice** is Microsoft's open-source speech model family released in
late 2025. The relevant variant for this tool is
**`microsoft/VibeVoice-ASR-HF`** (transformers-native, ~8B, bf16):

- **Type**: ~8B parameter encoder-decoder ASR model (bf16, ~16 GB on disk)
- **Languages**: 50+ including English, Hungarian, German, French,
  Spanish, Italian, Polish, Dutch, Japanese, Korean, Chinese, etc.
- **Output**: structured transcription with optional speaker diarization,
  hotword biasing, language anchoring and free-form context prompts.
- **License**: MIT (see upstream repo)
- **Repo**: https://github.com/microsoft/VibeVoice
- **HF model card**: https://huggingface.co/microsoft/VibeVoice-ASR-HF

Microsoft also ships a 0.5B realtime TTS sibling (`VibeVoice-Realtime-0.5B`),
which is unrelated to this tool — vibe-dictate only consumes the ASR side.

vibe-dictate speaks the **OpenAI Audio API** (`POST
/v1/audio/transcriptions`, multipart/form-data) — not the legacy Gradio
API. This repo ships a tiny FastAPI shim (`scripts/openai_asr_server.py`)
that wraps VibeVoice-ASR-HF in that shape, so any OpenAI-compatible STT
client (Open WebUI, LibreChat, the OpenAI Python SDK, vibe-dictate, …)
can point at it interchangeably.

### Hungarian quality

Hungarian has only ~0.18% representation in VibeVoice-ASR's training corpus
(20th in the language list, comparable to Turkish / Thai / Czech). Out of
the box accuracy is usable but benefits from:

- A precise `language_hint` ("Hungarian", not "hu")
- A `context_info` prompt that anchors language and allows code-mixing
  (the default prompt does this — see `config.rs::default_context_info`)
- Clean audio (close-talk mic, low background noise)

For specialized vocabularies (medical, legal, custom jargon) consider
LoRA fine-tuning via the upstream `finetuning-asr/` recipes.

---

## System requirements

### Client (vibe-dictate, Windows)

- **OS**: Windows 10 (1903+) or Windows 11
- **Architecture**: x86_64
- **CPU**: any modern x86_64 (the client is a thin push-to-talk wrapper,
  no inference happens locally)
- **RAM**: ~30 MB resident
- **Audio**: any input device WASAPI exposes (USB headsets, built-in
  mics, virtual cables, etc.)
- **Network**: TCP reachability to the STT backend
  (default `http://localhost:8080`)
- **Privileges**: regular user — no admin needed unless the target window
  you want to type into is itself elevated (UIPI: a non-elevated process
  can't SendInput into an elevated one)

### Backend (VibeVoice ASR, Linux + NVIDIA GPU)

- **GPU VRAM**: ~16 GB minimum for bf16 weights, ~18-22 GB peak with KV
  cache. RTX 4090 (24 GB) is comfortable; A6000 / L40S work fine; data
  center cards (H100, B200) are overkill but supported.
- **CUDA**: 12.x or 13.x. NVIDIA Blackwell (sm_120/sm_121) needs CUDA 13
  and `TORCH_CUDA_ARCH_LIST="12.0+PTX"`.
- **Driver**: 550+ for CUDA 12, 560+ for CUDA 13.
- **CPU inference**: technically possible, practically pointless
  (~1 token/sec → minutes per utterance).
- **OS**: any Linux with Docker + NVIDIA Container Toolkit. Bare-metal
  install also works if you can reproduce the upstream `pip install -e .`
  recipe.
- **Disk**: ~30 GB for HuggingFace model cache + Docker image layers.

Not self-hosting? vibe-dictate also works against hosted OpenAI-compat
endpoints — see [Option C](#option-c--managed-cloud).

---

## Hosting a backend

vibe-dictate doesn't bundle an ASR server — you point it at an
OpenAI-compatible endpoint someone (you, your team, OpenAI, an Azure
deployment, …) runs. This repo ships two minimal compose files that
stand a VibeVoice-backed endpoint up with zero checkout of the upstream
source: `transformers>=5.3` carries `microsoft/VibeVoice-ASR-HF`
natively, so the entrypoint just pip-installs the deps on first boot.

### Expected repo layout

```
vibe-dictate/                            <- this repo
  docker-compose-vibevoice.yml           <- x86_64 / consumer NVIDIA backend
  docker-compose-vibevoice-asr-gb10.yml  <- GB10 DGX Spark backend
  docker-compose-vibedictate-build.yml   <- Rust cross-compile pipeline
  Dockerfile.build                       <- builder image for the .exe
  setup_vibevoice.sh                     <- one-shot .env + pull (Linux/macOS)
  setup_vibevoice.bat                    <- one-shot .env + pull (Windows)
  scripts/openai_asr_server.py           <- FastAPI shim, OpenAI-compat
  scripts/openai_asr_entrypoint.sh       <- runtime pip install (transformers 5.x)
  .env.vibevoice.example                 <- copy to .env for x86_64
  .env.vibevoice-gb10.example            <- copy to .env for GB10
  src/                                   <- Rust client source
  vibevoice-lab/                         <- experimental playground (see its README)
```

Gradio demo, realtime TTS, vLLM experiments and the B-opció prebuilt
GB10 image all live under `vibevoice-lab/` — a self-contained stack
that ships its own compose file, `.env`, and setup script. None of it
is needed for dictation. Lift it out into its own project if/when you
want to experiment.

### One-shot setup

The backend host has a helper that seeds `.env`, pre-pulls the base
image, and optionally brings the stack up. Linux/macOS hosts use the
bash version, Windows hosts the batch twin:

```bash
# Linux/macOS (default --arch gb10)
./setup_vibevoice.sh                    # seed + pull
./setup_vibevoice.sh --arch x86 --up    # consumer NVIDIA + compose up -d
```

```bat
:: Windows (default --arch x86)
setup_vibevoice.bat
setup_vibevoice.bat --up
setup_vibevoice.bat --arch gb10
```

The sections below walk through the same steps manually.

### Option A — local Docker (RTX 4090 / dev workstation)

```bash
cp .env.vibevoice.example .env
# edit HUGGING_FACE_HUB_TOKEN if you have one
docker compose -f docker-compose-vibevoice.yml up -d
# OpenAI-compat ASR on http://localhost:8080
```

First boot downloads the ~16 GB model from HuggingFace; a token avoids
the anon rate limit. The runtime-install entrypoint runs once per
container lifetime (marker file at `/root/.vibevoice_openai_installed`)
— subsequent restarts skip pip entirely.

### Option B — NVIDIA GB10 DGX Spark (aarch64, sm_121)

For Grace Blackwell unified-memory machines:

```bash
cp .env.vibevoice-gb10.example .env
# edit VIBEVOICE_BASE_DIR (USB mount on GB10) + token
docker compose up -d    # COMPOSE_FILE is set in .env
```

Runs on `nvcr.io/nvidia/pytorch:25.12-py3` (CUDA 13, aarch64) and coexists
with other GPU workloads (e.g. a Gemma vLLM container) within the 128 GB
unified pool. Peak VRAM ~16-22 GB.

### Option C — managed cloud

Any OpenAI-compatible STT endpoint works out of the box:

- **OpenAI Whisper**: `base_url = "https://api.openai.com"`,
  `model = "whisper-1"`, `api_key = "sk-..."`.
- **Self-hosted vLLM / Open WebUI Whisper**: `base_url` = your endpoint,
  `model` = whatever that deployment serves.
- **VibeVoice on Azure AI Foundry**: set `base_url` to the deployed
  URL and drop the bearer token in `api_key`. The Azure Foundry endpoint
  speaks OpenAI-compat natively.

### Sanity-checking the backend

Before pointing vibe-dictate at it:

```bash
curl -fsS http://your-host:8080/healthz        # our shim
curl -fsS http://your-host:8080/v1/models      # works on anything OpenAI-compat
```

Either returns JSON when the server is alive. For a full round-trip
check, POST a WAV:

```bash
curl -fsS http://your-host:8080/v1/audio/transcriptions \
    -F file=@sample.wav \
    -F model=microsoft/VibeVoice-ASR-HF \
    -F language=Hungarian \
    -F response_format=json
```

---

## Building the client

There's no toolchain install required on the host — a Docker-based
cross-compile pipeline produces the Windows .exe from any Linux/macOS/
Windows machine that has Docker.

From this repo's root:

```bash
docker compose -f docker-compose-vibedictate-build.yml run --rm vibedictate-build
```

Output: `target/x86_64-pc-windows-msvc/release/vibe-dictate.exe`

- First build: ~5–10 min (cargo-xwin downloads MSVC headers + cold compile)
- Incremental: ~10–30 sec (cargo cache + xwin cache live in named volumes)

If you'd rather build natively on Windows, the standard Rust toolchain
works: `rustup target add x86_64-pc-windows-msvc && cargo build --release`.

### Build dependencies (auto-installed by the Docker image)

- Rust 1.89+ (edition 2021)
- `cargo-xwin` 0.18.6 for the MSVC target
- clang / lld / llvm

See `Dockerfile.build` for the exact recipe.

---

## Running

Double-click the produced `vibe-dictate.exe`. It goes straight to the tray
(blue square icon by default). Right-click for the menu.

On first launch, a config file is created at
`%APPDATA%\chestercs\vibe-dictate\config\config.toml` and a log file at
`%LOCALAPPDATA%\chestercs\vibe-dictate\cache\vibe-dictate.log`. Both are
openable from the tray menu ("Open config file" / "Open log file" — they
launch in `notepad.exe`).

### First-run checklist

1. **Backend up**: confirm `curl http://localhost:8080/v1/models` returns
   200, or set `server.base_url` to wherever yours runs.
2. **Hotkey**: default is `F8`. Change via tray → Hotkey → either pick a
   preset or `Rebind…` and press the combination you want.
3. **Microphone**: tray → Microphone → pick a device, or leave on
   `(System default)`.
4. **Input mode**: default is push-to-talk. Tray → Input → Voice
   activation flips the mic to always-on with VAD-based utterance
   slicing (useful for long-form dictation).
5. **Hold-to-talk** (PTT mode): hold the hotkey, speak, release. Tray
   icon turns green while recording. Transcription should appear in
   the focused window 1-3 seconds after release.
6. **Cancel**: double-tap the hotkey within 400 ms while recording,
   or single-press shortly after release to drop the in-flight
   transcription. In voice-activation mode the hotkey cancels
   whatever VAD just captured (or is still processing).

---

## Configuration reference

All settings live in `%APPDATA%\chestercs\vibe-dictate\config\config.toml`.
Most are also exposed in the tray menu, so you rarely need to edit the
file by hand. After a manual edit: tray → "Reload config".

```toml
[server]
base_url       = "http://localhost:8080"              # OpenAI-compat endpoint base URL (no trailing slash)
api_key        = "vibe-dictate-default-change-me"     # Bearer token; matches the default in the shipped .env.*.example so a fresh install is locked out of the box. Rotate on both ends before exposing publicly. Leave empty for no-auth.
model          = "microsoft/VibeVoice-ASR-HF"         # sent in multipart `model` field (ignored by single-model servers; use "whisper-1" for OpenAI)
extra_ca_cert  = ""                                   # absolute path to a PEM cert/bundle for self-signed CAs

[stt]
context_info    = "..."         # free-form prompt; reserved (not yet threaded to OpenAI-compat servers)
max_new_tokens  = 8192          # reserved; generation budget is enforced server-side
language_hint   = "Hungarian"   # full English language name; ISO codes ("hu") auto-expand

[audio]
mic_device  = ""        # WASAPI device name; empty = system default
sample_rate = 16000     # VibeVoice expects 16 kHz mono

[hotkey]
binding = "F8"          # see "Hotkey reference" below

[input]
mode = "push_to_talk"   # "push_to_talk" or "voice_activation"
                        # voice_activation = the mic stays hot; energy-based
                        # VAD chops speech into utterances and sends each
                        # one on its own. In that mode the hotkey binding
                        # above becomes a cancel-in-flight button.

[vad]
start_frames     = 3    # consecutive 20 ms frames of speech needed to open an utterance
end_frames       = 35   # consecutive 20 ms frames of silence that close it (≈ 700 ms)
max_seconds      = 30   # hard cap — utterance auto-closes regardless of silence
min_utterance_ms = 300  # drop utterances shorter than this (typing noise, throat clears)
speech_ratio     = 3.0  # RMS must exceed noise_floor × this to count as speech (~+9.5 dB SNR)
noise_floor_min  = 80.0 # absolute floor on the adaptive noise estimate

[output]
mode                    = "clipboard"  # "clipboard" or "sendinput"
trailing_space          = true         # append " " after transcription (helps when chaining utterances)
send_enter              = false        # press Enter after the text — useful in chat clients / terminals
send_key_delay_ms       = 20           # SendInput: ms sleep between characters; safe default, lower for faster typing on well-behaved editors
send_key_down_delay_ms  = 10           # SendInput: ms to hold each key "down" (some apps filter zero-duration presses; 10 ms is reliable & imperceptible)

[startup]
autostart       = false   # add to HKCU\...\Run on next save
start_minimized = true    # always-true for tray apps; reserved for future
```

Older configs that still carry a `[gradio]` section with `url` /
`api_token` load transparently (serde aliases) and get rewritten to
`[server]` / `base_url` / `api_key` on the next save. A one-shot
migration also bumps the legacy `http://localhost:7860` default to
`http://localhost:8080`.

### Editing remote-backend settings

For a remote VibeVoice host (e.g. behind Tailscale, internal CA, or a
public Cloudflare tunnel):

```toml
[server]
base_url      = "https://stt.example.internal"
api_key       = "sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxx"
model         = "microsoft/VibeVoice-ASR-HF"
extra_ca_cert = "C:/Users/you/certs/internal-ca.pem"
```

The bearer token is sent as `Authorization: Bearer <token>` on every
`POST /v1/audio/transcriptions`. The CA file may be a single PEM cert
or a concatenated bundle — reqwest's rustls backend handles both.

---

## Hotkey reference

vibe-dictate accepts a single push-to-talk binding, configured via
`hotkey.binding` or via the tray "Rebind…" capture popup.

### Accepted forms

- **Function keys**: `F1` … `F12`
- **Letters**: `A` … `Z`
- **Digits**: `0` … `9`
- **System keys**: `Pause`, `ScrollLock`
- **Mouse buttons**:
  - `Mouse3` (also `Middle`) — middle / scroll-wheel click
  - `Mouse4` (also `XButton1`) — back side button
  - `Mouse5` (also `XButton2`) — forward side button
- **Modifiers**: prefix any of the above with `Ctrl+`, `Shift+`, `Alt+`
  (combinable, e.g. `Ctrl+Shift+F12`, `Shift+Mouse4`)

### Why no Alt-only bindings?

Plain Alt collides with Windows app menus and AltGr (which is
`Ctrl+Alt` on Hungarian/German/etc. layouts). Alt-based hotkeys are
auto-migrated to `F8` on config load to avoid stuck-modifier bugs.
Use `Ctrl+Alt+...` if you must combine Alt with something else.

### Mouse buttons don't suppress the click

Mouse3/4/5 are intercepted by a low-level `WH_MOUSE_LL` hook but the
event still bubbles through to the foreground window. So middle-click
still pastes in editors, Mouse4/5 still navigate browser history, and
push-to-talk is purely additive.

---

## Tray menu reference

Right-click the tray icon to get:

- **Reload config** — re-reads `config.toml` from disk; useful after
  hand-edits or for picking up changes from other tools.
- **Input: Push-to-talk / Voice activation** — picks between the two
  modes. Push-to-talk is the default: the hotkey opens the mic while
  held. Voice activation keeps the mic always on and uses an internal
  energy-based VAD to slice speech into utterances; the hotkey then
  doubles as a cancel button for whatever the VAD just captured or is
  still processing.
- **Hotkey** — submenu with preset bindings (F7-F12, Pause, ScrollLock)
  + `Rebind…` (Win32 capture popup that accepts any key/mouse +
  modifiers). The active binding is checked. Custom captured bindings
  show up as "Custom: …".
- **Microphone** — system default + every WASAPI input device. Re-listed
  on each menu open so newly-plugged USB mics appear.
- **STT server** — opens text-input popups for base URL / API key /
  model id / CA cert path. The key isn't shown in the label (just
  "set" / "empty").
- **Language** — preset list of common languages + Custom… for ISO
  codes / less common names.
- **Edit context info…** — multi-purpose prompt used for language
  anchoring and code-mixing hints. (Reserved — the OpenAI Audio API has
  a `prompt` field but the VibeVoice shim currently ignores it; the
  value is kept in the config for future use.)
- **Max tokens** — generation budget presets (4k / 8k / 16k / 32k tokens
  ≈ 2.5 / 5 / 10 / 20 min audio). Server-enforced — the client only
  stores the preference.
- **Start with Windows** — toggles HKCU\Software\Microsoft\Windows\
  CurrentVersion\Run.
- **Output** → **Clipboard + Ctrl+V** / **SendInput (direct typing)** —
  see [Output modes](#output-modes). The submenu also exposes the
  SendInput pacing presets.
- **Append Enter after dictation** — toggles `output.send_enter`.
- **Open log file** / **Open config file** — both via notepad.exe.
- **Quit**.

---

## Output modes

Two ways to deliver the transcription to the focused window:

### Clipboard + Ctrl+V (default)

Copies the transcription to the clipboard, sends `Ctrl+V`, sleeps 120ms,
then restores the previous clipboard contents. Pros: works in essentially
every Windows app. Cons: uses the clipboard slot briefly; some apps with
custom paste handlers may behave unexpectedly.

### SendInput (direct typing)

Injects each UTF-16 code unit as a `KEYEVENTF_UNICODE` keystroke via the
Win32 `SendInput` API, paced by `output.send_key_delay_ms` between
characters (default 20 ms ≈ 50 chars/sec — safe for Electron, Notepad,
and most terminals) and `output.send_key_down_delay_ms` as the per-key
hold time (default 10 ms — imperceptible, but several apps drop
zero-duration presses). Errors and partial deliveries are logged.

Pros: no clipboard side-effects, clean for chat clients and terminals.

Cons:

- Slightly slower for very long dictations.
- Some applications filter injected input:
  - **Elevated targets**: a non-elevated vibe-dictate can't type into an
    admin-elevated window (Windows UIPI). Either run vibe-dictate as
    admin too, or stick with Clipboard mode for elevated apps.
  - **Games using DirectInput**: bypass SendInput entirely.
  - **Some hardened apps**: may reject `VK_PACKET` injected keys.

If SendInput silently drops events, the log will show
`SendInput partial: N/M events sent (last error: ...)`.

### Append Enter

Independent toggle that fires a single `VK_RETURN` keystroke after the
text is delivered. Works in both output modes. Useful when dictation is
also the "send" gesture (Discord, Slack, terminals, …).

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Tray icon never appears | Another instance is already running | Singleton check holds it back; check Task Manager for `vibe-dictate.exe` |
| Hotkey does nothing | Another app grabs it (Discord, OBS, GeForce Experience) | Pick a different binding via tray → Rebind… |
| Recording starts but transcription is empty / `[Silence]` | Wrong mic selected | Tray → Microphone → pick the actual physical input |
| `transcribe failed: HTTP 404` | Wrong base URL, or server doesn't expose `/v1/audio/transcriptions` | `curl <base_url>/v1/models` from the same machine |
| `transcribe failed: HTTP 401/403` | Missing / wrong bearer token | Set `server.api_key` via tray → STT server → API key |
| TLS errors against remote backend | Self-signed / private CA | Set `server.extra_ca_cert` to the PEM path |
| SendInput types nothing into focused window | Target window is elevated, or filters injected input | Switch to Clipboard mode, or run vibe-dictate as admin |
| Tray icon stuck on red after a cancel | Bug — please file an issue with logs | Reload config or restart |
| Hungarian text comes back in English | Language anchoring failed | Set `language_hint = "Hungarian"` in tray → Language |
| Truncated transcription on long audio | Server-side `max_new_tokens` too low | Restart the backend with a higher `VIBEVOICE_MAX_NEW_TOKENS` |

Logs live at `%LOCALAPPDATA%\chestercs\vibe-dictate\cache\vibe-dictate.log`.
The log level is `info` by default.

---

## Architecture

```
┌─────────────────┐    ┌─────────────────┐    ┌──────────────────┐
│   tao event     │    │  global-hotkey  │    │ WH_MOUSE_LL hook │
│   loop (main)   │◄───┤  (kb bindings)  │    │  (Mouse3/4/5)    │
│                 │    └─────────────────┘    └────────┬─────────┘
│   ticks every   │             ▲                      │
│   ~33ms,        │             │                      │
│   dispatches    │             │ BindingManager       │
│   PushAction    │◄────────────┴──── routes binding ──┘
│   ::Press/      │                  string to either
│   Release       │
│                 │
│                 │   ┌────────────┐   ┌──────────┐   ┌──────────┐
│                 │──►│ audio.rs   │──►│ openai.rs│──►│injector  │
│                 │   │ (cpal/     │   │ (reqwest │   │.rs       │
│                 │   │  WASAPI)   │   │ multipart│   │ (clip /  │
│                 │   │  WAV bytes │   │  POST)   │   │  SendIn) │
│                 │   └────────────┘   └──────────┘   └──────────┘
└─────────────────┘
        │
        ▼
┌─────────────────┐   ┌──────────────────────┐   ┌──────────────┐
│  tray.rs        │   │ hotkey_capture.rs    │   │ text_input.rs│
│  (right-click   │   │ (Win32 modal popup,  │   │ (Win32 modal │
│   menu, status  │   │  PeekMessage pump,   │   │  text-entry  │
│   icon)         │   │  worker thread)      │   │  popup)      │
└─────────────────┘   └──────────────────────┘   └──────────────┘
```

Module map:

| File | Responsibility |
|---|---|
| `src/main.rs` | tao event loop, BindingManager, dispatch glue |
| `src/audio.rs` | WASAPI capture via cpal, WAV encoding (hound) |
| `src/openai.rs` | reqwest blocking client: single multipart POST to `/v1/audio/transcriptions` |
| `src/injector.rs` | clipboard paste (arboard) + SendInput (windows-rs) |
| `src/tray.rs` | tray icon + menu construction + menu event handling |
| `src/hotkey_capture.rs` | Win32 modal that captures next key/mouse press |
| `src/mouse_hook.rs` | low-level WH_MOUSE_LL hook for Mouse3/4/5 PTT |
| `src/text_input.rs` | Win32 modal for free-form config field editing |
| `src/config.rs` | TOML serde + ProjectDirs paths + migrations |
| `src/autostart.rs` | HKCU Run-key toggle |
| `src/singleton.rs` | named-mutex single-instance lock |

### Why blocking reqwest, not async?

The whole app is single-threaded around the tao event loop with worker
threads for the long-running tasks. Blocking IO inside a thread is
simpler than dragging tokio in for one HTTP call per dictation.

### Why `cpal::Stream` is awkward

`cpal::Stream` is `!Send`. The recording thread builds the WAV bytes
locally and hands the bytes (which *are* `Send`) to a network worker.
Don't try to pass the Stream itself across thread boundaries.

### Why two Win32 modals (capture + text input) instead of native dialogs?

`tray-icon` and `tao` don't ship modal dialog primitives, and the
Win32 modals only need ~150 lines each. Pulling in a UI framework
(egui, slint, fltk) for two popups would dwarf the rest of the binary.

---

## Development

### Repo layout

```
vibe-dictate/
├── Cargo.toml          # crate manifest
├── Dockerfile.build    # rust + cargo-xwin builder image
├── README.md           # this file
├── CLAUDE.md           # guidance for Claude Code sessions
├── assets/             # icon resources
├── build.rs            # winres for embedded icon
├── src/                # see Architecture
└── target/             # cargo output (gitignored)
```

The Docker compose for the cross-compile lives at the repo root as
`docker-compose-vibedictate-build.yml`.

### Common workflows

**Rebuild after editing source**:

```bash
taskkill //IM vibe-dictate.exe //F  # Windows; release the exe lock
docker compose -f docker-compose-vibedictate-build.yml run --rm vibedictate-build
```

**Tail the log while testing**:

```powershell
Get-Content "$env:LOCALAPPDATA\chestercs\vibe-dictate\cache\vibe-dictate.log" -Wait -Tail 30
```

**Reset config to defaults**:

```bash
rm "$APPDATA\chestercs\vibe-dictate\config\config.toml"
# next launch will recreate from defaults
```

**Bump dependencies**:

```bash
cargo update --dry-run    # see what would change
cargo update
# rebuild + smoke-test
```

### Testing

There's no automated test suite — vibe-dictate is a thin glue layer over
Win32 + an HTTP call, both of which are awkward to mock. Smoke-test by
recording a few utterances in different output modes and target
windows. The architecture intentionally keeps the side-effecting code
(audio capture, HTTP, Win32 injection) thin so manual tests are quick.

### License

MIT. Upstream VibeVoice is also MIT.
