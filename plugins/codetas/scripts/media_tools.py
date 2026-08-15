"""CODETAS media capability helpers used by the Codex-facing MCP server."""

from __future__ import annotations

import base64
import json
import mimetypes
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Any
from urllib import error, request


MAX_IMAGE_BYTES = 18 * 1024 * 1024
MAX_GATEWAY_RESPONSE_BYTES = 48 * 1024 * 1024
MAX_ANALYSIS_ITEM_CHARS = 12 * 1024 * 1024
MAX_ANALYSIS_BATCH_CHARS = 20 * 1024 * 1024
MAX_ANALYSIS_BATCH_ITEMS = 4
MAX_ANALYSIS_RESULT_CHARS = 160 * 1024
MAX_SETTINGS_BYTES = 4 * 1024 * 1024


def _settings_candidates() -> list[Path]:
    explicit = os.environ.get("CODETAS_SETTINGS") or os.environ.get("CODETAS_SETTINGS_PATH")
    candidates = [Path(explicit).expanduser()] if explicit else []
    home = Path.home()
    if sys.platform == "darwin":
        candidates.append(home / "Library/Application Support/jp.kinocode.codetas/providers.json")
    elif os.name == "nt":
        app_data = os.environ.get("APPDATA")
        if app_data:
            candidates.append(Path(app_data) / "jp.kinocode.codetas/providers.json")
    else:
        config_home = Path(os.environ.get("XDG_CONFIG_HOME", home / ".config"))
        candidates.append(config_home / "jp.kinocode.codetas/providers.json")
    return candidates


def _read_codetas_settings() -> tuple[Path | None, dict[str, Any]]:
    for path in _settings_candidates():
        try:
            if not path.is_file() or path.stat().st_size > MAX_SETTINGS_BYTES:
                continue
            value = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(value, dict):
                return path, value
        except (OSError, ValueError):
            continue
    return None, {}


def _safe_loopback_url(value: Any) -> str | None:
    url = str(value or "").rstrip("/")
    safe = (
        url.startswith("http://127.0.0.1:")
        or url.startswith("http://[::1]:")
        or url.startswith("http://localhost:")
    )
    if not safe or len(url) > 512:
        return None
    return url[:-3] if url.endswith("/v1") else url


def _gateway_root() -> str:
    explicit = os.environ.get("CODETAS_GATEWAY_URL")
    if explicit:
        return explicit.rstrip("/")[:-3] if explicit.rstrip("/").endswith("/v1") else explicit.rstrip("/")
    settings_path, settings = _read_codetas_settings()
    if settings_path:
        runtime_path = settings_path.with_name("gateway-runtime.json")
        try:
            if runtime_path.is_file() and runtime_path.stat().st_size <= 64 * 1024:
                runtime = json.loads(runtime_path.read_text(encoding="utf-8"))
                if runtime.get("version") == 1:
                    runtime_url = _safe_loopback_url(runtime.get("url"))
                    if runtime_url:
                        return runtime_url
        except (OSError, ValueError):
            pass
    runtime = settings.get("runtime") or {}
    port = runtime.get("port", 42421)
    if isinstance(port, int) and 1 <= port <= 65535:
        return f"http://127.0.0.1:{port}"
    return "http://127.0.0.1:42421"


def _configured_gateway_token() -> str | None:
    token_env = os.environ.get("CODETAS_GATEWAY_TOKEN_ENV", "").strip()
    if token_env:
        token = os.environ.get(token_env, "")
        if token:
            return token
    now = int(time.time())
    _, settings = _read_codetas_settings()
    keys = ((settings.get("security") or {}).get("externalAccessKeys") or [])
    for key in keys:
        if not isinstance(key, dict) or not key.get("enabled"):
            continue
        expires = key.get("expiresAtUnix")
        if isinstance(expires, (int, float)) and expires <= now:
            continue
        scopes = key.get("scopes") or []
        if "gateway:*" not in scopes and "sidecars:write" not in scopes:
            continue
        env_var = str(key.get("envVar") or "").strip()
        token = os.environ.get(env_var, "") if env_var else ""
        if token:
            return token
    return None


