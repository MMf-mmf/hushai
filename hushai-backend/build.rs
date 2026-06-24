//! Compiles the wire contract proto (`hushai.v1.SegmentManifest`) with prost-build.
//!
//! The generated Rust lands in `$OUT_DIR/hushai.v1.rs` and is pulled in by
//! `src/proto.rs` via `include!`. This same `.proto` is compiled by the Android
//! app via Square Wire — the wire shape must never diverge (contract §4/§8).
//!
//! Requires `protoc` on PATH (or the `PROTOC` env var pointing at it).

fn main() {
    let proto = "proto/hushai/v1/segment.proto";
    println!("cargo:rerun-if-changed={proto}");
    println!("cargo:rerun-if-changed=proto");

    prost_build::compile_protos(&[proto], &["proto"])
        .expect("failed to compile proto/hushai/v1/segment.proto with prost-build");
}
