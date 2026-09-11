#!/usr/bin/env python3
"""Local VibeVoice-ASR-Streaming websocket backend.

The model and streaming window logic are loaded from Microsoft's pinned
upstream demo.  This file only supplies the small localhost protocol used by
the native client, plus readiness and diagnostics endpoints.
"""

from __future__ import annotations

import argparse
import asyncio
import importlib.util
import json
import logging
import math
import os
import re
import traceback
from pathlib import Path
from types import ModuleType
from typing import Any

import numpy as np
from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse


LOG = logging.getLogger("vibevoice-streaming")
DEFAULT_MODEL = "microsoft/VibeVoice-ASR-Streaming-1.5B"

# The pinned streaming model emits ``Speaker 0:`` prefixes.  Dictation pastes
# the words while diagnostics retain the exact model output in `text`.
_SPEAKER_MARKERS = re.compile(r"(?<!\w)speaker\s+\d+\s*:\s*", re.IGNORECASE)
_CONTROL_MARKERS = re.compile(r"<\|(?:text_chunk_end|eos|endoftext)\|>|\[Silence\]", re.IGNORECASE)


def clean_text(raw_text: str) -> str:
    """Remove model-only speaker/control markers from pasteable text."""

    cleaned = _SPEAKER_MARKERS.sub("", raw_text)
    cleaned = _CONTROL_MARKERS.sub("", cleaned)
    return re.sub(r"[ \t]{2,}", " ", cleaned).strip()


