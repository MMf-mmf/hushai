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
    /// Base URL of the Ollama server the worker sends embedding requests to.
    /// From `EMBED_OLLAMA_BASE_URL`, falling back to `OLLAMA_BASE_URL`. Keeping this
    /// separate from the RAG answer-LLM endpoint lets always-on embedding load be
    /// routed to its own instance so it can't starve query answering (see RagConfig).
    pub embed_ollama_base_url: String,
    /// Embedding model name (must produce 1024-dim vectors to match the schema).
    pub embed_model: String,
    /// Number of concurrent per-segment pipelines.
    pub worker_concurrency: usize,
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
    pub clip_image_model_path: String,
    /// How many frames to sample per ~2s segment for detection/embedding.
    pub frames_per_segment: usize,
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
    /// Ordered class→char map for the OCR head (sidecar JSON, an array of single-char strings).
    pub plate_ocr_charset_path: String,
    /// Square input side of the plate detector (letterboxed). Validate at provisioning.
    pub plate_detect_input_size: usize,
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

    /// Address for the worker's Prometheus `/metrics` + `/healthz` server (roadmap B1/B5). The
    /// worker has no other HTTP port. `WORKER_METRICS_ADDR`; empty disables it. Default :9100.
    pub metrics_addr: Option<SocketAddr>,
}

impl WorkerConfig {
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
            embed_ollama_base_url: opt("EMBED_OLLAMA_BASE_URL", &ollama_base_url),
            embed_model: opt("EMBED_MODEL", "mxbai-embed-large"),
            worker_concurrency: parse("WORKER_CONCURRENCY", "2")?,
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
            clip_image_model_path: opt("CLIP_IMAGE_MODEL_PATH", "./models/clip_vit_b32_image.onnx"),
            frames_per_segment: parse("FRAMES_PER_SEGMENT", "2")?,
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
            plate_ocr_charset_path: opt("PLATE_OCR_CHARSET_PATH", "./models/lp_ocr_charset.json"),
            plate_detect_input_size: parse("PLATE_DETECT_INPUT_SIZE", "640")?,
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
