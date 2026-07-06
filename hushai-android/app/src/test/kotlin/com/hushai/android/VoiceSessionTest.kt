package com.hushai.android

import com.hushai.android.assistant.VoiceSession
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * The voice chat session's idle-window + persistence-callback behaviour, exercised with in-memory
 * stubs for the load/save/clear lambdas (the real wiring persists to DataStore).
 */
class VoiceSessionTest {
    /** A stub store standing in for DataStore. */
    private class Store {
        var saved: Pair<String, Long>? = null
        var clears = 0
        fun session(initial: Pair<String, Long>? = null) = VoiceSession(
            load = { initial },
            save = { id, at -> saved = id to at },
            clear = { clears++; saved = null },
        )
    }

    @Test fun recordThenContinueWithinWindow() {
        val store = Store()
        val s = store.session()
        val t0 = 1_000_000L
        s.record("sess-1", t0)
        assertEquals("sess-1", s.currentOrNull(t0 + 60_000)) // 1 min later, well inside 15 min
        assertEquals("sess-1" to t0, store.saved)
    }

    @Test fun expiresAfterIdleWindow() {
        val store = Store()
        val s = store.session()
        val t0 = 5_000_000L
        s.record("sess-1", t0)
        val justPast = t0 + VoiceSession.IDLE_WINDOW_MILLIS + 1
        assertNull("stale session must not be reused", s.currentOrNull(justPast))
    }

    @Test fun loadsPersistedSessionOnFirstUse() {
        val t0 = 9_000_000L
        val store = Store()
        val s = store.session(initial = "persisted" to t0)
        // Within the window from the persisted timestamp → reused without a fresh record().
        assertEquals("persisted", s.currentOrNull(t0 + 1000))
    }

    @Test fun loadedButStaleSessionIsNotReused() {
        val t0 = 9_000_000L
        val store = Store()
        val s = store.session(initial = "persisted" to t0)
        assertNull(s.currentOrNull(t0 + VoiceSession.IDLE_WINDOW_MILLIS + 1))
    }

    @Test fun resetClearsAndForgets() {
        val store = Store()
        val s = store.session()
        s.record("sess-1", 1_000L)
        s.reset()
        assertEquals(1, store.clears)
        assertNull(s.currentOrNull(1_500L))
    }

    @Test fun noSessionReturnsNull() {
        val store = Store()
        val s = store.session()
        assertNull(s.currentOrNull(123L))
    }

    @Test fun resetCommandsMatchWholePhrase() {
        for (cmd in listOf(
            "new chat", "New Chat", "new conversation", "start over", "Start fresh!",
            "forget that", "please start over", "start a new conversation",
        )) {
            org.junit.Assert.assertTrue(cmd, VoiceSession.isResetCommand(cmd))
        }
    }

    @Test fun resetCommandsDoNotMatchRealQuestions() {
        for (q in listOf(
            "start over the recording from monday",
            "what did we chat about",
            "did the new conversation about money happen today",
            "forget that box, where is the car",
            "when does the movie start",
            "",
        )) {
            org.junit.Assert.assertFalse(q, VoiceSession.isResetCommand(q))
        }
    }
}