def _gateway_headers(*, json_content: bool = False) -> dict[str, str]:
    headers = {"content-type": "application/json"} if json_content else {}
    token = (
        os.environ.get("CODETAS_GATEWAY_TOKEN")
        or os.environ.get("CODETAS_CLIENT_TOKEN")
        or _configured_gateway_token()
    )
    if token:
        # Keep the provider Authorization header available for gateways that
        # forward caller credentials; admission has its own dedicated header.
        headers["x-codetas-token"] = token
    return headers


def _gateway_json(path: str, payload: dict[str, Any], timeout: float | None = None) -> dict[str, Any]:
    req = request.Request(
        f"{_gateway_root()}{path}",
        data=json.dumps(payload).encode("utf-8"),
        headers=_gateway_headers(json_content=True),
        method="POST",
    )
    try:
        with request.urlopen(req, timeout=timeout or 180) as response:
            body = response.read(MAX_GATEWAY_RESPONSE_BYTES + 1)
            if len(body) > MAX_GATEWAY_RESPONSE_BYTES:
                raise RuntimeError("CODETAS gateway response exceeded 48 MiB")
            return json.loads(body.decode("utf-8"))
    except error.HTTPError as exc:
        detail = exc.read(64 * 1024).decode("utf-8", "replace")
        raise RuntimeError(f"CODETAS gateway returned HTTP {exc.code}: {detail}") from exc
    except error.URLError as exc:
        raise RuntimeError(f"Could not reach the CODETAS gateway at {_gateway_root()}: {exc}") from exc


def _gateway_config() -> dict[str, Any]:
    req = request.Request(f"{_gateway_root()}/v1/sidecars/config", headers=_gateway_headers(), method="GET")
    try:
        with request.urlopen(req, timeout=30) as response:
            body = response.read(1024 * 1024 + 1)
            if len(body) > 1024 * 1024:
                raise RuntimeError("CODETAS gateway config response exceeded 1 MiB")
            return json.loads(body.decode("utf-8"))
    except error.HTTPError as exc:
        detail = exc.read(64 * 1024).decode("utf-8", "replace")
        raise RuntimeError(f"CODETAS gateway returned HTTP {exc.code}: {detail}") from exc
    except error.URLError as exc:
        raise RuntimeError(f"Could not reach the CODETAS gateway at {_gateway_root()}: {exc}") from exc


def _image_reference(value: str) -> str:
    source = str(value or "").strip()
    if source.startswith("data:image/") or source.startswith("https://"):
        return source
    path = Path(source).expanduser().resolve()
    if not path.is_file():
        raise ValueError(f"Image file does not exist: {path}")
    data = path.read_bytes()
    if not data or len(data) > MAX_IMAGE_BYTES:
        raise ValueError(f"Image must contain 1-{MAX_IMAGE_BYTES // (1024 * 1024)} MiB: {path}")
    mime = mimetypes.guess_type(path.name)[0] or "image/png"
    if not mime.startswith("image/"):
        raise ValueError(f"Unsupported image type: {path}")
    return f"data:{mime};base64,{base64.b64encode(data).decode('ascii')}"


def vision_analyze(image: str, question: str = "") -> dict[str, Any]:
    return _gateway_json(
        "/v1/sidecars/vision",
        {"image_url": _image_reference(image), "prompt": question or "Describe this image precisely, including all visible text and task-relevant details."},
    )


def _required_command(name: str) -> str:
    command = shutil.which(name)
    if not command:
        raise RuntimeError(f"{name} is required for this media analysis but was not found on PATH")
    return command


def _video_duration(path: Path) -> float:
    command = _required_command("ffprobe")
    result = subprocess.run(
        [command, "-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1", str(path)],
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or "ffprobe could not read the video")
    duration = float(result.stdout.strip())
    if duration <= 0:
        raise RuntimeError("Video duration is unavailable")
    return duration


def _native_input_result(kind: str, path: Path, question: str, reason: str) -> dict[str, Any]:
    mime_type = mimetypes.guess_type(path.name)[0] or "application/octet-stream"
    return {
        "object": "codetas.native_input",
        "kind": kind,
        "mode": "native",
        "path": str(path),
        "uri": path.as_uri(),
        "name": path.name,
        "mimeType": mime_type,
        "size": path.stat().st_size,
        "question": question,
        "result": (
            f"{reason} CODETASの補助モデルでは解析していません。"
            "元ファイルをMCPリソースとして返します。現在のモデルで直接処理してください。"
        ),
    }


