//! The node-configuration-operator binary: the controller.

#[tokio::main]
async fn main() {
    node_configuration_operator::start().await;
}
