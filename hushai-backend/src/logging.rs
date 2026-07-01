//! Centralized, production-grade logging init — shared by every binary in the workspace.
//!
//! The worker, RAG, and viewer all depend on `hushai-backend` as a library, so they delegate
//! their `init_tracing()` here instead of each re-implementing `tracing_subscriber::fmt()`.
//! One place to configure means one place to make production-ready:
//!
//! - **Format** (`LOG_FORMAT`): `text` (default, human-readable) or `json` (one object per line,
//!   for log aggregators — ELK / Loki / CloudWatch). JSON includes the active span fields
//!   (notably `request_id`) so a line is self-describing.
//! - **On-disk trail** (`LOG_DIR`): when set, logs are *also* written to a daily-rotated file
//!   `<LOG_DIR>/<service>.log` through a non-blocking background writer — a durable record that
//!   survives the process, which stdout-only logging does not. stdout is always kept too.
//! - **Filter** (`RUST_LOG`): unchanged `EnvFilter` semantics; each crate passes its own default.
//! - **Panic capture**: a process-wide panic hook routes panics through `tracing::error!` (with
//!   location + backtrace) so a crash leaves a structured record instead of a bare stderr dump.
//!
//! It is idempotent: repeated calls (the test suites init per-test) are no-ops after the first.

use std::sync::OnceLock;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, Response};
use tracing::Span;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// Keeps the non-blocking file appender's background worker alive for the whole process. A
/// dropped guard would stop flushing (and lose buffered lines), so we stash it for the program's
/// lifetime; the `OnceLock` also enforces a single file sink even if `init` is somehow re-entered.
static FILE_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Initialise tracing for `service`, honouring `RUST_LOG` / `LOG_FORMAT` / `LOG_DIR`.
///
/// `default_filter` is the crate's fallback `EnvFilter` when `RUST_LOG` is unset (preserving each
/// service's historical default). Safe to call repeatedly — only the first call wins.
pub fn init(service: &str, default_filter: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let json = std::env::var("LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    // stdout sink. Boxed so the json/text branches unify to one type.
    let stdout_layer = if json {
        fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .with_target(true)
            .with_line_number(true)
            .boxed()
    } else {
        fmt::layer().with_target(true).with_line_number(true).boxed()
    };

    // Optional daily-rotated file sink (kept alongside stdout). Empty/unset LOG_DIR ⇒ no file.
    let file_layer = match std::env::var("LOG_DIR") {
        Ok(dir) if !dir.trim().is_empty() => {
            let appender = tracing_appender::rolling::daily(dir.trim(), format!("{service}.log"));
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let _ = FILE_GUARD.set(guard);
            let layer = if json {
                fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(true)
                    .with_writer(writer)
                    .boxed()
            } else {
                // ANSI off: a log file should hold plain text, not terminal colour codes.
                fmt::layer()
                    .with_ansi(false)
                    .with_target(true)
                    .with_line_number(true)
                    .with_writer(writer)
                    .boxed()
            };
            Some(layer)
        }
        _ => None,
    };

    let started = tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(file_layer)
        .try_init()
        .is_ok();

    // Only arm the panic hook once we actually own the global subscriber (so we don't install a
    // hook that logs into a subscriber some other init owns — e.g. a second call inside tests).
    if started {
        install_panic_hook();
        tracing::debug!(service, json_format = json, "logging initialised");
    }
}

/// Route panics through tracing so a crash is captured in the same (possibly JSON, possibly
/// file-backed) log stream as everything else. `Backtrace::capture()` is populated when
/// `RUST_BACKTRACE=1` (or `full`); otherwise the field is the cheap "disabled" marker.
fn install_panic_hook() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.set(()).is_err() {
        return;
    }
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let backtrace = std::backtrace::Backtrace::capture();
        tracing::error!(
            panic.message = %message,
            panic.location = %location,
            backtrace = %backtrace,
            "thread panicked"
        );
    }));
}

/// A short, secret-free description of the active log sinks for a service's startup banner
/// (e.g. `format=json dir=./logs`). Never includes credentials — safe to log verbatim.
pub fn summary() -> String {
    let fmt = if std::env::var("LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false)
    {
        "json"
    } else {
        "text"
    };
    match std::env::var("LOG_DIR") {
        Ok(d) if !d.trim().is_empty() => format!("format={fmt} dir={}", d.trim()),
        _ => format!("format={fmt} dir=<stdout-only>"),
    }
}

// ---------------------------------------------------------------------------
// HTTP request tracing — shared by backend / rag / viewer TraceLayers
// ---------------------------------------------------------------------------

/// Open one span per inbound request carrying a fresh `request_id`. Every log emitted while the
/// handler runs inherits this span, so a single request's lines are all correlatable by the id.
/// Concrete `Body` type (rather than generic) so it slots directly into `.make_span_with(...)`.
pub fn make_http_span(req: &Request<Body>) -> Span {
    tracing::info_span!(
        "http",
        request_id = %uuid::Uuid::now_v7(),
        method = %req.method(),
        path = %req.uri().path(),
    )
}

/// Log the completion of each request at INFO with status + latency. Pairs with
/// [`make_http_span`]; together they turn the previously-silent default `TraceLayer` into one
/// structured access line per request.
pub fn on_http_response(res: &Response<Body>, latency: Duration, _span: &Span) {
    tracing::info!(
        status = res.status().as_u16(),
        latency_ms = latency.as_millis() as u64,
        "request completed"
    );
}

#[cfg(test)]
mod tests {
    /// `init` must be safe to call repeatedly (the test suites init per-test, and a service could
    /// re-enter it) — the global subscriber + panic hook can only be set once. A second call is a
    /// no-op, never a panic. This also exercises the registry build + panic-hook install paths.
    #[test]
    fn init_is_idempotent_and_unwinding_still_works() {
        super::init("test-logging-a", "info");
        super::init("test-logging-b", "info,test_logging=debug"); // second call: must not panic

        // With the panic hook armed, a panic is still a normal unwind that `catch_unwind` recovers
        // (the hook logs it; it does not abort the process).
        let caught = std::panic::catch_unwind(|| panic!("intentional panic to exercise the hook"));
        assert!(caught.is_err());
    }
}
