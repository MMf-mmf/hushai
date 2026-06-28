package com.hushai.android

import com.hushai.android.assistant.ReflectionIntent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ReflectionIntentTest {

    @Test fun routesIntrospectiveQuestionsToReflection() {
        val reflective = listOf(
            "how have I been doing lately",
            "how am I doing",
            "how have my conversational skills been",
            "how are my social skills",
            "what improvements can you give me",
            "how can I improve",
            "how can I do better in conversations",
            "how productive have I been this week",
            "how's my mood been",
            "give me feedback on how I talk",
            "what's my communication like",
            "reflect on my week",
        )
        for (q in reflective) {
            assertTrue("should be reflective: '$q'", ReflectionIntent.isReflective(q))
            assertEquals("reflection", ReflectionIntent.agentFor(q))
        }
    }

    @Test fun leavesFactualRecallOnDefaultAgent() {
        val recall = listOf(
            "what did Bob say about the cameras",
            "when is the meeting tomorrow",
            "what did I say about lunch",
            "did anyone mention the deadline",
            "what time did we agree on",
            "remind me what the plan was",
        )
        for (q in recall) {
            assertFalse("should NOT be reflective: '$q'", ReflectionIntent.isReflective(q))
            assertNull(ReflectionIntent.agentFor(q))
        }
    }

    @Test fun emptyIsNotReflective() {
        assertFalse(ReflectionIntent.isReflective(""))
        assertFalse(ReflectionIntent.isReflective("   "))
    }
}