class Backend:
    def __init__(self, runtime_root: Path, model_id: str, device: str, attention: str):
        self.runtime_root = runtime_root
        self.model_id = model_id
        self.device = device
        self.attention = attention
        self.status = "loading"
        self.error: str | None = None
        self.demo: ModuleType | None = None
        self.loaded_model: Any = None
        self.loaded_processor: Any = None
        self.sample_rate: int | None = None
        self.chunk_seconds: float | None = None
        self.chunk_samples = 0
        self.window_samples = 0
        self.gpu_lock = asyncio.Lock()
        self.load_task: asyncio.Task[None] | None = None

    def health(self) -> dict[str, Any]:
        result: dict[str, Any] = {
            "status": self.status,
            "model": self.model_id,
            "device": self.device,
            "attention": self.attention,
        }
        if self.status == "ok":
            result["sample_rate"] = self.sample_rate
            result["chunk_seconds"] = self.chunk_seconds
        if self.error:
            result["error"] = self.error
        return result

    def config(self) -> dict[str, Any]:
        result: dict[str, Any] = {"status": self.status}
        if self.status == "ok":
            result.update(sample_rate=self.sample_rate, chunk_seconds=self.chunk_seconds)
        if self.error:
            result["error"] = self.error
        return result

    def _load_upstream_demo(self) -> ModuleType:
        demo_path = self.runtime_root / "upstream" / "demo" / (
            "vibevoice_asr_streaming_fastapi_demo.py"
        )
        if not demo_path.is_file():
            raise RuntimeError(f"pinned upstream demo is missing: {demo_path}")
        spec = importlib.util.spec_from_file_location("vibevoice_upstream_streaming_demo", demo_path)
        if spec is None or spec.loader is None:
            raise RuntimeError(f"cannot import upstream demo: {demo_path}")
        # Upstream's pinned model config contains a dtype object that old
        # Transformers tries to render at INFO level as JSON.  Suppress that
        # diagnostic while retaining the official loader and model code.
        from transformers.utils import logging as transformers_logging

        transformers_logging.set_verbosity_error()
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    def load_sync(self) -> None:
        os.environ.setdefault("HF_HOME", str(self.runtime_root / "model-cache"))
        demo = self._load_upstream_demo()
        # Transformers 4.51.3 evaluates this representation before logging
        # the upstream config; that config contains a torch.dtype object in a
        # nested decoder config.  Keep the official loader while making this
        # diagnostic serialization tolerant of that typed field.
        from transformers.configuration_utils import PretrainedConfig
        import torch

        original_to_json_string = PretrainedConfig.to_json_string

        def safe_to_json_string(config: Any, *args: Any, **kwargs: Any) -> str:
            try:
                return original_to_json_string(config, *args, **kwargs)
            except TypeError:
                def encode_dtype(value: Any) -> str:
                    if isinstance(value, torch.dtype):
                        return str(value).split(".")[-1]
                    raise TypeError(f"unsupported config value: {type(value).__name__}")

                return json.dumps(
                    config.to_dict(),
                    indent=2,
                    sort_keys=True,
                    default=encode_dtype,
                ) + "\n"

        PretrainedConfig.to_json_string = safe_to_json_string  # type: ignore[assignment]
        # The pinned upstream demo uses the Transformers 5.x `dtype` keyword.
        # Transformers 4.51.3 accepts only `torch_dtype`, and silently ignores
        # the former, constructing the 1.5B model in fp32 before dispatch.
        # Adapt that one official loader call so the requested bf16 load is
        # effective on the 12 GB Blackwell card.
        model_class = demo.VibeVoiceASRForConditionalGeneration
        original_from_pretrained = model_class.from_pretrained

        def compat_from_pretrained(path: Any, *args: Any, **kwargs: Any) -> Any:
            if "dtype" in kwargs and "torch_dtype" not in kwargs:
                kwargs["torch_dtype"] = kwargs.pop("dtype")
            return original_from_pretrained(path, *args, **kwargs)

        model_class.from_pretrained = staticmethod(compat_from_pretrained)
        model_source: str = self.model_id
        # The upstream demo resolves a repo id's preprocessor metadata online
        # before loading.  Setup has already materialized the public model in
        # our private cache, so resolve that snapshot locally at runtime.
        if self.model_id.startswith("microsoft/"):
            from huggingface_hub import snapshot_download

            model_source = snapshot_download(
                self.model_id,
                cache_dir=str(self.runtime_root / "model-cache"),
                local_files_only=True,
            )
        demo.load_model(model_source, self.device, self.attention)
        self.demo = demo
        self.loaded_model = demo.state["model"]
        self.loaded_processor = demo.state["tokenizer"]
        self.sample_rate = int(demo.state["sample_rate"])
        self.chunk_samples = int(demo.state["chunk_samples"])
        self.window_samples = int(demo.state["window_samples"])
        self.chunk_seconds = float(demo.state["chunk_seconds"])
        LOG.info("checkpoint sample rate is %d Hz", self.sample_rate)
        LOG.info(
            "model ready: %s device=%s chunk=%.3fs window=%d samples",
            self.model_id,
            self.device,
            self.chunk_seconds,
            self.window_samples,
        )

    async def load(self) -> None:
        try:
            await asyncio.to_thread(self.load_sync)
        except BaseException as exc:  # model loaders may raise SystemExit
            self.status = "error"
            self.error = f"{type(exc).__name__}: {exc}"
            LOG.error("model load failed: %s\n%s", self.error, traceback.format_exc())
            return
        self.status = "ok"

    async def transcribe_window(self, window: np.ndarray, session: dict[str, Any]) -> str:
        if self.demo is None:
            raise RuntimeError("model is not loaded")
        async with self.gpu_lock:
            return await asyncio.to_thread(self.demo.transcribe_window, window, session)


def _json_error(message: str) -> dict[str, str]:
    return {"error": message}


def _option_number(value: Any, default: float, *, integer: bool = False) -> float | int:
    if value is None:
        return int(default) if integer else default
    parsed = int(value) if integer else float(value)
    if not math.isfinite(float(parsed)):
        raise ValueError("option must be finite")
    return parsed


