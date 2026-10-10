//! Logging setup with a filter that PUT /log-level can change at runtime.

use tracing_subscriber::{EnvFilter, Registry, fmt, prelude::*, reload};

use crate::config::LogFormat;

pub type LogFilterHandle = reload::Handle<EnvFilter, Registry>;

/// Install the global subscriber. `RUST_LOG` overrides `level`.
pub fn init(level: &str, format: LogFormat) -> LogFilterHandle {
    let (filter, handle) = reload::Layer::new(initial_filter(level));
    let registry = tracing_subscriber::registry().with(filter);
    match format {
        LogFormat::Json => registry
            .with(fmt::layer().json().flatten_event(true))
            .init(),
        LogFormat::Text => registry.with(fmt::layer().with_target(false)).init(),
    }
    handle
}

fn initial_filter(level: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_reloads_while_layer_lives() {
        let (layer, handle): (reload::Layer<EnvFilter, Registry>, LogFilterHandle) =
            reload::Layer::new(EnvFilter::new("info"));
        assert!(handle.reload(EnvFilter::new("debug")).is_ok());
        assert_eq!(
            handle.with_current(ToString::to_string).expect("current"),
            "debug"
        );
        drop(layer);
        assert!(handle.reload(EnvFilter::new("warn")).is_err());
        assert_ne!(initial_filter("trace").to_string(), "");
    }
}
