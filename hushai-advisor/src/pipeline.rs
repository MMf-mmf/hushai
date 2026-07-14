//! The per-turn consultation state machine (the spec's answering agents, orchestrated).
//!
//! gathering:
//!   assess_sufficiency (Min-Info + Yenta, ONE judgment)
//!     Ask(qs) & rounds < cap -> persist a followup_questions turn, stay gathering
//!     Proceed (or cap hit)   -> refine_question (Message Refiner) -> phase 'answering'
//! answering (bounded refine loop, max_refine_iters):
//!   route_chapters (Traffic Controller; synopses + semantic candidates + current draft)
//!     -> converged when a later iteration proposes NO new chapters
//!   draft_answer (Answer Agent, grounded in the accumulated chapters)
//!   critique (Answer Controller) -> ok after at least one re-route -> exit loop
//!   polish_stream (Answer Refiner) -> streamed final answer -> persist -> memory -> 'done'
//!
//! Every LLM call is sequential ON PURPOSE: Ollama is shared with the worker/rag
//! services, so the pipeline queues rather than floods. The loop can never wedge:
//! unparseable gate output means Proceed, unparseable critique means ok, the round and
//! iteration caps are hard, and a 'done' session starts a fresh gathering cycle on the
//! next user turn.

use futures_util::{Stream, StreamExt};
use uuid::Uuid;

use crate::books::{self, ChapterRef};
use crate::llm::Sufficiency;
use crate::memory;
use crate::state::AppState;

