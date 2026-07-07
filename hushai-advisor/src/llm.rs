//! The advisor's LLM steps: one small typed function per pipeline agent, all funneling
//! through one decode profile (temp 0 + optional seed + explicit num_ctx).
//!
//! Style follows hushai-rag/src/llm.rs: strict output contracts, forgiving parsers with
//! SAFE DEFAULTS (a flaky local 7B must never wedge the pipeline — an unparseable gate
//! verdict means "proceed", an unparseable critique means "ok"), best-effort steps that
//! never fail the turn. The judge steps (sufficiency gate, critique) can run on a larger
//! model via `ADVISOR_JUDGE_MODEL` without touching the drafting model.

use anyhow::{Context, anyhow};
use futures_util::{Stream, StreamExt};
use rig::agent::MultiTurnStreamItem;
use rig::client::CompletionClient;
use rig::completion::Prompt;
use rig::providers::ollama;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};

/// Sufficiency-gate verdict (spec agents 1+2, Min-Info + Yenta, one judgment).
#[derive(Debug, Clone, PartialEq)]
pub enum Sufficiency {
    Proceed,
    Ask(Vec<String>),
}

/// Answer-Controller verdict (spec agent 6).
#[derive(Debug, Clone, PartialEq)]
pub struct Critique {
    pub ok: bool,
    pub note: String,
}

pub struct Llm {
    client: ollama::Client,
    model: String,
    /// Optional override for the JUDGE steps (sufficiency + critique).
    judge_model: Option<String>,
    temperature: f64,
    seed: Option<i64>,
    /// Ollama `options.num_ctx` — the advisor's multi-chapter prompts overflow the
    /// 4096 default SILENTLY without this (see config.rs).
    num_ctx: i64,
}

impl Llm {
    pub fn new(
        ollama_base_url: &str,
        model: &str,
        judge_model: Option<&str>,
        temperature: f64,
        seed: Option<i64>,
        num_ctx: i64,
    ) -> anyhow::Result<Self> {
        let client = ollama::Client::builder()
            .api_key(ollama::OllamaApiKey::default())
            .base_url(ollama_base_url)
            .build()
            .with_context(|| format!("building Ollama client for {ollama_base_url}"))?;
        Ok(Self {
            client,
            model: model.to_string(),
            judge_model: judge_model.map(str::to_string),
            temperature,
            seed,
            num_ctx,
        })
    }

    /// Shared decode profile. `.temperature()` maps to `options.temperature`; `num_ctx`
    /// and the optional `seed` ride in `additional_params`, which Rig merges into the
    /// Ollama `options` object (only think/keep_alive are lifted top-level).
    fn tune(
        &self,
        b: rig::agent::AgentBuilder<ollama::CompletionModel>,
    ) -> rig::agent::AgentBuilder<ollama::CompletionModel> {
        let mut params = serde_json::json!({ "num_ctx": self.num_ctx });
        if let Some(seed) = self.seed {
            params["seed"] = serde_json::json!(seed);
        }
        b.temperature(self.temperature).additional_params(params)
    }

    fn agent_on(&self, model: &str, preamble: &str) -> rig::agent::Agent<ollama::CompletionModel> {
        self.tune(self.client.agent(model)).preamble(preamble).build()
    }

    fn drafter(&self, preamble: &str) -> rig::agent::Agent<ollama::CompletionModel> {
        self.agent_on(&self.model, preamble)
    }

    fn judge(&self, preamble: &str) -> rig::agent::Agent<ollama::CompletionModel> {
        let model = self.judge_model.clone().unwrap_or_else(|| self.model.clone());
        self.agent_on(&model, preamble)
    }

    // ---- ingest-time steps ---------------------------------------------------------

