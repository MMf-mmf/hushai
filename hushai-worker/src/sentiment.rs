//! Per-segment sentiment classification via the in-stack local Ollama LLM.
//!
//! Lexical only: the classifier sees the ~2 seconds of transcript text for a segment
//! and returns ONE of `positive | neutral | negative`, which `process_segment`
//! denormalizes onto every sentence chunked from that segment (same pattern as
//! `device_id`). Acoustic prosody is a deliberate non-goal — the `emotion` column
//! stays NULL.
//!
//! Always-on safety: the call is wrapped in a hard timeout so a slow/stuck LLM can
//! never stall the pipeline; on timeout, error, or an unparseable reply we return
//! `None` (the `sentiment` column is left NULL — a NULL beats a wrong guess).
//!
//! Mirrors `hushai-rag/src/llm.rs`'s rig Ollama client surface (rig 0.37).

use std::time::Duration;

use rig::client::CompletionClient;
use rig::completion::Prompt;
use rig::providers::ollama;

const PREAMBLE: &str = "You are a sentiment classifier. Read the transcript snippet and decide its \
overall sentiment. Respond with EXACTLY ONE word — one of: positive, negative, neutral. \
Output only that single lowercase word, with no punctuation, quotes, or explanation.";

/// The three accepted labels. Anything else parses to `None`.
const LABELS: [&str; 3] = ["positive", "neutral", "negative"];

/// Classifies the sentiment of a segment's transcript text.
#[derive(Clone)]
pub struct SentimentClassifier {
    client: ollama::Client,
    model: String,
    timeout: Duration,
    enabled: bool,
}

impl SentimentClassifier {
    pub fn new(
        ollama_base_url: &str,
        model: &str,
        timeout: Duration,
        enabled: bool,
    ) -> anyhow::Result<Self> {
        let client = ollama::Client::builder()
            .api_key(ollama::OllamaApiKey::default())
            .base_url(ollama_base_url)
            .build()
            .map_err(|e| anyhow::anyhow!("building Ollama client for {ollama_base_url}: {e}"))?;
        Ok(Self {
            client,
            model: model.to_string(),
            timeout,
            enabled,
        })
    }

    /// Classify the overall sentiment of `text`. Returns `Some(label)` only for a
    /// confident, parseable result; `None` when disabled, empty, timed out, errored,
    /// or unparseable (caller writes NULL in those cases).
    pub async fn classify(&self, text: &str) -> Option<String> {
        if !self.enabled || text.trim().is_empty() {
            return None;
        }

        let agent = self.client.agent(&self.model).preamble(PREAMBLE).build();
        let prompt = format!("Transcript snippet:\n\"{}\"\n\nSentiment:", text.trim());

        match tokio::time::timeout(self.timeout, agent.prompt(prompt)).await {
            Ok(Ok(reply)) => parse_label(&reply),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "sentiment LLM prompt failed; writing NULL");
                None
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = self.timeout.as_millis(),
                    "sentiment classification timed out; writing NULL"
                );
                None
            }
        }
    }
}

/// Strict-ish parse: accept the reply only if exactly one of the three labels appears
/// as a standalone word (case-insensitive). A reply naming none — or more than one —
/// of the labels is ambiguous and yields `None`.
fn parse_label(reply: &str) -> Option<String> {
    let lower = reply.to_ascii_lowercase();
    let mut found: Option<&str> = None;
    for label in LABELS {
        let present = lower
            .split(|c: char| !c.is_ascii_alphabetic())
            .any(|w| w == label);
        if present {
            if found.is_some() {
                return None; // more than one label mentioned -> ambiguous
            }
            found = Some(label);
        }
    }
    found.map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_label() {
        assert_eq!(parse_label("positive").as_deref(), Some("positive"));
        assert_eq!(parse_label("NEGATIVE").as_deref(), Some("negative"));
        assert_eq!(parse_label("  neutral\n").as_deref(), Some("neutral"));
    }

    #[test]
    fn parses_label_in_a_sentence_with_punctuation() {
        assert_eq!(
            parse_label("The sentiment is positive.").as_deref(),
            Some("positive")
        );
        assert_eq!(parse_label("\"negative\"").as_deref(), Some("negative"));
    }

    #[test]
    fn rejects_ambiguous_or_missing() {
        assert_eq!(parse_label("positive or negative"), None);
        assert_eq!(parse_label("mixed feelings"), None);
        assert_eq!(parse_label(""), None);
        // substring of a longer word must not match
        assert_eq!(parse_label("neutrality"), None);
    }
}
