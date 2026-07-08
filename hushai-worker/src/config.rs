//! Worker-specific configuration (model paths, polling, concurrency, retry).
//!
//! DB/connection config is reused from `hushai_backend::config::Config` so we don't
//! duplicate the schema-owning crate's knobs; this struct holds only what the
//! transcription/embedding worker adds on top.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::anyhow;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Path to the whisper.cpp GGML model file (local ASR).
    pub whisper_model_path: String,
    /// Whisper anti-hallucination decode-quality params (see `asr::DecodeQuality`).
    pub whisper_no_speech_thold: f32,
    pub whisper_logprob_thold: f32,
    pub whisper_entropy_thold: f32,
    pub whisper_suppress_nst: bool,
    /// Base URL of the Ollama server the worker sends embedding requests to.
    /// From `EMBED_OLLAMA_BASE_URL`, falling back to `OLLAMA_BASE_URL`. Keeping this
    /// separate from the RAG answer-LLM endpoint lets always-on embedding load be
    /// routed to its own instance so it can't starve query answering (see RagConfig).
    pub embed_ollama_base_url: String,
    /// Embedding model name (must produce 1024-dim vectors to match the schema).
    pub embed_model: String,
    /// Number of concurrent per-segment AUDIO pipelines (whisper/embed/speaker lane).
    pub worker_concurrency: usize,
    /// Number of concurrent VIDEO/MUXED VISION pipelines (face/object/plate lane). The single
    /// shared `VisionModels` (Arc-based ORT sessions; `Session` is `Send+Sync` and `Run` is
    /// thread-safe) is cloned per loop, so this is pure segment-level fan-out over the
    /// SKIP-LOCKED vision queue — no model duplication. Each segment is still processed
    /// start-to-finish on one loop, so intra-segment frame ordering (plate clustering) holds.
    pub vision_concurrency: usize,
    /// Per-call whisper thread override (`ASR_THREADS`). `0`/unset ⇒ the derived CPU budget
    /// `clamp(cores / (worker_concurrency + vision_concurrency), 1, cores)`, so N parallel
    /// transcriptions don't each request all cores and oversubscribe the box. See `asr_n_threads`.
    pub asr_threads: usize,
    /// Per-session ORT intra-op thread override (`ORT_INTRA_THREADS`). `0`/unset ⇒ the same
    /// derived budget. CoreML-offloaded nodes are unaffected; this caps only CPU-fallback ops.
    pub ort_intra_threads: usize,
    /// How long to wait between polls when the queue is drained (keep-up mode).
    pub poll_interval: Duration,
    /// Max attempts before a segment is left in `error` and no longer retried.
    pub max_attempts: i32,
    /// A `processing` claim older than this (seconds) is considered crashed and re-leased.
    pub lease_timeout_secs: f64,
    /// ffmpeg binary used to decode segment audio to PCM.
    pub ffmpeg_bin: String,
    /// Base URL of the Ollama server used for the answer/classification LLM (sentiment).
    /// From `LLM_OLLAMA_BASE_URL`, falling back to `OLLAMA_BASE_URL`. Kept separate from
    /// the embedding endpoint so chat-model load doesn't compete with embedding load.
    pub llm_ollama_base_url: String,
    /// Ollama chat model used for lexical per-segment sentiment classification.
    pub sentiment_model: String,
    /// Whether per-segment sentiment classification runs at all.
    pub sentiment_enabled: bool,
    /// Hard timeout for one sentiment classification (ms); on timeout the worker writes
    /// NULL so a slow LLM never stalls the always-on pipeline.
    pub sentiment_timeout_ms: u64,
    /// Path to the speaker-embedding ONNX model (TitaNet-large, 192-dim), loaded by sherpa.
    pub speaker_model_path: String,
    /// Cosine DISTANCE (1 - similarity) at/under which an embedding MATCHES an existing
    /// speaker and folds into its centroid. The tight end of the mint-guard hysteresis.
    pub speaker_match_threshold: f32,
    /// Minimum POST-VAD speech seconds in a segment to attempt speaker work at all; below
    /// this we leave speaker_id NULL (embedding sub-second speech is the dominant accuracy
    /// killer). Now measured on VAD-cleaned speech, not the raw whisper-timestamp span.
    pub speaker_min_speech_secs: f64,
    /// If the first vs second half of a segment's CLEANED speech differ by more than this
    /// cosine distance, treat it as multi-speaker and refuse (speaker_id NULL, no centroid).
    pub speaker_split_threshold: f32,

    // --- Real VAD (sherpa Silero) — strip static/silence before embedding ---
    /// Path to the Silero VAD ONNX model. Not committed (gitignored under models/),
    /// downloaded + sha-pinned in .env.example, same as the TitaNet model. Runs in the
    /// already-linked sherpa-onnx native lib (no extra crate).
    pub vad_model_path: String,
    /// Silero speech-probability threshold; frames above this are speech. Higher = stricter
    /// (rejects more static at the cost of clipping quiet speech). Uncalibrated.
    pub vad_threshold: f32,
    /// Silence (s) that ends a speech segment (bridges shorter inter-word gaps).
    pub vad_min_silence_secs: f32,
    /// Minimum speech (s) for a Silero segment to be emitted (drops isolated blips).
    pub vad_min_speech_secs: f32,

    // --- Skip-silent gate — don't pay for whisper on a segment with no speech ---
    /// Master switch for the pre-ASR skip-silent gate. On (default) skips whisper + sentiment +
    /// speaker work on segments with no speech (writing an empty transcript + reject tombstone,
    /// still marked `done`). Set false to fully restore legacy "always transcribe" behavior.
    pub audio_silence_skip_enabled: bool,
    /// Stage-1 free floor: a segment whose RMS amplitude is at/under this is dead air and is
    /// skipped WITHOUT running the VAD model (~-46 dBFS at 0.005). Uncalibrated starting guess.
    pub audio_silence_rms_floor: f32,
    /// Stage-2 threshold: after the VAD runs, less than this many seconds of detected speech ⇒
    /// skip. Kept BELOW `speaker_min_speech_secs` (0.3) so we only skip on essentially-no-speech;
    /// a short-but-real utterance is still transcribed (the speaker stage may reject it later).
    pub audio_silence_min_speech_secs: f64,

    // --- Mint quality gates — a NEW identity may only be born from clean audio ---
    /// MINT gate: minimum cleaned speech (s) to be allowed to mint a NEW identity (must be
    /// >= speaker_min_speech_secs; marginal/short audio can still ATTACH but never mints).
    pub speaker_mint_min_speech_secs: f64,
    /// MINT gate: minimum estimated SNR (dB, speech-RMS vs noise-floor-RMS) to mint.
    pub speaker_mint_min_snr_db: f32,
    /// MINT gate: minimum voiced fraction (cleaned speech / total segment) to mint.
    pub speaker_mint_min_voiced_frac: f64,

    // --- Multi-vector k-NN matcher + mint-guard hysteresis ---
    /// Cosine DISTANCE above which a clean embedding MINTS a new identity. Between
    /// speaker_match_threshold and this floor is the gray zone: ATTACH to the nearest
    /// existing speaker, never mint, never update the centroid. Biases hard against
    /// duplicate "unknown speaker" rows. Must be > speaker_match_threshold.
    pub speaker_mint_distance_floor: f32,
    /// Number of nearest RAW neighbors (speaker_segments) pulled per match for the vote.
    pub speaker_knn_k: i64,
    /// Per-neighbor cosine-distance ceiling; neighbors beyond this don't vote.
    pub speaker_knn_neighbor_ceiling: f32,
    /// Below this many qualifying neighbors, fall back to the centroid catalog scan
    /// (cold-start: a freshly-minted speaker has too few raw rows to vote reliably).
    pub speaker_knn_min_neighbors: i64,
    /// HNSW ef_search for the vote-pool walk; raised so the filtered ANN scan fills k
    /// (the metadata-filter recall-cliff mitigation — see hushai-rag/src/retrieve.rs).
    pub speaker_knn_ef_search: i64,
    /// Transaction-scoped statement_timeout (ms) guarding a pathological ANN scan. Generous
    /// so it never threatens the transcript write that shares the same transaction.
    pub speaker_knn_statement_timeout_ms: i64,
    /// Recompute the centroid from the most recent N CLEAN segments (self-healing) instead
    /// of an unbounded online running-mean a single noisy fold could permanently corrupt.
    pub speaker_centroid_window: i64,

    // --- Going-forward auto-merge ("auto-merge near-certain duplicates; suggest the rest") ---
    /// Whether the worker auto-merges near-certain duplicate voices after a backlog drain.
    pub speaker_autoheal_enabled: bool,
    /// Cosine DISTANCE under which two speakers are auto-merged with no confirmation. Very
    /// tight (near-certain identical) — looser candidates are only SUGGESTED in the app.
    pub speaker_autoheal_distance: f32,
    /// Only consider speakers active within this many seconds as merge candidates, so the
    /// going-forward auto-merge never mass-collapses the historical backlog (the user's
    /// "fix going forward only"). New splits still fold into older canonical voices.
    pub speaker_autoheal_recent_secs: i64,
    /// Minimum cross-speaker raw-neighbor agreements to link two speakers (not one coincidence).
    pub speaker_autoheal_min_links: i64,
    /// k for the per-segment cross-speaker neighbor probe in auto-merge.
    pub speaker_autoheal_knn_k: i64,
    /// Minimum seconds between auto-merge passes (only worker 0 runs it, after a drain).
    pub speaker_autoheal_interval_secs: u64,
    /// Retro-attach (going-forward): distinct raw-neighbor agreements (each within
    /// `speaker_match_threshold`) required before a recent unattributed segment is attached
    /// to a NAMED/owner speaker. Multi-vector evidence — strictly tighter than the online
    /// matcher's single-nearest gray-zone attach; never loosen below 2.
    pub speaker_retro_attach_min_agree: i64,
    /// Per-pass budget of unattributed segments the retro-attach may claim.
    pub speaker_retro_attach_max_segments: i64,

    // --- Entity profiles ("running memory" per person/speaker, migration 0024) ---
    /// Whether the worker folds new events into entity profiles after a backlog drain
    /// (worker 0 only, same cadence idiom as the speaker auto-heal).
    pub profiles_enabled: bool,
    /// Minimum seconds between profile passes.
    pub profiles_interval_secs: u64,
    /// Person sightings closer than this (seconds) are ONE visit in the profile.
    pub profiles_visit_gap_secs: i64,
    /// Speech events closer than this (seconds) are ONE conversation in the profile.
    pub profiles_convo_gap_secs: i64,
    /// Ignore events updated within this grace window (a 30s session bucket keeps being
    /// UPSERT-extended while a visit is in progress; ≥2× the bucket lets it settle).
    pub profiles_grace_secs: i64,
    /// Per-pass event budget per subject type (bounded backfill; converges over passes).
    pub profiles_max_events_per_pass: i64,
    /// Profile text cap (chars); oldest lines are deterministically compacted beyond it.
    pub profiles_max_chars: usize,

    // --- Gotham entity graph (migrations 0028–0030; worker 0 drives hushai_backend::graph_pass) ---
    /// Master switch for the interval-gated graph fold on worker 0 (Gotham.md §5).
    pub graph_enabled: bool,
    /// Minimum seconds between graph passes.
    pub graph_interval_secs: u64,
    /// Per-pass event/conversation budget (bounded backfill; converges over passes).
    pub graph_max_events_per_pass: i64,
    /// Truncate derived rows + reset watermarks once on start, then refold.
    pub graph_rebuild_on_start: bool,
    // ★ determinism-relevant (folded into graph::config_hash):
    pub graph_grace_secs: i64,
    pub graph_copresence_slack_secs: i64,
    pub graph_copresence_max_subjects: usize,
    pub graph_vehicle_corr_window_secs: i64,
    pub graph_edge_sample_cap: usize,
    pub graph_bind_min_sessions: i64,
    pub graph_bind_min_confidence: f32,
    pub graph_bind_margin: f32,
    pub graph_baseline_window_days: i64,
    pub graph_anomaly_min_visits: i64,
    pub graph_anomaly_hour_min_frac: f32,
    pub graph_anomaly_unknown_cluster_min: i64,
    pub graph_journey_gap_secs: i64,

    // --- Conversation threading (migration 0025; worker 0 drives hushai_backend::conversations) ---
    /// Master switch for the interval-gated threading pass on worker 0. Unlike autoheal
    /// this runs even while the queue is busy (checked BEFORE claiming) so sustained
    /// ingest can never starve threading.
    pub threader_enabled: bool,
    /// Minimum seconds between threading passes.
    pub threader_interval_secs: u64,
    /// Sentences younger than this (created_at) are not yet threadable — the lag lets
    /// speaker attribution / autoheal settle before the threader sees a row.
    pub threader_min_age_secs: i64,
    /// Open-tail revision window (capture time). Unassigned sentences older than this
    /// wait for an explicit backfill.
    pub threader_lookback_secs: i64,
    /// Per-pass scan bound (embeddings of the working set are held in RAM).
    pub threader_max_rows_per_pass: i64,
    /// Hard temporal conversation boundary in seconds. SHARED TRUTH with the RAG's
    /// CONVERSATION_GAP_SECS and PROFILES_CONVO_GAP_SECS — read from the same env name.
    pub conversation_gap_secs: f64,
    /// A conversation closes (and emits its `conversation` event) when
    /// now - last speech > gap + this grace.
    pub convo_close_grace_secs: i64,
    /// Stage A: consecutive same-speaker sentences closer than this merge into one utterance.
    pub threader_utterance_merge_max_gap_secs: f64,
    /// Stage C: reply-shaped adjacency window between different speakers.
    pub threader_alternation_max_secs: f64,
    /// Stage C: adjacency counts full weight above this cosine similarity.
    pub threader_reply_sim_floor: f32,
    /// Stage C: speaker-pair topic affinity bonus threshold.
    pub threader_topic_attract_sim: f32,
    /// Stage C: speaker-pair topic repulsion threshold.
    pub threader_topic_repel_sim: f32,
    /// Stage C: minimum edge weight for "these two speakers are conversing".
    pub threader_speaker_link_min: f32,
    /// Split gate: minimum utterances per candidate sub-conversation.
    pub threader_min_cluster_utterances: usize,
    /// Split gate: maximum mean cross-component similarity for an accepted split.
    pub threader_split_max_cross_sim: f32,
    /// When false (default) a block is never split on topic alone without temporal interleave.
    pub threader_topic_only_split: bool,
    /// Cross-device conversation LINKING (link_group_id; never a merge).
    pub threader_link_enabled: bool,
    /// Fraction of the shorter conversation's span that must overlap to link.
    pub threader_link_min_overlap_frac: f64,
    /// One-shot: thread ALL historical NULL-conversation rows on startup (worker 0).
    pub threader_backfill_on_start: bool,

    /// On startup, re-queue `done` AUDIO/MUXED segments that have no voiceprint yet
    /// (transcribed while the speaker stage was unavailable) so the pipeline re-runs
    /// and assigns/mints their speaker. Self-healing + convergent (a no-op once every
    /// segment has a speaker_segments row). Default on; set `0`/`false` to skip the
    /// one-time re-ASR cost of any currently-stranded backlog.
    pub speaker_backfill_on_start: bool,

    /// One-shot: on startup, delete `quality='reject'` tombstones and re-queue those segments
    /// so they're re-evaluated under the CURRENT gates. Use after changing the speaker
    /// thresholds / window settings (a tombstone is otherwise sticky by design). Default off;
    /// turn on for one run after a calibration change, then turn back off.
    pub speaker_reprocess_rejects_on_start: bool,

    // --- Rolling-window speaker embedding (short-segment aggregation) ---
    /// Embed the speaker voiceprint over a WINDOW of adjacent same-stream segments rather
    /// than one segment alone. Clients upload short (~2s) clips, so a single segment often
    /// has < `speaker_min_speech_secs` of post-VAD speech and gets rejected; aggregating its
    /// contiguous predecessors gives the VAD/quality gate enough speech to attribute (or
    /// mint) the voice honestly. Off => legacy per-segment behavior.
    pub speaker_window_enabled: bool,
    /// Accumulate look-back audio until the window's RAW duration reaches this many seconds
    /// (then stop adding older segments). Bigger = more speech per voiceprint but more decode.
    pub speaker_window_target_secs: f64,
    /// Hard cap on segments per window (decode bound; also limits how far a window can reach).
    pub speaker_window_max_segments: usize,

    // ---- Vision (Phase A: face identity + open-vocab objects) ----
    /// Master switch. When on, the worker also runs the VIDEO/MUXED vision pipeline. If the
    /// models or ORT dylib are missing, vision self-disables with a warning (audio keeps running).
    pub vision_enabled: bool,
    /// Path to the ONNX Runtime dylib that `ort` dlopen()s at runtime (load-dynamic). MUST be a
    /// 1.20.x build (ort-sys rc.9 target) and is a SEPARATE copy from sherpa's bundled 1.17.1 —
    /// see AGENTS.md "vision ONNX runtime". Not committed; provisioned by fetch_onnxruntime.sh.
    pub ort_dylib_path: String,
    /// Register the CoreML execution provider (Apple Silicon) ahead of CPU. Best-effort: ort
    /// falls back to CPU if CoreML can't take a node.
    pub vision_coreml: bool,
    pub face_detect_model_path: String,
    pub face_embed_model_path: String,
    pub object_det_model_path: String,
    /// Authoritative column→label map for the object detector (RF-DETR's COCO 91-slot layout). The
    /// worker falls back to its built-in COCO-91 map if this file is absent/unreadable.
    pub object_classes_path: String,
    /// Class-aware NMS IoU for the object lane (RF-DETR duplicate-box suppression). Default 0.5.
    pub object_nms_iou: f32,
    pub clip_image_model_path: String,
    /// How many frames to sample per ~2s segment for detection/embedding.
    pub frames_per_segment: usize,

    // --- Skip-static gate — don't re-run vision on an unchanged scene ---
    /// Master switch for the cross-segment motion gate. On (default) skips ALL vision inference on
    /// a segment whose representative frame is near-identical to the same camera's last analyzed
    /// frame (writing nothing, still marked `done`). Set false to fully restore legacy behavior —
    /// also do this for a full reprocess/calibration run (the in-memory baseline isn't meaningful
    /// when replaying old segments out of order). Mirrors `speaker_reprocess_rejects_on_start`'s
    /// "on for one run" convention.
    pub vision_motion_skip_enabled: bool,
    /// Mean-subtracted MSE distance (0..~65025) at/under which a segment is "static" and skipped.
    /// Conservative default skips only near-identical frames. UNCALIBRATED starting guess —
    /// calibrate per deployment against real footage (see plan "How to Test").
    pub vision_motion_threshold: f32,
    /// Fingerprint tile side (NxN grayscale). Larger = more sensitive + slightly more cost; 32 is
    /// the recommended sweet spot (~1 KB/camera).
    pub vision_motion_fp_side: usize,
    /// One-frame probe (default on): decode a SINGLE mid-segment frame for the motion gate and
    /// only pay the full `frames_per_segment` decode after motion is confirmed — ~1/3 the
    /// decode-to-discard cost on a static camera. The gate fingerprint becomes the probe frame
    /// (still camera-consistent: both sides of the diff come through the same path). Off = legacy
    /// behavior (decode all frames, gate on the last).
    pub vision_gate_one_frame_probe: bool,
    /// Face match-or-mint tunables (cosine DISTANCE; mirror the SPEAKER_* set). UNCALIBRATED
    /// starting guesses — calibrate on a real face fixture (see plan "How to Test").
    pub face_match_threshold: f32,
    pub face_mint_distance_floor: f32,
    pub face_knn_k: i64,
    pub face_knn_neighbor_ceiling: f32,
    pub face_knn_min_neighbors: i64,
    pub face_knn_ef_search: i64,
    pub face_knn_statement_timeout_ms: i64,
    pub face_centroid_window: i64,
    /// Quality gates before a face crop is embedded (the visual analogue of the VAD gate).
    pub face_min_det_score: f32,
    pub face_min_px: i64,
    pub face_min_sharpness: f32,
    /// Minimum detector confidence to keep an object detection.
    pub object_min_det_score: f32,
    /// RF-DETR square input side (letterboxed). Variant-dependent; validate at provisioning.
    pub object_det_input_size: usize,
    /// Cap on region detections kept per frame (highest-confidence first). Bounds scene_objects
    /// growth + the write tx on busy scenes. 0 = unlimited.
    pub object_max_per_frame: usize,
    /// Drop region boxes whose smaller side is below this many ORIGINAL-frame pixels (tiny/garbage
    /// detections aren't worth a CLIP embed + row).
    pub object_min_box_px: f32,
    /// When true, a missing/unloadable RF-DETR or CLIP model disables the WHOLE vision subsystem
    /// with a loud warning (audio still runs) instead of silently self-disabling just the object
    /// lane. Default false: objects are best-effort and never block face identity.
    pub object_required: bool,

    // ---- Image cleanup / face restoration (the "zoom + clean up before we recognize" stage) ----
    /// Which face detector to run (`scrfd` default — best small/distant recall — or `yunet`). When
    /// the chosen model can't load, `build_vision_models` falls back to whichever IS provisioned.
    pub face_detector_kind: crate::vision::enhance::DetectorKind,
    /// SCRFD ONNX path (used when `face_detector_kind = scrfd`). Not committed; see fetch_scrfd.sh.
    pub face_scrfd_model_path: String,
    /// Embed both a crop and its horizontal mirror, average + renormalize (InsightFace TTA). A pure
    /// accuracy win; default on.
    pub face_embed_flip_tta: bool,
    /// Context margin (fraction of bbox side) added around a face before cropping, so the restorer
    /// has hairline/jaw context the tight alignment warp throws away.
    pub face_crop_margin_frac: f32,
    /// Blind-face-restoration model path (GFPGANv1.4 / CodeFormer ONNX). Empty/unloadable ⇒ the
    /// restoration sub-lane self-disables (low-quality faces are dropped as before).
    pub face_restore_model_path: String,
    /// Which restorer the model is (`gfpgan` default | `codeformer`).
    pub face_restore_kind: crate::vision::enhance::RestorerKind,
    /// CodeFormer fidelity weight `w` (0=quality .. 1=fidelity); ignored by GFPGAN.
    pub face_restore_codeformer_w: f32,
    /// Restore (recover-then-embed) when a raw crop's sharpness is BELOW this. Clean faces above it
    /// skip restoration (no embedding-space drift for already-good faces).
    pub face_restore_max_sharpness: f32,
    /// Restore when a raw crop's smaller side is BELOW this many pixels (small/distant face).
    pub face_restore_min_px: i64,
    /// Absolute floors below which even restoration can't help → hard drop (no row).
    pub face_hard_min_px: i64,
    pub face_hard_min_det_score: f32,
    /// Super-resolution model path (Real-ESRGAN ONNX). Optional; upscales a tiny crop before restore.
    pub face_upscale_model_path: String,
    /// Upscale a crop whose smaller side is below this many pixels before restoring.
    pub face_upscale_min_px: i64,
    /// Max |yaw|/|pitch| (degrees, from the landmark pose proxy) for a face to be allowed to MINT a
    /// new identity. A profile face minting a "new person" is a classic over-split bug.
    pub face_mint_max_yaw_deg: f32,
    pub face_mint_max_pitch_deg: f32,
    /// When true, a restored (generatively-cleaned) face may mint a new identity + fold into the
    /// centroid. Dev-stage default true; set false during a transition on a populated catalog.
    pub face_restored_may_mint: bool,
    /// Persist the cleaned best-shot crop to disk so the UI shows the restored thumbnail instead of
    /// re-cropping raw frames. Written under `<blob_dir>/face_crops/`.
    pub face_persist_crop: bool,
    /// Tag the highest-quality face per segment as the best shot (drives the sample-face thumbnail).
    pub face_best_shot_enabled: bool,
    /// Blob root shared with the backend (its `BLOB_DIR`); face crops live under `<blob_dir>/face_crops`.
    pub blob_dir: String,

    // ---- License-plate recognition (ALPR) — runs after vehicle detection ----
    /// Master switch for the plate lane (still self-disables if its models aren't provisioned).
    pub plate_enabled: bool,
    /// Plate-detector ONNX (YOLO bbox / 4-corner pose). Empty/unloadable ⇒ plate lane off.
    pub plate_detect_model_path: String,
    /// Plate-OCR ONNX (fast-plate-ocr CCT / PaddleOCR rec).
    pub plate_ocr_model_path: String,
    /// OCR decode head: false = fixed-length per-slot (fast-plate-ocr CCT, default), true = CTC (CRNN).
    pub plate_ocr_ctc: bool,
    /// Ordered class→char map for the OCR head (sidecar JSON, an array of single-char strings).
    pub plate_ocr_charset_path: String,
    /// Square input side of the plate detector (letterboxed). Validate at provisioning.
    pub plate_detect_input_size: usize,
    /// Plate-detector output layout: true = END2END (open-image-models YOLOv9 `[N,7]` xyxy, NMS baked
    /// in — the default provisioned model), false = raw YOLOv8/11 `[C,N]` cxcywh [+ corner keypoints].
    pub plate_detect_end2end: bool,
    /// Plate-detector confidence floor.
    pub plate_min_det_score: f32,
    /// Drop plates whose smaller side (original-frame px) is below this.
    pub plate_min_px: f32,
    /// Mean per-char OCR confidence floor to keep a read at all.
    pub plate_min_ocr_conf: f32,
    /// Higher OCR-confidence bar a read must clear to MINT a new catalog plate.
    pub plate_mint_min_ocr_conf: f32,
    /// Reject reads shorter than this many characters.
    pub plate_min_len: usize,
    /// Max edit distance for a fuzzy (OCR-noise) match to an existing plate.
    pub plate_max_edit_distance: usize,
    /// Trigram similarity floor for a fuzzy match candidate.
    pub plate_fuzzy_min_similarity: f32,
    /// Fractional expansion of the vehicle bbox before cropping the ROI we run plate-detect on.
    pub plate_vehicle_roi_margin: f32,
    /// Super-resolve a rectified plate whose smaller side is below this many pixels before OCR.
    pub plate_sr_min_side_px: f32,
    /// Also scan the whole frame for plates when no vehicle was detected (off by default).
    pub plate_detect_whole_frame: bool,
    /// When true, a missing/unloadable plate model disables the WHOLE vision subsystem loudly.
    pub plate_required: bool,

    // --- Liveness heartbeat (read by the viewer's /api/dashboard) ---
    /// How often the worker upserts its `worker_heartbeat` row. The viewer treats a row whose
    /// `last_beat` is older than ~3× this as "down".
    pub heartbeat_interval: Duration,
    /// Stable id for this worker process's heartbeat row. Defaults to `<host>:<pid>` when unset;
    /// set explicitly (e.g. per host) when running multiple worker instances.
    pub worker_id: Option<String>,

    // ---- Proactive events + alerts (roadmap A3 — the VSaaS layer) ----
    /// Event producer + alert evaluator tunables (`EVENTS_*`). See `events_producer::EventsConfig`.
    pub events: crate::events_producer::EventsConfig,

    /// Notification delivery loop tunables (`ALERT_*`, roadmap A4). See `delivery::DeliveryConfig`.
    pub delivery: crate::delivery::DeliveryConfig,

    /// Device load governor tunables (`LOAD_*`). Paces processing so the box is never overloaded;
    /// disabled ⇒ legacy always-claim behavior. See `governor::GovernorConfig`.
    pub governor: crate::governor::GovernorConfig,

    /// Address for the worker's Prometheus `/metrics` + `/healthz` server (roadmap B1/B5). The
    /// worker has no other HTTP port. `WORKER_METRICS_ADDR`; empty disables it. Default :9100.
    pub metrics_addr: Option<SocketAddr>,
}

