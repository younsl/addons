//! Security insights: a periodic analysis of every repository, artifact,
//! account, token and recent audit entry that turns them into a ranked list of
//! improvements for operators, security engineers and developers.

mod alarm;
mod collect;
mod engine;
mod model;
mod rules;
pub mod settings;

pub use alarm::alarm_report;
pub use engine::{Analyzer, Error};
pub use model::{
    AFFECTED_LIMIT, AFFECTED_PAGE, AckState, Acknowledgement, Affected, Audience, Category, Check,
    CheckStatus, Finding, InsightSummary, Inventory, Report, ScanCount, Severity, SeverityCounts,
};
pub use settings::Settings;
