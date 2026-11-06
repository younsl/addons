//! The kuo controller binary.

#[tokio::main]
async fn main() {
    kuo::start().await;
}
