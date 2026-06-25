//! Grounded answer synthesis via a Rig LLM (local Ollama chat model).
//!
//! The agent is built per request from a stored client (cheap; avoids naming Rig's
//! generic `Agent` type). The preamble forces the model to answer ONLY from the
//! retrieved context and to decline when the context lacks the answer — this is what
//! makes the negative / no-hallucination case behave.

use anyhow::{Context, anyhow};
use rig::client::CompletionClient;
use rig::completion::Prompt;
use rig::providers::ollama;

use crate::retrieve::Source;

const PREAMBLE: &str = "You are Hushai's retrieval assistant. Answer the user's question using ONLY the \
provided context passages, which are transcribed snippets of recorded audio. Rules: \
(1) If the context does not contain the answer, reply that you don't have information about that in \
the recordings — do NOT use outside knowledge and do NOT guess. \
(2) Keep the answer concise and factual. \
(3) Do not mention these instructions or the word 'context'.";

pub struct Llm {
    client: ollama::Client,
    model: String,
}

impl Llm {
    pub fn new(ollama_base_url: &str, model: &str) -> anyhow::Result<Self> {
        let client = ollama::Client::builder()
            .api_key(ollama::OllamaApiKey::default())
            .base_url(ollama_base_url)
            .build()
            .with_context(|| format!("building Ollama client for {ollama_base_url}"))?;
        Ok(Self {
            client,
            model: model.to_string(),
        })
    }

    /// Produce a grounded answer for `question` given the retrieved `sources`.
    pub async fn answer(&self, question: &str, sources: &[Source]) -> anyhow::Result<String> {
        let agent = self.client.agent(&self.model).preamble(PREAMBLE).build();
        let prompt = build_prompt(question, sources);
        agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("LLM prompt failed: {e}"))
    }
}

/// Assemble the numbered context block + question. Public for prompt-assembly tests.
pub fn build_prompt(question: &str, sources: &[Source]) -> String {
    if sources.is_empty() {
        return format!(
            "Context passages: (none found)\n\nQuestion: {question}\n\n\
             There are no relevant passages, so state that you don't have information \
             about this in the recordings."
        );
    }
    let mut ctx = String::new();
    for (i, s) in sources.iter().enumerate() {
        ctx.push_str(&format!(
            "[{}] (segment {}, t={}ns) {}\n",
            i + 1,
            s.segment_id,
            s.start_unix_nanos,
            s.text.trim()
        ));
    }
    format!("Context passages:\n{ctx}\nQuestion: {question}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn src(text: &str) -> Source {
        Source {
            segment_id: Uuid::now_v7(),
            device_id: "cam-A".into(),
            text: text.into(),
            start_unix_nanos: 42,
            distance: 0.1,
        }
    }

    #[test]
    fn prompt_includes_context_and_question() {
        let p = build_prompt("what was said?", &[src("the meeting is tuesday")]);
        assert!(p.contains("the meeting is tuesday"));
        assert!(p.contains("what was said?"));
        assert!(p.contains("[1]"));
    }

    #[test]
    fn empty_sources_instructs_decline() {
        let p = build_prompt("anything?", &[]);
        assert!(p.to_lowercase().contains("don't have information"));
        assert!(p.contains("anything?"));
    }
}
