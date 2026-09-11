# Windows streaming installation

The Windows client is installed under `%LOCALAPPDATA%\Programs\VibeVoice Dictation`. The Start Menu and Desktop shortcuts are both named `VibeVoice 语音输入`.

Hold **F8** to record. A non-activating card shows a red recording indicator, elapsed seconds, and the latest streaming text on the active monitor. Release F8 to finalize and paste the full transcript once into the focused input. The first model window needs about 3.47 seconds of audio; on this host the first visible partial arrived at 3.84 seconds. Shorter utterances may only produce text after release, while the recording indicator remains visible throughout capture.

For diagnostics, `vibe-dictate.exe --preview-overlay` displays a timed recording/processing card without recording audio. `vibe-dictate.exe --transcribe-file sample.wav --realtime --preview` sends a WAV in real time and displays actual model partials; it writes no text into another app unless `--inject` is also supplied.

Provision the isolated model runtime first. This requires Ubuntu WSL2, an NVIDIA driver with WSL CUDA support, Python 3.12, git, and uv. The setup downloads CUDA PyTorch and about 5.6 GB of model weights. From PowerShell in this checkout at `D:\WorkSpace\vibe-dictate`:

```powershell
wsl.exe -d Ubuntu -u qq110 -- bash /mnt/d/WorkSpace/vibe-dictate/scripts/setup-streaming.sh
```

On this host, the HTTPS inspection certificate is available at `/home/qq110/.local/share/vibevoice-dictation/proxy-ca.pem`. If setup is repeated behind the same proxy, pass `env VIBEVOICE_CA_BUNDLE=/home/qq110/.local/share/vibevoice-dictation/proxy-ca.pem` before `bash`; TLS verification stays enabled.

Then build the native client and run the installer:

```powershell
cargo build --release --locked --target x86_64-pc-windows-msvc
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-windows-streaming.ps1
```

The installer also accepts `-SourceExe` when the executable is in a different build directory. It checks that the WSL Python environment and official demo exist before installing, then copies every `scripts/streaming*` file to `/home/qq110/.local/share/vibevoice-dictation/scripts/` in the `Ubuntu` WSL distribution as user `qq110`. Use `-SkipBackendScripts` only when those runtime scripts were already installed.

On a fresh install the client config is created at `%APPDATA%\chestercs\vibe-dictate\config\config.toml` with streaming transport, `ws://127.0.0.1:7870/ws/asr`, Chinese plus English technical terms, F8 push-to-talk, clipboard output, and no automatic login startup. Existing config is backed up to `.bak-YYYYMMDD-HHMMSS`; an existing streaming profile is preserved, and a legacy HTTP profile is replaced with the streaming profile.

The shortcut runs `launch-windows-streaming.ps1` with a hidden PowerShell window. The launcher:

1. Acquires a named Windows mutex so duplicate shortcuts do nothing.
2. Starts a hidden WSL host running `streaming-runtime.sh start --hold`, captures its exact startup PID, and waits up to 180 seconds for `GET http://127.0.0.1:7870/healthz` to return JSON with `status: "ok"` and a non-empty `model`.
3. Starts the native client hidden and waits for it to exit.
4. Stops the backend only when this launcher received the `started pid=<pid>` result and that same owned PID is still current.

The runtime script has this small machine-readable contract:

| Command | Success output and exit code |
|---|---|
| `start [--hold]` | Prints `started pid=<pid>` or `already running pid=<pid>`; `--hold` keeps the foreground WSL command alive for its newly started process |
| `status` | `running pid=<pid>` with exit `0`, or `stopped` with exit `1` |
| `stop [expected-pid]` | `stopped` or `stopped pid=<pid>`; exit `0`; refuses a different PID or failed ownership check |

The foreground holder is necessary on this host: a detached `nohup` child alone stops when the last foreground WSL command exits. The launcher reads the startup line asynchronously, keeps that hidden host alive, and treats only its exact `started pid=<pid>` as owned. The launcher log is `%LOCALAPPDATA%\Programs\VibeVoice Dictation\launch-windows-streaming.log`; backend logs stay in the WSL runtime directory.

The scripts do not register login autostart. To remove the client, close it, delete the installed directory, and remove the two `.lnk` files from the Start Menu and Desktop.
