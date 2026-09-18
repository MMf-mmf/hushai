//! Live-DB test for the chat session store (migration 0008). Gated on `DATABASE_URL`.
//! Exercises the DB round-trip — create session, append turns, persisted citation jsonb,
//! chronological + trailing-window history — WITHOUT the LLM/Ollama (that path is covered
//! by the end-to-end manual + curl SSE checks).

use sqlx::PgPool;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use hushai_rag::chat;
use hushai_rag::retrieve::Source;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .ok()?;
    // Ensure 0008 (and the rest) are applied before the test touches chat tables.
    sqlx::migrate!("../hushai-backend/migrations")
        .run(&pool)
        .await
        .ok()?;
    Some(pool)
}

fn fake_source(text: &str, start: i64) -> Source {
    // Spread over `Default` rather than naming every field: this helper only cares about the
    // four it sets, and an exhaustive initializer breaks the whole test binary every time
    // `Source` gains an enrichment field (it has twice — `visual_context`, `conversation_id`).
    Source {
        segment_id: Uuid::now_v7(),
        device_id: "test-cam".into(),
        text: text.into(),
        start_unix_nanos: start,
        distance: 0.12,
        ..Default::default()
    }
}

#[tokio::test]
async fn chat_session_roundtrip() {
    let Some(pool) = pool().await else {
        eprintln!("skipping chat_session_roundtrip: DATABASE_URL unset");
        return;
    };

    // Create a conversation bound to the default agent.
    let session_id = chat::create_session(&pool, "recordings", "What did we discuss?")
        .await
        .unwrap();

    // The immutable agent binding is readable back.
    assert_eq!(
        chat::session_agent(&pool, session_id)
            .await
            .unwrap()
            .as_deref(),
        Some("recordings")
    );

    // Append a user turn, then an assistant turn carrying a citation.
    chat::insert_message(
        &pool,
        session_id,
        "user",
        "What did we discuss?",
        None,
        "recordings",
    )
    .await
    .unwrap();
    let src = fake_source("the meeting is on tuesday", 1_700_000_000_000_000_000);
    let assistant_id = chat::insert_message(
        &pool,
        session_id,
        "assistant",
        "The meeting is on Tuesday.",
        Some(&[src.clone()]),
        "recordings",
    )
    .await
    .unwrap();

    // History comes back in chronological order (DESC fetch reversed).
    let history = chat::load_history(&pool, session_id, 10, 0).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].0, "user");
    assert_eq!(history[0].1, "What did we discuss?");
    assert_eq!(history[1].0, "assistant");
    assert_eq!(history[1].1, "The meeting is on Tuesday.");

    // The citation jsonb round-trips back to a Source with the right segment.
    let row = sqlx::query("SELECT sources FROM chat_messages WHERE message_id = $1")
        .bind(assistant_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let stored: sqlx::types::Json<Vec<Source>> = row.get("sources");
    assert_eq!(stored.0.len(), 1);
    assert_eq!(stored.0[0].segment_id, src.segment_id);
    assert_eq!(stored.0[0].start_unix_nanos, src.start_unix_nanos);

    // Trailing-window trimming: with 3 turns, a limit of 2 keeps the last two in order.
    chat::insert_message(
        &pool,
        session_id,
        "user",
        "And the time?",
        None,
        "recordings",
    )
    .await
    .unwrap();
    let trimmed = chat::load_history(&pool, session_id, 2, 0).await.unwrap();
    assert_eq!(trimmed.len(), 2);
    assert_eq!(trimmed[0].1, "The meeting is on Tuesday.");
    assert_eq!(trimmed[1].1, "And the time?");

    // Cleanup (cascades to chat_messages).
    sqlx::query("DELETE FROM chat_sessions WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();
}
