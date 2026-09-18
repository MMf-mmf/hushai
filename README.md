# Hushai

**A self-hosted NVR that transcribes, identifies and indexes everything your cameras see and
hear — then answers questions about it in plain English. Nothing leaves the machine.**

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg)](https://www.rust-lang.org/)
[![Local-first](https://img.shields.io/badge/cloud-none-brightgreen.svg)](#privacy)

![The Hushai timeline: a scrubbable HLS NVR stitched from thousands of 2-second segments](docs/img/01-timeline.png)

Point a camera at something. Hushai stores every 2-second segment exactly once, transcribes the
audio, learns who the voices and faces belong to, reads licence plates, notices when something is
unusual — and lets you ask *"was anything delivered on Tuesday?"* instead of scrubbing three hours
of footage. Speech recognition, embeddings, language models, vision and text-to-speech all run
locally: there is no cloud account, no subscription and no egress.

## What it does

- **Ask your footage questions.** Grounded retrieval over every transcript, with streaming answers
  and source citations that deep-link straight to the moment in the timeline.
- **Know who and what.** Voiceprints, face re-identification, open-vocabulary object detection and
  licence-plate recognition, each maintaining its own catalog you can name and merge.
- **Notice things.** Sessionized events, an alert-rule engine (camera, type, severity, time window,
  cooldown), watchlists, webhook delivery, and daily digests.
- **Scrub like an NVR.** A single continuous timeline stitched from thousands of 2-second clips,
  with per-second AI processing status, a detections overlay, and clip export.
- **Never phone home.** whisper.cpp for speech, Ollama for embeddings and generation, ONNX for
  vision and speech synthesis, Postgres + pgvector for storage. All on your hardware.

## Screenshots

| | |
|---|---|
| ![Detections overlay: people and objects boxed on the live frame](docs/img/02-detections.png) <br> **Detections overlay** — faces, people and objects boxed on the frame, named where Hushai recognises them. | ![Chat answering a question with citations into the timeline](docs/img/03-chat.png) <br> **Ask the footage** — a grounded answer with citations that jump to the exact second. |
| ![Investigate: an entity page with same-identity candidates and the evidence behind each match](docs/img/04-investigate.png) <br> **Investigate** — who an identity was seen with, which cameras it frequents, and the matches it proposed but wasn't sure enough to make. | ![System dashboard: cameras, services, work queues and audit trail](docs/img/05-dashboard.png) <br> **Operate** — cameras, service health, work queues and the audit trail in one place. |

The alert centre, and notes on how every image here is generated, are in
[docs/screenshots.md](docs/screenshots.md).

## Quickstart

macOS on Apple Silicon is the tested path; Linux is supported best-effort.

```bash
git clone https://github.com/MMf-mmf/hushai.git && cd hushai
./local_dev/onboard.sh          # installs deps, starts Postgres + Ollama, fetches models,
                                # writes .env, mints a camera token, brings the stack up
```

`onboard.sh` asks what you need and does the rest; re-running it is always safe. Once set up:

```bash
./local_dev/run_stack.sh        # backend + worker + rag + viewer; one Ctrl-C stops everything
open http://127.0.0.1:8070
```

To see the UI with data before you have a camera, build a demo dataset — every frame comes from a
public-domain photograph and every word is synthesized, so it is safe to show anyone:

```bash
createdb hushai_demo
DATABASE_URL="postgres://$USER@localhost:5432/hushai_demo" \
BLOB_DIR="$PWD/local_dev/.demo_work/blobs" VISION_MOTION_SKIP_ENABLED=false \
  ./local_dev/run_stack.sh &
./local_dev/build_demo.sh        # three cameras, three days; the worker then needs ~an hour
```

It builds three fictional cameras from public-domain stills, synthesizes the dialogue with `say`,
and refuses to run against any database whose name doesn't end in `_demo`. Every screenshot in this
README comes from it — see [docs/screenshots.md](docs/screenshots.md).

Adding a real camera: install the Android client (`hushai-android`) or write your own against
[the camera→backend contract](contracts/cameraToBackendContract.md), then
`./local_dev/run_stack.sh --add-camera <name>` and follow
[docs/onboarding-a-camera.md](docs/onboarding-a-camera.md).

**Requirements:** Rust (edition 2024), Postgres 16 with the `vector`, `pg_trgm` and
`fuzzystrmatch` extensions (tested against pgvector 0.8), ffmpeg, and Ollama with
`mxbai-embed-large`, `qwen2.5:7b` and `llama3.2:3b`. The vision lanes need ONNX weights, which
`./local_dev/provision_vision.sh` fetches; **each lane self-disables when its weights are absent**,
so you can run only the parts you want.

## How it works

A capture client cuts ~2-second muxed fMP4 segments and POSTs each one with a manifest. The backend
stores the blob content-addressed and the metadata in Postgres, exactly once, resumably. A worker
drains two `FOR UPDATE SKIP LOCKED` queues — audio (speech → embeddings → sentiment → speaker
identity) and vision (faces → objects → plates) — and produces events. The RAG service answers
questions over the result; the viewer is the browser app and the authenticating gateway in front of
everything.

| Component | Port | Role |
|---|---|---|
| [`hushai-backend`](hushai-backend/) | `:8080` | Segment ingest, schema and migrations, admin API |
| [`hushai-worker`](hushai-worker/) | — | Audio and vision processing lanes, events, alert delivery |
| [`hushai-rag`](hushai-rag/) | `:8090` | Grounded Q&A, streaming chat, local text-to-speech |
| [`hushai-viewer`](hushai-viewer/) | `:8070` | Browser NVR, admin console, auth gateway, reverse proxy |
| [`hushai-advisor`](hushai-advisor/) | `:8095` | Optional book-grounded advice agent |
| [`hushai-android`](hushai-android/) | — | Kotlin capture client with an on-device voice assistant |
| [`hushai-eval`](hushai-eval/) · [`hushai-loadtest`](hushai-loadtest/) | — | Regression harness · capacity harness |

Crash safety is the design centre: a killed worker's in-flight segment is re-leased and finished on
restart with no duplicate sentences, and re-POSTing a segment is a no-op. Schema changes are
forward-only migrations applied at startup (currently through `0031`).

## Status & limitations

Hushai runs a real household. It is also honest about what it is not:

- **Single-owner, single-node.** No multi-tenancy, no roles, no horizontal scale. Everyone who can
  authenticate is assumed to be the owner. A capture-device token also reaches the destructive admin
  API — keep `:8080` off untrusted networks. See [SECURITY.md](SECURITY.md) for the full list.
- **Identity is a hint, not evidence.** Speaker, face and plate thresholds are tuned against clean
  audio and clean photographs. On noisy real footage they both over-merge and over-split.
- **No containers and no CI yet.** You build it and run it from source.
- **Transcription is as good as whisper.cpp on your audio** — which on a doorbell in the rain is
  not very good.

Capacity: the network is never the limit, compute is. At stock settings one machine saturates
**below** 30 cameras; a Linux host with an NVIDIA GPU is the realistic path to 30 on a single box,
Apple Silicon reaches good mid density, and CPU-only needs several. Measure your own with
`./local_dev/run_loadtest.sh` — see
[docs/hardware-sizing-30-cameras.md](docs/hardware-sizing-30-cameras.md).

## Privacy

Captured media never leaves the machine. There is no telemetry, no analytics and no cloud
dependency; the only outbound traffic is what you configure yourself, such as an alert webhook.

Recording people is regulated, and the rules differ by jurisdiction — consent for audio, notice for
video, and separate rules again for biometric data like faceprints and voiceprints. Hushai puts
face recognition, voice identification and plate reading in one box. **Operating it lawfully is
your responsibility.** Please don't point it at people who haven't agreed to it.

## Contributing

Issues and pull requests are welcome — start with [CONTRIBUTING.md](CONTRIBUTING.md). Architecture,
invariants and the non-obvious gotchas are in [AGENTS.md](AGENTS.md); per-component detail is in
each crate's own README; dated history is in [CHANGELOG.md](CHANGELOG.md).

## License

MIT — see [LICENSE](LICENSE). The machine-learning models Hushai downloads at setup time carry
their own licenses, some of them restrictive: see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) before any commercial deployment.
