# Security policy

Hushai captures and stores audio, video, transcripts, voiceprints, and face and plate
crops of real people. Security and privacy reports are taken seriously.

## Reporting a vulnerability

Report privately — please do not open a public issue for anything that could expose user
data. Use GitHub's **Report a vulnerability** button (Security → Advisories) on this
repository. Include the affected component, the commit you tested, and steps to reproduce.
You'll get an acknowledgement, and we'll agree a fix and disclosure timeline with you.

## Threat model

Hushai is designed for a **single owner running it on their own hardware, on their own
LAN**. It is not multi-tenant and has no notion of organizations, roles, or per-user
scoping. Captured media never leaves the machine: ASR runs through whisper.cpp, embeddings
and LLM inference through a local Ollama server, vision and TTS through local ONNX models.
There is no telemetry and no cloud dependency.

What that means in practice: **everyone who can authenticate to a Hushai deployment is
assumed to be the owner.** If that assumption doesn't hold for your deployment, the gaps
below matter to you.

## What is protected

- **Transport.** Run with `./local_dev/run_stack.sh --tls` for HTTPS across the stack
  (rustls, no OpenSSL, no native-tls anywhere in the dependency tree). `./local_dev/serve.sh`
  provisions a local CA and a trusted LAN certificate on macOS.
- **The admin UI** (`hushai-viewer`, `:8070`) binds to `127.0.0.1` by default and is gated
  by an argon2 password (`VIEWER_ADMIN_PASSWORD_HASH`), an IP allowlist
  (`VIEWER_ADMIN_IP_ALLOWLIST`), and an HMAC-SHA256 session cookie. Token comparisons use
  constant-time equality.
- **Per-camera tokens.** `./local_dev/run_stack.sh --add-camera <name>` mints a distinct
  token per device (`openssl rand -hex 32`) rather than sharing one.
- **The RAG service fails closed.** It refuses to start unauthenticated on a non-loopback
  bind unless `RAG_ALLOW_INSECURE=true` is set explicitly.
- **Mutations through the admin UI are audited** to an `audit_log` table.

## Known gaps

These are real, understood, and deliberately unfixed for now — they are architectural
decisions rather than oversights, and they are listed here so you can judge the risk for
your own deployment.

- **A capture-device token is also an admin token.** One middleware gates both
  `POST /v1/segments` (every camera) and the destructive admin API — device deletion,
  footage purge, alert-rule creation (a webhook sink), watchlist, audit read. Tokens carry
  no scope or role. A LAN host holding or sniffing a camera token can therefore delete
  footage or install an exfiltrating webhook by talking to the backend directly, bypassing
  the viewer's password entirely. **Mitigation: keep `:8080` off untrusted networks, and
  mint per-camera tokens instead of sharing one.**
- **Segment attribution trusts the client manifest.** `segments.device_id` is written from
  the uploaded manifest, not derived from the authenticated token, so a token holder can
  stamp another device's ID. This affects storage accounting, retention, and teardown
  scoping.
- **Direct backend calls are not audited.** The `audit_log` is written at the viewer
  gateway, so a request made straight to `:8080` with a device token leaves no entry.
- **Token revocation requires a restart** (`DEVICE_TOKENS` edit + restart). There is no
  hot revocation path yet.
- **Identity thresholds are uncalibrated.** Speaker, face, and plate matching thresholds
  are starting guesses tuned against clean-room audio and clean photographs. On noisy real
  captures they will produce both false merges and false splits. Treat every identity
  Hushai asserts as a hint, never as evidence.

`AGENTS.md` ("Known gaps / TODO") and `Issues/unfinished/` track these alongside the
non-security work.

## Responsible use

Recording people has legal and ethical constraints that vary by jurisdiction — one-party
versus all-party consent for audio, notice requirements for video, and separate rules again
for biometric data such as faceprints and voiceprints. Hushai gives you face recognition,
voice identification, and licence-plate reading in one box; that combination is regulated
in many places. **You are responsible for operating it lawfully.** Please don't point it at
people who haven't agreed to it.
