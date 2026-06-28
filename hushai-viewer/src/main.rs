#[tokio::main]
async fn main() -> anyhow::Result<()> {
    hushai_viewer::run().await
}