/// Pipeline progress events, mapped 1:1 onto SSE events by `chat.rs`.
#[derive(Debug)]
pub enum AdvisorEvent {
    /// Progress heartbeat so the UI is never silent during a 30–90s turn.
    Phase { phase: &'static str },
    /// A Yenta round: the turn ends here, phase stays 'gathering'.
    Questions { round: i64, questions: Vec<String> },
    /// The accumulated chapter grounding after a routing iteration.
    Chapters { iteration: usize, chapters: Vec<ChapterRef> },
    /// Long-term memory retrieval outcome (fired once, right after `recalling`): how many past
    /// consultations cleared the distance cutoff, and the nearest one's cosine distance (`None`
    /// when nothing was recalled). Calibration telemetry + the retrieval-proof signal the eval
    /// harness gates on; UI clients ignore it (unknown-event tolerant by design).
    Memory { recalled: i64, nearest_distance: Option<f64> },
    Token { delta: String },
    Done { message_id: Uuid },
    Error { message: String },
}

/// What the HTTP handler resolved before starting the pipeline.
pub struct TurnCtx {
    pub session_id: Uuid,
    /// The user's latest message (already persisted by the handler).
    pub message: String,
    /// Prior turns rendered as "user:/advisor:" lines (trailing window, oldest→newest),
    /// NOT including `message`.
    pub history_text: String,
    /// Yenta rounds already spent this gathering cycle.
    pub followup_rounds: i64,
}

/// Run one consultation turn, yielding progress events. All persistence happens inside
/// (assistant turns, session phase transitions, memory rows).
pub fn run_turn(st: AppState, ctx: TurnCtx) -> impl Stream<Item = AdvisorEvent> + Send {
    async_stream::stream! {
        // ---- gathering: the sufficiency gate --------------------------------------
        yield AdvisorEvent::Phase { phase: "gathering" };
        let force_proceed = ctx.followup_rounds >= st.cfg.max_followup_rounds;
        let verdict = if force_proceed {
            // Cap hit: a flaky judge can never wedge the session in 'gathering'.
            Sufficiency::Proceed
        } else {
            st.llm
                .assess_sufficiency(
                    &ctx.history_text,
                    &ctx.message,
                    st.cfg.max_questions_per_round,
                )
                .await
        };
        if let Sufficiency::Ask(questions) = verdict {
            let round = ctx.followup_rounds + 1;
            let content = questions
                .iter()
                .enumerate()
                .map(|(i, q)| format!("{}. {q}", i + 1))
                .collect::<Vec<_>>()
                .join("\n");
            let persisted =
                crate::chat::insert_followup_round(&st.pool, ctx.session_id, &content, round)
                    .await;
            match persisted {
                Ok(message_id) => {
                    yield AdvisorEvent::Questions { round, questions };
                    yield AdvisorEvent::Done { message_id };
                }
                Err(e) => {
                    tracing::error!(error = format!("{e:#}"), "failed to persist follow-up round");
                    yield AdvisorEvent::Error { message: "failed to save the follow-up questions".into() };
                }
            }
            return;
        }

        // ---- refine: fold the gathered context into one problem statement ---------
        yield AdvisorEvent::Phase { phase: "refining" };
        let refined = st.llm.refine_question(&ctx.history_text, &ctx.message).await;
        if let Err(e) = sqlx::query(
            "UPDATE advisor_sessions \
             SET refined_question = $2, phase = 'answering', updated_at = now() \
             WHERE session_id = $1",
        )
        .bind(ctx.session_id)
        .bind(&refined)
        .execute(&st.pool)
        .await
        {
            tracing::error!(error = format!("{e:#}"), "failed to persist refined question");
        }

        // ---- answering: the corpus + memory context -------------------------------
        let Some((book_id, _book_title)) = books::default_book(&st.pool).await.ok().flatten()
        else {
            yield AdvisorEvent::Error {
                message: "no book has been ingested yet — run the ingest-book binary first".into(),
            };
            return;
        };
        let cards = match books::load_cards(&st.pool, book_id).await {
            Ok(c) if !c.is_empty() => c,
            Ok(_) | Err(_) => {
                yield AdvisorEvent::Error {
                    message: "the book corpus is empty — run the ingest-book binary first".into(),
                };
                return;
            }
        };
        let max_chapter = cards.iter().map(|c| c.chapter_no).max().unwrap_or(0);
        let cards_text = cards
            .iter()
            .map(|c| {
                format!(
                    "{}: {} — {}",
                    c.chapter_no,
                    c.title.as_deref().unwrap_or("(untitled)"),
                    c.synopsis.as_deref().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let titles: std::collections::HashMap<i32, Option<String>> = cards
            .iter()
            .map(|c| (c.chapter_no, c.title.clone()))
            .collect();

        let refined_embedding = match st.embedder.embed_one(&refined).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = format!("{e:#}"), "failed to embed refined question");
                yield AdvisorEvent::Error { message: "the assistant hit an internal error".into() };
                return;
            }
        };

        // Memory retrieval (spec agent 10) — no LLM, embed + cosine top-k.
        let memory_context = if st.cfg.memory_enabled {
            yield AdvisorEvent::Phase { phase: "recalling" };
            match memory::retrieve_memories(
                &st.pool,
                &refined_embedding,
                st.cfg.memory_top_k,
                st.cfg.memory_distance_threshold,
            )
            .await
            {
                Ok(m) => {
                    // Retrieval proof (SSE): count + nearest distance, BEFORE rendering (render
                    // borrows, doesn't consume). `None` distance ⇒ nothing cleared the cutoff.
                    yield AdvisorEvent::Memory {
                        recalled: m.len() as i64,
                        nearest_distance: m.first().map(|x| x.distance),
                    };
                    memory::render_memory_context(&m)
                }
                Err(e) => {
                    // Best-effort: advise without memory rather than fail the turn. Still emit the
                    // event (recalled=0) so a retrieval error reads as "no recall" downstream
                    // rather than looking like an old binary that never shipped the event.
                    tracing::warn!(error = format!("{e:#}"), "memory retrieval failed");
                    yield AdvisorEvent::Memory { recalled: 0, nearest_distance: None };
                    String::new()
                }
            }
        } else {
            String::new()
        };

        // Semantic candidate-widening for the router (anti-tunnel-vision).
        let semantic_candidates: Vec<i32> = match books::nearest_chunks(
            &st.pool,
            book_id,
            &refined_embedding,
            st.cfg.route_semantic_top_k,
            None,
        )
        .await
        {
            Ok(hits) => {
                let mut nos: Vec<i32> = Vec::new();
                for h in hits {
                    if !nos.contains(&h.chapter_no) {
                        nos.push(h.chapter_no);
                    }
                }
                nos
            }
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "semantic candidate search failed");
                Vec::new()
            }
        };

