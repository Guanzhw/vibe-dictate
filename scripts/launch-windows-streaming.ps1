[CmdletBinding()]
param(
    [string]$InstallRoot,
    [string]$WslDistribution = 'Ubuntu',
    [string]$WslUser = 'qq110',
    [string]$WslRuntime = '/home/qq110/.local/share/vibevoice-dictation',
    [string]$HealthUrl = 'http://127.0.0.1:7870/healthz',
    [int]$ReadinessTimeoutSec = 180
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

if (-not $InstallRoot) { $InstallRoot = Split-Path -Parent $PSCommandPath }
$exePath = Join-Path $InstallRoot 'vibe-dictate.exe'
$logPath = Join-Path $InstallRoot 'launch-windows-streaming.log'
$mutexName = 'Local\VibeVoiceDictationStreamingLauncher'
$runtimeScript = "$WslRuntime/scripts/streaming-runtime.sh"
$mutex = $null
$mutexCreated = $false
$ownedBackendPid = $null
$backendHost = $null
$failure = $null

function Write-LauncherLog([string]$Message) {
    $line = "$(Get-Date -Format o) $Message"
    Add-Content -LiteralPath $logPath -Value $line
}

function Show-LauncherError([string]$Message) {
    try {
        Add-Type -AssemblyName System.Windows.Forms
        [System.Windows.Forms.MessageBox]::Show(
            $Message,
            'VibeVoice 语音输入启动失败',
            [System.Windows.Forms.MessageBoxButtons]::OK,
            [System.Windows.Forms.MessageBoxIcon]::Error) | Out-Null
    } catch {
        Write-Error $Message
    }
}

function Invoke-WslHidden([string[]]$CommandArguments) {
    $stdout = [System.IO.Path]::GetTempFileName()
    $stderr = [System.IO.Path]::GetTempFileName()
    try {
        $process = Start-Process -FilePath 'wsl.exe' -WindowStyle Hidden -Wait -PassThru `
            -ArgumentList (@('-d', $WslDistribution, '-u', $WslUser, '--') + $CommandArguments) `
            -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        $out = @()
        if (Test-Path -LiteralPath $stdout) { $out += Get-Content -LiteralPath $stdout }
        if (Test-Path -LiteralPath $stderr) { $out += Get-Content -LiteralPath $stderr }
        return [pscustomobject]@{ ExitCode = $process.ExitCode; Output = @($out) }
    } finally {
        if (Test-Path -LiteralPath $stdout) { Remove-Item -LiteralPath $stdout -Force }
        if (Test-Path -LiteralPath $stderr) { Remove-Item -LiteralPath $stderr -Force }
    }
}

function Start-OwnedBackend {
    $stdout = Join-Path $InstallRoot 'backend-host.stdout.log'
    $stderr = Join-Path $InstallRoot 'backend-host.stderr.log'
    # Keep a foreground WSL command alive while its model process exists.
    # A detached nohup child alone does not keep this host's WSL instance alive.
    $script:backendHost = Start-Process -FilePath 'wsl.exe' -WindowStyle Hidden -PassThru `
        -ArgumentList @('-d', $WslDistribution, '-u', $WslUser, '--', 'env', "VIBEVOICE_RUNTIME_ROOT=$WslRuntime", 'bash', $runtimeScript, 'start', '--hold') `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $deadline = (Get-Date).AddSeconds(20)
    do {
        $joined = if (Test-Path -LiteralPath $stdout) { [string](Get-Content -LiteralPath $stdout -Raw) } else { '' }
        if ($joined -match '(?im)^started\s+pid=(\d+)\b') {
            $script:ownedBackendPid = $matches[1]
            Write-LauncherLog "Started owned backend: $($joined.Trim())"
            return
        }
        if ($joined -match '(?im)^already running\s+pid=') {
            Write-LauncherLog "Using existing backend: $($joined.Trim())"
            return
        }
        if ($script:backendHost.HasExited) {
            $detail = [string](Get-Content -LiteralPath $stderr -Raw)
            throw "WSL backend host exited ($($script:backendHost.ExitCode)): $joined $detail"
        }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    throw 'WSL backend did not report a startup PID within 20 seconds.'
}

function Wait-ForBackendHostExit {
    if ($script:backendHost -and -not $script:backendHost.HasExited) {
        if (-not $script:backendHost.WaitForExit(5000)) {
            Write-LauncherLog 'Backend host is still exiting; leaving its PID ownership check in control.'
        }
    }
}

function Stop-OwnedBackend {
    if (-not $script:ownedBackendPid) { return }
    $result = Invoke-WslHidden @('env', "VIBEVOICE_RUNTIME_ROOT=$WslRuntime", 'bash', $runtimeScript, 'stop', $script:ownedBackendPid)
    Write-LauncherLog "Stopped owned backend (exit $($result.ExitCode)): $($result.Output -join ' ')"
    $script:ownedBackendPid = $null
}

function Wait-ForBackend {
    $deadline = (Get-Date).AddSeconds($ReadinessTimeoutSec)
    do {
        $response = $null
        try {
            $response = Invoke-WebRequest -UseBasicParsing -Uri $HealthUrl -TimeoutSec 2
        } catch {
            # Retry only transport failures while the service starts.
        }
        if ($response -and $response.StatusCode -eq 200) {
                $health = $response.Content | ConvertFrom-Json
                if ($health.status -eq 'error') {
                    $detail = [string]$health.error
                    if ([string]::IsNullOrWhiteSpace($detail)) { $detail = 'backend reported status=error' }
                    throw "Streaming backend failed while loading: $detail"
                }
                if ($health.status -eq 'ok' -and -not [string]::IsNullOrWhiteSpace([string]$health.model)) {
                    Write-LauncherLog "Backend ready: status=$($health.status), model=$($health.model)"
                    return
                }
        }
        Start-Sleep -Seconds 1
    } while ((Get-Date) -lt $deadline)
    throw "Streaming backend did not become ready within $ReadinessTimeoutSec seconds at $HealthUrl."
}

try {
    New-Item -ItemType Directory -Path $InstallRoot -Force | Out-Null
    Write-LauncherLog 'Launcher started.'
    $mutex = [System.Threading.Mutex]::new($false, $mutexName, [ref]$mutexCreated)
    if (-not $mutexCreated) {
        Write-LauncherLog 'Another launcher instance is already running.'
        return
    }
    if (-not (Test-Path -LiteralPath $exePath -PathType Leaf)) {
        throw "Installed executable was not found: $exePath"
    }

    Start-OwnedBackend
    Wait-ForBackend
    $app = Start-Process -FilePath $exePath -WorkingDirectory $InstallRoot -WindowStyle Hidden -PassThru
    Write-LauncherLog "Native client started (PID $($app.Id))."
    $app.WaitForExit()
    Write-LauncherLog "Native client exited with code $($app.ExitCode)."
} catch {
    $failure = $_.Exception.Message
    Write-LauncherLog "ERROR: $failure"
} finally {
    try { Stop-OwnedBackend } catch { Write-LauncherLog "ERROR stopping owned backend: $($_.Exception.Message)" }
    Wait-ForBackendHostExit
    if ($mutex -and $mutexCreated) {
        try { $mutex.ReleaseMutex() | Out-Null } catch { }
    }
    if ($mutex) { $mutex.Dispose() }
}

if ($failure) {
    Show-LauncherError "$failure`n`nDetails: $logPath"
    exit 1
}
