use std::fs::File;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::LogFormat;

/// Logs go to a file only, because the TUI owns the terminal. No file means no logging.
pub fn init(path: Option<&Path>, level: &str, format: LogFormat) -> anyhow::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    // Dependency INFO logs include credential provider details such as access key ids.
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(format!("warn,{}={level}", env!("CARGO_CRATE_NAME"))))
        .context("invalid log level")?;
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(Mutex::new(file))
        .with_ansi(false);
    let registry = tracing_subscriber::registry().with(filter);
    match format {
        LogFormat::Json => registry.with(layer.json()).try_init(),
        LogFormat::Text => registry.with(layer).try_init(),
    }
    .context("installing tracing subscriber")
}
