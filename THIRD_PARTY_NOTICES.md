# Third-party notices

Hushai's own source code is MIT-licensed (see [LICENSE](LICENSE)). The system also relies
on third-party **machine-learning models** and **libraries**, each under its own license.
The models are **downloaded at setup time** (via the `local_dev/fetch_*.sh` scripts, `local_dev/provision_vision.sh`, and `ollama pull`), not committed to this repository — so this file is the attribution record.

**You are responsible for confirming that each model's license fits your use**, especially
for any commercial or redistributed deployment. The notes below are a convenience, not legal
advice; always check upstream.

## Machine-learning models

| Model | Used by | Source | License (verify upstream) |
|-------|---------|--------|---------------------------|
| **whisper.cpp `ggml-base.en.bin`** (ASR) | `hushai-worker` | [ggerganov/whisper.cpp](https://huggingface.co/ggerganov/whisper.cpp) (OpenAI Whisper weights) | MIT |
| **NVIDIA TitaNet-large** `nemo_en_titanet_large.onnx` (speaker embeddings, 192-d) | `hushai-worker` | [k2-fsa/sherpa-onnx speaker-recognition models](https://github.com/k2-fsa/sherpa-onnx/releases/tag/speaker-recongition-models) (orig. NVIDIA NeMo) | NVIDIA NeMo model — typically CC-BY-4.0; **verify upstream** |
| **Silero VAD** `silero_vad.onnx` (voice activity detection) | `hushai-worker` | [k2-fsa/sherpa-onnx asr-models](https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models) (orig. [snakers4/silero-vad](https://github.com/snakers4/silero-vad)) | MIT |
| **Kokoro-82M** `kokoro-en-v0_19` (neural TTS) | `hushai-rag` | [k2-fsa/sherpa-onnx tts-models](https://github.com/k2-fsa/sherpa-onnx/releases/tag/tts-models) | Apache-2.0 (LICENSE bundled in the download) |
| **Vosk** `vosk-model-small-en-us-0.15`, `vosk-model-spk-0.4` (on-device wake-word / STT / x-vector) | `hushai-android` | [alphacephei.com/vosk/models](https://alphacephei.com/vosk/models) | Apache-2.0 |
| **mxbai-embed-large** (text embeddings, 1024-d) | `hushai-worker`, `hushai-rag` | [mixedbread-ai](https://ollama.com/library/mxbai-embed-large) via Ollama | Apache-2.0 |
| **qwen2.5:7b** (RAG answers, `RAG_LLM_MODEL`) | `hushai-rag` | [Qwen2.5](https://ollama.com/library/qwen2.5) via Ollama | Apache-2.0 — **verify upstream** |
| **SCRFD** `scrfd_10g_bnkps.onnx` (face detection) | `hushai-worker` | [InsightFace](https://github.com/deepinsight/insightface) | ⚠️ InsightFace *code* is MIT/Apache-2.0 but the **pretrained packs are published for non-commercial research use** — read the model-zoo terms before any commercial deployment |
| **ArcFace** `w600k_r50.onnx` (face embeddings, 512-d) | `hushai-worker` | [InsightFace `buffalo_l`](https://github.com/deepinsight/insightface) | ⚠️ Same non-commercial research restriction as SCRFD |
| **YuNet** `face_detection_yunet_2023mar.onnx` (alternative face detector) | `hushai-worker` | [opencv/opencv_zoo](https://github.com/opencv/opencv_zoo) | MIT — **verify upstream** |
| **RF-DETR-nano** `rf-detr-nano.onnx` (open-vocabulary object detection) | `hushai-worker` | [roboflow/rf-detr](https://github.com/roboflow/rf-detr) | Apache-2.0 — **verify upstream** |
| **CLIP ViT-B/32** `clip_vit_b32_{image,text}.onnx` (image/text embeddings, 512-d) | `hushai-worker`, `hushai-rag` | [openai/CLIP](https://github.com/openai/CLIP) | MIT — **verify upstream** |
| **Licence-plate detector + OCR** `lp_detector.onnx`, `lp_ocr_cct.onnx` | `hushai-worker` | **No default source.** `local_dev/fetch_plate_detector.sh` requires the operator to supply a URL | ⚠️ **Operator-sourced — licensing is entirely your responsibility** |
| **ONNX Runtime** shared library (`models/onnxruntime/`) | `hushai-worker`, `hushai-rag` | [microsoft/onnxruntime](https://github.com/microsoft/onnxruntime) | MIT |
| **llama3.2:3b** (RAG answers + sentiment) | `hushai-worker`, `hushai-rag` | [Meta Llama 3.2](https://ollama.com/library/llama3.2) via Ollama | **Llama 3.2 Community License** (not OSI-approved; has use restrictions — read it) |

> ⚠️ **Read these before any commercial or redistributed deployment.** The **InsightFace**
> face models (SCRFD + ArcFace) are published for **non-commercial research use** — they are
> the sharpest restriction in this list, and face recognition is the lane most likely to be
> regulated where you operate. **Llama 3.2** carries Meta's community licence (acceptable-use
> policy plus an MAU threshold) and **TitaNet** carries NVIDIA NeMo terms. The
> **licence-plate** weights have no default source at all: you supply them, so you own that
> licensing decision.
>
> All of them are swappable. The embedding and LLM models are Ollama model names
> (`EMBED_MODEL`, `RAG_LLM_MODEL`, `SENTIMENT_MODEL`); every ONNX model is a path
> (`SPEAKER_MODEL_PATH`, `FACE_DETECT_MODEL_PATH`, `FACE_EMBED_MODEL_PATH`,
> `OBJECT_DET_MODEL_PATH`, `CLIP_IMAGE_MODEL_PATH`, `PLATE_*`). Every vision lane
> **self-disables when its weights are absent**, so you can run Hushai with only the lanes
> whose licences suit you.

Model download URLs and SHA-256 checksums live in the `local_dev/fetch_*.sh` scripts and `local_dev/provision_vision.sh` and the
`*.env.example` files.

## Notable libraries

Rust crates are predominantly MIT / Apache-2.0 dual-licensed (see each crate's `Cargo.toml`
and `cargo tree`). Notable native/embedded components:

- **whisper-rs** → binds [whisper.cpp](https://github.com/ggerganov/whisper.cpp) (MIT).
- **sherpa-rs / sherpa-onnx** ([k2-fsa/sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx), Apache-2.0) — runs the VAD, speaker, and TTS ONNX models; statically links onnxruntime (MIT). The `download-binaries` feature fetches a prebuilt native lib at build time.
- **rig** (LLM/embedding orchestration), **axum / tokio / tower / sqlx** (MIT/Apache-2.0).
- **pgvector** Postgres extension ([pgvector/pgvector](https://github.com/pgvector/pgvector), PostgreSQL License).
- **hls.js** (`hushai-viewer/ui/vendor/hls.min.js`, v1.5.x) — [video-dev/hls.js](https://github.com/video-dev/hls.js), Apache-2.0. The only vendored frontend dependency (no build step).

## Android dependencies

`hushai-android` pulls standard AndroidX / Jetpack Compose, OkHttp (Apache-2.0), Square Wire
(Apache-2.0), and vosk-android (Apache-2.0) via Gradle. See
`hushai-android/app/build.gradle.kts`.
