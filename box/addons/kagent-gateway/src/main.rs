#[tokio::main]
async fn main() -> anyhow::Result<()> {
    kagent_gateway::start().await
}
