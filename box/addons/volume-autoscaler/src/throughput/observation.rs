//! One node's fully gathered input, before the decision.

use super::decide::Input;
use crate::awsx::Volume;
use crate::k8s::nodes::Node;

/// One node's fully gathered input, before the decision.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Observation {
    pub node: Node,
    pub volume: Volume,
    pub input: Input,
    /// Distinguishes a node the query returned no series for from one whose
    /// measured peak is genuinely zero.
    pub has_metrics: bool,
    /// The reason this node cannot be evaluated, empty when it can.
    pub blocked: String,
}