        // ---- the bounded refine loop ----------------------------------------------
        let mut used: Vec<i32> = Vec::new();
        let mut draft: Option<String> = None;
        let mut last_note = String::new();
        for iteration in 0..st.cfg.max_refine_iters.max(1) {
            yield AdvisorEvent::Phase { phase: "routing" };
            let proposed = match st
                .llm
                .route_chapters(
                    &refined,
                    draft.as_deref(),
                    &memory_context,
                    &cards_text,
                    &semantic_candidates,
                    &used,
                    max_chapter,
                    st.cfg.max_chapters_per_route,
                )
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(error = format!("{e:#}"), "chapter routing failed");
                    Vec::new()
                }
            };
            // Only chapters that actually exist in the corpus (a hallucinated in-range
            // number would otherwise become a phantom citation with no text behind it).
            let mut fresh: Vec<i32> = proposed
                .into_iter()
                .filter(|n| titles.contains_key(n) && !used.contains(n))
                .collect();
            if used.is_empty() && fresh.is_empty() {
                // First routing produced nothing usable: fall back to the semantic
                // candidates so the consultation still gets grounded advice.
                fresh = semantic_candidates.iter().take(2).copied().collect();
            }
            let before = used.len();
            for n in fresh {
                if used.len() >= st.cfg.max_total_chapters {
                    break;
                }
                used.push(n);
            }
            // CONVERGED when the accumulated set stopped GROWING — not merely when the
            // router proposed nothing. Once max_total_chapters is saturated, proposals
            // are dropped on the floor, and re-drafting the identical grounding at
            // temp 0 reproduces the same draft verbatim (pure wasted wall-clock).
            if iteration > 0 && used.len() == before {
                break;
            }
            if used.is_empty() {
                yield AdvisorEvent::Error {
                    message: "I couldn't match your situation to the book — try rephrasing it".into(),
                };
                return;
            }
            let mut cited = used.clone();
            cited.sort_unstable();
            let chapter_refs: Vec<ChapterRef> = cited
                .iter()
                .map(|n| ChapterRef { no: *n, title: titles.get(n).cloned().flatten() })
                .collect();
            yield AdvisorEvent::Chapters { iteration, chapters: chapter_refs };

            yield AdvisorEvent::Phase { phase: "drafting" };
            let chapters_block = match render_chapters_block(&st, book_id, &used, &refined_embedding).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(error = format!("{e:#}"), "loading chapter texts failed");
                    yield AdvisorEvent::Error { message: "the assistant hit an internal error".into() };
                    return;
                }
            };
            match st.llm.draft_answer(&refined, &chapters_block, &memory_context).await {
                Ok(d) => draft = Some(d),
                Err(e) => {
                    tracing::error!(error = format!("{e:#}"), "draft failed");
                    yield AdvisorEvent::Error { message: "the assistant hit an internal error".into() };
                    return;
                }
            }

            yield AdvisorEvent::Phase { phase: "reviewing" };
            let c = st.llm.critique(&refined, draft.as_deref().unwrap_or("")).await;
            last_note = c.note.clone();
            // An OK verdict exits only after at least one re-route has had the chance to
            // widen the grounding (the spec's loop-back-to-Traffic-Controller behaviour).
            if c.ok && iteration > 0 {
                break;
            }
        }
        let Some(draft) = draft else {
            yield AdvisorEvent::Error { message: "the assistant hit an internal error".into() };
            return;
        };
        let mut cited = used.clone();
        cited.sort_unstable();
        let chapter_refs: Vec<ChapterRef> = cited
            .iter()
            .map(|n| ChapterRef { no: *n, title: titles.get(n).cloned().flatten() })
            .collect();

        // ---- polish + stream the final answer -------------------------------------
        yield AdvisorEvent::Phase { phase: "polishing" };
        // Box the opaque stream (`.boxed()` = Pin<Box<dyn Stream>>) so `.next().await`
        // works without an Unpin bound — same as hushai-rag's chat handler.
        let stream_res = st
            .llm
            .polish_stream(&refined, &draft, &last_note, None)
            .await
            .map(StreamExt::boxed);
        let mut answer = String::new();
        match stream_res {
            Ok(mut token_stream) => {
                while let Some(item) = token_stream.next().await {
                    match item {
                        Ok(delta) => {
                            answer.push_str(&delta);
                            yield AdvisorEvent::Token { delta };
                        }
                        Err(e) => {
                            tracing::error!(error = format!("{e:#}"), "advisor token stream failed");
                            yield AdvisorEvent::Error { message: "the assistant hit an internal error".into() };
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = format!("{e:#}"), "advisor stream setup failed");
                yield AdvisorEvent::Error { message: "the assistant hit an internal error".into() };
                return;
            }
        }
        if answer.trim().is_empty() {
            // A degenerate polish must not lose the substance: deliver the draft.
            answer = draft.clone();
            yield AdvisorEvent::Token { delta: answer.clone() };
        }

        let message_id = match crate::chat::insert_final_answer(
            &st.pool,
            ctx.session_id,
            &answer,
            &chapter_refs,
        )
        .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(error = format!("{e:#}"), "failed to persist advisor answer");
                yield AdvisorEvent::Error { message: "failed to save the answer".into() };
                return;
            }
        };

        // ---- memory (spec agent 9) — best-effort, never fails the turn ------------
        if st.cfg.memory_enabled {
            yield AdvisorEvent::Phase { phase: "memorizing" };
            match st.llm.summarize_qa(&refined, &answer).await {
                Ok(summary) => {
                    if let Err(e) = memory::store_memory(
                        &st.pool,
                        &st.embedder,
                        &st.cfg.embed_model,
                        ctx.session_id,
                        &refined,
                        &summary,
                        &chapter_refs,
                    )
                    .await
                    {
                        tracing::warn!(error = format!("{e:#}"), "memory store failed");
                    }
                }
                Err(e) => tracing::warn!(error = format!("{e:#}"), "memory summary failed"),
            }
        }

        yield AdvisorEvent::Done { message_id };
    }
}

