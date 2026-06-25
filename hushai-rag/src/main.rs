#[tokio::main]
async fn main() -> anyhow::Result<()> {
    hushai_rag::run().await
}
