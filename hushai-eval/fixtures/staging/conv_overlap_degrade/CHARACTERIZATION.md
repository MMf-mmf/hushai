# conv_overlap_degrade — characterization (2026-07-06)

Staging probe, NOT gated (no train/holdout split; the harness never scores it). Graceful-
degradation check only — true acoustic double-talk (two dialogues amixed onto one mono
track) is information-theoretically unseparable for this pipeline and is documented as such.

Injected against the eval stack (hushai_test, threader knobs from `local_dev/eval.env`,
config_hash lineage `9101d523bb6b975c`, git 37a8372-dirty + 0025 threading branch):

- Pipeline COMPLETED: 6 transcript sentences, no crash, no stuck segments.
- 2/6 sentences refused speaker attribution (NULL speaker_id) — the SAFE path: overlapped
  speech is never misattributed to a specific speaker.
- 1 distinct conversation observed (gate was ≤ 2 with tolerance): all rows threaded, none
  left NULL after quiesce.
- Chat no-fabrication probe: "Did anyone talk about buying a boat?" →
  "I don't have information about that in the recordings." — no invented specifics from
  the garbled double-talk ASR.

This probe gates NOTHING about clustering quality on purpose. Its only job: prove
degradation stays safe (complete + refuse + don't fabricate) when the acoustic premise
of diarization is violated.
