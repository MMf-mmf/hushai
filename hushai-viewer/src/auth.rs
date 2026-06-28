//! Admin access control for the viewer — the viewer IS the admin panel, so the whole
//! app is gated by two stacked layers (outermost first):
//!
//! 1. [`ip_allowlist`] — only configured admin computer IP(s)/CIDRs (plus loopback)
//!    may reach ANY route. This MUST live here, not in the backend/rag: the viewer
//!    reverse-proxies `/v1/*` from its own process, so those services only ever see
//!    `127.0.0.1` and can't tell admin from non-admin LAN devices. TLS terminates
//!    in-process (no front proxy), so the transport peer IP is the real client — we
//!    never trust `X-Forwarded-For`.
//! 2. [`require_session`] — a signed (HMAC-SHA256) session cookie minted by `/login`
//!    after an argon2 password check. Stateless: no server-side session store.
//!
//! `/healthz` sits outside both; `/login` sits inside the IP gate but in front of the
//! password gate (so the login page is reachable pre-cookie). Wiring is in `routes.rs`.

use std::net::{IpAddr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{self, HeaderMap};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

use crate::config::ViewerConfig;
use crate::state::ViewerState;

type HmacSha256 = Hmac<Sha256>;

/// Name of the signed session cookie.
const COOKIE: &str = "hushai_admin";

const FALLBACK_LOGIN_HTML: &str = r#"<!doctype html><meta charset=utf-8>
<title>Hushai — sign in</title>
<form method=POST action=/login style="font-family:system-ui;max-width:20rem;margin:6rem auto">
<h2>Hushai</h2>
<input type=password name=password placeholder=Password autofocus
 style="width:100%;padding:.6rem;font-size:1rem">
<button style="margin-top:.8rem;padding:.6rem 1rem;font-size:1rem">Sign in</button>
</form>"#;

// --- Layer 1: IP allowlist ------------------------------------------------------

/// Middleware: reject any peer not in the admin allowlist with 403. Requires the
/// server to be built with `into_make_service_with_connect_info::<SocketAddr>()`
/// (the viewer serves via `hushai_backend::tls::serve_with_connect_info`).
pub async fn ip_allowlist(
    State(st): State<ViewerState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let ip = peer.ip();
    if ip_allowed(&st.cfg, ip) {
        Ok(next.run(req).await)
    } else {
        tracing::warn!(%ip, path = %req.uri().path(), "viewer: IP not in admin allowlist → 403");
        Err(StatusCode::FORBIDDEN)
    }
}

fn ip_allowed(cfg: &ViewerConfig, ip: IpAddr) -> bool {
    // Normalize IPv4-mapped IPv6 (e.g. `::ffff:192.168.1.10`) so a v4 client on a
    // dual-stack listener still matches a v4 CIDR.
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    if cfg.allow_loopback && ip.is_loopback() {
        return true;
    }
    cfg.admin_ip_allowlist.iter().any(|net| net.contains(&ip))
}

// --- Layer 2: password / session cookie ----------------------------------------

/// Middleware: pass requests carrying a valid session cookie; otherwise redirect
/// HTML navigations to `/login` and return a clean 401 to API/XHR/SSE (so JSON/SSE
/// bodies aren't corrupted by an HTML login page).
pub async fn require_session(State(st): State<ViewerState>, req: Request, next: Next) -> Response {
    if st.cfg.auth_disabled {
        return next.run(req).await;
    }
    if cookie_is_valid(&st.cfg, req.headers()) {
        return next.run(req).await;
    }
    if wants_html(req.headers()) {
        Redirect::to("/login").into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// Top-level browser navigations send `Accept: text/html`; fetch/XHR/SSE/hls.js don't.
fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("text/html"))
        .unwrap_or(false)
}

#[derive(Deserialize)]
pub struct LoginForm {
    password: String,
}

/// `GET /login` — serve the static login page (reachable pre-cookie).
pub async fn login_page(State(st): State<ViewerState>) -> Response {
    let path = st.cfg.ui_dir.join("login.html");
    match tokio::fs::read_to_string(&path).await {
        Ok(html) => Html(html).into_response(),
        Err(_) => Html(FALLBACK_LOGIN_HTML).into_response(),
    }
}

/// `POST /login` — verify the password, set the session cookie, redirect to `/`.
pub async fn login_submit(State(st): State<ViewerState>, Form(form): Form<LoginForm>) -> Response {
    if st.cfg.auth_disabled {
        return Redirect::to("/").into_response();
    }
    let ok = st
        .cfg
        .admin_password_hash
        .as_deref()
        .map(|hash| verify_password(&form.password, hash))
        .unwrap_or(false);
    if ok {
        let value = make_session_value(&st.cfg);
        let cookie = set_cookie_header(&st.cfg, &value, st.cfg.session_ttl_secs);
        redirect_with_cookie("/", &cookie)
    } else {
        Redirect::to("/login?error=1").into_response()
    }
}

/// `POST /logout` — clear the cookie and bounce to `/login`.
pub async fn logout(State(st): State<ViewerState>) -> Response {
    let cookie = set_cookie_header(&st.cfg, "", 0);
    redirect_with_cookie("/login", &cookie)
}

// --- helpers --------------------------------------------------------------------

fn verify_password(plain: &str, phc: &str) -> bool {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(plain.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sign(secret: &[u8], msg: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(msg.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn verify_tag(secret: &[u8], msg: &str, tag_b64: &str) -> bool {
    let Ok(tag) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(tag_b64) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(msg.as_bytes());
    mac.verify_slice(&tag).is_ok() // constant-time tag comparison
}

/// `v1.<issued_unix>.<base64url(hmac)>`
fn make_session_value(cfg: &ViewerConfig) -> String {
    let issued = now_unix();
    let msg = format!("v1.{issued}");
    let tag = sign(&cfg.session_secret, &msg);
    format!("{msg}.{tag}")
}

fn cookie_is_valid(cfg: &ViewerConfig, headers: &HeaderMap) -> bool {
    let Some(val) = read_cookie(headers, COOKIE) else {
        return false;
    };
    let mut parts = val.splitn(3, '.');
    let (Some("v1"), Some(issued), Some(tag)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let Ok(issued_n) = issued.parse::<i64>() else {
        return false;
    };
    if !verify_tag(&cfg.session_secret, &format!("v1.{issued_n}"), tag) {
        return false;
    }
    let now = now_unix();
    // Reject future timestamps (small clock-skew tolerance) and expired sessions.
    issued_n <= now + 60 && now - issued_n <= cfg.session_ttl_secs
}

fn set_cookie_header(cfg: &ViewerConfig, value: &str, max_age: i64) -> String {
    let mut c = format!("{COOKIE}={value}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}");
    if cfg.cookie_secure {
        c.push_str("; Secure");
    }
    c
}

fn redirect_with_cookie(location: &str, cookie: &str) -> Response {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, location)
        .header(header::SET_COOKIE, cookie)
        .body(Body::empty())
        .expect("static redirect response is valid")
}

/// Find a single cookie value in the `Cookie` request header.
fn read_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> Vec<u8> {
        b"a-very-long-test-secret-key-0123456789".to_vec()
    }

    #[test]
    fn sign_then_verify_roundtrips() {
        let msg = "v1.1700000000";
        let tag = sign(&secret(), msg);
        assert!(verify_tag(&secret(), msg, &tag));
        // Tampered message or wrong secret must fail.
        assert!(!verify_tag(&secret(), "v1.1700000001", &tag));
        assert!(!verify_tag(b"other-secret", msg, &tag));
        assert!(!verify_tag(&secret(), msg, "not-base64!!"));
    }

    #[test]
    fn read_cookie_picks_the_named_value() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "foo=1; hushai_admin=abc.def; bar=2".parse().unwrap());
        assert_eq!(read_cookie(&h, COOKIE), Some("abc.def"));
        assert_eq!(read_cookie(&h, "missing"), None);
    }
}
