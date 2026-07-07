#[tokio::main]
async fn main() -> anyhow::Result<()> {
    hushai_advisor::run().await
}