impl WorkerConfig {
    /// Logical-core count with a safe fallback — the basis of the inference thread budget.
    pub fn cores() -> usize {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    }

    /// Total inference loops sharing the CPU (audio + vision), at least 1. Denominator of the
    /// per-call thread budget so M audio + V vision loops don't each grab all cores.
    pub fn total_inference_loops(&self) -> usize {
        (self.worker_concurrency + self.vision_concurrency).max(1)
    }

    /// Per-call whisper `n_threads`. `ASR_THREADS` overrides; `0`/unset ⇒ the derived budget
    /// `clamp(cores / total_inference_loops, 1, cores)`.
    pub fn asr_n_threads(&self) -> i32 {
        let c = Self::cores();
        let derived = (c / self.total_inference_loops()).clamp(1, c);
        let n = if self.asr_threads == 0 { derived } else { self.asr_threads };
        n.clamp(1, c) as i32
    }

    /// Per-session ORT intra-op thread count (same budget). `ORT_INTRA_THREADS` overrides;
    /// `0`/unset ⇒ derived. `0` is never returned (the core fallback guarantees ≥ 1).
    pub fn ort_intra_op_threads(&self) -> usize {
        let c = Self::cores();
        let derived = (c / self.total_inference_loops()).clamp(1, c);
        if self.ort_intra_threads == 0 {
            derived
        } else {
            self.ort_intra_threads.clamp(1, c)
        }
    }

