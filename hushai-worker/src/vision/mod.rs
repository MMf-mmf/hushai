//! Vision pipeline (Phase A: face identity + open-vocabulary objects).
//!
//! Runs BESIDE the audio/ASR path, on VIDEO/MUXED segments (`media_type IN (2,3)`), via its own
//! claim/status queue (`segment_vision_status`) so a vision failure and an ASR failure are
//! retried independently. The inference boundary mirrors the asr/speaker layer:
//!   * `model`      — `ort` (ONNX Runtime) session setup; load-dynamic coexistence with sherpa-rs.
//!   * `frames`     — sample RGB frames from the stored segment via ffmpeg.
//!   * `detect`     — YuNet face detection + 5-pt landmarks (Phase A, next).
//!   * `face_embed` — ArcFace 512-d face embedding with landmark alignment + quality gates.
//!   * `objects`    — RF-DETR boxes + open-vocab CLIP image embeddings (Phase B).
//!   * `face_match` — advisory-locked global match-or-mint into persons/person_segments
//!                    (mirrors `speaker_match`).
//!   * `write`      — the single idempotent vision write transaction.

pub mod detect;
pub mod detect_scrfd;
pub mod enhance;
pub mod face_embed;
pub mod face_match;
pub mod frames;
pub mod geom;
pub mod model;
pub mod objects;
pub mod plates;
pub mod write;
