# conv_topic_shift — characterization (2026-07-06)

Staging probe, NOT gated (no train/holdout split; the harness never scores it).

Injected against the eval stack (hushai_test, threader knobs from `local_dev/eval.env`,
config_hash lineage `9101d523bb6b975c`, git 37a8372-dirty + 0025 threading branch):

- 11 transcript sentences, ALL threaded (0 NULL conversation_id after quiesce).
- **1 distinct conversation observed — matches frozen semantics exactly**: a hard topic
  pivot (holiday → insurance) with the same two voices, no silence gap and no temporal
  interleave must NOT split (`THREADER_TOPIC_ONLY_SPLIT=false`). Splits require temporal
  interleave evidence; topic drift alone is a normal property of one human conversation.
- 4/11 sentences carry NULL speaker_id (TTS voice-collapse boundary refusals) — orphan
  attachment (stage D) still placed them in the single conversation.

If this probe ever starts showing 2 conversations, a topic-only split leaked into the
split gate — that is a regression in `hushai-backend/src/threading.rs` stage C, not a
fixture problem.
