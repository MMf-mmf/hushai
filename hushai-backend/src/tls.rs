//! Native rustls TLS termination shared by all three Hushai services.
//!
//! [`serve`] / [`serve_with_connect_info`] replace `axum::serve(listener, app)`:
//! when TLS is configured they terminate rustls via `axum-server`; otherwise they
//! fall back to cleartext `axum::serve` (the localhost/USB dev path). Both honour the
//! same graceful-shutdown future the callers already pass (`shutdown_signal()`), so a
//! single Ctrl-C still drains in-flight requests and exits cleanly.
//!
//! Backend is a path dependency of `hushai-rag` and `hushai-viewer`, so this one
//! helper serves all three (they call `hushai_backend::tls::serve*`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;

/// Optional TLS material. `None` ⇒ serve cleartext (dev / USB / localhost).
#[derive(Debug, Clone)]
pub struct TlsPaths {
    /// PEM chain: leaf certificate first, then intermediates / the signing CA.
    pub cert: PathBuf,
    /// PEM private key (PKCS#8).
    pub key: PathBuf,
}

impl TlsPaths {
    /// Read `<PREFIX>TLS_CERT_PATH` + `<PREFIX>TLS_KEY_PATH`, falling back to the bare
    /// `TLS_CERT_PATH`/`TLS_KEY_PATH` so one shared cert/key (same SAN covers every
    /// service on one host) can be configured once and picked up by all three.
    ///
    /// Both-or-neither: `Ok(None)` when neither is set; an error if exactly one is.
    /// Use **absolute** paths in env — the backend runs with a different CWD than
    /// rag/viewer (see AGENTS.md "Run the full stack").
    pub fn from_env(prefix: &str) -> anyhow::Result<Option<Self>> {
        let pick = |suffix: &str| {
            std::env::var(format!("{prefix}{suffix}"))
                .ok()
                .or_else(|| std::env::var(suffix).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        match (pick("TLS_CERT_PATH"), pick("TLS_KEY_PATH")) {
            (Some(c), Some(k)) => Ok(Some(Self {
                cert: c.into(),
                key: k.into(),
            })),
            (None, None) => Ok(None),
            _ => anyhow::bail!(
                "{prefix}TLS_CERT_PATH and {prefix}TLS_KEY_PATH must be set together (both-or-neither)"
            ),
        }
    }
}

/// Install the aws-lc-rs rustls crypto provider exactly once per process. The
/// dependency tree already vendors aws-lc-rs (via sqlx `tls-rustls`); using it keeps
/// exactly one provider — a second `ring` provider would panic at the first handshake.
/// Idempotent and safe to call from tests.
fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

async fn load_tls(tls: &TlsPaths) -> anyhow::Result<RustlsConfig> {
    ensure_crypto_provider();
    RustlsConfig::from_pem_file(&tls.cert, &tls.key)
        .await
        .with_context(|| {
            format!(
                "loading TLS cert {} / key {}",
                tls.cert.display(),
                tls.key.display()
            )
        })
}

/// Bridge a shutdown *future* to an `axum-server` [`Handle`] (which uses an explicit
/// `graceful_shutdown(timeout)` call rather than a future), so the callers' existing
/// graceful-shutdown semantics survive the cleartext→TLS switch.
fn spawn_shutdown_bridge<F>(handle: Handle, shutdown: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        shutdown.await;
        handle.graceful_shutdown(Some(Duration::from_secs(10)));
    });
}

/// Serve `app` on `addr` until `shutdown` resolves — TLS when `tls` is `Some`, else
/// cleartext. Mirror of `axum::serve(listener, app).with_graceful_shutdown(shutdown)`.
pub async fn serve<F>(
    addr: SocketAddr,
    app: Router,
    tls: Option<TlsPaths>,
    shutdown: F,
) -> anyhow::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match tls {
        None => {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding {addr}"))?;
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
                .context("server error")?;
        }
        Some(tls) => {
            let cfg = load_tls(&tls).await?;
            let handle = Handle::new();
            spawn_shutdown_bridge(handle.clone(), shutdown);
            axum_server::bind_rustls(addr, cfg)
                .handle(handle)
                .serve(app.into_make_service())
                .await
                .context("tls server error")?;
        }
    }
    Ok(())
}

/// Like [`serve`], but builds the service with `ConnectInfo<SocketAddr>` so middleware
/// / handlers can read the peer IP (the viewer's IP allowlist needs this). Works the
/// same under cleartext and TLS — TLS terminates in-process, so the connection peer
/// address IS the real client (we never trust `X-Forwarded-For`: there's no proxy).
pub async fn serve_with_connect_info<F>(
    addr: SocketAddr,
    app: Router,
    tls: Option<TlsPaths>,
    shutdown: F,
) -> anyhow::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match tls {
        None => {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding {addr}"))?;
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown)
            .await
            .context("server error")?;
        }
        Some(tls) => {
            let cfg = load_tls(&tls).await?;
            let handle = Handle::new();
            spawn_shutdown_bridge(handle.clone(), shutdown);
            axum_server::bind_rustls(addr, cfg)
                .handle(handle)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .context("tls server error")?;
        }
    }
    Ok(())
}
