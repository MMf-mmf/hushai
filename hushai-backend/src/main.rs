//! Thin binary entrypoint. All logic lives in the library crate so integration
//! tests can build the same router.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    hushai_backend::run().await
}
