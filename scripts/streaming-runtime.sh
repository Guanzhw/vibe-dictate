#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
RUNTIME_ROOT="${VIBEVOICE_RUNTIME_ROOT:-/home/qq110/.local/share/vibevoice-dictation}"
PYTHON="$RUNTIME_ROOT/.venv/bin/python"
SERVER="$SCRIPT_DIR/streaming_asr_server.py"
PID_FILE="$RUNTIME_ROOT/streaming.pid"
LOG_FILE="$RUNTIME_ROOT/logs/streaming.log"
PORT="${VIBEVOICE_STREAMING_PORT:-7870}"

mkdir -p "$RUNTIME_ROOT/logs"

read_pid() {
    [[ -s "$PID_FILE" ]] || return 1
    local pid
    pid="$(tr -dc '0-9' < "$PID_FILE")"
    [[ -n "$pid" ]] || return 1
    printf '%s\n' "$pid"
}

owned_pid() {
    local pid="$1" cmdline
    [[ -r "/proc/$pid/cmdline" ]] || return 1
    cmdline="$(tr '\0' ' ' < "/proc/$pid/cmdline")"
    [[ "$cmdline" == *"$SERVER"* && "$cmdline" == *"--runtime-root $RUNTIME_ROOT"* ]]
}

running_pid() {
    local pid
    pid="$(read_pid 2>/dev/null || true)"
    [[ -n "$pid" ]] || return 1
    owned_pid "$pid" || return 1
    printf '%s\n' "$pid"
}

status() {
    local pid
    if pid="$(running_pid 2>/dev/null)"; then
        printf 'running pid=%s\n' "$pid"
        if command -v curl >/dev/null 2>&1; then
            curl --silent --show-error --max-time 2 "http://127.0.0.1:$PORT/healthz" || true
            printf '\n'
        fi
        return 0
    fi
    printf 'stopped\n'
    return 1
}

start() {
    local hold="${1:-}"
    if [[ -n "$hold" && "$hold" != "--hold" ]]; then
        echo "usage: $0 start [--hold]" >&2
        return 2
    fi
    if pid="$(running_pid 2>/dev/null)"; then
        printf 'already running pid=%s\n' "$pid"
        return 0
    fi
    [[ -x "$PYTHON" ]] || { echo "streaming venv missing: $PYTHON" >&2; return 1; }
    [[ -f "$SERVER" ]] || { echo "streaming server missing: $SERVER" >&2; return 1; }
    rm -f "$PID_FILE"
    : > "$LOG_FILE"
    nohup env \
        HF_HOME="$RUNTIME_ROOT/model-cache" \
        "$PYTHON" "$SERVER" \
        --runtime-root "$RUNTIME_ROOT" \
        --host 127.0.0.1 \
        --port "$PORT" \
        >> "$LOG_FILE" 2>&1 < /dev/null &
    pid=$!
    printf '%s\n' "$pid" > "$PID_FILE"
    printf 'started pid=%s log=%s\n' "$pid" "$LOG_FILE"
    if [[ "$hold" == "--hold" ]]; then
        # WSL ends this command's process tree when its foreground command
        # exits.  Keep this shell alive for exactly the ASR process we just
        # started; stop() makes the ownership check fail, so this loop exits.
        while owned_pid "$pid"; do
            sleep 1
        done
    fi
}

stop() {
    local pid deadline expected_pid="${1:-}"
    pid="$(read_pid 2>/dev/null || true)"
    if [[ -z "$pid" ]]; then
        printf 'stopped\n'
        return 0
    fi
    if [[ -n "$expected_pid" && "$pid" != "$expected_pid" ]]; then
        echo "refusing to stop pid $pid: expected pid $expected_pid" >&2
        return 1
    fi
    if ! owned_pid "$pid"; then
        echo "refusing to signal pid $pid: ownership check failed" >&2
        return 1
    fi
    kill -TERM "$pid"
    deadline=$((SECONDS + 10))
    while kill -0 "$pid" 2>/dev/null; do
        (( SECONDS >= deadline )) && break
        sleep 0.2
    done
    if kill -0 "$pid" 2>/dev/null; then
        if owned_pid "$pid"; then
            kill -KILL "$pid"
        else
            echo "refusing to force-stop pid $pid after ownership changed" >&2
            return 1
        fi
    fi
    rm -f "$PID_FILE"
    printf 'stopped pid=%s\n' "$pid"
}

case "${1:-status}" in
    start) shift; start "${1:-}" ;;
    stop) shift; stop "$@" ;;
    status) status ;;
    *) echo "usage: $0 start [--hold]|stop [expected-pid]|status" >&2; exit 2 ;;
esac