    /// Best-effort LLM copy-edit of a heuristically-cleaned chapter (fix residual OCR
    /// artifacts ONLY). Returns `None` — caller keeps the heuristic text — when the model
    /// errors or the output length deviates more than 15% from the input (the guard that
    /// stops a 7B from paraphrasing the chapter wholesale).
    pub async fn clean_ocr(&self, text: &str) -> Option<String> {
        let agent = self.drafter(
            "You are a copy editor fixing OCR extraction artifacts in a book chapter: broken \
             words, stray characters, wrong hyphenation, misrecognized letters. Fix ONLY such \
             artifacts. Do NOT paraphrase, summarize, reorder, or change wording. Preserve the \
             paragraph breaks (blank lines) exactly. Output ONLY the corrected text.",
        );
        match agent.prompt(text.to_string()).await {
            Ok(out) => {
                let out = out.trim().to_string();
                let (a, b) = (out.len() as f64, text.len() as f64);
                if out.is_empty() || a < b * 0.85 || a > b * 1.15 {
                    None
                } else {
                    Some(out)
                }
            }
            Err(_) => None,
        }
    }

    /// 1–2 sentence chapter synopsis for the Traffic Controller's routing cards.
    pub async fn synopsize(&self, title: Option<&str>, body: &str) -> anyhow::Result<String> {
        let agent = self.drafter(
            "You summarize a book chapter about persuasion into ONE or TWO sentences that let a \
             router decide when the chapter applies: name the principle and the situations it \
             helps with. Output ONLY the synopsis, nothing else.",
        );
        let excerpt: String = body.chars().take(8000).collect();
        let prompt = match title {
            Some(t) => format!("Chapter title: {t}\n\nChapter text:\n{excerpt}\n\nSynopsis:"),
            None => format!("Chapter text:\n{excerpt}\n\nSynopsis:"),
        };
        let raw = agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("synopsis prompt failed: {e}"))?;
        Ok(raw.trim().trim_matches('"').to_string())
    }

    // ---- consultation steps --------------------------------------------------------

    /// Spec agents 1+2 in one judgment: is there enough to advise on, and if not, what
    /// (at most `max_questions`) targeted follow-up questions to ask. Contract: the model
    /// outputs either the literal word PROCEED or a numbered question list; anything
    /// unparseable means Proceed (the gate can slow a consultation, never block it).
    pub async fn assess_sufficiency(
        &self,
        history_text: &str,
        message: &str,
        max_questions: usize,
    ) -> Sufficiency {
        // Tuned against qwen2.5:7b: the earlier "is the situation understood?" phrasing
        // PROCEEDed on a bare "I walked into my house." (no problem stated = nothing to
        // ask, per the model). Requiring BOTH a concrete problem AND a discernible goal —
        // with that exact bare-statement counter-example inline — flips it to asking.
        let agent = self.judge(
            "You are the intake gate of a personal advisor. The person is asking for advice. \
             Output PROCEED only when BOTH hold: (1) the message describes a concrete situation \
             or problem, and (2) it is clear what they want to decide, resolve, or achieve, with \
             the stakes that would change the advice known (people involved, relationships, \
             money, time pressure, emotional state — whichever matter here). If either is \
             missing, or the message is a bare statement with no problem in it (e.g. \"I walked \
             into my house.\"), output ONLY a numbered list of the most important follow-up \
             questions (at most 3), one per line, no preamble. Never output anything besides \
             PROCEED or the numbered questions.",
        );
        let prompt = if history_text.trim().is_empty() {
            format!("Situation:\n{message}\n\nVerdict:")
        } else {
            format!("Conversation so far:\n{history_text}\n\nLatest message:\n{message}\n\nVerdict:")
        };
        match agent.prompt(prompt).await {
            Ok(raw) => parse_sufficiency(&raw, max_questions),
            // Best-effort: an unreachable judge must not block the consultation.
            Err(_) => Sufficiency::Proceed,
        }
    }

    /// Spec agent 3 (Message Refiner): fold the situation + follow-up answers into one
    /// clean standalone problem statement. Falls back to the latest message on any doubt
    /// (the condense() contract from hushai-rag).
    pub async fn refine_question(&self, history_text: &str, message: &str) -> String {
        if history_text.trim().is_empty() {
            return message.to_string();
        }
        let agent = self.drafter(
            "You rewrite a conversation between a person and their advisor into ONE clear, \
             self-contained problem statement: the situation, the relevant facts learned from \
             the follow-up answers, and what the person wants to decide or achieve. Correct \
             spelling and grammar. Do not add facts, do not advise. Output ONLY the problem \
             statement.",
        );
        let prompt =
            format!("Conversation:\n{history_text}\n\nLatest message:\n{message}\n\nProblem statement:");
        match agent.prompt(prompt).await {
            Ok(raw) => {
                let r = raw.trim().trim_matches('"').trim().to_string();
                if r.is_empty() || r.len() > 2000 {
                    message.to_string()
                } else {
                    r
                }
            }
            Err(_) => message.to_string(),
        }
    }

    /// Spec agent 4 (Traffic Controller): pick the chapters to advise from. `cards_text`
    /// is the rendered synopsis list; `semantic_candidates` are chapter numbers surfaced
    /// by the embedding search (the anti-tunnel-vision widening signal); `draft` is the
    /// current draft on refine iterations (so the router can spot NEWLY relevant
    /// chapters); `already_used` chapters may be re-picked but only NEW ones extend the
    /// loop. Returns validated, deduped chapter numbers (≤ `max_pick`).
    #[allow(clippy::too_many_arguments)]
    pub async fn route_chapters(
        &self,
        question: &str,
        draft: Option<&str>,
        memory_context: &str,
        cards_text: &str,
        semantic_candidates: &[i32],
        already_used: &[i32],
        max_chapter: i32,
        max_pick: usize,
    ) -> anyhow::Result<Vec<i32>> {
        let agent = self.drafter(
            "You are a routing agent for an advisor grounded in one book. Given the chapter \
             list (number: synopsis), pick the 2-4 chapters whose principles best apply to the \
             person's situation. Avoid tunnel vision: principles interact, so consider chapters \
             beyond the single most obvious one — the semantic search candidates are hints worth \
             weighing, not orders. Output ONLY the chosen chapter numbers separated by commas, \
             nothing else.",
        );
        let mut prompt = format!("Chapters:\n{cards_text}\n\nSituation:\n{question}\n");
        if !memory_context.trim().is_empty() {
            prompt.push_str(&format!("\nPast context:\n{memory_context}\n"));
        }
        if !semantic_candidates.is_empty() {
            prompt.push_str(&format!(
                "\nSemantic search candidates (chapters whose text matched the situation): {}\n",
                render_numbers(semantic_candidates)
            ));
        }
        if let Some(d) = draft {
            prompt.push_str(&format!(
                "\nA draft answer exists (below). Pick any chapters — INCLUDING ones not used \
                 yet — that the draft's proposed course of action makes newly relevant.\nChapters \
                 already used: {}\nDraft:\n{d}\n",
                render_numbers(already_used)
            ));
        }
        prompt.push_str("\nChosen chapter numbers:");
        let raw = agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("chapter routing prompt failed: {e}"))?;
        Ok(parse_chapter_numbers(&raw, max_chapter, max_pick))
    }

    /// Spec agent 5 (Answer Agent): draft advice grounded ONLY in the supplied chapters
    /// (+ past-consultation context). `chapters_block` is the pre-budgeted rendered text.
    pub async fn draft_answer(
        &self,
        question: &str,
        chapters_block: &str,
        memory_context: &str,
    ) -> anyhow::Result<String> {
        let agent = self.drafter(
            "You are a personal advisor. Advise using ONLY the principles in the book chapters \
             provided — apply them concretely to the person's situation, citing which chapter a \
             recommendation comes from as (ch. N). Where past consultations are provided, keep \
             the advice consistent with them. If the chapters genuinely don't cover the \
             situation, say so honestly instead of inventing principles. Be specific and \
             actionable, not generic.",
        );
        let mut prompt = format!("Book chapters:\n{chapters_block}\n");
        if !memory_context.trim().is_empty() {
            prompt.push_str(&format!("\nPast consultations:\n{memory_context}\n"));
        }
        prompt.push_str(&format!("\nSituation:\n{question}\n\nAdvice:"));
        agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("draft prompt failed: {e}"))
    }

    /// Spec agent 6 (Answer Controller): does the draft actually answer the core
    /// question? Kept separate from the rewrite (a 7B doing judge+editor in one output is
    /// a parse-fragility trap). Unparseable output means OK — never loop on garbage.
    pub async fn critique(&self, question: &str, draft: &str) -> Critique {
        let agent = self.judge(
            "You review a draft of advice against the person's situation. Judge ONE thing: does \
             the draft actually answer the core question with concrete, applicable advice? \
             Output exactly one line: 'OK' if it does, or 'REVISE: <one short reason>' if it \
             misses the question, stays generic, or contradicts itself.",
        );
        let prompt = format!("Situation:\n{question}\n\nDraft advice:\n{draft}\n\nVerdict:");
        match agent.prompt(prompt).await {
            Ok(raw) => parse_critique(&raw),
            Err(_) => Critique { ok: true, note: String::new() },
        }
    }

    /// Spec agent 7 (Answer Refiner), delivery half: final clarity edit, streamed to the
    /// client as token deltas. The loop-back-to-routing behaviour lives in pipeline.rs.
    /// `temperament_hint` is the deferred Temperament agent's hook (always None in v1).
    pub async fn polish_stream(
        &self,
        question: &str,
        draft: &str,
        critique_note: &str,
        temperament_hint: Option<&str>,
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<String>> + Send> {
        let agent = self.drafter(
            "You edit draft advice for clarity and concision, keeping ALL of its substance, \
             recommendations, and (ch. N) citations. Fix the reviewer's note when one is given. \
             Address the person directly. Output ONLY the final advice.",
        );
        let mut prompt = format!("Situation:\n{question}\n\nDraft advice:\n{draft}\n");
        if !critique_note.trim().is_empty() {
            prompt.push_str(&format!("\nReviewer's note: {critique_note}\n"));
        }
        if let Some(hint) = temperament_hint {
            prompt.push_str(&format!("\nTemperament of those involved: {hint}\n"));
        }
        prompt.push_str("\nFinal advice:");
        let stream = agent.stream_prompt(prompt).await;
        Ok(stream.filter_map(|item| async move {
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
                    Some(Ok(t.text))
                }
                Ok(_) => None,
                Err(e) => Some(Err(anyhow!("LLM stream failed: {e}"))),
            }
        }))
    }

    /// Spec agent 9 (Memory): 2–3 sentence Q&A summary for the long-term memory store.
    /// Best-effort at the call site — a failed summary never fails the turn.
    pub async fn summarize_qa(&self, question: &str, answer: &str) -> anyhow::Result<String> {
        let agent = self.drafter(
            "You summarize one advisor consultation into 2-3 sentences for a long-term memory: \
             the situation, and the advice given. Write in the third person, past tense. Output \
             ONLY the summary.",
        );
        let prompt = format!("Situation:\n{question}\n\nAdvice given:\n{answer}\n\nSummary:");
        let raw = agent
            .prompt(prompt)
            .await
            .map_err(|e| anyhow!("memory summary prompt failed: {e}"))?;
        Ok(raw.trim().to_string())
    }
}