/// Render the accumulated chapters into the draft prompt's grounded block, enforcing the
/// per-chapter and total character budgets. An over-budget chapter is represented by its
/// nearest chunks to the refined question instead of its full text.
async fn render_chapters_block(
    st: &AppState,
    book_id: Uuid,
    used: &[i32],
    refined_embedding: &[f32],
) -> anyhow::Result<String> {
    let chapters = books::load_chapters(&st.pool, book_id, used).await?;
    let mut out = String::new();
    for ch in chapters {
        if out.len() >= st.cfg.context_max_total_chars {
            tracing::warn!(
                chapter_no = ch.chapter_no,
                "total chapter budget exhausted; dropping chapter from the draft prompt"
            );
            break;
        }
        let header = format!(
            "=== Chapter {}: {} ===\n",
            ch.chapter_no,
            ch.title.as_deref().unwrap_or("(untitled)")
        );
        let body = if ch.clean_text.len() > st.cfg.chapter_max_chars {
            let hits = books::nearest_chunks(
                &st.pool,
                book_id,
                refined_embedding,
                3,
                Some(ch.chapter_no),
            )
            .await?;
            let excerpts: Vec<String> = if hits.is_empty() {
                // Semantic search found nothing IN this chapter (it was routed on its
                // synopsis) — fall back to its opening chunks rather than rendering a
                // chapter header with an empty body under it.
                books::first_chunks(&st.pool, book_id, ch.chapter_no, 3).await?
            } else {
                hits.into_iter().map(|h| h.content).collect()
            };
            excerpts.join("\n[...]\n")
        } else {
            ch.clean_text
        };
        let remaining = st.cfg.context_max_total_chars.saturating_sub(out.len());
        out.push_str(&header);
        if body.len() > remaining {
            out.push_str(truncate_on_char_boundary(&body, remaining));
        } else {
            out.push_str(&body);
        }
        out.push_str("\n\n");
    }
    Ok(out)
}

fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::truncate_on_char_boundary;

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_on_char_boundary("hello", 10), "hello");
        assert_eq!(truncate_on_char_boundary("hello", 3), "hel");
        // Multi-byte: never panics mid-codepoint.
        let s = "héllo";
        let t = truncate_on_char_boundary(s, 2);
        assert!(s.starts_with(t));
        assert!(t.len() <= 2);
    }
}