def build_app(backend: Backend) -> FastAPI:
    app = FastAPI(title="VibeVoice local streaming ASR")

    @app.on_event("startup")
    async def start_model_load() -> None:
        backend.load_task = asyncio.create_task(backend.load())

    @app.get("/healthz")
    async def healthz() -> JSONResponse:
        return JSONResponse(backend.health())

    @app.get("/config")
    async def config() -> JSONResponse:
        return JSONResponse(backend.config())

    @app.websocket("/ws/asr")
    async def ws_asr(ws: WebSocket) -> None:
        await ws.accept()
        if backend.status != "ok":
            await ws.send_json(_json_error(f"model not ready: {backend.status}"))
            await ws.close(code=1013)
            return

        try:
            first = await ws.receive_text()
            opts = json.loads(first)
            if not isinstance(opts, dict):
                raise ValueError("first websocket message must be a JSON object")
            max_tokens = int(_option_number(opts.get("max_tokens"), 256, integer=True))
            if max_tokens < 1 or max_tokens > 4096:
                raise ValueError("max_tokens must be between 1 and 4096")
            temperature = float(_option_number(opts.get("temperature"), 0.0))
            if temperature < 0:
                raise ValueError("temperature must be non-negative")
        except Exception as exc:
            await ws.send_json(_json_error(str(exc)))
            await ws.close(code=1003)
            return

        assert backend.demo is not None
        model = backend.loaded_model
        tokenizer = backend.loaded_processor
        try:
            async with backend.gpu_lock:
                stream = await asyncio.to_thread(
                    model.init_streaming_state,
                    tokenizer,
                    context_info=opts.get("context_info") or None,
                )
            session = {
                "stream": stream,
                "max_tokens": max_tokens,
                "temperature": temperature,
            }
            buffer = np.zeros(0, dtype=np.float32)
            raw_chunks: list[str] = []

            async def drain(flush: bool) -> None:
                nonlocal buffer
                while True:
                    available = len(buffer)
                    if available >= backend.window_samples:
                        window = buffer[: backend.window_samples]
                    elif flush and available > 0:
                        window = np.zeros(backend.window_samples, dtype=np.float32)
                        window[:available] = buffer
                    else:
                        return
                    raw_chunks.append(await backend.transcribe_window(window, session))
                    buffer = buffer[backend.chunk_samples :]
                    raw_text = "".join(raw_chunks)
                    await ws.send_json(
                        {
                            "chunks": len(raw_chunks),
                            "text": raw_text,
                            "clean_text": clean_text(raw_text),
                        }
                    )
                    if flush and len(buffer) <= 0:
                        return

            while True:
                message = await ws.receive()
                if message.get("type") == "websocket.disconnect":
                    return
                if message.get("bytes") is not None:
                    payload = message["bytes"]
                    if len(payload) % 4:
                        raise ValueError("audio payload length must be a multiple of 4 bytes")
                    pcm = np.frombuffer(payload, dtype="<f4")
                    buffer = np.concatenate([buffer, pcm])
                    await drain(flush=False)
                elif message.get("text") == "end":
                    await drain(flush=True)
                    raw_text = "".join(raw_chunks)
                    await ws.send_json(
                        {
                            "done": True,
                            "text": raw_text,
                            "clean_text": clean_text(raw_text),
                            "total_chunks": len(raw_chunks),
                        }
                    )
                    return
        except WebSocketDisconnect:
            return
        except Exception as exc:
            LOG.exception("streaming websocket failed")
            try:
                await ws.send_json(_json_error(str(exc)))
            except Exception:
                pass

    return app


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime-root", default=os.environ.get("VIBEVOICE_RUNTIME_ROOT", "/home/qq110/.local/share/vibevoice-dictation"))
    parser.add_argument("--model", default=os.environ.get("VIBEVOICE_STREAMING_MODEL", DEFAULT_MODEL))
    parser.add_argument("--host", default=os.environ.get("VIBEVOICE_STREAMING_HOST", "127.0.0.1"))
    parser.add_argument("--port", type=int, default=int(os.environ.get("VIBEVOICE_STREAMING_PORT", "7870")))
    parser.add_argument("--device", default=os.environ.get("VIBEVOICE_STREAMING_DEVICE", "cuda"))
    parser.add_argument("--attention", default=os.environ.get("VIBEVOICE_STREAMING_ATTENTION", "sdpa"))
    parser.add_argument("--log-level", default=os.environ.get("VIBEVOICE_STREAMING_LOG_LEVEL", "info"))
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    logging.basicConfig(
        level=args.log_level.upper(),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    runtime_root = Path(args.runtime_root).expanduser().resolve()
    backend = Backend(runtime_root, args.model, args.device, args.attention)
    LOG.info("starting localhost backend on %s:%d (model status=loading)", args.host, args.port)
    import uvicorn

    uvicorn.run(build_app(backend), host=args.host, port=args.port, log_level=args.log_level)


if __name__ == "__main__":
    main()
