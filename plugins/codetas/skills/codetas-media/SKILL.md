---
name: codetas-media
description: Analyze images, local videos, PDFs, scanned documents, screenshots, and OCR content through CODETAS auxiliary models, or generate images through the configured CODETAS image model. Use when the user references visual media that the active model may not understand, asks what an image/video/PDF contains, requests OCR, or asks to create an image.
---

# CODETAS Media

Use the CODETAS MCP media tools. The CODETAS app owns provider credentials, model selection, routing, and limits.

## Choose the tool

- Use `vision_analyze` for an image URL, data URL, screenshot, or local image file.
- Use `video_analyze` for a local video. It follows the app's input mode, sampling and batching frames for an auxiliary model or attaching the original as an MCP resource.
- Use `document_analyze` for a PDF, scan, or image document. It follows the app's input mode and batches resized pages when auxiliary analysis is selected.
- Use `image_generate` when the user asks to create an image.

## Workflow

1. Prefer the user's original local path or URL; do not make an unnecessary copy.
2. Pass the user's actual question so the auxiliary result is focused rather than a generic caption.
3. Treat auxiliary output as evidence for the active model, then answer the user in context.
4. If a tool returns `codetas.native_input`, use the attached MCP file resource with the active model's native media flow instead of claiming that CODETAS analyzed it.
5. If a dependency is unavailable, report the concrete requirement: video analysis needs `ffmpeg` and `ffprobe`; PDF rendering needs `pdftoppm`.

## Safety

- Do not expose provider keys or gateway tokens in prompts or responses.
- Do not claim that every video frame or PDF page was inspected when sampling or page limits were applied.
- Ask before analyzing media outside paths or URLs the user supplied or clearly placed in scope.