def _uses_auxiliary_model(configured: dict[str, Any], mode_key: str, capability_key: str) -> tuple[bool, str]:
    mode = str(configured.get(mode_key, "auto")).strip().lower()
    available = bool((configured.get("configured") or {}).get(capability_key))
    if mode == "native":
        return False, "入力モードが「直接渡す」に設定されています。"
    if mode == "text":
        if not available:
            raise RuntimeError("入力モードは補助モデル解析ですが、対応する補助モデルが設定されていません")
        return True, ""
    if available:
        return True, ""
    return False, "自動モードで利用可能な補助モデルが見つかりません。"


def _analysis_timeout(configured: dict[str, Any], item_count: int) -> float:
    per_item = max(1.0, min(float(configured.get("auxiliaryTimeoutMs", 120_000)) / 1000.0, 600.0))
    bounded_items = max(1, min(item_count, MAX_ANALYSIS_BATCH_ITEMS))
    return max(60.0, per_item * bounded_items + 15.0)


def _analyze_media_files(
    endpoint: str,
    field: str,
    files: list[Path],
    prompt: str,
    configured: dict[str, Any],
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    results: list[str] = []
    models: list[str] = []
    batch: list[str] = []
    batch_chars = 0
    batches = 0
    processed = 0
    batch_item_limit = max(1, min(int(configured.get("analysisBatchItems", MAX_ANALYSIS_BATCH_ITEMS)), MAX_ANALYSIS_BATCH_ITEMS))
    batch_char_limit = max(
        1024 * 1024,
        min(int(configured.get("analysisMaxPayloadBytes", MAX_ANALYSIS_BATCH_CHARS)), MAX_ANALYSIS_BATCH_CHARS),
    )

    def flush() -> None:
        nonlocal batch, batch_chars, batches, processed
        if not batch:
            return
        payload: dict[str, Any] = {field: batch, "prompt": prompt, "startIndex": processed}
        if extra:
            payload.update(extra)
        value = _gateway_json(endpoint, payload, timeout=_analysis_timeout(configured, len(batch)))
        result = str(value.get("result") or "").strip()
        if result:
            remaining = MAX_ANALYSIS_RESULT_CHARS - sum(len(item) for item in results)
            if remaining > 0:
                results.append(result[:remaining])
        model = value.get("model")
        if isinstance(model, str) and model and model not in models:
            models.append(model)
        batches += 1
        processed += len(batch)
        batch = []
        batch_chars = 0

    for media_file in files:
        encoded = _image_reference(str(media_file))
        if len(encoded) > MAX_ANALYSIS_ITEM_CHARS:
            raise ValueError(f"Extracted media item is too large after compression: {media_file.name}")
        if batch and (
            len(batch) >= batch_item_limit
            or batch_chars + len(encoded) > batch_char_limit
        ):
            flush()
        batch.append(encoded)
        batch_chars += len(encoded)
    flush()
    if not results:
        raise RuntimeError("The auxiliary model completed without an analysis result")
    return {
        "object": "codetas.sidecar.batched_result",
        "kind": endpoint.rsplit("/", 1)[-1],
        "result": "\n\n".join(results),
        "itemCount": len(files),
        "batches": batches,
        "models": models,
    }


def video_analyze(video_path: str, question: str = "", sample_frames: int | None = None) -> dict[str, Any]:
    path = Path(video_path).expanduser().resolve()
    if not path.is_file():
        raise ValueError(f"Video file does not exist: {path}")
    configured = _gateway_config()
    use_auxiliary, reason = _uses_auxiliary_model(configured, "videoInputMode", "videoAnalysis")
    if not use_auxiliary:
        return _native_input_result("video", path, question, reason)
    configured_limit = max(1, min(int(configured.get("videoSampleFrames", 8)), 64))
    requested_frames = configured_limit if sample_frames is None else int(sample_frames)
    frame_count = max(1, min(requested_frames, configured_limit))
    duration = _video_duration(path)
    ffmpeg = _required_command("ffmpeg")
    with tempfile.TemporaryDirectory(prefix="codetas-video-") as directory:
        frames: list[Path] = []
        for index in range(frame_count):
            timestamp = duration * (index + 0.5) / frame_count
            output = Path(directory) / f"frame-{index + 1:03d}.jpg"
            result = subprocess.run(
                [
                    ffmpeg, "-v", "error", "-ss", f"{timestamp:.3f}", "-i", str(path),
                    "-frames:v", "1", "-vf", "scale=1280:-2:force_original_aspect_ratio=decrease",
                    "-q:v", "5", str(output),
                ],
                capture_output=True,
                text=True,
                timeout=60,
                check=False,
            )
            if result.returncode != 0 or not output.is_file():
                raise RuntimeError(result.stderr.strip() or f"ffmpeg could not extract frame {index + 1}")
            frames.append(output)
        return _analyze_media_files(
            "/v1/sidecars/video-analysis",
            "frames",
            frames,
            question or "Explain the video sequence, visible text, actions, scene changes, and important details.",
            configured,
        )


def document_analyze(document_path: str, question: str = "", max_pages: int | None = None, ocr: bool | None = None) -> dict[str, Any]:
    path = Path(document_path).expanduser().resolve()
    if not path.is_file():
        raise ValueError(f"Document file does not exist: {path}")
    configured = _gateway_config()
    use_auxiliary, reason = _uses_auxiliary_model(configured, "documentInputMode", "document")
    if not use_auxiliary:
        return _native_input_result("document", path, question, reason)
    configured_limit = max(1, min(int(configured.get("documentMaxPages", 12)), 100))
    requested_pages = configured_limit if max_pages is None else int(max_pages)
    page_limit = max(1, min(requested_pages, configured_limit))
    ocr_enabled = bool(ocr if ocr is not None else configured.get("ocrEnabled", True))
    prompt = question or "Explain this document accurately, preserving headings, tables, code, and important values."
    if path.suffix.lower() in {".png", ".jpg", ".jpeg", ".webp", ".gif"}:
        return _analyze_media_files(
            "/v1/sidecars/document", "pages", [path], prompt, configured, {"ocr": ocr_enabled}
        )
    elif path.suffix.lower() == ".pdf":
        pdftoppm = _required_command("pdftoppm")
        with tempfile.TemporaryDirectory(prefix="codetas-pdf-") as directory:
            prefix = Path(directory) / "page"
            result = subprocess.run(
                [
                    pdftoppm, "-f", "1", "-l", str(page_limit), "-jpeg", "-r", "120",
                    "-scale-to", "1600", "-jpegopt", "quality=78,optimize=y", str(path), str(prefix),
                ],
                capture_output=True,
                text=True,
                timeout=180,
                check=False,
            )
            if result.returncode != 0:
                raise RuntimeError(result.stderr.strip() or "pdftoppm could not render the PDF")
            rendered = sorted(
                Path(directory).glob("page-*.jpg"),
                key=lambda page: int(page.stem.rsplit("-", 1)[-1]),
            )
            if not rendered:
                raise RuntimeError("PDF rendering produced no pages")
            return _analyze_media_files(
                "/v1/sidecars/document",
                "pages",
                rendered[:page_limit],
                prompt,
                configured,
                {"ocr": ocr_enabled},
            )
    else:
        raise ValueError("Document analysis currently accepts PDF, PNG, JPEG, WebP, or GIF files")


def image_generate(prompt: str, size: str = "1024x1024", quality: str = "auto") -> dict[str, Any]:
    if not str(prompt).strip():
        raise ValueError("prompt is required")
    return _gateway_json(
        "/v1/sidecars/image",
        {"prompt": str(prompt).strip(), "size": size, "quality": quality, "n": 1},
        timeout=600,
    )


def generated_image_result(value: dict[str, Any]) -> dict[str, Any]:
    display = dict(value)
    data = value.get("data")
    display_data: list[Any] = []
    if isinstance(data, list):
        for item in data:
            if isinstance(item, dict):
                summary = {key: item_value for key, item_value in item.items() if key != "b64_json"}
                if "b64_json" in item:
                    summary["image"] = "attached"
                display_data.append(summary)
            else:
                display_data.append(item)
        display["data"] = display_data
    content: list[dict[str, Any]] = [{"type": "text", "text": json.dumps(display, ensure_ascii=False, indent=2)}]
    if isinstance(data, list) and data and isinstance(data[0], dict):
        encoded = data[0].get("b64_json")
        if isinstance(encoded, str) and encoded:
            content.append({"type": "image", "data": encoded, "mimeType": "image/png"})
    return {"content": content}
