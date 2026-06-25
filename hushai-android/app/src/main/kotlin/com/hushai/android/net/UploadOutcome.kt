package com.hushai.android.net

/**
 * Classification of a `POST /v1/segments` response into the client action the
 * contract (§6) + the backend's verified superset (`error.rs`) demand. The
 * cardinal rule: only [Accepted] permits deleting the local copy.
 */
sealed interface UploadOutcome {
    /** 200 — durably accepted (or idempotent re-accept). Safe to delete locally. */
    data object Accepted : UploadOutcome

    /** 429 / 507 / 408 / 5xx / network error — transient. Retain, backoff, retry. */
    data class RetryLater(val reason: String) : UploadOutcome

    /** 422 — integrity mismatch. Re-send the SAME segment_id + bytes (bounded). */
    data class Resend(val reason: String) : UploadOutcome

    /** 401 — bad/expired token. Retain, surface re-auth, never drop. */
    data class Unauthorized(val reason: String) : UploadOutcome

    /** 400 / 409 / 413 — permanent client error. Quarantine; re-send cannot help. */
    data class PermanentClientError(val code: Int, val reason: String) : UploadOutcome
}