    /// Mint-quality gates for the input-quality classifier (see `vad::assess_quality`).
    pub fn mint_gates(&self) -> crate::vad::MintGates {
        crate::vad::MintGates {
            min_speech_secs: self.speaker_min_speech_secs,
            mint_min_speech_secs: self.speaker_mint_min_speech_secs,
            mint_min_snr_db: self.speaker_mint_min_snr_db,
            mint_min_voiced_frac: self.speaker_mint_min_voiced_frac,
        }
    }

    /// Quality gates handed to the face quality classifier (`face_embed::assess_quality`).
    pub fn face_gates(&self) -> crate::vision::face_embed::FaceGates {
        crate::vision::face_embed::FaceGates {
            min_det_score: self.face_min_det_score,
            min_px: self.face_min_px as f32,
            min_sharpness: self.face_min_sharpness,
        }
    }

    /// Tuning bundle handed to `vision::face_match::assign_faces` (mirrors `speaker_match_cfg`).
    pub fn face_match_cfg(&self) -> crate::vision::face_match::FaceMatchConfig {
        crate::vision::face_match::FaceMatchConfig {
            match_threshold: self.face_match_threshold,
            mint_distance_floor: self.face_mint_distance_floor,
            knn_k: self.face_knn_k,
            knn_neighbor_ceiling: self.face_knn_neighbor_ceiling,
            knn_min_neighbors: self.face_knn_min_neighbors,
            knn_ef_search: self.face_knn_ef_search,
            knn_statement_timeout_ms: self.face_knn_statement_timeout_ms,
            centroid_window: self.face_centroid_window,
            restored_may_mint: self.face_restored_may_mint,
        }
    }

