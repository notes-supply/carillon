#[tokio::main]
async fn main() -> anyhow::Result<()> {
    carillon_discord::application::run().await
}
