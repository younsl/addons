//! The node-configuration-operator binary, serving both the controller and the agent.

#[tokio::main]
async fn main() {
    node_configuration_operator::start().await;
}
