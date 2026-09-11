[CmdletBinding()]
param(
    [string]$SourceExe,
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA 'Programs\VibeVoice Dictation'),
    [string]$WslDistribution = 'Ubuntu',
    [string]$WslUser = 'qq110',
    [string]$WslRuntime = '/home/qq110/.local/share/vibevoice-dictation',
    [switch]$SkipBackendScripts,
    [switch]$SkipConfig,
    [switch]$SkipShortcuts
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

$repoRoot = Split-Path -Parent $PSScriptRoot
$launcherSource = Join-Path $PSScriptRoot 'launch-windows-streaming.ps1'
$appName = 'VibeVoice 语音输入'

function ConvertTo-BashSingleQuoted([string]$Value) {
    return "'" + $Value.Replace("'", "'\''") + "'"
}

function Invoke-Wsl([string]$Command) {
    $output = @(& wsl.exe -d $WslDistribution -u $WslUser -- bash -lc $Command 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "WSL command failed ($LASTEXITCODE): $Command`n$($output -join "`n")"
    }
    return $output
}

function Get-WslPath([string]$WindowsPath) {
    $resolved = (Resolve-Path -LiteralPath $WindowsPath).Path
    # WSL exposes fixed Windows drives at /mnt/<lowercase-drive>. Convert
    # locally instead of passing backslashes through wsl.exe, whose argument
    # conversion strips them when it receives an unquoted native path.
    if ($resolved -match '^([A-Za-z]):\\(.*)$') {
        return "/mnt/$($matches[1].ToLowerInvariant())/$($matches[2] -replace '\\', '/')"
    }
    throw "Cannot map Windows path to WSL /mnt path: $resolved"
}

function Resolve-SourceExePath {
    if ($SourceExe) {
        if (-not (Test-Path -LiteralPath $SourceExe -PathType Leaf)) {
            throw "Source executable was not found: $SourceExe"
        }
        return (Resolve-Path -LiteralPath $SourceExe).Path
    }

    $candidates = @(
        (Join-Path $repoRoot 'target\x86_64-pc-windows-msvc\release\vibe-dictate.exe'),
        (Join-Path $repoRoot 'target\release\vibe-dictate.exe')
    )
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }
    throw "No built vibe-dictate.exe found. Pass -SourceExe or build target\release first."
}

function New-StreamingConfig([string]$Path) {
    $config = @'
enabled = true

[server]
backend = "streaming"
base_url = "ws://127.0.0.1:7870/ws/asr"
api_key = ""
model = "microsoft/VibeVoice-ASR-Streaming-1.5B"
extra_ca_cert = ""

[stt]
context_info = "Use Chinese as the primary language. Preserve English technical terms, proper nouns, product names, and code verbatim. Transcribe exactly and do not translate."
max_new_tokens = 256
language_hint = "Chinese"

[audio]
mic_device = ""
sample_rate = 16000

[hotkey]
binding = "F8"

[input]
mode = "push_to_talk"

[vad]
start_frames = 3
end_frames = 35
max_seconds = 30
min_utterance_ms = 300
speech_ratio = 3.0
noise_floor_min = 80.0

[output]
mode = "clipboard"
trailing_space = false
send_enter = false
send_key_delay_ms = 20
send_key_down_delay_ms = 10
interactive_keystrokes = false

[startup]
autostart = false
start_minimized = true
'@
    $directory = Split-Path -Parent $Path
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    # The client reads TOML as UTF-8. Write the config without a BOM so older
    # TOML parsers do not treat the BOM as part of the first key.
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [IO.File]::WriteAllText($Path, $config, $utf8)
}

function Install-BackendScripts {
    $backendScripts = @(Get-ChildItem -LiteralPath $PSScriptRoot -File | Where-Object { $_.Name -like 'streaming*' })
    if ($backendScripts.Count -eq 0) {
        throw "No scripts/streaming* backend files were found. Add the backend first or pass -SkipBackendScripts."
    }

    $runtimeQ = ConvertTo-BashSingleQuoted $WslRuntime
    Invoke-Wsl "mkdir -p $runtimeQ/scripts"
    foreach ($script in $backendScripts) {
        $sourceWsl = Get-WslPath $script.FullName
        $sourceQ = ConvertTo-BashSingleQuoted $sourceWsl
        $targetQ = ConvertTo-BashSingleQuoted "$WslRuntime/scripts/$($script.Name)"
        Invoke-Wsl "install -m 0755 $sourceQ $targetQ"
    }
}

function New-AppShortcut([string]$Path, [string]$LauncherPath, [string]$TargetExe) {
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($Path)
    $shortcut.TargetPath = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $shortcut.Arguments = '-NoLogo -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "' + $LauncherPath + '"'
    $shortcut.WorkingDirectory = $InstallRoot
    $shortcut.IconLocation = "$TargetExe,0"
    $shortcut.Description = '启动 VibeVoice 中文语音输入'
    $shortcut.Save()
}

$source = Resolve-SourceExePath
if (-not (Test-Path -LiteralPath $launcherSource -PathType Leaf)) {
    throw "Launcher script was not found: $launcherSource"
}

if (-not $SkipBackendScripts) {
    $pythonQ = ConvertTo-BashSingleQuoted "$WslRuntime/.venv/bin/python"
    $upstreamQ = ConvertTo-BashSingleQuoted "$WslRuntime/upstream/demo/vibevoice_asr_streaming_fastapi_demo.py"
    try {
        Invoke-Wsl "test -x $pythonQ && test -f $upstreamQ"
    } catch {
        throw "The WSL model runtime must be provisioned before installing the client. Run scripts/setup-streaming.sh in $WslDistribution with VIBEVOICE_RUNTIME_ROOT=$WslRuntime, then retry. $($_.Exception.Message)"
    }
}

New-Item -ItemType Directory -Path $InstallRoot -Force | Out-Null
$installedExe = Join-Path $InstallRoot 'vibe-dictate.exe'
$installedLauncher = Join-Path $InstallRoot 'launch-windows-streaming.ps1'
Copy-Item -LiteralPath $source -Destination $installedExe -Force
Copy-Item -LiteralPath $launcherSource -Destination $installedLauncher -Force

if (-not $SkipBackendScripts) {
    Install-BackendScripts
}

if (-not $SkipConfig) {
    $configPath = Join-Path $env:APPDATA 'chestercs\vibe-dictate\config\config.toml'
    if (Test-Path -LiteralPath $configPath -PathType Leaf) {
        $existingConfig = Get-Content -LiteralPath $configPath -Raw
        $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
        $backupPath = "$configPath.bak-$stamp"
        Copy-Item -LiteralPath $configPath -Destination $backupPath -Force
        if ($existingConfig -match "(?im)^\s*backend\s*=\s*[`"']?streaming[`"']?" -and
            $existingConfig -match "(?im)^\s*base_url\s*=\s*[`"']?wss?://") {
            Write-Host "Existing streaming config preserved; backup written to $backupPath"
        } else {
            New-StreamingConfig $configPath
            Write-Host "Existing config backed up to $backupPath; wrote streaming config to $configPath"
        }
    } else {
        New-StreamingConfig $configPath
        Write-Host "Created streaming config at $configPath"
    }
}

if (-not $SkipShortcuts) {
    $startMenuDir = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
    $desktopDir = [Environment]::GetFolderPath('Desktop')
    New-Item -ItemType Directory -Path $startMenuDir -Force | Out-Null
    New-AppShortcut (Join-Path $startMenuDir "$appName.lnk") $installedLauncher $installedExe
    New-AppShortcut (Join-Path $desktopDir "$appName.lnk") $installedLauncher $installedExe
}

Write-Host "Installed $appName to $InstallRoot"
Write-Host 'The launcher starts the WSL backend on demand and stops only a backend it started.'