    /// Quality gates handed to the plate read classifier (`plates::normalize::assess_quality`).
    pub fn plate_gates(&self) -> crate::vision::plates::normalize::PlateGates {
        crate::vision::plates::normalize::PlateGates {
            min_det_score: self.plate_min_det_score,
            min_ocr_conf: self.plate_min_ocr_conf,
            mint_min_ocr_conf: self.plate_mint_min_ocr_conf,
            min_px: self.plate_min_px,
            min_len: self.plate_min_len,
        }
    }

    /// Tuning bundle handed to `plates::plate_match::assign_plates`.
    pub fn plate_match_cfg(&self) -> crate::vision::plates::plate_match::PlateMatchConfig {
        crate::vision::plates::plate_match::PlateMatchConfig {
            max_edit_distance: self.plate_max_edit_distance,
            fuzzy_min_similarity: self.plate_fuzzy_min_similarity,
            min_len: self.plate_min_len,
        }
    }

    /// Tuning bundle handed to `speaker_match::assign_speaker` (one borrow, not 8 args).
    pub fn speaker_match_cfg(&self) -> crate::speaker_match::SpeakerMatchConfig {
        crate::speaker_match::SpeakerMatchConfig {
            match_threshold: self.speaker_match_threshold,
            mint_distance_floor: self.speaker_mint_distance_floor,
            knn_k: self.speaker_knn_k,
            knn_neighbor_ceiling: self.speaker_knn_neighbor_ceiling,
            knn_min_neighbors: self.speaker_knn_min_neighbors,
            knn_ef_search: self.speaker_knn_ef_search,
            knn_statement_timeout_ms: self.speaker_knn_statement_timeout_ms,
            centroid_window: self.speaker_centroid_window,
        }
    }

