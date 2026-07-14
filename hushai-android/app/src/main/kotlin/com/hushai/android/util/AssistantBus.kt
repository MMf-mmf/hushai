package com.hushai.android.util

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.update

enum class AssistantPhase {
    OFF, LISTENING, AWAIT_QUESTION, THINKING, SPEAKING, ENROLLING,
    // Wake-word-free window while a multi-turn advisor consult waits for the owner's answer to
    // the advisor's follow-up questions (see assistant/VoiceAssistant advisor consult loop).
    AWAIT_FOLLOWUP
}

/** Live voice-assistant state shared from the service to the UI. */
data class AssistantStatus(
    val enabled: Boolean = false,
    val ready: Boolean = false,          // Vosk models loaded
    val enrolled: Boolean = false,       // owner voice profile exists
    val phase: AssistantPhase = AssistantPhase.OFF,
    val lastHeard: String? = null,       // most recent recognized utterance
    val lastQuestion: String? = null,
    val lastAnswer: String? = null,
    val note: String? = null,            // errors, rejections, hints
    val enrollProgress: Int = 0,         // 0..100 while enrolling
    val enrollStep: Int = 0,             // accepted samples so far (guided enrollment)
    val enrollTotal: Int = 0,            // samples the guided flow asks for
    val enrollPrompt: String? = null,    // the phrase the user should say right now
)

/** Process-wide assistant status bus; the service publishes, the UI observes. */
object AssistantBus {
    val state = MutableStateFlow(AssistantStatus())

    fun update(transform: (AssistantStatus) -> AssistantStatus) {
        state.update(transform)
    }

    fun reset() {
        state.value = AssistantStatus()
    }
}
