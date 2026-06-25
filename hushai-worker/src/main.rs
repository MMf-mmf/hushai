#[tokio::main]
async fn main() -> anyhow::Result<()> {
    hushai_worker::run().await
}