// ---- parsers (pure; unit-tested) --------------------------------------------------

/// Parse the sufficiency verdict. PROCEED anywhere on its own line wins; otherwise
/// numbered/bulleted lines become questions (capped); no parseable content -> Proceed.
pub fn parse_sufficiency(raw: &str, max_questions: usize) -> Sufficiency {
    let mut questions: Vec<String> = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        if t.eq_ignore_ascii_case("proceed") || t.eq_ignore_ascii_case("proceed.") {
            return Sufficiency::Proceed;
        }
        let stripped = strip_list_marker(t);
        if !stripped.is_empty() && stripped.contains('?') {
            questions.push(stripped.to_string());
        }
    }
    questions.truncate(max_questions.max(1));
    if questions.is_empty() {
        Sufficiency::Proceed
    } else {
        Sufficiency::Ask(questions)
    }
}

/// Parse the critique verdict: a line starting with OK -> ok; REVISE: <reason> -> not ok.
/// Anything else -> ok (never loop on an unparseable verdict).
pub fn parse_critique(raw: &str) -> Critique {
    let first = raw.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let upper = first.to_ascii_uppercase();
    if upper.starts_with("REVISE") {
        let note = first
            .split_once(':')
            .map(|(_, n)| n.trim())
            .unwrap_or("")
            .to_string();
        Critique { ok: false, note }
    } else {
        Critique { ok: true, note: String::new() }
    }
}