    /// Bundle the threading knobs for `hushai_backend::conversations`. Every field here
    /// feeds the threader config-hash (and the eval manifest reads the same env names),
    /// so a knob change starts a new eval lineage on both sides.
    pub fn threader_opts(&self) -> hushai_backend::conversations::ThreaderOpts {
        hushai_backend::conversations::ThreaderOpts {
            cfg: hushai_backend::threading::ThreaderCfg {
                gap_secs: self.conversation_gap_secs,
                utterance_merge_max_gap_secs: self.threader_utterance_merge_max_gap_secs,
                alternation_max_secs: self.threader_alternation_max_secs,
                reply_sim_floor: self.threader_reply_sim_floor,
                topic_attract_sim: self.threader_topic_attract_sim,
                topic_repel_sim: self.threader_topic_repel_sim,
                speaker_link_min: self.threader_speaker_link_min,
                min_cluster_utterances: self.threader_min_cluster_utterances,
                split_max_cross_sim: self.threader_split_max_cross_sim,
                topic_only_split: self.threader_topic_only_split,
            },
            min_age_secs: self.threader_min_age_secs,
            lookback_secs: self.threader_lookback_secs,
            max_rows_per_pass: self.threader_max_rows_per_pass,
            close_grace_secs: self.convo_close_grace_secs,
            link_enabled: self.threader_link_enabled,
            link_min_overlap_frac: self.threader_link_min_overlap_frac,
        }
    }

    /// Build the Gotham graph-pass options from the GRAPH_* knobs (Gotham.md §5).
    pub fn graph_opts(&self) -> hushai_backend::graph_pass::GraphOpts {
        hushai_backend::graph_pass::GraphOpts {
            cfg: hushai_backend::graph::GraphCfg {
                grace_secs: self.graph_grace_secs,
                copresence_slack_secs: self.graph_copresence_slack_secs,
                copresence_max_subjects: self.graph_copresence_max_subjects,
                vehicle_corr_window_secs: self.graph_vehicle_corr_window_secs,
                edge_sample_cap: self.graph_edge_sample_cap,
                bind_min_sessions: self.graph_bind_min_sessions,
                bind_min_confidence: self.graph_bind_min_confidence,
                bind_margin: self.graph_bind_margin,
                baseline_window_days: self.graph_baseline_window_days,
                anomaly_min_visits: self.graph_anomaly_min_visits,
                anomaly_hour_min_frac: self.graph_anomaly_hour_min_frac,
                anomaly_unknown_cluster_min: self.graph_anomaly_unknown_cluster_min,
                journey_gap_secs: self.graph_journey_gap_secs,
            },
            max_events_per_pass: self.graph_max_events_per_pass,
            tz_offset_secs: 0,
        }
    }
}

