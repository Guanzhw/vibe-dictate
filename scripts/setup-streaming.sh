#!/usr/bin/env bash
set -euo pipefail

# Reproducible WSL-native setup for the Microsoft VibeVoice streaming model.
# Runtime state is intentionally kept outside the checkout so the Windows
# launcher can start and stop it without modifying the user's repository.

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
RUNTIME_ROOT="${VIBEVOICE_RUNTIME_ROOT:-/home/qq110/.local/share/vibevoice-dictation}"
UPSTREAM_COMMIT="1541f590c7099820f10ea012f48d2399282df69f"
MODEL_ID="${VIBEVOICE_STREAMING_MODEL:-microsoft/VibeVoice-ASR-Streaming-1.5B}"
UV_BIN="${UV_BIN:-/home/qq110/.local/bin/uv}"
PYTHON_BIN="${PYTHON_BIN:-python3.12}"
export VIBEVOICE_STREAMING_MODEL="$MODEL_ID"

# If the host uses a private HTTPS inspection root, point this at its PEM
# bundle.  Requests, git, and Hugging Face then use normal certificate
# verification with that explicit trust root.
if [[ -n "${VIBEVOICE_CA_BUNDLE:-}" ]]; then
    [[ -f "$VIBEVOICE_CA_BUNDLE" ]] || {
        echo "[setup-streaming] CA bundle not found: $VIBEVOICE_CA_BUNDLE" >&2
        exit 1
    }
    export REQUESTS_CA_BUNDLE="$VIBEVOICE_CA_BUNDLE"
    export CURL_CA_BUNDLE="$VIBEVOICE_CA_BUNDLE"
fi

if [[ ! -x "$UV_BIN" ]]; then
    echo "[setup-streaming] uv not found at $UV_BIN" >&2
    exit 1
fi
command -v git >/dev/null 2>&1 || { echo "[setup-streaming] git is required" >&2; exit 1; }
command -v "$PYTHON_BIN" >/dev/null 2>&1 || { echo "[setup-streaming] $PYTHON_BIN is required" >&2; exit 1; }

mkdir -p "$RUNTIME_ROOT"/{logs,model-cache}

if [[ ! -d "$RUNTIME_ROOT/upstream/.git" ]]; then
    echo "[setup-streaming] cloning Microsoft/VibeVoice"
    git clone --filter=blob:none https://github.com/microsoft/VibeVoice.git "$RUNTIME_ROOT/upstream"
fi
git -C "$RUNTIME_ROOT/upstream" fetch --depth 1 origin "$UPSTREAM_COMMIT"
git -C "$RUNTIME_ROOT/upstream" checkout --detach "$UPSTREAM_COMMIT"
actual_commit="$(git -C "$RUNTIME_ROOT/upstream" rev-parse HEAD)"
[[ "$actual_commit" == "$UPSTREAM_COMMIT" ]] || {
    echo "[setup-streaming] upstream commit mismatch: $actual_commit" >&2
    exit 1
}
echo "[setup-streaming] upstream pinned at $actual_commit"

if [[ ! -x "$RUNTIME_ROOT/.venv/bin/python" ]]; then
    echo "[setup-streaming] creating Python 3.12 virtual environment"
    "$UV_BIN" venv --python "$PYTHON_BIN" "$RUNTIME_ROOT/.venv"
fi
PYTHON="$RUNTIME_ROOT/.venv/bin/python"
export HF_HOME="$RUNTIME_ROOT/model-cache"
export TRANSFORMERS_CACHE="$RUNTIME_ROOT/model-cache"

echo "[setup-streaming] installing CUDA PyTorch wheel"
"$UV_BIN" pip install --python "$PYTHON" \
    --index-url https://download.pytorch.org/whl/cu128 \
    "torch==2.8.0"

echo "[setup-streaming] installing pinned upstream package and runtime dependencies"
"$UV_BIN" pip install --python "$PYTHON" --no-deps -e "$RUNTIME_ROOT/upstream"
"$UV_BIN" pip install --python "$PYTHON" \
    "transformers==4.51.3" accelerate numpy tqdm \
    diffusers "uvicorn[standard]" fastapi "huggingface-hub==0.36.2"

echo "[setup-streaming] downloading $MODEL_ID into $RUNTIME_ROOT/model-cache"
"$PYTHON" - <<'PY'
import os
from huggingface_hub import snapshot_download

model_id = os.environ["VIBEVOICE_STREAMING_MODEL"]
cache_dir = os.environ["HF_HOME"]
snapshot_download(repo_id=model_id, cache_dir=cache_dir)
print(f"[setup-streaming] model cache ready: {model_id}")
PY

echo "[setup-streaming] verifying CUDA and model metadata"
"$PYTHON" - <<'PY'
import os
import torch
from huggingface_hub import snapshot_download

model_id = os.environ["VIBEVOICE_STREAMING_MODEL"]
print(f"torch={torch.__version__} cuda={torch.version.cuda} available={torch.cuda.is_available()}")
if not torch.cuda.is_available():
    raise SystemExit("CUDA is unavailable; refusing to call setup complete")
print(f"gpu={torch.cuda.get_device_name(0)} capability={torch.cuda.get_device_capability(0)}")
snapshot = snapshot_download(model_id, cache_dir=os.environ["HF_HOME"], local_files_only=True)
path = os.path.join(snapshot, "preprocessor_config.json")
if not os.path.isfile(path):
    raise SystemExit(f"model metadata missing: {path}")
print(f"preprocessor_config={path}")
PY

echo
echo "[setup-streaming] complete"
echo "  runtime: $RUNTIME_ROOT"
echo "  start:   $SCRIPT_DIR/streaming-runtime.sh start"
echo "  status:  $SCRIPT_DIR/streaming-runtime.sh status"
echo "  stop:    $SCRIPT_DIR/streaming-runtime.sh stop"