/// Extract distinct chapter numbers in [1, max_chapter] from a model response, in order
/// of first appearance, capped at `max_pick`.
pub fn parse_chapter_numbers(raw: &str, max_chapter: i32, max_pick: usize) -> Vec<i32> {
    let mut out: Vec<i32> = Vec::new();
    let mut current = String::new();
    for c in raw.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() {
            current.push(c);
        } else if !current.is_empty() {
            if let Ok(n) = current.parse::<i32>()
                && (1..=max_chapter).contains(&n)
                && !out.contains(&n)
            {
                out.push(n);
            }
            current.clear();
        }
    }
    out.truncate(max_pick.max(1));
    out
}

fn strip_list_marker(line: &str) -> &str {
    let t = line.trim_start_matches(|c: char| {
        c.is_ascii_digit() || matches!(c, '.' | ')' | '-' | '*' | ':')
    });
    t.trim()
}

fn render_numbers(nums: &[i32]) -> String {
    if nums.is_empty() {
        return "(none)".to_string();
    }
    nums.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::{
        Critique, Sufficiency, parse_chapter_numbers, parse_critique, parse_sufficiency,
    };

    #[test]
    fn sufficiency_proceed_and_questions() {
        assert_eq!(parse_sufficiency("PROCEED", 3), Sufficiency::Proceed);
        assert_eq!(parse_sufficiency("  proceed.  ", 3), Sufficiency::Proceed);
        let v = parse_sufficiency(
            "1. How long have you been married?\n2) Do you have children?\n3. What is your financial situation?\n4. Extra?",
            3,
        );
        match v {
            Sufficiency::Ask(qs) => {
                assert_eq!(qs.len(), 3); // capped
                assert_eq!(qs[0], "How long have you been married?");
                assert_eq!(qs[1], "Do you have children?");
            }
            _ => panic!("expected Ask"),
        }
        // Unparseable output -> safe default Proceed.
        assert_eq!(parse_sufficiency("hmm not sure", 3), Sufficiency::Proceed);
        // A PROCEED line wins even when the model rambles around it.
        assert_eq!(
            parse_sufficiency("The situation is clear.\nPROCEED", 3),
            Sufficiency::Proceed
        );
    }

    #[test]
    fn critique_parsing() {
        assert_eq!(parse_critique("OK"), Critique { ok: true, note: String::new() });
        assert_eq!(parse_critique("ok, looks good"), Critique { ok: true, note: String::new() });
        let c = parse_critique("REVISE: too generic, no concrete steps");
        assert!(!c.ok);
        assert_eq!(c.note, "too generic, no concrete steps");
        // Unparseable -> ok (never loop on garbage).
        assert!(parse_critique("maybe?").ok);
        assert!(parse_critique("").ok);
    }

    #[test]
    fn chapter_number_parsing() {
        assert_eq!(parse_chapter_numbers("3, 17, 41", 50, 4), vec![3, 17, 41]);
        assert_eq!(
            parse_chapter_numbers("Chapters 3 and 17, plus 3 again", 50, 4),
            vec![3, 17]
        );
        // Out-of-range and over-cap picks are dropped.
        assert_eq!(parse_chapter_numbers("0, 51, 200, 7", 50, 4), vec![7]);
        assert_eq!(parse_chapter_numbers("1,2,3,4,5,6", 50, 4), vec![1, 2, 3, 4]);
        assert_eq!(parse_chapter_numbers("none apply", 50, 4), Vec::<i32>::new());
    }
}