impl WorkerConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let ollama_base_url = opt("OLLAMA_BASE_URL", "http://localhost:11434");
        // Validate the unknown-person severity against the canonical set NOW (it's free-text env);
        // an invalid value would otherwise silently rank as 'info' in the evaluator and suppress
        // unknown-person alerts that an operator scoped to min_severity='warning'.
        let unknown_person_severity = opt("EVENTS_UNKNOWN_PERSON_SEVERITY", "warning");
        if !hushai_backend::events::SEVERITIES.contains(&unknown_person_severity.as_str()) {
            return Err(anyhow!(
                "EVENTS_UNKNOWN_PERSON_SEVERITY={unknown_person_severity:?} is invalid; expected one of {:?}",
                hushai_backend::events::SEVERITIES
            ));
        }
        Ok(Self {
            whisper_model_path: opt("WHISPER_MODEL_PATH", "./models/ggml-base.en.bin"),
            whisper_no_speech_thold: parse("WHISPER_NO_SPEECH_THOLD", "0.6")?,
            whisper_logprob_thold: parse("WHISPER_LOGPROB_THOLD", "-1.0")?,
            whisper_entropy_thold: parse("WHISPER_ENTROPY_THOLD", "2.4")?,
            whisper_suppress_nst: parse("WHISPER_SUPPRESS_NST", "false")?,
            embed_ollama_base_url: opt("EMBED_OLLAMA_BASE_URL", &ollama_base_url),
            embed_model: opt("EMBED_MODEL", "mxbai-embed-large"),
            worker_concurrency: parse("WORKER_CONCURRENCY", "2")?,
            vision_concurrency: parse("VISION_CONCURRENCY", "2")?,
            asr_threads: parse("ASR_THREADS", "0")?,
            ort_intra_threads: parse("ORT_INTRA_THREADS", "0")?,
            poll_interval: Duration::from_secs(parse("POLL_INTERVAL_SECS", "5")?),
            max_attempts: parse("MAX_ATTEMPTS", "5")?,
            lease_timeout_secs: parse("LEASE_TIMEOUT_SECS", "300")?,
            ffmpeg_bin: opt("FFMPEG_BIN", "ffmpeg"),
            llm_ollama_base_url: opt("LLM_OLLAMA_BASE_URL", &ollama_base_url),
            sentiment_model: opt("SENTIMENT_MODEL", "llama3.2:3b"),
            sentiment_enabled: parse("SENTIMENT_ENABLED", "true")?,
            sentiment_timeout_ms: parse("SENTIMENT_TIMEOUT_MS", "3000")?,
            speaker_model_path: opt("SPEAKER_MODEL_PATH", "./models/nemo_en_titanet_large.onnx"),
            speaker_match_threshold: parse("SPEAKER_MATCH_THRESHOLD", "0.5")?,
            // Calibrated for short-clip clients (~2s segments). The reject gate is on POST-VAD
            // speech and a single brief utterance is ~0.3s; lowered from 0.8 so real (if short)
            // speech stores a voiceprint instead of being dropped (windowing accumulates more).
            speaker_min_speech_secs: parse("SPEAKER_MIN_SPEECH_SECS", "0.3")?,
            speaker_split_threshold: parse("SPEAKER_SPLIT_THRESHOLD", "0.6")?,
            vad_model_path: opt("VAD_MODEL_PATH", "./models/silero_vad.onnx"),
            vad_threshold: parse("VAD_THRESHOLD", "0.5")?,
            vad_min_silence_secs: parse("VAD_MIN_SILENCE_SECS", "0.3")?,
            vad_min_speech_secs: parse("VAD_MIN_SPEECH_SECS", "0.25")?,
            audio_silence_skip_enabled: parse("AUDIO_SILENCE_SKIP_ENABLED", "true")?,
            audio_silence_rms_floor: parse("AUDIO_SILENCE_RMS_FLOOR", "0.005")?,
            audio_silence_min_speech_secs: parse("AUDIO_SILENCE_MIN_SPEECH_SECS", "0.2")?,
            speaker_mint_min_speech_secs: parse("SPEAKER_MINT_MIN_SPEECH_SECS", "1.2")?,
            // Lowered from 10.0: real phone/room audio is noisier than clean-room `say` voices.
            // The voiced-fraction gate still keeps mostly-silence windows out of minting, so this
            // mainly lets genuinely-spoken (if noisy) audio mint instead of only ever attaching.
            speaker_mint_min_snr_db: parse("SPEAKER_MINT_MIN_SNR_DB", "3.0")?,
            speaker_mint_min_voiced_frac: parse("SPEAKER_MINT_MIN_VOICED_FRAC", "0.5")?,
            speaker_mint_distance_floor: parse("SPEAKER_MINT_DISTANCE_FLOOR", "0.72")?,
            speaker_knn_k: parse("SPEAKER_KNN_K", "15")?,
            speaker_knn_neighbor_ceiling: parse("SPEAKER_KNN_NEIGHBOR_CEILING", "0.55")?,
            speaker_knn_min_neighbors: parse("SPEAKER_KNN_MIN_NEIGHBORS", "3")?,
            speaker_knn_ef_search: parse("SPEAKER_KNN_EF_SEARCH", "200")?,
            speaker_knn_statement_timeout_ms: parse("SPEAKER_KNN_STATEMENT_TIMEOUT_MS", "5000")?,
            speaker_centroid_window: parse("SPEAKER_CENTROID_WINDOW", "50")?,
            speaker_autoheal_enabled: parse("SPEAKER_AUTOHEAL_ENABLED", "true")?,
            speaker_autoheal_distance: parse("SPEAKER_AUTOHEAL_DISTANCE", "0.15")?,
            speaker_autoheal_recent_secs: parse("SPEAKER_AUTOHEAL_RECENT_SECS", "3600")?,
            speaker_autoheal_min_links: parse("SPEAKER_AUTOHEAL_MIN_LINKS", "2")?,
            speaker_autoheal_knn_k: parse("SPEAKER_AUTOHEAL_KNN_K", "5")?,
            speaker_autoheal_interval_secs: parse("SPEAKER_AUTOHEAL_INTERVAL_SECS", "300")?,
            speaker_retro_attach_min_agree: parse("SPEAKER_RETRO_ATTACH_MIN_AGREE", "2")?,
            speaker_retro_attach_max_segments: parse("SPEAKER_RETRO_ATTACH_MAX_SEGMENTS", "500")?,
            profiles_enabled: parse("PROFILES_ENABLED", "true")?,
            profiles_interval_secs: parse("PROFILES_INTERVAL_SECS", "300")?,
            profiles_visit_gap_secs: parse("PROFILES_VISIT_GAP_SECS", "120")?,
            profiles_convo_gap_secs: parse("PROFILES_CONVO_GAP_SECS", "300")?,
            profiles_grace_secs: parse("PROFILES_GRACE_SECS", "90")?,
            profiles_max_events_per_pass: parse("PROFILES_MAX_EVENTS_PER_PASS", "2000")?,
            profiles_max_chars: parse("PROFILES_MAX_CHARS", "8000")?,
            graph_enabled: parse("GRAPH_ENABLED", "true")?,
            graph_interval_secs: parse("GRAPH_INTERVAL_SECS", "300")?,
            graph_max_events_per_pass: parse("GRAPH_MAX_EVENTS_PER_PASS", "2000")?,
            graph_rebuild_on_start: parse("GRAPH_REBUILD_ON_START", "false")?,
            graph_grace_secs: parse("GRAPH_GRACE_SECS", "90")?,
            graph_copresence_slack_secs: parse("GRAPH_COPRESENCE_SLACK_SECS", "120")?,
            graph_copresence_max_subjects: parse("GRAPH_COPRESENCE_MAX_SUBJECTS", "12")?,
            graph_vehicle_corr_window_secs: parse("GRAPH_VEHICLE_CORR_WINDOW_SECS", "180")?,
            graph_edge_sample_cap: parse("GRAPH_EDGE_SAMPLE_CAP", "16")?,
            graph_bind_min_sessions: parse("GRAPH_BIND_MIN_SESSIONS", "3")?,
            graph_bind_min_confidence: parse("GRAPH_BIND_MIN_CONFIDENCE", "0.6")?,
            graph_bind_margin: parse("GRAPH_BIND_MARGIN", "0.2")?,
            graph_baseline_window_days: parse("GRAPH_BASELINE_WINDOW_DAYS", "30")?,
            graph_anomaly_min_visits: parse("GRAPH_ANOMALY_MIN_VISITS", "5")?,
            graph_anomaly_hour_min_frac: parse("GRAPH_ANOMALY_HOUR_MIN_FRAC", "0.05")?,
            graph_anomaly_unknown_cluster_min: parse("GRAPH_ANOMALY_UNKNOWN_CLUSTER_MIN", "3")?,
            graph_journey_gap_secs: parse("GRAPH_JOURNEY_GAP_SECS", "600")?,
            threader_enabled: parse("THREADER_ENABLED", "true")?,
            threader_interval_secs: parse("THREADER_INTERVAL_SECS", "30")?,
            threader_min_age_secs: parse("THREADER_MIN_AGE_SECS", "10")?,
            threader_lookback_secs: parse("THREADER_LOOKBACK_SECS", "900")?,
            threader_max_rows_per_pass: parse("THREADER_MAX_ROWS_PER_PASS", "5000")?,
            conversation_gap_secs: parse("CONVERSATION_GAP_SECS", "300")?,
            convo_close_grace_secs: parse("CONVO_CLOSE_GRACE_SECS", "120")?,
            threader_utterance_merge_max_gap_secs: parse(
                "THREADER_UTTERANCE_MERGE_MAX_GAP_SECS",
                "1.0",
            )?,
            threader_alternation_max_secs: parse("THREADER_ALTERNATION_MAX_SECS", "5.0")?,
            threader_reply_sim_floor: parse("THREADER_REPLY_SIM_FLOOR", "0.45")?,
            threader_topic_attract_sim: parse("THREADER_TOPIC_ATTRACT_SIM", "0.60")?,
            threader_topic_repel_sim: parse("THREADER_TOPIC_REPEL_SIM", "0.35")?,
            threader_speaker_link_min: parse("THREADER_SPEAKER_LINK_MIN", "2.0")?,
            threader_min_cluster_utterances: parse("THREADER_MIN_CLUSTER_UTTERANCES", "4")?,
            threader_split_max_cross_sim: parse("THREADER_SPLIT_MAX_CROSS_SIM", "0.40")?,
            threader_topic_only_split: parse("THREADER_TOPIC_ONLY_SPLIT", "false")?,
            threader_link_enabled: parse("THREADER_LINK_ENABLED", "true")?,
            threader_link_min_overlap_frac: parse("THREADER_LINK_MIN_OVERLAP_FRAC", "0.5")?,
            threader_backfill_on_start: parse("THREADER_BACKFILL_ON_START", "false")?,
            speaker_backfill_on_start: parse("SPEAKER_BACKFILL_ON_START", "true")?,
            speaker_reprocess_rejects_on_start: parse(
                "SPEAKER_REPROCESS_REJECTS_ON_START",
                "false",
            )?,
            speaker_window_enabled: parse("SPEAKER_WINDOW_ENABLED", "true")?,
            // 6s (≈3 short clips) so low-speech-fraction audio accumulates enough speech to clear
            // the reject gate; bounded by max_segments. Raised from 3.0.
            speaker_window_target_secs: parse("SPEAKER_WINDOW_TARGET_SECS", "6.0")?,
            speaker_window_max_segments: parse("SPEAKER_WINDOW_MAX_SEGMENTS", "6")?,

            vision_enabled: parse("VISION_ENABLED", "true")?,
            ort_dylib_path: opt(
                "ORT_DYLIB_PATH",
                "./models/onnxruntime/onnxruntime-osx-arm64-1.20.0/lib/libonnxruntime.1.20.0.dylib",
            ),
            vision_coreml: parse("VISION_COREML", "true")?,
            face_detect_model_path: opt(
                "FACE_DETECT_MODEL_PATH",
                "./models/face_detection_yunet_2023mar.onnx",
            ),
            face_embed_model_path: opt("FACE_EMBED_MODEL_PATH", "./models/w600k_r50.onnx"),
            object_det_model_path: opt("OBJECT_DET_MODEL_PATH", "./models/rf-detr-nano.onnx"),
            object_classes_path: opt("OBJECT_CLASSES_PATH", "./models/rf-detr-classes.json"),
            object_nms_iou: parse("OBJECT_NMS_IOU", "0.5")?,
            clip_image_model_path: opt("CLIP_IMAGE_MODEL_PATH", "./models/clip_vit_b32_image.onnx"),
            frames_per_segment: parse("FRAMES_PER_SEGMENT", "3")?,
            vision_motion_skip_enabled: parse("VISION_MOTION_SKIP_ENABLED", "true")?,
            vision_motion_threshold: parse("VISION_MOTION_THRESHOLD", "8.0")?,
            vision_motion_fp_side: parse("VISION_MOTION_FP_SIDE", "32")?,
            vision_gate_one_frame_probe: parse("WORKER_GATE_ONE_FRAME_PROBE", "true")?,
            face_match_threshold: parse("FACE_MATCH_THRESHOLD", "0.5")?,
            face_mint_distance_floor: parse("FACE_MINT_DISTANCE_FLOOR", "0.72")?,
            face_knn_k: parse("FACE_KNN_K", "15")?,
            face_knn_neighbor_ceiling: parse("FACE_KNN_NEIGHBOR_CEILING", "0.55")?,
            face_knn_min_neighbors: parse("FACE_KNN_MIN_NEIGHBORS", "3")?,
            face_knn_ef_search: parse("FACE_KNN_EF_SEARCH", "200")?,
            face_knn_statement_timeout_ms: parse("FACE_KNN_STATEMENT_TIMEOUT_MS", "5000")?,
            face_centroid_window: parse("FACE_CENTROID_WINDOW", "50")?,
            face_min_det_score: parse("FACE_MIN_DET_SCORE", "0.6")?,
            face_min_px: parse("FACE_MIN_PX", "40")?,
            face_min_sharpness: parse("FACE_MIN_SHARPNESS", "30.0")?,
            object_min_det_score: parse("OBJECT_MIN_DET_SCORE", "0.4")?,
            object_det_input_size: parse("OBJECT_DET_INPUT_SIZE", "384")?,
            object_max_per_frame: parse("OBJECT_MAX_PER_FRAME", "20")?,
            object_min_box_px: parse("OBJECT_MIN_BOX_PX", "16.0")?,
            object_required: parse("OBJECT_REQUIRED", "false")?,

            face_detector_kind: parse("FACE_DETECTOR_KIND", "scrfd")?,
            face_scrfd_model_path: opt("FACE_SCRFD_MODEL_PATH", "./models/scrfd_10g_bnkps.onnx"),
            face_embed_flip_tta: parse("FACE_EMBED_FLIP_TTA", "true")?,
            face_crop_margin_frac: parse("FACE_CROP_MARGIN_FRAC", "0.35")?,
            face_restore_model_path: opt("FACE_RESTORE_MODEL_PATH", "./models/gfpgan_v1.4.onnx"),
            face_restore_kind: parse("FACE_RESTORE_KIND", "gfpgan")?,
            face_restore_codeformer_w: parse("FACE_RESTORE_CODEFORMER_W", "0.6")?,
            // Crops sharper than ~1.5× the reject gate are already clean → skip restoration.
            face_restore_max_sharpness: parse("FACE_RESTORE_MAX_SHARPNESS", "45.0")?,
            face_restore_min_px: parse("FACE_RESTORE_MIN_PX", "80")?,
            face_hard_min_px: parse("FACE_HARD_MIN_PX", "16")?,
            face_hard_min_det_score: parse("FACE_HARD_MIN_DET_SCORE", "0.3")?,
            face_upscale_model_path: opt("FACE_UPSCALE_MODEL_PATH", "./models/realesrgan_x4plus.onnx"),
            face_upscale_min_px: parse("FACE_UPSCALE_MIN_PX", "48")?,
            face_mint_max_yaw_deg: parse("FACE_MINT_MAX_YAW_DEG", "35.0")?,
            face_mint_max_pitch_deg: parse("FACE_MINT_MAX_PITCH_DEG", "30.0")?,
            face_restored_may_mint: parse("FACE_RESTORED_MAY_MINT", "true")?,
            face_persist_crop: parse("FACE_PERSIST_CROP", "true")?,
            face_best_shot_enabled: parse("FACE_BEST_SHOT_ENABLED", "true")?,
            blob_dir: opt("BLOB_DIR", "./blobs"),

            plate_enabled: parse("PLATE_ENABLED", "true")?,
            plate_detect_model_path: opt("PLATE_DETECT_MODEL_PATH", "./models/lp_detector.onnx"),
            plate_ocr_model_path: opt("PLATE_OCR_MODEL_PATH", "./models/lp_ocr_cct.onnx"),
            plate_ocr_ctc: parse("PLATE_OCR_CTC", "false")?,
            plate_ocr_charset_path: opt("PLATE_OCR_CHARSET_PATH", "./models/lp_ocr_charset.json"),
            plate_detect_input_size: parse("PLATE_DETECT_INPUT_SIZE", "640")?,
            plate_detect_end2end: parse("PLATE_DETECT_END2END", "true")?,
            plate_min_det_score: parse("PLATE_MIN_DET_SCORE", "0.35")?,
            plate_min_px: parse("PLATE_MIN_PX", "16.0")?,
            plate_min_ocr_conf: parse("PLATE_MIN_OCR_CONF", "0.55")?,
            plate_mint_min_ocr_conf: parse("PLATE_MINT_MIN_OCR_CONF", "0.80")?,
            plate_min_len: parse("PLATE_MIN_LEN", "4")?,
            plate_max_edit_distance: parse("PLATE_MAX_EDIT_DISTANCE", "1")?,
            plate_fuzzy_min_similarity: parse("PLATE_FUZZY_MIN_SIMILARITY", "0.7")?,
            plate_vehicle_roi_margin: parse("PLATE_VEHICLE_ROI_MARGIN", "0.10")?,
            plate_sr_min_side_px: parse("PLATE_SR_MIN_SIDE_PX", "64.0")?,
            plate_detect_whole_frame: parse("PLATE_DETECT_WHOLE_FRAME", "false")?,
            plate_required: parse("PLATE_REQUIRED", "false")?,

            heartbeat_interval: Duration::from_secs(parse("WORKER_HEARTBEAT_SECS", "10")?),
            worker_id: std::env::var("WORKER_ID").ok().filter(|s| !s.trim().is_empty()),

            events: crate::events_producer::EventsConfig {
                enabled: parse("EVENTS_ENABLED", "true")?,
                alerts_enabled: parse("EVENTS_ALERTS_ENABLED", "true")?,
                session_bucket_secs: parse("EVENTS_SESSION_BUCKET_SECS", "30")?,
                object_min_score: parse("EVENTS_OBJECT_MIN_SCORE", "0.4")?,
                object_suppress_person: parse("EVENTS_OBJECT_SUPPRESS_PERSON", "true")?,
                plate_seen_min_conf: parse("EVENTS_PLATE_SEEN_MIN_CONF", "0.55")?,
                negative_sentiment_warns: parse("EVENTS_NEGATIVE_SENTIMENT_WARNS", "true")?,
                unknown_person_severity,
            },

            delivery: {
                let timeout_ms: u64 = parse("ALERT_DELIVERY_TIMEOUT_MS", "8000")?;
                crate::delivery::DeliveryConfig {
                    enabled: parse("ALERT_DELIVERY_ENABLED", "true")?,
                    poll_secs: parse("ALERT_DELIVERY_POLL_SECS", "10")?,
                    batch: parse("ALERT_DELIVERY_BATCH", "20")?,
                    max_attempts: parse("ALERT_DELIVERY_MAX_ATTEMPTS", "6")?,
                    timeout_ms,
                    // Lease must exceed the request timeout so an in-flight send isn't re-claimed
                    // before it can finish; timeout + 30s buffer.
                    lease_secs: (timeout_ms as f64) / 1000.0 + 30.0,
                    backoff_base_secs: parse("ALERT_DELIVERY_BACKOFF_BASE_SECS", "30")?,
                    backoff_max_secs: parse("ALERT_DELIVERY_BACKOFF_MAX_SECS", "3600")?,
                    signing_secret: std::env::var("ALERT_WEBHOOK_SIGNING_SECRET")
                        .ok()
                        .filter(|s| !s.trim().is_empty()),
                    // Local-first default: LAN webhook targets (Home Assistant, etc.) are allowed.
                    allow_private: parse("ALERT_WEBHOOK_ALLOW_PRIVATE", "true")?,
                }
            },

            governor: crate::governor::GovernorConfig {
                enabled: parse("LOAD_GOVERNOR_ENABLED", "true")?,
                sample: Duration::from_secs(parse("LOAD_SAMPLE_SECS", "5")?),
                slope_elevated: parse("LOAD_SLOPE_ELEVATED", "0.05")?,
                slope_saturated: parse("LOAD_SLOPE_SATURATED", "0.10")?,
                use_cpu: parse("LOAD_GOVERNOR_USE_CPU", "true")?,
                cpu_elevated: parse("LOAD_CPU_ELEVATED", "0.80")?,
                cpu_saturated: parse("LOAD_CPU_SATURATED", "0.95")?,
                recover_samples: parse("LOAD_RECOVER_SAMPLES", "3")?,
                pause_vision_first: parse("LOAD_PAUSE_VISION_FIRST", "true")?,
                cooldown_elevated: Duration::from_millis(parse("BACKLOG_COOLDOWN_MS", "0")?),
                cooldown_saturated: Duration::from_millis(parse(
                    "BACKLOG_SATURATED_COOLDOWN_MS",
                    "250",
                )?),
            },

            metrics_addr: {
                let s = opt("WORKER_METRICS_ADDR", "127.0.0.1:9100");
                if s.trim().is_empty() {
                    None
                } else {
                    Some(
                        s.trim()
                            .parse()
                            .map_err(|e| anyhow!("WORKER_METRICS_ADDR={s:?} invalid: {e}"))?,
                    )
                }
            },
        })
    }
}

fn opt(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn parse<T>(key: &str, default: &str) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = std::env::var(key).unwrap_or_else(|_| default.to_string());
    raw.trim()
        .parse::<T>()
        .map_err(|e| anyhow!("env var {key}={raw:?} is invalid: {e}"))
}
